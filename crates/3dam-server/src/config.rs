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
    #[serde(default)]
    pub upload: UploadBlock,
}

/// `[upload]` — the per-file size ceiling for writes into a source (issue #80).
///
/// Config, not a runtime flag, for the reason `[resources]` gives: "a reasonable maximum asset
/// size" is a deployment property (disk, link speed, what the library holds), not a live exposure
/// toggle. An audio library and a 3D one disagree by two orders of magnitude, and neither wants to
/// discover the limit by having a drop fail halfway. Note that *whether* uploads are allowed at all
/// is a runtime decision and lives in `[flags] upload` (off by default), narrowed further by
/// `Scope::Write` and the `network_writes` ceiling — this block only sizes them.
#[derive(Debug, Default, serde::Deserialize)]
pub struct UploadBlock {
    /// Largest single uploaded file, in MiB. Default 2048 (2 GiB).
    pub max_file_mb: Option<u64>,
}

/// Default per-file upload ceiling: 2 GiB. Chosen to clear a large uncompressed 3D scene or a
/// video without ceremony, while still bounding what one request can write.
pub const DEFAULT_MAX_UPLOAD_MB: u64 = 2048;

impl UploadBlock {
    /// The ceiling in bytes.
    ///
    /// `saturating_mul`, not `*`: an operator writing a very large number to mean "unlimited" is a
    /// natural reading of an optional field, and a plain multiply would panic on overflow in a debug
    /// build (taking down startup with a message naming no config key) or *wrap* in release — where
    /// any multiple of 2^44 lands on a ceiling of 0 and silently rejects every non-empty upload.
    /// A literal `0` is treated the same way as absurdly-large, since "allow nothing" is far more
    /// likely a mistake than an intent, and disabling uploads is what the Write scope is for.
    pub fn max_bytes(&self) -> u64 {
        match self.max_file_mb {
            Some(0) | None => DEFAULT_MAX_UPLOAD_MB,
            Some(mb) => mb,
        }
        .saturating_mul(1024 * 1024)
    }
}

/// `[resources]` — the good-neighbour knobs (tech-spec 14 §5). Unset worker/governor values fall
/// back to their `3DAM_*` environment variables, then host-derived defaults; unset cache ceilings
/// derive from current free disk according to ADR 0009.
/// Plain config, not runtime flags: resource limits are an operator/deployment property, not a
/// live exposure toggle.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ResourcesBlock {
    #[serde(default)]
    pub storage: Vec<dam_core::StorageOverride>,
    /// Per backing device background read + write cap (MiB/s). Defaults: HDD 8, SSD 128,
    /// unknown/network 4. Queue/latency feedback may lower this while the host is busy.
    pub io_max_mib_per_sec: Option<u64>,
    /// Per device concurrency cap. Defaults: HDD/unknown 1, SSD 2.
    pub io_concurrency: Option<usize>,
    /// Background pool size (thumbnail/analysis grind). Default: effective CPUs − 2, capped at 4.
    pub background_threads: Option<usize>,
    /// Pause background work while host available memory is below this floor (MiB).
    /// Default: 10% of the memory ceiling, clamped to [256 MiB, 2 GiB].
    pub min_free_memory_mb: Option<u64>,
    /// Pause bulk reads (scan hashing) and grind while the disk's PSI full-stall `avg10` exceeds
    /// this percentage. Default 25; ≥ 100 disables the I/O gate.
    pub max_io_stall_pct: Option<f64>,
    /// Local thumbnail + model-preview cache ceiling (MiB). Default: min(10 GiB, 10% free disk).
    pub derivative_cache_mb: Option<u64>,
    /// Federated preview cache ceiling (MiB). Default: min(2 GiB, 10% free disk).
    pub peer_cache_mb: Option<u64>,
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
    /// Exact socket-peer addresses allowed to assert one client address in `X-Forwarded-For` for
    /// auth rate limiting. Empty by default: forwarding headers from every peer are ignored.
    /// Proxies named here must strip the inbound header and replace it with one bare IPv4/IPv6
    /// address; malformed/missing attribution is deliberately collapsed into a shared bucket.
    #[serde(default)]
    pub trusted_proxies: Vec<IpAddr>,
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
    /// Accept uploads — writes of *new* files into a registered source (issue #80). Off by default;
    /// off means `POST /api/v1/upload` is absent. Sized by `[upload] max_file_mb`, which is a
    /// deployment property rather than a posture, hence the separate block.
    pub upload: Option<bool>,
    /// Accept OIDC/OAuth2 logins (phase 6, issue #41). Off by default; off means the
    /// `/api/v1/auth/oidc` surface is absent. The provider itself is configured in `[oidc]` — this
    /// is only the switch, so a deployment can carry the config while the door stays shut.
    pub oidc: Option<bool>,
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
        if let Some(o) = self.flags.oidc {
            out.push((FlagKey::Oidc, FlagValue::Bool(o)));
        }
        if let Some(f) = self.flags.federation {
            out.push((FlagKey::Federation, FlagValue::Bool(f)));
        }
        if let Some(u) = self.flags.user_accounts {
            out.push((FlagKey::UserAccounts, FlagValue::Bool(u)));
        }
        if let Some(u) = self.flags.upload {
            out.push((FlagKey::Upload, FlagValue::Bool(u)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_derivative_cache_budgets() {
        let file: ServeFile = toml::from_str(
            r#"
                [resources]
                derivative_cache_mb = 1536
                peer_cache_mb = 384
            "#,
        )
        .unwrap();

        assert_eq!(file.resources.derivative_cache_mb, Some(1536));
        assert_eq!(file.resources.peer_cache_mb, Some(384));
    }

    #[test]
    fn derivative_cache_budgets_are_optional() {
        let file = ServeFile::default();
        assert_eq!(file.resources.derivative_cache_mb, None);
        assert_eq!(file.resources.peer_cache_mb, None);
    }
}
#[test]
fn parses_storage_resource_overrides() {
    let file: ServeFile = toml::from_str(
        r#"
            [resources]
            io_max_mib_per_sec = 8
            io_concurrency = 1
            [[resources.storage]]
            path = "/data"
            resource = "shared-hdd"
            kind = "rotational"
            max_mib_per_sec = 2
            [[resources.storage]]
            path = "/assets"
            resource = "shared-hdd"
            kind = "rotational"
        "#,
    )
    .unwrap();
    assert_eq!(file.resources.io_max_mib_per_sec, Some(8));
    assert_eq!(file.resources.storage.len(), 2);
    assert_eq!(
        file.resources.storage[0].resource,
        file.resources.storage[1].resource
    );
    assert_eq!(file.resources.storage[0].max_mib_per_sec, Some(2));
}
