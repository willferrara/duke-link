use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Prefix for compact, URL-safe `duke-link` invite tickets.
pub const INVITE_TICKET_PREFIX: &str = "duke1:";

/// Typed errors for Ed25519 allowlist loading and invite ticket encoding/decoding.
#[derive(Debug, Error)]
pub enum AllowlistError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid Ed25519 EndpointId '{raw}': {message}")]
    InvalidEndpointId { raw: String, message: String },
    #[error("invalid base64 in invite ticket: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("invalid JSON in invite ticket: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invite ticket server_name must not be empty")]
    EmptyServerName,
}

#[derive(Debug, Deserialize)]
struct AllowlistToml {
    #[serde(default)]
    allowed_endpoints: Vec<String>,
}

/// Strict Ed25519 [`EndpointId`] allowlist checked on every incoming QUIC connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    allowed: HashSet<EndpointId>,
}

impl Allowlist {
    /// Creates an allowlist directly from an iterator of validated [`EndpointId`]s.
    pub fn from_ids(ids: impl IntoIterator<Item = EndpointId>) -> Self {
        Self {
            allowed: ids.into_iter().collect(),
        }
    }

    /// Parses a TOML allowlist containing `allowed_endpoints = ["<endpoint-id>", ...]`.
    pub fn from_toml_str(input: &str) -> Result<Self, AllowlistError> {
        let parsed: AllowlistToml = toml::from_str(input)?;
        let mut allowed = HashSet::with_capacity(parsed.allowed_endpoints.len());
        for raw in parsed.allowed_endpoints {
            let trimmed = raw.trim();
            let id =
                EndpointId::from_str(trimmed).map_err(|err| AllowlistError::InvalidEndpointId {
                    raw: raw.clone(),
                    message: err.to_string(),
                })?;
            allowed.insert(id);
        }
        Ok(Self { allowed })
    }

    /// Loads and validates a TOML allowlist from disk.
    pub fn load(path: &Path) -> Result<Self, AllowlistError> {
        let content = fs::read_to_string(path).map_err(|source| AllowlistError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&content)
    }

    /// Returns `true` if `id` is explicitly permitted to connect.
    pub fn is_allowed(&self, id: &EndpointId) -> bool {
        self.allowed.contains(id)
    }

    /// Returns the number of permitted peers in the allowlist.
    pub fn len(&self) -> usize {
        self.allowed.len()
    }

    /// Returns `true` if no peers are permitted.
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

/// Compact shareable invite ticket containing everything a friend needs to join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteTicket {
    pub server_endpoint_id: EndpointId,
    pub server_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}

impl InviteTicket {
    /// Encodes the invite ticket as a compact, copy-pasteable `duke1:<base64url(json)>` string.
    pub fn encode(&self) -> Result<String, AllowlistError> {
        if self.server_name.trim().is_empty() {
            return Err(AllowlistError::EmptyServerName);
        }
        let json = serde_json::to_vec(self)?;
        let b64 = URL_SAFE_NO_PAD.encode(json);
        Ok(format!("{INVITE_TICKET_PREFIX}{b64}"))
    }

    /// Decodes and validates a compact invite ticket string.
    pub fn decode(encoded: &str) -> Result<Self, AllowlistError> {
        let trimmed = encoded.trim();
        let body = trimmed
            .strip_prefix(INVITE_TICKET_PREFIX)
            .unwrap_or(trimmed);
        let json = URL_SAFE_NO_PAD.decode(body)?;
        let ticket: Self = serde_json::from_slice(&json)?;
        if ticket.server_name.trim().is_empty() {
            return Err(AllowlistError::EmptyServerName);
        }
        Ok(ticket)
    }
}
