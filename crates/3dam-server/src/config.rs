//! The serve config file (tech-spec 09 §A.1) — the declarative **seed** for the flag store.
//!
//! This file owns only the file's *shape* and how its `[flags]` block maps to seed values; the flag
//! *semantics* and reconciliation are the store's (tech-spec 10, `config_authority = seed-only`).
//! Format is TOML (the loader is serde-driven, so JSON parses too). Everything is optional — a
//! missing file is not an error; the built-in safe defaults stand.

use dam_api::admin::{AuthMode, FlagKey, FlagValue, McpMode};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

/// The parsed serve config. Only the phase-5 surface (bind + flag seeds) is modelled; other blocks
/// (`[[sources]]`, `[analysis]`, `[web]`) are accepted and ignored here so an operator's fuller file
/// still loads (they are owned by their own subsystems).
#[derive(Debug, Default, serde::Deserialize)]
pub struct ServeFile {
    #[serde(default)]
    pub server: ServerBlock,
    #[serde(default)]
    pub flags: FlagsBlock,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ServerBlock {
    pub bind: Option<String>,
    pub port: Option<u16>,
    /// PEM cert chain + key for in-process TLS (issue #75). Both together enable HTTPS.
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct FlagsBlock {
    /// `off | anonymous | token`.
    pub auth: Option<String>,
    /// `off | read_only | read-only | readonly | read_write | read-write | writes`.
    pub mcp: Option<String>,
    pub network_writes: Option<bool>,
    /// Hosted-mode background pipeline (issue #71). Both default on in the store; set here to seed a
    /// low-power host that wants to defer the render/analysis grind.
    pub auto_thumbnail: Option<bool>,
    pub auto_analyze: Option<bool>,
}

impl ServeFile {
    /// Load and parse a config file. Missing file → default (safe) config, not an error.
    pub fn load(path: &Path) -> anyhow::Result<ServeFile> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ServeFile::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// The bind address named by the file, if any (`[server] bind`/`port`).
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        let ip: IpAddr = self.server.bind.as_deref()?.parse().ok()?;
        let port = self.server.port.unwrap_or(7878);
        Some(SocketAddr::new(ip, port))
    }

    /// The `(FlagKey, FlagValue)` seeds this file declares — applied seed-only at startup.
    pub fn flag_seeds(&self) -> Vec<(FlagKey, FlagValue)> {
        let mut out = Vec::new();
        if let Some(a) = self.flags.auth.as_deref().and_then(parse_auth) {
            out.push((FlagKey::Authentication, FlagValue::Auth(a)));
        }
        if let Some(m) = self.flags.mcp.as_deref().and_then(parse_mcp) {
            out.push((FlagKey::McpServer, FlagValue::Mcp(m)));
        }
        if let Some(w) = self.flags.network_writes {
            out.push((FlagKey::NetworkWrites, FlagValue::Bool(w)));
        }
        if let Some(t) = self.flags.auto_thumbnail {
            out.push((FlagKey::AutoThumbnail, FlagValue::Bool(t)));
        }
        if let Some(a) = self.flags.auto_analyze {
            out.push((FlagKey::AutoAnalyze, FlagValue::Bool(a)));
        }
        out
    }
}

fn parse_auth(s: &str) -> Option<AuthMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(AuthMode::Off),
        "anonymous" | "anon" => Some(AuthMode::Anonymous),
        "token" => Some(AuthMode::Token),
        _ => None,
    }
}

fn parse_mcp(s: &str) -> Option<McpMode> {
    match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "off" => Some(McpMode::Off),
        "read_only" | "readonly" | "read" => Some(McpMode::ReadOnly),
        "read_write" | "readwrite" | "writes" => Some(McpMode::ReadWrite),
        _ => None,
    }
}
