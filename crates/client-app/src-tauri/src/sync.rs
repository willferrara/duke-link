use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};
use shared_protocol::config_overlay::{
    apply_overlay_file, decide_file_action, BaselineStore, MergeDecision, OverlayError,
};
use shared_protocol::manifest::{
    compute_file_blake3, validate_relative_path, FileEntry, ManifestError, PackManifest,
};
use shared_protocol::wire::{
    read_chunk_frame, read_json_frame, write_json_frame, SyncRequest, SyncResponse, ALPN_SYNC,
    FILE_CHUNK_SIZE, MAX_CONTROL_FRAME_BYTES,
};
use thiserror::Error;

/// Planned reconciliation action for a single manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAction {
    Skip,
    DownloadNew,
    UpdateChanged,
    PreserveUserModified,
}

/// One item in a BLAKE3 delta synchronization plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub entry: FileEntry,
    pub action: SyncAction,
}

/// Live progress payload emitted to the frontend over the `sync-progress` event channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncProgressEvent {
    pub stage: String,
    pub current_file: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub mbps: f64,
}

/// Summary returned after a `sync/1` pass completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSummary {
    pub pack_name: String,
    pub pack_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_rev: Option<String>,
    pub downloaded_files: usize,
    pub skipped_files: usize,
    pub preserved_user_files: usize,
    pub bytes_downloaded: u64,
}

/// Typed errors for `sync/1` delta planning, path canonicalization, and file transfer.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("overlay error: {0}")]
    Overlay(#[from] OverlayError),
    #[error("wire error: {0}")]
    Wire(#[from] shared_protocol::wire::WireError),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("path escapes managed instance directory: {0}")]
    PathTraversal(String),
    #[error("QUIC sync connection failed: {0}")]
    Connect(String),
    #[error("QUIC sync stream failed: {0}")]
    Stream(String),
    #[error("remote relay sync error: {0}")]
    Remote(String),
}

/// Canonicalizes `instance_dir` and validates that `relative_path` resolves strictly inside it.
pub fn resolve_safe_instance_path(
    instance_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf, SyncError> {
    let clean_rel = validate_relative_path(relative_path)?;
    fs::create_dir_all(instance_dir).map_err(|source| SyncError::Io {
        path: instance_dir.to_path_buf(),
        source,
    })?;
    let canonical_root = fs::canonicalize(instance_dir).map_err(|source| SyncError::Io {
        path: instance_dir.to_path_buf(),
        source,
    })?;

    let candidate = canonical_root.join(&clean_rel);

    // Canonicalize the deepest existing ancestor so symlinks cannot escape instance_dir.
    let mut existing = candidate.as_path();
    while !existing.exists() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return Err(SyncError::PathTraversal(relative_path.to_owned())),
        }
    }
    let canonical_existing = fs::canonicalize(existing).map_err(|source| SyncError::Io {
        path: existing.to_path_buf(),
        source,
    })?;
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(SyncError::PathTraversal(relative_path.to_owned()));
    }

    if !candidate.starts_with(&canonical_root) {
        return Err(SyncError::PathTraversal(relative_path.to_owned()));
    }

    Ok(candidate)
}

/// Maps a validated manifest relative path to its destination path inside a Horizon MC 26.3
/// game directory (`horizon-mods/*.hmod`, `horizon-mods.txt`, `dukes-horizon.hpack`).
/// Non-Horizon paths (such as `mods/...` or `config/...`) are preserved verbatim.
pub fn map_horizon_target_rel_path(relative_path: &str) -> Result<String, SyncError> {
    validate_relative_path(relative_path)?;
    let lower = relative_path.to_ascii_lowercase();

    if lower.ends_with(".hmod") {
        let file_name = relative_path.rsplit('/').next().unwrap_or(relative_path);
        return Ok(format!("horizon-mods/{file_name}"));
    }

    if lower == "horizon-mods.txt" || lower.ends_with("/horizon-mods.txt") {
        return Ok("horizon-mods.txt".to_owned());
    }

    if lower.ends_with(".hpack") {
        let file_name = relative_path.rsplit('/').next().unwrap_or(relative_path);
        return Ok(file_name.to_owned());
    }

    Ok(relative_path.to_owned())
}

/// Computes a deterministic BLAKE3 delta plan against `instance_dir` and `baseline`.
pub fn plan_manifest_delta(
    instance_dir: &Path,
    manifest: &PackManifest,
    baseline: &BaselineStore,
) -> Result<Vec<PlannedFile>, SyncError> {
    let mut plan = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        let mapped_rel = map_horizon_target_rel_path(&entry.path)?;
        let target = resolve_safe_instance_path(instance_dir, &mapped_rel)?;
        let local_hash = if target.exists() {
            Some(compute_file_blake3(&target)?)
        } else {
            None
        };
        let baseline_hash = baseline.hashes.get(&entry.path).map(String::as_str);
        let action = match decide_file_action(entry, local_hash.as_deref(), baseline_hash) {
            MergeDecision::InstallNew => SyncAction::DownloadNew,
            MergeDecision::AlreadyUpToDate => SyncAction::Skip,
            MergeDecision::OverwriteEnforced | MergeDecision::UpdateUnmodifiedDefault => {
                SyncAction::UpdateChanged
            }
            MergeDecision::PreserveUserModified => SyncAction::PreserveUserModified,
        };
        plan.push(PlannedFile {
            entry: entry.clone(),
            action,
        });
    }
    Ok(plan)
}

/// Fetches the authoritative [`PackManifest`] over `duke-link/sync/1` and synchronizes
/// missing/changed files into `instance_dir`, emitting progress updates via `on_progress`.
pub async fn sync_pack_over_quic<F>(
    endpoint: &Endpoint,
    server_addr: &EndpointAddr,
    instance_dir: &Path,
    mut on_progress: F,
) -> Result<SyncSummary, SyncError>
where
    F: FnMut(SyncProgressEvent),
{
    on_progress(SyncProgressEvent {
        stage: "connecting".to_owned(),
        current_file: String::new(),
        files_done: 0,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
        mbps: 0.0,
    });

    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(server_addr.clone(), ALPN_SYNC),
    )
    .await
    .map_err(|_| SyncError::Connect("timed out connecting to sync/1".to_owned()))?
    .map_err(|err| SyncError::Connect(err.to_string()))?;

    // 1. Fetch manifest
    let manifest = {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|err| SyncError::Stream(err.to_string()))?;
        write_json_frame(&mut send, &SyncRequest::GetManifest).await?;
        let _ = send.finish();
        match read_json_frame::<_, SyncResponse>(&mut recv, MAX_CONTROL_FRAME_BYTES).await? {
            SyncResponse::Manifest { manifest } => manifest,
            SyncResponse::Error { message } => return Err(SyncError::Remote(message)),
            SyncResponse::FileHeader { .. } => {
                return Err(SyncError::Remote(
                    "unexpected file header when requesting manifest".to_owned(),
                ));
            }
        }
    };

    let baseline_path = instance_dir.join(".duke-link-baseline.json");
    let mut baseline = BaselineStore::load(&baseline_path)?;
    let plan = plan_manifest_delta(instance_dir, &manifest, &baseline)?;

    let to_download: Vec<&PlannedFile> = plan
        .iter()
        .filter(|p| {
            matches!(
                p.action,
                SyncAction::DownloadNew | SyncAction::UpdateChanged
            )
        })
        .collect();
    let files_total = to_download.len();
    let bytes_total: u64 = to_download.iter().map(|p| p.entry.size).sum();

    let skipped_files = plan.iter().filter(|p| p.action == SyncAction::Skip).count();
    let preserved_user_files = plan
        .iter()
        .filter(|p| p.action == SyncAction::PreserveUserModified)
        .count();

    let started = Instant::now();
    let mut files_done = 0usize;
    let mut bytes_done = 0u64;

    for item in to_download {
        let mapped_rel = map_horizon_target_rel_path(&item.entry.path)?;
        let target_path = resolve_safe_instance_path(instance_dir, &mapped_rel)?;

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|err| SyncError::Stream(err.to_string()))?;
        write_json_frame(
            &mut send,
            &SyncRequest::GetFile {
                path: item.entry.path.clone(),
            },
        )
        .await?;
        let _ = send.finish();

        match read_json_frame::<_, SyncResponse>(&mut recv, MAX_CONTROL_FRAME_BYTES).await? {
            SyncResponse::FileHeader { .. } => {}
            SyncResponse::Error { message } => return Err(SyncError::Remote(message)),
            SyncResponse::Manifest { .. } => {
                return Err(SyncError::Remote(
                    "unexpected manifest when requesting file".to_owned(),
                ));
            }
        }

        let mut payload = Vec::with_capacity(usize::try_from(item.entry.size).unwrap_or(0));
        while let Some(chunk) = read_chunk_frame(&mut recv, FILE_CHUNK_SIZE * 4).await? {
            bytes_done = bytes_done.saturating_add(chunk.len() as u64);
            payload.extend_from_slice(&chunk);

            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let mbps = (bytes_done as f64 / (1024.0 * 1024.0)) / elapsed;
            on_progress(SyncProgressEvent {
                stage: "downloading".to_owned(),
                current_file: item.entry.path.clone(),
                files_done,
                files_total,
                bytes_done,
                bytes_total,
                mbps,
            });
        }

        apply_overlay_file(&target_path, &item.entry, &payload, &mut baseline)?;
        files_done += 1;
    }

    baseline.save_atomic(&baseline_path)?;
    conn.close(0u32.into(), b"sync complete");

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    on_progress(SyncProgressEvent {
        stage: "complete".to_owned(),
        current_file: String::new(),
        files_done,
        files_total,
        bytes_done,
        bytes_total,
        mbps: (bytes_done as f64 / (1024.0 * 1024.0)) / elapsed,
    });

    Ok(SyncSummary {
        pack_name: manifest.pack_name,
        pack_version: manifest.pack_version,
        horizon_rev: manifest.horizon_rev,
        downloaded_files: files_done,
        skipped_files,
        preserved_user_files,
        bytes_downloaded: bytes_done,
    })
}
