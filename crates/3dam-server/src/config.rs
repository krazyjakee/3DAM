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
    #[serde(default)]
    pub resources: ResourcesBlock,
    #[serde(default)]
    pub accounts: AccountsBlock,
}

/// `[resources]` — the good-neighbour knobs (tech-spec 14 §5). Unset values fall back to the
/// `3DAM_BG_THREADS` / `3DAM_MIN_FREE_MEMORY_MB` / `3DAM_MAX_IO_STALL_PCT` environment variables,
/// then host-derived defaults (cgroup-aware CPU budget, 10% memory floor, 25% I/O-stall ceiling).
/// Plain config, not runtime flags: resource limits are an operator/deployment property, not a
/// live exposure toggle.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ResourcesBlock {
    /// Background pool size (thumbnail/analysis grind). Default: effective CPUs − 2, capped at 4.
    pub background_threads: Option<usize>,
    /// Pause background work while host available memory is below this floor (MiB).
    /// Default: 10% of the memory ceiling, clamped to [256 MiB, 2 GiB].
    pub min_free_memory_mb: Option<u64>,
    /// Pause bulk reads (scan hashing) and grind while the disk's PSI full-stall `avg10` exceeds
    /// this percentage. Default 25; ≥ 100 disables the I/O gate.
    pub max_io_stall_pct: Option<f64>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ServerBlock {
    pub bind: Option<String>,
    pub port: Option<u16>,
    /// PEM cert chain + key for in-process TLS (issue #75). Both together enable HTTPS.
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
    /// Force `Secure` on the session + CSRF cookies even though *this process* speaks plaintext.
    /// The deployment `docs/DEPLOYMENT.md` recommends terminates TLS in a reverse proxy and talks
    /// to us over loopback HTTP, so the TLS posture we can observe is `false` while the browser is
    /// genuinely on HTTPS — without this the 90-day session cookie ships without `Secure`.
    ///
    /// Deliberately an **operator opt-in**, not an `X-Forwarded-Proto` sniff: a forwarded header is
    /// attacker-controlled on any deployment that doesn't strip it, so trusting it would make the
    /// flag settable by the client. Pairs with `[accounts] require_claim_token`.
    pub secure_cookies: Option<bool>,
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
    /// Serve as a federation peer (phase 6, issue #39): mounts `GET /api/v1/advertise` so other
    /// 3DAM instances can register this one as a federated source. Off by default.
    pub federation: Option<bool>,
    /// Full user accounts (phase 6, issue #42): login/session auth, groups, sharing. Off by
    /// default; on raises the effective auth gate to at least `token`.
    pub user_accounts: Option<bool>,
}

/// `[accounts]` — the config-plane recovery hatch (ADR 0009 §3, issue #42). Not a flag: it acts
/// once per boot, not as live-toggleable state.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AccountsBlock {
    /// Re-open the first-run claim window this boot, so a lost sole admin can be recovered: the
    /// next claim from localhost becomes a (new) admin account. Remove it again after recovery.
    pub reopen_claim: Option<bool>,
    /// Require the bootstrap owner token for **every** claim, including one from a loopback peer
    /// (ADR 0014). Behind a same-host reverse proxy every request *is* a loopback peer, so the
    /// open-claim window would otherwise be reachable from the internet. The claim gate already
    /// refuses any request carrying a forwarding header, but a proxy that strips them (or one
    /// speaking a scheme we don't model) leaves no signal — this is the operator's explicit
    /// "I know I am proxied" switch. Bootstrap-token redemption keeps working either way.
    pub require_claim_token: Option<bool>,
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
        if let Some(f) = self.flags.federation {
            out.push((FlagKey::Federation, FlagValue::Bool(f)));
        }
        if let Some(u) = self.flags.user_accounts {
            out.push((FlagKey::UserAccounts, FlagValue::Bool(u)));
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
