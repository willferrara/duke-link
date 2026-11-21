use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::manifest::PackManifest;

/// ALPN for bidirectional Minecraft TCP-over-QUIC tunneling.
pub const ALPN_MC_TUNNEL: &[u8] = b"duke-link/mc-tunnel/1";

/// ALPN for pack manifest queries and mod/config file synchronization.
pub const ALPN_SYNC: &[u8] = b"duke-link/sync/1";

/// ALPN for server status polling and client crash-log uploads.
pub const ALPN_STATUS: &[u8] = b"duke-link/status/1";

/// Maximum permitted size for a single length-prefixed JSON control frame (16 MiB).
pub const MAX_CONTROL_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Maximum permitted size for an uploaded crash log (2 MiB).
pub const MAX_CRASH_LOG_BYTES: usize = 2 * 1024 * 1024;

/// Chunk size used when streaming file blobs over `sync/1` (64 KiB).
pub const FILE_CHUNK_SIZE: usize = 64 * 1024;

/// Typed errors for length-prefixed wire framing.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("I/O error on wire stream: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame size {size} exceeds maximum permitted {max}")]
    FrameTooLarge { size: usize, max: usize },
}

/// Request messages sent by clients over [`ALPN_SYNC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncRequest {
    GetManifest,
    GetFile { path: String },
}

/// Response messages returned by the relay daemon over [`ALPN_SYNC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncResponse {
    Manifest {
        manifest: PackManifest,
    },
    FileHeader {
        path: String,
        size: u64,
        blake3: String,
    },
    Error {
        message: String,
    },
}

/// Request messages sent by clients over [`ALPN_STATUS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StatusRequest {
    QueryServerStatus,
    UploadCrashLog {
        filename_hint: String,
        payload_utf8: String,
    },
}

/// Snapshot of the local Horizon MC server status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusSnapshot {
    pub online: bool,
    pub players_online: u32,
    pub max_players: u32,
    pub motd: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<serde_json::Value>,
}

/// Response messages returned by the relay daemon over [`ALPN_STATUS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StatusResponse {
    ServerStatus(ServerStatusSnapshot),
    CrashLogAccepted {
        stored_name: String,
        bytes_written: usize,
    },
    Error {
        message: String,
    },
}

/// Writes a 4-byte big-endian length-prefixed JSON frame to `writer`.
pub async fn write_json_frame<W, T>(writer: &mut W, value: &T) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(WireError::FrameTooLarge {
            size: payload.len(),
            max: MAX_CONTROL_FRAME_BYTES,
        });
    }
    let len = u32::try_from(payload.len()).map_err(|_| WireError::FrameTooLarge {
        size: payload.len(),
        max: MAX_CONTROL_FRAME_BYTES,
    })?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads a 4-byte big-endian length-prefixed JSON frame from `reader`.
pub async fn read_json_frame<R, T>(reader: &mut R, max_bytes: usize) -> Result<T, WireError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_bytes {
        return Err(WireError::FrameTooLarge {
            size: len,
            max: max_bytes,
        });
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let value = serde_json::from_slice(&buf)?;
    Ok(value)
}

/// Writes a single raw length-prefixed chunk frame (`len == 0` signals EOF).
pub async fn write_chunk_frame<W>(writer: &mut W, chunk: &[u8]) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(chunk.len()).map_err(|_| WireError::FrameTooLarge {
        size: chunk.len(),
        max: u32::MAX as usize,
    })?;
    writer.write_all(&len.to_be_bytes()).await?;
    if !chunk.is_empty() {
        writer.write_all(chunk).await?;
    }
    Ok(())
}

/// Reads a single raw length-prefixed chunk frame. Returns `Ok(None)` on EOF sentinel (`len == 0`).
pub async fn read_chunk_frame<R>(
    reader: &mut R,
    max_chunk_bytes: usize,
) -> Result<Option<Vec<u8>>, WireError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(None);
    }
    if len > max_chunk_bytes {
        return Err(WireError::FrameTooLarge {
            size: len,
            max: max_chunk_bytes,
        });
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}
