use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manifest::{
    atomic_write_bytes, compute_bytes_sha256, compute_file_blake3, FileEntry, FileRole,
    ManifestError,
};

/// Persistent local store recording the last-synced BLAKE3 baseline hash for each pack file.
///
/// Used for 3-way merge decisions on [`FileRole::InitialDefault`] files so user edits are
/// preserved while unmodified defaults can safely fast-forward to newer server versions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineStore {
    pub hashes: BTreeMap<String, String>,
}

/// Outcome of 3-way reconciliation for a single pack file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeDecision {
    /// Local file does not exist yet; install server bytes and record baseline.
    InstallNew,
    /// Local file already matches the server digest; no disk write required.
    AlreadyUpToDate,
    /// File is [`FileRole::EnforcedServerState`]; overwrite local file with server bytes.
    OverwriteEnforced,
    /// File is [`FileRole::InitialDefault`] and unmodified since baseline; fast-forward to new server default.
    UpdateUnmodifiedDefault,
    /// File is [`FileRole::InitialDefault`] and modified by the user; preserve local edits.
    PreserveUserModified,
}

/// Typed errors for baseline storage and 3-way config overlay reconciliation.
#[derive(Debug, Error)]
pub enum OverlayError {
    #[error("manifest/file error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON error at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("server payload hash mismatch for {path}: expected {expected}, got {actual}")]
    ServerHashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
}

impl BaselineStore {
    /// Loads a [`BaselineStore`] from `path`, returning an empty store if `path` does not exist.
    pub fn load(path: &Path) -> Result<Self, OverlayError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path).map_err(|source| OverlayError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|source| OverlayError::Json {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Atomically persists the [`BaselineStore`] to `path`.
    pub fn save_atomic(&self, path: &Path) -> Result<(), OverlayError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|source| OverlayError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        atomic_write_bytes(path, &bytes)?;
        Ok(())
    }
}

/// Pure 3-way merge decision function comparing server entry, local file hash, and stored baseline hash.
pub fn decide_file_action(
    entry: &FileEntry,
    local_hash: Option<&str>,
    baseline_hash: Option<&str>,
) -> MergeDecision {
    let Some(local) = local_hash else {
        return MergeDecision::InstallNew;
    };

    if local.eq_ignore_ascii_case(&entry.blake3) {
        return MergeDecision::AlreadyUpToDate;
    }

    match entry.role {
        FileRole::EnforcedServerState => MergeDecision::OverwriteEnforced,
        FileRole::InitialDefault => match baseline_hash {
            Some(base) if base.eq_ignore_ascii_case(local) => {
                MergeDecision::UpdateUnmodifiedDefault
            }
            _ => MergeDecision::PreserveUserModified,
        },
    }
}

/// Reconciles `target_path` against `entry` and `server_bytes`, updating `baseline` as needed.
pub fn apply_overlay_file(
    target_path: &Path,
    entry: &FileEntry,
    server_bytes: &[u8],
    baseline: &mut BaselineStore,
) -> Result<MergeDecision, OverlayError> {
    let local_hash = if target_path.exists() {
        Some(compute_file_blake3(target_path)?)
    } else {
        None
    };

    let baseline_hash = baseline.hashes.get(&entry.path).map(String::as_str);
    let decision = decide_file_action(entry, local_hash.as_deref(), baseline_hash);

    match decision {
        MergeDecision::InstallNew
        | MergeDecision::OverwriteEnforced
        | MergeDecision::UpdateUnmodifiedDefault => {
            let actual_blake3 = blake3::hash(server_bytes).to_hex().to_string();
            if !actual_blake3.eq_ignore_ascii_case(&entry.blake3) {
                return Err(OverlayError::ServerHashMismatch {
                    path: entry.path.clone(),
                    expected: entry.blake3.to_ascii_lowercase(),
                    actual: actual_blake3,
                });
            }
            if let Some(expected_sha256) = entry.sha256.as_deref() {
                let actual_sha256 = compute_bytes_sha256(server_bytes);
                if !actual_sha256.eq_ignore_ascii_case(expected_sha256) {
                    return Err(OverlayError::ServerHashMismatch {
                        path: entry.path.clone(),
                        expected: expected_sha256.to_ascii_lowercase(),
                        actual: actual_sha256,
                    });
                }
            }
            atomic_write_bytes(target_path, server_bytes)?;
            baseline
                .hashes
                .insert(entry.path.clone(), entry.blake3.to_ascii_lowercase());
        }
        MergeDecision::AlreadyUpToDate => {
            baseline
                .hashes
                .entry(entry.path.clone())
                .or_insert_with(|| entry.blake3.to_ascii_lowercase());
        }
        MergeDecision::PreserveUserModified => {
            // Intentionally leave the user's file and baseline untouched.
        }
    }

    Ok(decision)
}
