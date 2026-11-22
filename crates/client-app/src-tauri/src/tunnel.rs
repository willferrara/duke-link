use std::net::SocketAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};
use shared_protocol::allowlist::InviteTicket;
use shared_protocol::wire::{
    read_json_frame, write_json_frame, ServerStatusSnapshot, StatusRequest, StatusResponse,
    ALPN_MC_TUNNEL, ALPN_STATUS, MAX_CONTROL_FRAME_BYTES, MAX_CRASH_LOG_BYTES,
};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Transport path classification reported to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPathKind {
    Direct,
    Relay,
    Unknown,
}

/// Live server + tunnel telemetry snapshot returned to the frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelStatusSnapshot {
    pub connected: bool,
    pub local_bind_addr: String,
    pub server_name: String,
    pub server_endpoint_id: String,
    pub client_endpoint_id: String,
    pub path_kind: ConnectionPathKind,
    pub rtt_ms: Option<u64>,
    pub server_online: bool,
    pub players_online: u32,
    pub max_players: u32,
    pub motd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Typed errors for Iroh P2P tunneling, status polling, and crash-log uploads.
#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("invalid relay URL '{url}': {message}")]
    InvalidRelayUrl { url: String, message: String },
    #[error("failed to bind iroh client endpoint: {0}")]
    EndpointBind(String),
    #[error("failed to bind local TCP listener at {addr}: {source}")]
    TcpBind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("QUIC connect failed: {0}")]
    Connect(String),
    #[error("QUIC stream error: {0}")]
    Stream(String),
    #[error("wire framing error: {0}")]
    Wire(#[from] shared_protocol::wire::WireError),
    #[error("remote relay returned error: {0}")]
    Remote(String),
}

/// Converts a decoded [`InviteTicket`] into an `iroh 1.5.0` [`EndpointAddr`].
pub fn build_endpoint_addr(ticket: &InviteTicket) -> Result<EndpointAddr, TunnelError> {
    let mut addr = EndpointAddr::new(ticket.server_endpoint_id);
    if let Some(ref raw_url) = ticket.relay_url {
        let trimmed = raw_url.trim();
        if !trimmed.is_empty() {
            let relay_url =
                RelayUrl::from_str(trimmed).map_err(|err| TunnelError::InvalidRelayUrl {
                    url: raw_url.clone(),
                    message: err.to_string(),
                })?;
            addr = addr.with_relay_url(relay_url);
        }
    }
    Ok(addr)
}

/// Active local TCP -> QUIC `mc-tunnel/1` proxy handle.
pub struct TunnelHandle {
    local_addr: SocketAddr,
    endpoint: Endpoint,
    server_addr: EndpointAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    accept_task: JoinHandle<()>,
}

impl TunnelHandle {
    /// Local loopback address where Minecraft connects (e.g. `127.0.0.1:25565`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Underlying `iroh 1.5.0` client endpoint (shared with `sync/1` and `status/1`).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Target server [`EndpointAddr`].
    pub fn server_addr(&self) -> &EndpointAddr {
        &self.server_addr
    }

    /// Tears down the local TCP listener and closes the Iroh endpoint cleanly.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.accept_task.await;
        self.endpoint.close().await;
    }
}

/// Starts a local TCP listener on `local_bind` (e.g. `127.0.0.1:25565`) that forwards every
/// connection over `duke-link/mc-tunnel/1` to `server_addr`.
pub async fn start_tunnel_to_addr(
    secret_key: SecretKey,
    server_addr: EndpointAddr,
    local_bind: &str,
) -> Result<TunnelHandle, TunnelError> {
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await
        .map_err(|err| TunnelError::EndpointBind(err.to_string()))?;

    let listener = TcpListener::bind(local_bind)
        .await
        .map_err(|source| TunnelError::TcpBind {
            addr: local_bind.to_owned(),
            source,
        })?;
    let local_addr = listener
        .local_addr()
        .map_err(|source| TunnelError::TcpBind {
            addr: local_bind.to_owned(),
            source,
        })?;

    info!(
        client_id = %endpoint.id(),
        %local_addr,
        server_id = %server_addr.id,
        "started local mc-tunnel/1 TCP proxy"
    );

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let ep_clone = endpoint.clone();
    let target_clone = server_addr.clone();

    let accept_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    debug!("mc-tunnel/1 listener shutting down");
                    break;
                }
                accepted = listener.accept() => {
                    let (mut tcp, peer_addr) = match accepted {
                        Ok(pair) => pair,
                        Err(err) => {
                            warn!(error = %err, "local TCP accept failed");
                            continue;
                        }
                    };

                    let ep = ep_clone.clone();
                    let target = target_clone.clone();
                    tokio::spawn(async move {
                        let conn = match ep.connect(target, ALPN_MC_TUNNEL).await {
                            Ok(c) => c,
                            Err(err) => {
                                warn!(%peer_addr, error = %err, "failed to dial relay mc-tunnel/1");
                                return;
                            }
                        };

                        let (mut send, mut recv) = match conn.open_bi().await {
                            Ok(streams) => streams,
                            Err(err) => {
                                warn!(%peer_addr, error = %err, "failed to open QUIC bi-stream");
                                return;
                            }
                        };

                        let (mut tcp_read, mut tcp_write) = tcp.split();
                        let local_to_remote = async {
                            let res = tokio::io::copy(&mut tcp_read, &mut send).await;
                            let _ = send.finish();
                            res
                        };
                        let remote_to_local = async {
                            let res = tokio::io::copy(&mut recv, &mut tcp_write).await;
                            let _ = tcp_write.shutdown().await;
                            res
                        };

                        if let Err(err) = tokio::try_join!(local_to_remote, remote_to_local) {
                            debug!(%peer_addr, error = %err, "mc-tunnel/1 proxy stream ended");
                        }
                    });
                }
            }
        }
    });

    Ok(TunnelHandle {
        local_addr,
        endpoint,
        server_addr,
        shutdown_tx: Some(shutdown_tx),
        accept_task,
    })
}

/// Polls `duke-link/status/1` for server status, measuring RTT and classifying Direct vs Relay.
pub async fn poll_status(
    endpoint: &Endpoint,
    server_addr: &EndpointAddr,
) -> Result<(ServerStatusSnapshot, ConnectionPathKind, Option<u64>), TunnelError> {
    let start = Instant::now();
    let conn = tokio::time::timeout(
        Duration::from_secs(8),
        endpoint.connect(server_addr.clone(), ALPN_STATUS),
    )
    .await
    .map_err(|_| TunnelError::Connect("timed out connecting to status/1".to_owned()))?
    .map_err(|err| TunnelError::Connect(err.to_string()))?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|err| TunnelError::Stream(err.to_string()))?;

    write_json_frame(&mut send, &StatusRequest::QueryServerStatus).await?;
    let _ = send.finish();

    let resp: StatusResponse = read_json_frame(&mut recv, MAX_CONTROL_FRAME_BYTES).await?;
    let rtt_ms = u64::try_from(start.elapsed().as_millis()).ok();

    let mut path_kind = ConnectionPathKind::Unknown;
    for path in conn.paths().iter() {
        if path.is_selected() {
            if path.is_ip() {
                path_kind = ConnectionPathKind::Direct;
            } else if path.is_relay() {
                path_kind = ConnectionPathKind::Relay;
            }
            break;
        }
    }

    conn.close(0u32.into(), b"status done");

    match resp {
        StatusResponse::ServerStatus(snapshot) => Ok((snapshot, path_kind, rtt_ms)),
        StatusResponse::Error { message } => Err(TunnelError::Remote(message)),
        StatusResponse::CrashLogAccepted { .. } => {
            Err(TunnelError::Remote("unexpected status response".to_owned()))
        }
    }
}

/// Uploads a client crash log (capped safely at 2 MiB) over `duke-link/status/1`.
pub async fn upload_crash_log(
    endpoint: &Endpoint,
    server_addr: &EndpointAddr,
    filename_hint: &str,
    payload: &str,
) -> Result<(String, usize), TunnelError> {
    let capped = cap_utf8_to_bytes(payload, MAX_CRASH_LOG_BYTES);

    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(server_addr.clone(), ALPN_STATUS),
    )
    .await
    .map_err(|_| TunnelError::Connect("timed out connecting to status/1".to_owned()))?
    .map_err(|err| TunnelError::Connect(err.to_string()))?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|err| TunnelError::Stream(err.to_string()))?;

    write_json_frame(
        &mut send,
        &StatusRequest::UploadCrashLog {
            filename_hint: filename_hint.to_owned(),
            payload_utf8: capped.to_owned(),
        },
    )
    .await?;
    let _ = send.finish();

    let resp: StatusResponse = read_json_frame(&mut recv, MAX_CONTROL_FRAME_BYTES).await?;
    conn.close(0u32.into(), b"crash upload done");

    match resp {
        StatusResponse::CrashLogAccepted {
            stored_name,
            bytes_written,
        } => Ok((stored_name, bytes_written)),
        StatusResponse::Error { message } => Err(TunnelError::Remote(message)),
        StatusResponse::ServerStatus(_) => Err(TunnelError::Remote(
            "unexpected crash upload response".to_owned(),
        )),
    }
}

fn cap_utf8_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}
