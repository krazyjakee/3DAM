//! Admin / feature-flag wire types (tech-spec 10 §3, §5).
//!
//! These are the shapes the `/admin/api/*` surface exchanges, shared by the server (which owns the
//! store + semantics) and every admin client (the CLI over `--connect`, later the web Settings
//! surface). Kept in `dam-api` — the dependency-free seam — so both sides agree on one definition.
//!
//! Phase-5 scope: the flags with a real served surface (**authentication**, **MCP server**,
//! **network writes**) plus the read-only exposure/status view. User accounts, sessions, OIDC, and
//! inbound federation are their own capability phases (spec §9 phases 5→6) and are **not** modelled
//! here — "off removes the surface" (ADR 0004) means we do not advertise flags for absent surfaces.

use crate::service::Scopes;
use serde::{Deserialize, Serialize};

/// The authentication mode — one gate over the whole shared surface (tech-spec 10 §1.1). `Oidc` is
/// the phase-6 extension path (feature-gated) and is intentionally absent from the v1 enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// No authentication; every request is the local owner (full trust). The safe default *when
    /// bound to localhost*.
    Off,
    /// Public read: unauthenticated callers get the anonymous scope set; a valid token elevates.
    Anonymous,
    /// Bearer token / API key required. The simple default for a private, exposed instance.
    Token,
}

/// The MCP served-endpoint tri-state (tech-spec 10 §3, 11 §7). `Off` unmounts `POST /mcp` entirely.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpMode {
    Off,
    ReadOnly,
    ReadWrite,
}

/// A feature-flag key (tech-spec 10 §3). Only the flags with a live-toggleable phase-5 surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKey {
    /// `AuthMode` — the auth gate over API / MCP / admin.
    Authentication,
    /// `McpMode` — mounts/unmounts `POST /mcp` and gates its write tools.
    McpServer,
    /// `bool` — server read-only vs writes-enabled to the network (the network-level write ceiling).
    NetworkWrites,
}

impl FlagKey {
    pub const ALL: [FlagKey; 3] = [
        FlagKey::Authentication,
        FlagKey::McpServer,
        FlagKey::NetworkWrites,
    ];
    /// The stable string used in the URL, the store, and the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            FlagKey::Authentication => "authentication",
            FlagKey::McpServer => "mcp_server",
            FlagKey::NetworkWrites => "network_writes",
        }
    }
    pub fn parse(s: &str) -> Option<FlagKey> {
        FlagKey::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl std::fmt::Display for FlagKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A flag's typed value (tech-spec 10 §2.2 — JSON `bool | enum | struct`). One variant per key.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlagValue {
    Auth(AuthMode),
    Mcp(McpMode),
    Bool(bool),
}

impl FlagValue {
    /// Reject a value that does not match its key's type (a `bool` for `authentication`, etc.).
    pub fn matches(self, key: FlagKey) -> bool {
        matches!(
            (key, self),
            (FlagKey::Authentication, FlagValue::Auth(_))
                | (FlagKey::McpServer, FlagValue::Mcp(_))
                | (FlagKey::NetworkWrites, FlagValue::Bool(_))
        )
    }
}

/// One flag as returned by `GET /admin/api/flags` (value + version + change semantics).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlagInfo {
    pub key: FlagKey,
    pub value: FlagValue,
    /// Optimistic-concurrency version, bumped on every set (tech-spec 10 §2.2).
    pub version: u64,
    /// `true` if flipping this flag takes effect on the running server (all phase-5 flags are live).
    pub live: bool,
    /// `true` if setting this value increases exposure and so needs `confirm` (tech-spec 10 §5).
    pub exposure_increasing: bool,
}

/// Body of `PUT /admin/api/flags/{key}` — the new value, the version the caller believes is current
/// (optimistic concurrency), and the exposure confirmation (tech-spec 10 §5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetFlag {
    pub value: FlagValue,
    /// Expected current version; a mismatch is a `409 Conflict` (two admins racing a toggle).
    pub expected_version: Option<u64>,
    /// Must be `true` to apply an exposure-increasing change (the machine form of warn-and-confirm).
    #[serde(default)]
    pub confirm: bool,
}

/// Server posture at a glance (`GET /admin/api/status`, tech-spec 10 §5) — what an operator needs to
/// answer "am I safe to expose?".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdminStatus {
    pub bind: String,
    pub localhost_only: bool,
    pub tls: bool,
    pub auth: AuthMode,
    pub mcp: McpMode,
    pub network_writes: bool,
    /// Bound beyond localhost with neither auth nor TLS — the headline exposure warning.
    pub exposed_without_auth: bool,
    pub token_count: usize,
}

/// A token/API-key record as listed by the admin API — **never** the secret (tech-spec 10 §1.4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenInfo {
    pub token_id: String,
    pub label: String,
    pub scopes: Scopes,
    pub created: i64,
    pub expires: Option<i64>,
    pub last_used: Option<i64>,
}

/// Body of `POST /admin/api/tokens` — issue a scoped API key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewToken {
    pub label: String,
    pub scopes: Scopes,
    pub expires: Option<i64>,
}

/// Reply to token creation — carries the plaintext secret **once** (tech-spec 10 §1.4, §5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewTokenReply {
    pub token_id: String,
    pub label: String,
    pub scopes: Scopes,
    /// Shown once, never retrievable again. Present it as `Authorization: Bearer <secret>`.
    pub secret: String,
}

/// One audit-log entry (tech-spec 10 §4.5). Append-only; readable by admins.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<serde_json::Value>,
}
