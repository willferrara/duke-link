use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Pinned Horizon MC release tag for Dukes Horizon.
pub const DEFAULT_HORIZON_RELEASE_TAG: &str = "nightly-2026-11-21";

/// Pinned Horizon MC commit short revision for Dukes Horizon.
pub const DEFAULT_HORIZON_REV: &str = "f037744";

/// Pinned Minecraft version for Horizon MC.
pub const DEFAULT_MINECRAFT_VERSION: &str = "26.3";

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
    /// Normalized relative path within the pack root (e.g. `"horizon-mods/dukes-core.hmod"`).
    pub path: String,
    /// Lowercase hex-encoded BLAKE3 digest of the file bytes.
    pub blake3: String,
    /// Optional lowercase hex-encoded SHA-256 digest of the file bytes (used by Horizon `.hpack` manifests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_release_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minecraft_version: Option<String>,
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

/// Computes the lowercase hex SHA-256 digest of an in-memory byte slice.
pub fn compute_bytes_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Computes the lowercase hex SHA-256 digest of the file at `path`.
pub fn compute_file_sha256(path: &Path) -> Result<String, ManifestError> {
    let mut file = File::open(path).map_err(|source| ManifestError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut hasher = Sha256::new();
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

    Ok(hex_lower(&hasher.finalize()))
}

/// Verifies that the file at `path` matches `expected_hex` (case-insensitive SHA-256 hex).
pub fn verify_file_sha256(path: &Path, expected_hex: &str) -> Result<(), ManifestError> {
    let actual = compute_file_sha256(path)?;
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

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

/// Returns `true` if `rel_path` is a recognized Horizon MC 26.3 pack artifact
/// (`.hpack`, `.hmod`, or `horizon-mods.txt`).
pub fn is_horizon_pack_artifact(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    lower.ends_with(".hpack")
        || lower.ends_with(".hmod")
        || lower == "horizon-mods.txt"
        || lower.ends_with("/horizon-mods.txt")
}

/// Scans `pack_dir` for Horizon MC 26.3 artifacts (`.hpack`, `.hmod`, `extra-mods/*.hmod`,
/// `horizon-mods/*.hmod`, and `horizon-mods.txt`) and constructs a deterministic [`PackManifest`]
/// with both BLAKE3 and SHA-256 checksums.
pub fn build_horizon_pack_manifest(
    pack_dir: &Path,
    pack_name: &str,
    pack_version: &str,
) -> Result<PackManifest, ManifestError> {
    let mut rel_paths = Vec::new();
    collect_horizon_files(pack_dir, pack_dir, &mut rel_paths)?;
    rel_paths.sort();

    let mut files = Vec::with_capacity(rel_paths.len());
    for rel in rel_paths {
        let full = pack_dir.join(&rel);
        let meta = fs::metadata(&full).map_err(|source| ManifestError::Io {
            path: full.clone(),
            source,
        })?;
        let blake3 = compute_file_blake3(&full)?;
        let sha256 = compute_file_sha256(&full)?;
        files.push(FileEntry {
            path: rel,
            blake3,
            sha256: Some(sha256),
            size: meta.len(),
            role: FileRole::EnforcedServerState,
        });
    }

    Ok(PackManifest {
        schema_version: 2,
        pack_name: pack_name.to_owned(),
        pack_version: pack_version.to_owned(),
        horizon_release_tag: Some(DEFAULT_HORIZON_RELEASE_TAG.to_owned()),
        horizon_rev: Some(DEFAULT_HORIZON_REV.to_owned()),
        minecraft_version: Some(DEFAULT_MINECRAFT_VERSION.to_owned()),
        files,
    })
}

fn collect_horizon_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
) -> Result<(), ManifestError> {
    let entries = fs::read_dir(dir).map_err(|source| ManifestError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    for entry_res in entries {
        let entry = entry_res.map_err(|source| ManifestError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| ManifestError::Io {
            path: path.clone(),
            source,
        })?;

        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "horizon-mods" || name_str == "extra-mods" || name_str == "mods" {
                collect_horizon_files(root, &path, out)?;
            }
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if is_horizon_pack_artifact(&rel_str) {
                validate_relative_path(&rel_str)?;
                out.push(rel_str);
            }
        }
    }

    Ok(())
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
