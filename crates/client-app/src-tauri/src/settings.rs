use std::fs;
use std::path::{Path, PathBuf};

use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use shared_protocol::manifest::{atomic_write_bytes, ManifestError};
use thiserror::Error;

/// Persisted user configuration for the `duke-link` client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSettings {
    #[serde(default)]
    pub invite_ticket: Option<String>,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default = "default_mc_port")]
    pub local_mc_port: u16,
    #[serde(default)]
    pub instance_dir: Option<PathBuf>,
}

fn default_username() -> String {
    "Player".to_owned()
}

fn default_mc_port() -> u16 {
    25565
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            invite_ticket: None,
            username: default_username(),
            local_mc_port: default_mc_port(),
            instance_dir: None,
        }
    }
}

/// Typed errors for client settings and identity key persistence.
#[derive(Debug, Error)]
pub enum SettingsError {
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
    #[error("atomic write failed: {0}")]
    Manifest(#[from] ManifestError),
    #[error("invalid client key length at {path}: expected 32 bytes, found {len}")]
    InvalidKeyLength { path: PathBuf, len: usize },
}

/// Resolves the client data directory.
///
/// Uses `<exe_dir>/data` in portable mode when `portable.txt` or `data/` exists beside the
/// executable; otherwise falls back to `%LOCALAPPDATA%\duke-link` (or `~/.local/share/duke-link`).
pub fn resolve_data_dir() -> PathBuf {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let portable_marker = exe_dir.join("portable.txt");
            let portable_data = exe_dir.join("data");
            if portable_marker.exists() || portable_data.is_dir() {
                return portable_data;
            }
        }
    }

    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        if !local_app_data.trim().is_empty() {
            return PathBuf::from(local_app_data).join("duke-link");
        }
    }

    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("duke-link");
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("duke-link");
        }
    }

    PathBuf::from("data")
}

/// Loads `settings.json` from `data_dir`, returning defaults if absent.
pub fn load_settings(data_dir: &Path) -> Result<ClientSettings, SettingsError> {
    let path = data_dir.join("settings.json");
    if !path.exists() {
        return Ok(ClientSettings::default());
    }
    let bytes = fs::read(&path).map_err(|source| SettingsError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| SettingsError::Json { path, source })
}

/// Atomically persists `settings.json` inside `data_dir`.
pub fn save_settings_atomic(
    data_dir: &Path,
    settings: &ClientSettings,
) -> Result<(), SettingsError> {
    let path = data_dir.join("settings.json");
    let bytes = serde_json::to_vec_pretty(settings).map_err(|source| SettingsError::Json {
        path: path.clone(),
        source,
    })?;
    atomic_write_bytes(&path, &bytes)?;
    Ok(())
}

/// Loads or generates the client's persistent 32-byte Ed25519 identity key.
pub fn load_or_create_client_key(data_dir: &Path) -> Result<SecretKey, SettingsError> {
    let key_path = data_dir.join("client.key");
    if key_path.exists() {
        let bytes = fs::read(&key_path).map_err(|source| SettingsError::Io {
            path: key_path.clone(),
            source,
        })?;
        let arr: [u8; 32] =
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| SettingsError::InvalidKeyLength {
                    path: key_path.clone(),
                    len: bytes.len(),
                })?;
        return Ok(SecretKey::from_bytes(&arr));
    }

    let key = SecretKey::generate();
    atomic_write_bytes(&key_path, &key.to_bytes())?;
    Ok(key)
}
