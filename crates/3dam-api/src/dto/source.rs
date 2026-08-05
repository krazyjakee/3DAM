//! Sources: registering, describing, configuring, and removing the file sources a library scans.

use crate::id::SourceId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    LocalFs,
    Sftp,
    Smb,
    Federated,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::LocalFs => "local_fs",
            SourceKind::Sftp => "sftp",
            SourceKind::Smb => "smb",
            SourceKind::Federated => "federated",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local_fs" => Some(SourceKind::LocalFs),
            "sftp" => Some(SourceKind::Sftp),
            "smb" => Some(SourceKind::Smb),
            "federated" => Some(SourceKind::Federated),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Online,
    Offline,
    Scanning,
    Error(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceStats {
    pub asset_count: u64,
    pub last_scanned_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
    pub uri: String,
    pub state: SourceState,
    pub stats: SourceStats,
    pub watch: bool,
    /// Can this source accept an upload *right now* (issue #80)?
    ///
    /// Probed, not inferred from `kind`: a local source can sit on a read-only mount, and a peer is
    /// never writable at all. The destination picker offers only sources where this is true, so the
    /// user learns a share is read-only before choosing files rather than after dropping two
    /// hundred of them. Defaults to `false` so an older server (or any surface that cannot answer)
    /// reads as read-only rather than advertising a write that would fail.
    #[serde(default)]
    pub writable: bool,
    /// Why an otherwise visible source is not an upload destination. In particular, this names a
    /// missing per-source write grant separately from backend/filesystem writability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writable_reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AddSource {
    pub kind: SourceKind,
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub options: SourceOptions,
}

impl std::fmt::Debug for AddSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddSource")
            .field("kind", &self.kind)
            // Network URI userinfo may itself contain a password. The parsed SourceConnection has
            // a safe display URI, but this transport DTO has not crossed that boundary yet.
            .field("uri", &"[REDACTED]")
            .field("name", &self.name)
            .field("options", &self.options)
            .finish()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SourceOptions {
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    // ── connection auth for network sources (SFTP/SMB); ignored for local (tech-spec 07 §3.2) ──
    /// Login user (overrides any `user@` in the URI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Password (SFTP or SMB). Write-only input; moved into the host secret store before the source
    /// row is created and never returned to clients or stored in the portable catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Path to a private key file for SFTP key auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// Passphrase for an encrypted private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// SMB domain/workgroup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Override the default port (22 SFTP / 445 SMB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl std::fmt::Debug for SourceOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceOptions")
            .field("watch", &self.watch)
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "[REDACTED]"),
            )
            .field("domain", &self.domain)
            .field("port", &self.port)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoveSource {
    /// Fail-soft: offline sources keep cached rows (PRODUCT_SPEC §6.1).
    #[serde(default)]
    pub keep_metadata: bool,
}
