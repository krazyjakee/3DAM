//! Admin / feature-flag wire types (tech-spec 10 §3, §5).
//!
//! These are the shapes the `/admin/api/*` surface exchanges, shared by the server (which owns the
//! store + semantics) and every admin client (the CLI over `--connect`, later the web Settings
//! surface). Kept in `dam-api` — the dependency-free seam — so both sides agree on one definition.
//!
//! Scope grows with the served surface: the phase-5 flags (**authentication**, **MCP server**,
//! **network writes**) plus the read-only exposure/status view, and then one flag per capability
//! phase as it lands — user accounts (#42), uploads (#80), inbound federation (#39), OIDC login
//! (#41). "Off removes the surface" (ADR 0004), so a flag appears here only once there is a real
//! surface for it to unmount.

use crate::service::Scopes;
use serde::{Deserialize, Serialize};

/// The authentication mode — one gate over the whole shared surface (tech-spec 10 §1.1).
///
/// There is deliberately no `Oidc` variant. Tech-spec 10 §1.1 sketched one, but OIDC turned out to
/// belong beside this enum rather than inside it: §1.5 has an OIDC login mint an ordinary server
/// session, so it composes with password login and bearer tokens instead of displacing them. It is
/// therefore [`FlagKey::Oidc`] — a capability you switch on — while this stays the single answer to
/// "what does this instance demand of an unauthenticated caller?".
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
    /// `bool` — the hosted-mode background pipeline auto-generates thumbnails + previews on ingest
    /// (issue #71). On by default so a fresh client hits ready previews; off lets a low-power host
    /// defer the render cost to on-demand generation.
    AutoThumbnail,
    /// `bool` — the hosted-mode background pipeline auto-runs the analysis pass (embeddings,
    /// auto-tags, derived attrs) on ingest (issue #71). On by default; off leaves thumbnails-only.
    AutoAnalyze,
    /// `bool` — serve this instance as a federation peer (phase 6, issue #39): mounts
    /// `GET /api/v1/advertise` so other 3DAM instances can register this one as a federated
    /// source. Off by default; off means the surface disappears (ADR 0004). The read queries a
    /// peer then issues are the ordinary read API under the ordinary auth gate.
    Federation,
    /// `bool` — full user accounts (phase 6, issue #42): login/session auth, groups, and
    /// source/collection sharing. Off by default; off means the whole `/api/v1/auth` +
    /// accounts/groups/shares admin surface 404s. Turning it on raises the effective auth gate to
    /// at least `Token` (accounts mean real identities) and, with zero accounts, enters the
    /// *unclaimed* state — the first loopback signup becomes admin.
    UserAccounts,
    /// `bool` — accept uploads: writes of *new* files into a registered source (issue #80). Off by
    /// default; off means `POST /api/v1/upload` 404s and the client's Upload view disappears.
    ///
    /// Its own flag rather than a corner of `NetworkWrites` or `Scope::Write`, because it is its
    /// own exposure class: every other write in 3DAM edits the catalog (tags, notes, collections),
    /// and this is the only one that puts bytes in the user's project folders (tech-spec 08 §5.1).
    /// A token minted for tagging should not silently also be able to do that, and an operator who
    /// wants tagging-without-uploading had no way to say so while the two shared one scope.
    Upload,
    /// `bool` — accept logins from a configured OIDC/OAuth2 provider (phase 6, issue #41). Off by
    /// default; off means the whole `/api/v1/auth/oidc` surface 404s and the client shows no
    /// "sign in with…" button.
    ///
    /// A **flag**, not a fourth `AuthMode`. The mode answers "what does this instance demand of a
    /// caller?", and OIDC does not replace that: tech-spec 10 §1.5 has OIDC issuing an ordinary
    /// server session, so an instance can accept passwords *and* OIDC at once, and every bearer
    /// token keeps working. Modelling it as a mode would have made those mutually exclusive.
    ///
    /// Implies `UserAccounts`: an OIDC login resolves to an account, so with accounts off there is
    /// nothing for a verified subject to become.
    Oidc,
}

impl FlagKey {
    pub const ALL: [FlagKey; 9] = [
        FlagKey::Authentication,
        FlagKey::McpServer,
        FlagKey::NetworkWrites,
        FlagKey::AutoThumbnail,
        FlagKey::AutoAnalyze,
        FlagKey::Federation,
        FlagKey::UserAccounts,
        FlagKey::Upload,
        FlagKey::Oidc,
    ];
    /// The stable string used in the URL, the store, and the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            FlagKey::Authentication => "authentication",
            FlagKey::McpServer => "mcp_server",
            FlagKey::NetworkWrites => "network_writes",
            FlagKey::AutoThumbnail => "auto_thumbnail",
            FlagKey::AutoAnalyze => "auto_analyze",
            FlagKey::Federation => "federation",
            FlagKey::UserAccounts => "user_accounts",
            FlagKey::Upload => "upload",
            FlagKey::Oidc => "oidc",
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
    ///
    /// Written as an exhaustive `match` on the key rather than a `matches!` list. `matches!` is
    /// closed over the arms it names, so a newly-added [`FlagKey`] compiles clean and silently
    /// answers `false` — i.e. the new flag is un-settable, with no error pointing here. The
    /// exhaustive form makes the compiler demand an arm for every future key instead.
    pub fn matches(self, key: FlagKey) -> bool {
        match key {
            FlagKey::Authentication => matches!(self, FlagValue::Auth(_)),
            FlagKey::McpServer => matches!(self, FlagValue::Mcp(_)),
            FlagKey::NetworkWrites
            | FlagKey::AutoThumbnail
            | FlagKey::AutoAnalyze
            | FlagKey::Federation
            | FlagKey::UserAccounts
            | FlagKey::Upload
            | FlagKey::Oidc => matches!(self, FlagValue::Bool(_)),
        }
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

/// Reply to `PUT /admin/api/flags/{key}`: the updated flag — plus, exactly when enabling
/// authentication activated a gate with zero admin credentials in the store, the auto-minted
/// **bootstrap owner token** (never locked out: the gate always comes with a key). The flag fields
/// are flattened, so readers expecting a bare [`FlagInfo`] keep working.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetFlagReply {
    #[serde(flatten)]
    pub flag: FlagInfo,
    /// Present only when this set minted the first admin credential; the secret is shown once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_token: Option<NewTokenReply>,
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
    /// The `UserAccounts` flag (phase 6, issue #42). Additive: defaults keep older clients working.
    #[serde(default)]
    pub accounts_enabled: bool,
    /// The `Upload` flag (issue #80) — whether this instance accepts writes of new files into a
    /// registered source. Reported beside `network_writes` because it is the same question one
    /// level further in: that one asks whether the network may write to the *catalog*, this one
    /// whether it may write to the user's *files*.
    #[serde(default)]
    pub upload_enabled: bool,
    /// Accounts are on but no account exists (or the claim window was re-opened by the config
    /// escape hatch): the next loopback signup becomes admin. Loud on purpose — an exposed
    /// unclaimed instance must never be silent.
    #[serde(default)]
    pub unclaimed: bool,
    #[serde(default)]
    pub account_count: usize,
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

// ── storage & maintenance (Settings §Storage) ────────────────────────────────
//
// The operator-plane maintenance surface behind `/admin/api/maintenance/*`: report disk usage,
// clear the regenerable caches, reset analysis, compact/vacuum, and the two destructive wipes
// (catalog reset and full factory reset). All non-destructive to files inside registered sources —
// only 3DAM's own SQLite DBs and its `<data_dir>/cache/` derivatives are ever touched.

/// One cache tier's on-disk footprint (the image thumbnails or the 3D preview meshes).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CacheUsage {
    pub bytes: u64,
    pub files: u64,
}

/// Storage report for the Settings surface: DB sizes, both cache tiers, and catalog counts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageUsage {
    /// The data directory the report is for (shown so the operator knows what they're clearing).
    pub data_dir: String,
    /// Size of `library.db` on disk (bytes; excludes the WAL/SHM sidecars).
    pub library_db_bytes: u64,
    /// Size of `server.db` on disk (bytes).
    pub server_db_bytes: u64,
    pub thumbnails: CacheUsage,
    pub previews: CacheUsage,
    pub asset_count: u64,
    pub source_count: u64,
}

/// Which regenerable cache tier(s) to clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTarget {
    /// The image thumbnail cache (`<data_dir>/cache/thumbnails`).
    Thumbnails,
    /// The 3D preview-mesh cache (`<data_dir>/cache/previews`).
    Previews,
    /// Both tiers.
    All,
}

/// Body of `POST /admin/api/maintenance/clear-cache`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ClearCacheRequest {
    pub target: CacheTarget,
}

/// Result of clearing a cache tier — what was freed. Regenerated on next thumbnail/preview read.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ClearCacheReport {
    pub bytes_freed: u64,
    pub files_deleted: u64,
}

/// Result of clearing analysis: dropped suggestions + embeddings (derived attrs are nulled and each
/// asset is marked due for re-analysis; user-confirmed tags are kept).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ClearAnalysisReport {
    pub suggestions_removed: u64,
    pub embeddings_removed: u64,
}

/// Result of a `VACUUM` — before/after `library.db` size and the bytes reclaimed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct VacuumReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub reclaimed_bytes: u64,
}

/// Body of the destructive maintenance ops (`wipe`, `factory-reset`) — the machine form of
/// warn-and-confirm: the server rejects the call unless `confirm` is `true`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ConfirmRequest {
    #[serde(default)]
    pub confirm: bool,
}

/// Result of a catalog wipe / library reset — what was cleared (files in sources untouched).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WipeReport {
    pub assets_removed: u64,
    pub sources_removed: u64,
    pub collections_removed: u64,
    pub tags_removed: u64,
}

/// Result of a factory reset: the catalog wipe + caches cleared + the server config erased
/// (tokens/flags/audit). Returns the app to first-run state.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FactoryResetReport {
    pub catalog: WipeReport,
    pub cache: ClearCacheReport,
    pub tokens_removed: u64,
}

// ── OIDC provider configuration (phase 6, issue #41) ────────────────────────
//
// Split into a read shape and a write shape on purpose. Tech-spec 10 §5 requires that OIDC client
// secrets are never returned by a `GET`, and the cheapest way to guarantee that is for the type the
// read path can construct to have nowhere to put one.

/// What an operator may configure about the OIDC provider (tech-spec 10 §1.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcConfig {
    /// Issuer URL. The discovery document at `{issuer}/.well-known/openid-configuration` supplies
    /// the authorization/token endpoints and the JWKS URI, so this is the only endpoint an operator
    /// gives us.
    pub issuer: String,
    pub client_id: String,
    /// Where the issuer sends the browser back. Must be registered with the provider, and is echoed
    /// in the token exchange, so it has to match exactly on both sides.
    pub redirect_url: String,
    /// Extra scopes beyond `openid`, which is always requested. `email`/`profile` are the usual
    /// additions and are what make a useful username available.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// What to do with a verified subject that no local account is linked to.
    #[serde(default)]
    pub provisioning: OidcProvisioning,
}

/// Policy for a verified-but-unknown subject. Tech-spec 10 §1.5 fixes the v1 default as
/// reject-unless-linked: a correctly-configured provider is not by itself authority to create
/// accounts on this instance, because for most providers *anyone* can hold a valid account.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OidcProvisioning {
    /// Refuse the login. An admin must link the subject to an account first.
    #[default]
    Linked,
    /// Create an account on first login, with [`OidcConfig::default_role`]-equivalent rights.
    /// Only sane when the issuer's audience *is* the intended user set (a corporate tenant).
    AutoViewer,
    AutoEditor,
}

/// The provider config as read back: identical to [`OidcConfig`] minus any secret, plus whether a
/// secret is on file at all — an operator needs to see "configured" without seeing the value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcConfigInfo {
    #[serde(flatten)]
    pub config: OidcConfig,
    /// True when a client secret is stored. Never the secret itself (tech-spec 10 §5).
    pub client_secret_set: bool,
}

/// Body of `PUT /admin/api/oidc`. The secret is write-only and *optional on update*: omitting it
/// keeps the stored one, so an operator can edit the issuer or scopes without re-entering it (and
/// without the UI having to round-trip a value it is never allowed to read).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetOidcConfig {
    #[serde(flatten)]
    pub config: OidcConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// A link between a provider subject and a local account (`GET /admin/api/oidc/identities`).
/// `username` is denormalised in so an admin screen can show who a link points at without a join.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcIdentity {
    pub issuer: String,
    pub subject: String,
    pub account_id: String,
    pub username: String,
    pub linked_at: i64,
}

/// Body of `POST /admin/api/oidc/identities`.
///
/// The issuer is not a field: it is taken from the configured provider, so an admin cannot
/// accidentally link a subject under an issuer this instance does not actually accept tokens from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkOidcIdentity {
    /// The provider's stable subject claim (`sub`) for the user being linked.
    pub subject: String,
    pub account_id: String,
}
