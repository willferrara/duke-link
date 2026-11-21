use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Governs how a pack file is reconciled against a player's local instance directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    /// Authoritative server state (mods, server-enforced configs). Local changes are overwritten.
    EnforcedServerState,
    /// Starter config shipped on first install. Local user edits are preserved across updates.
    InitialDefault,
}

/// Metadata for a single file tracked in a [`PackManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Normalized relative path within the pack root (e.g. `"mods/horizon-core.jar"`).
    pub path: String,
    /// Lowercase hex-encoded BLAKE3 digest of the file bytes.
    pub blake3: String,
    /// Exact file size in bytes.
    pub size: u64,
    /// Reconciliation policy for this file.
    pub role: FileRole,
}

/// Authoritative manifest describing all files in a `duke-link` Horizon MC pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    pub schema_version: u32,
    pub pack_name: String,
    pub pack_version: String,
    pub files: Vec<FileEntry>,
}

/// Typed errors for manifest I/O, validation, and BLAKE3 integrity verification.
#[derive(Debug, Error)]
pub enum ManifestError {
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
    #[error("BLAKE3 mismatch for {path}: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("invalid relative pack path: {0}")]
    InvalidRelativePath(String),
}

/// Validates that `rel` is a safe, strictly relative path without traversal or root prefixes.
pub fn validate_relative_path(rel: &str) -> Result<PathBuf, ManifestError> {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return Err(ManifestError::InvalidRelativePath(rel.to_owned()));
    }

    let path = Path::new(rel);
    if path.is_absolute() {
        return Err(ManifestError::InvalidRelativePath(rel.to_owned()));
    }

    let mut clean = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(part) => clean.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(ManifestError::InvalidRelativePath(rel.to_owned()));
            }
        }
    }

    if clean.as_os_str().is_empty() {
        return Err(ManifestError::InvalidRelativePath(rel.to_owned()));
    }

    Ok(clean)
}

/// Computes the lowercase hex BLAKE3 digest of the file at `path`.
pub fn compute_file_blake3(path: &Path) -> Result<String, ManifestError> {
    let mut file = File::open(path).map_err(|source| ManifestError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Verifies that the file at `path` matches `expected_hex` (case-insensitive BLAKE3 hex).
pub fn verify_file_blake3(path: &Path, expected_hex: &str) -> Result<(), ManifestError> {
    let actual = compute_file_blake3(path)?;
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(ManifestError::HashMismatch {
            path: path.to_path_buf(),
            expected: expected_hex.to_ascii_lowercase(),
            actual,
        })
    }
}

/// Reads and deserializes a [`PackManifest`] from a JSON file on disk.
pub fn read_manifest(path: &Path) -> Result<PackManifest, ManifestError> {
    let bytes = fs::read(path).map_err(|source| ManifestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| ManifestError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Atomically writes raw bytes to `path` using `.bak` backup + `.tmp` -> `sync_all` -> `rename`.
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), ManifestError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| ManifestError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    if path.exists() {
        let bak_path = sibling_with_suffix(path, ".bak");
        let existing = fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut bak_file = File::create(&bak_path).map_err(|source| ManifestError::Io {
            path: bak_path.clone(),
            source,
        })?;
        bak_file
            .write_all(&existing)
            .map_err(|source| ManifestError::Io {
                path: bak_path.clone(),
                source,
            })?;
        bak_file.sync_all().map_err(|source| ManifestError::Io {
            path: bak_path,
            source,
        })?;
    }

    let tmp_path = sibling_with_suffix(path, ".tmp");
    {
        let mut tmp_file = File::create(&tmp_path).map_err(|source| ManifestError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        tmp_file
            .write_all(bytes)
            .map_err(|source| ManifestError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        tmp_file.sync_all().map_err(|source| ManifestError::Io {
            path: tmp_path.clone(),
            source,
        })?;
    }

    fs::rename(&tmp_path, path).map_err(|source| ManifestError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(())
}

/// Atomically writes a [`PackManifest`] as pretty-printed UTF-8 JSON.
pub fn write_manifest_atomic(path: &Path, manifest: &PackManifest) -> Result<(), ManifestError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|source| ManifestError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    atomic_write_bytes(path, &bytes)
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}
