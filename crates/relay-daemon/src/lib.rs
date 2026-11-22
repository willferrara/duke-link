pub mod handlers;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iroh::endpoint::{presets, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use shared_protocol::allowlist::Allowlist;
use shared_protocol::wire::{ALPN_MC_TUNNEL, ALPN_STATUS, ALPN_SYNC};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::handlers::{handle_mc_tunnel, handle_status, handle_sync, RelayConfig};

/// Typed errors for `relay-daemon` startup, key persistence, and runtime orchestration.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid secret key file {path}: expected 32 bytes, found {len}")]
    InvalidKeyLength { path: PathBuf, len: usize },
    #[error("allowlist error: {0}")]
    Allowlist(#[from] shared_protocol::allowlist::AllowlistError),
    #[error("failed to bind iroh endpoint: {0}")]
    Bind(String),
}

/// Loads an existing 32-byte Ed25519 secret key from `path`, or generates and persists a new one.
pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey, DaemonError> {
    if path.exists() {
        let bytes = fs::read(path).map_err(|source| DaemonError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let arr: [u8; 32] =
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| DaemonError::InvalidKeyLength {
                    path: path.to_path_buf(),
                    len: bytes.len(),
                })?;
        return Ok(SecretKey::from_bytes(&arr));
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| DaemonError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    let key = SecretKey::generate();
    fs::write(path, key.to_bytes()).map_err(|source| DaemonError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        let _ = fs::set_permissions(path, perms);
    }

    Ok(key)
}

/// Handle to a running in-process `relay-daemon` Iroh endpoint (usable by CLI and integration tests).
pub struct RunningRelay {
    endpoint: Endpoint,
    shutdown_tx: Option<oneshot::Sender<()>>,
    accept_task: JoinHandle<()>,
}

impl RunningRelay {
    /// Returns this relay's Ed25519 [`EndpointId`].
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Returns this relay's current [`EndpointAddr`] (direct addresses + relay URLs).
    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Gracefully stops the accept loop and closes the underlying Iroh endpoint.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.accept_task.await;
    }
}

/// Spawns a live `iroh 1.5.0` relay endpoint enforcing `allowlist` on every incoming QUIC connection.
pub async fn start_relay(
    secret_key: SecretKey,
    allowlist: Allowlist,
    relay_cfg: RelayConfig,
) -> Result<RunningRelay, DaemonError> {
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(vec![
            ALPN_MC_TUNNEL.to_vec(),
            ALPN_SYNC.to_vec(),
            ALPN_STATUS.to_vec(),
        ])
        .bind()
        .await
        .map_err(|err| DaemonError::Bind(err.to_string()))?;

    let allowlist = Arc::new(allowlist);
    let relay_cfg = Arc::new(relay_cfg);

    info!(
        endpoint_id = %endpoint.id(),
        allow_all = allowlist.allow_all,
        allowlist_count = allowlist.len(),
        mc_addr = %relay_cfg.mc_addr,
        pack_dir = %relay_cfg.pack_dir.display(),
        "duke-link relay-daemon listening"
    );

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let ep_clone = endpoint.clone();

    let accept_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("shutdown signal received, closing iroh endpoint");
                    ep_clone.close().await;
                    break;
                }
                incoming_opt = ep_clone.accept() => {
                    let Some(incoming) = incoming_opt else {
                        info!("iroh endpoint accept loop ended");
                        break;
                    };

                    let allowlist = Arc::clone(&allowlist);
                    let cfg = Arc::clone(&relay_cfg);
                    tokio::spawn(async move {
                        let conn = match incoming.await {
                            Ok(c) => c,
                            Err(err) => {
                                warn!(error = %err, "failed to complete incoming QUIC handshake");
                                return;
                            }
                        };

                        let peer = conn.remote_id();
                        if !allowlist.is_allowed(&peer) {
                            warn!(%peer, "rejected unauthorized EndpointId");
                            conn.close(VarInt::from_u32(403), b"peer not in allowlist");
                            return;
                        }

                        match conn.alpn() {
                            alpn if alpn == ALPN_MC_TUNNEL => handle_mc_tunnel(conn, cfg).await,
                            alpn if alpn == ALPN_SYNC => handle_sync(conn, cfg).await,
                            alpn if alpn == ALPN_STATUS => handle_status(conn, cfg).await,
                            other => {
                                warn!(
                                    %peer,
                                    alpn = ?String::from_utf8_lossy(other),
                                    "unsupported ALPN"
                                );
                                conn.close(VarInt::from_u32(404), b"unsupported alpn");
                            }
                        }
                    });
                }
            }
        }
    });

    Ok(RunningRelay {
        endpoint,
        shutdown_tx: Some(shutdown_tx),
        accept_task,
    })
}
