use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::endpoint::Connection;
use iroh::EndpointId;
use shared_protocol::manifest::{
    build_horizon_pack_manifest, compute_file_blake3, read_manifest, validate_relative_path,
    ManifestError, DEFAULT_HORIZON_REV,
};
use shared_protocol::wire::{
    read_json_frame, write_chunk_frame, write_json_frame, ServerStatusSnapshot, StatusRequest,
    StatusResponse, SyncRequest, SyncResponse, FILE_CHUNK_SIZE, MAX_CONTROL_FRAME_BYTES,
    MAX_CRASH_LOG_BYTES,
};
use thiserror::Error;
use tokio::fs::File as TokioFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Runtime configuration shared across all QUIC connection handlers.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub mc_addr: String,
    pub status_addr: String,
    pub pack_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub crash_dir: PathBuf,
}

/// Typed errors produced by relay daemon stream handlers.
#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("QUIC connection error: {0}")]
    Connection(#[from] iroh::endpoint::ConnectionError),
    #[error("wire framing error: {0}")]
    Wire(#[from] shared_protocol::wire::WireError),
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path escapes configured pack_dir")]
    PathTraversal,
    #[error("requested file does not exist")]
    NotFound,
}

/// Resolves a client-requested relative path strictly inside `pack_dir` using canonicalization.
pub fn resolve_safe_pack_path(pack_dir: &Path, requested: &str) -> Result<PathBuf, HandlerError> {
    let clean_rel = validate_relative_path(requested)?;
    let canonical_root = fs::canonicalize(pack_dir)?;
    let candidate = canonical_root.join(clean_rel);
    let canonical_candidate = fs::canonicalize(&candidate).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            HandlerError::NotFound
        } else {
            HandlerError::Io(err)
        }
    })?;

    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(HandlerError::PathTraversal);
    }
    if !canonical_candidate.is_file() {
        return Err(HandlerError::NotFound);
    }

    Ok(canonical_candidate)
}

/// Proxies every bidirectional QUIC stream on `mc-tunnel/1` to the local Horizon MC TCP server.
pub async fn handle_mc_tunnel(conn: Connection, cfg: Arc<RelayConfig>) {
    let peer = conn.remote_id();
    info!(%peer, mc_addr = %cfg.mc_addr, "accepted mc-tunnel/1 session");

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                debug!(%peer, error = %err, "mc-tunnel/1 connection closed");
                break;
            }
        };

        let target = cfg.mc_addr.clone();
        tokio::spawn(async move {
            let mut tcp = match TcpStream::connect(&target).await {
                Ok(stream) => stream,
                Err(err) => {
                    warn!(%peer, target = %target, error = %err, "failed to connect to local horizon-server");
                    let _ = send.finish();
                    return;
                }
            };

            let (mut tcp_read, mut tcp_write) = tcp.split();
            let client_to_server = async {
                let res = tokio::io::copy(&mut recv, &mut tcp_write).await;
                let _ = tcp_write.shutdown().await;
                res
            };
            let server_to_client = async {
                let res = tokio::io::copy(&mut tcp_read, &mut send).await;
                let _ = send.finish();
                res
            };

            match tokio::try_join!(client_to_server, server_to_client) {
                Ok((up, down)) => {
                    debug!(%peer, bytes_up = up, bytes_down = down, "mc-tunnel/1 stream finished");
                }
                Err(err) => {
                    debug!(%peer, error = %err, "mc-tunnel/1 stream ended with I/O error");
                }
            }
        });
    }
}

/// Handles `sync/1` manifest queries and canonicalized mod/client file downloads.
pub async fn handle_sync(conn: Connection, cfg: Arc<RelayConfig>) {
    let peer = conn.remote_id();
    info!(%peer, "accepted sync/1 session");

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                debug!(%peer, error = %err, "sync/1 connection closed");
                break;
            }
        };

        let cfg_clone = Arc::clone(&cfg);
        tokio::spawn(async move {
            if let Err(err) = handle_sync_stream(&mut send, &mut recv, &cfg_clone).await {
                warn!(%peer, error = %err, "sync/1 stream failed");
            }
            let _ = send.finish();
        });
    }
}

async fn handle_sync_stream(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    cfg: &RelayConfig,
) -> Result<(), HandlerError> {
    let req: SyncRequest = read_json_frame(recv, MAX_CONTROL_FRAME_BYTES).await?;
    match req {
        SyncRequest::GetManifest => {
            let manifest_res = read_manifest(&cfg.manifest_path)
                .or_else(|_| build_horizon_pack_manifest(&cfg.pack_dir, "dukes-horizon", "0.1.0"));
            match manifest_res {
                Ok(manifest) => {
                    write_json_frame(send, &SyncResponse::Manifest { manifest }).await?;
                }
                Err(err) => {
                    write_json_frame(
                        send,
                        &SyncResponse::Error {
                            message: format!("failed to load pack manifest: {err}"),
                        },
                    )
                    .await?;
                }
            }
        }
        SyncRequest::GetFile { path } => {
            let safe_path = match resolve_safe_pack_path(&cfg.pack_dir, &path) {
                Ok(p) => p,
                Err(err) => {
                    write_json_frame(
                        send,
                        &SyncResponse::Error {
                            message: err.to_string(),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };

            let meta = fs::metadata(&safe_path)?;
            let blake3_hex = compute_file_blake3(&safe_path)?;

            write_json_frame(
                send,
                &SyncResponse::FileHeader {
                    path,
                    size: meta.len(),
                    blake3: blake3_hex,
                },
            )
            .await?;

            let mut file = TokioFile::open(&safe_path).await?;
            let mut buf = vec![0u8; FILE_CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                write_chunk_frame(send, &buf[..n]).await?;
            }
            // EOF sentinel
            write_chunk_frame(send, &[]).await?;
        }
    }

    Ok(())
}

/// Handles `status/1` queries against local Horizon REST `/api/v1/status` and crash-log uploads.
pub async fn handle_status(conn: Connection, cfg: Arc<RelayConfig>) {
    let peer = conn.remote_id();
    info!(%peer, "accepted status/1 session");

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                debug!(%peer, error = %err, "status/1 connection closed");
                break;
            }
        };

        let cfg_clone = Arc::clone(&cfg);
        tokio::spawn(async move {
            if let Err(err) = handle_status_stream(peer, &mut send, &mut recv, &cfg_clone).await {
                warn!(%peer, error = %err, "status/1 stream failed");
            }
            let _ = send.finish();
        });
    }
}

async fn handle_status_stream(
    peer: EndpointId,
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    cfg: &RelayConfig,
) -> Result<(), HandlerError> {
    let req: StatusRequest = read_json_frame(recv, MAX_CONTROL_FRAME_BYTES).await?;
    match req {
        StatusRequest::QueryServerStatus => {
            let snapshot = query_horizon_status(&cfg.status_addr).await;
            write_json_frame(send, &StatusResponse::ServerStatus(snapshot)).await?;
        }
        StatusRequest::UploadCrashLog {
            filename_hint,
            payload_utf8,
        } => {
            if payload_utf8.len() > MAX_CRASH_LOG_BYTES {
                write_json_frame(
                    send,
                    &StatusResponse::Error {
                        message: format!("crash log exceeds {} byte limit", MAX_CRASH_LOG_BYTES),
                    },
                )
                .await?;
                return Ok(());
            }

            match save_crash_log(
                &cfg.crash_dir,
                &peer,
                &filename_hint,
                payload_utf8.as_bytes(),
            ) {
                Ok((stored_name, bytes_written)) => {
                    info!(%peer, %stored_name, bytes_written, "stored client crash log");
                    write_json_frame(
                        send,
                        &StatusResponse::CrashLogAccepted {
                            stored_name,
                            bytes_written,
                        },
                    )
                    .await?;
                }
                Err(err) => {
                    write_json_frame(
                        send,
                        &StatusResponse::Error {
                            message: format!("failed to persist crash log: {err}"),
                        },
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

/// Queries local Horizon REST `/api/v1/status` over HTTP/1.1 with a graceful offline fallback.
async fn query_horizon_status(status_addr: &str) -> ServerStatusSnapshot {
    let offline = ServerStatusSnapshot {
        online: false,
        players_online: 0,
        max_players: 0,
        motd: "Horizon server offline".to_owned(),
        version: "unknown".to_owned(),
        tps: None,
        horizon_rev: Some(DEFAULT_HORIZON_REV.to_owned()),
        raw_json: None,
    };

    let fetch = async {
        let mut stream = TcpStream::connect(status_addr).await.ok()?;
        let req = format!(
            "GET /api/v1/status HTTP/1.1\r\nHost: {status_addr}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.ok()?;

        let mut resp = Vec::new();
        let mut limited = stream.take(256 * 1024);
        limited.read_to_end(&mut resp).await.ok()?;

        let body_start = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|idx| idx + 4)?;
        let body = &resp[body_start..];
        let json: serde_json::Value = serde_json::from_slice(body).ok()?;

        let players_online = json
            .get("players_online")
            .or_else(|| json.pointer("/players/online"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let max_players = json
            .get("max_players")
            .or_else(|| json.pointer("/players/max"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let motd = json
            .get("motd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Horizon MC Server")
            .to_owned();
        let version = json
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("horizon 26.3")
            .to_owned();
        let tps = json
            .get("tps")
            .or_else(|| json.pointer("/performance/tps"))
            .and_then(serde_json::Value::as_f64);
        let horizon_rev = json
            .get("horizon_rev")
            .or_else(|| json.get("rev"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| Some(DEFAULT_HORIZON_REV.to_owned()));

        Some(ServerStatusSnapshot {
            online: true,
            players_online,
            max_players,
            motd,
            version,
            tps,
            horizon_rev,
            raw_json: Some(json),
        })
    };

    match tokio::time::timeout(Duration::from_secs(2), fetch).await {
        Ok(Some(snapshot)) => snapshot,
        _ => offline,
    }
}

fn sanitize_component(raw: &str, max_len: usize) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(max_len)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

/// Safely stores an uploaded crash log under `<crash_dir>/<peer>/<timestamp>.log`.
pub fn save_crash_log(
    crash_dir: &Path,
    peer: &EndpointId,
    filename_hint: &str,
    payload: &[u8],
) -> Result<(String, usize), std::io::Error> {
    let peer_dir_name = sanitize_component(&peer.to_string(), 64);
    let peer_dir = crash_dir.join(peer_dir_name);
    fs::create_dir_all(&peer_dir)?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hint = sanitize_component(filename_hint, 32);
    let file_name = if hint == "unknown" {
        format!("{ts}.log")
    } else {
        format!("{ts}_{hint}.log")
    };

    let dest = peer_dir.join(&file_name);
    fs::write(&dest, payload)?;
    Ok((file_name, payload.len()))
}
