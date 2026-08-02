//! The server config/flags/token/audit store (tech-spec 10 §2.1).
//!
//! A small SQLite database — `server.db` — that lives **beside, but separate from,** the metadata
//! DB (tech-spec 02 owns that both files exist; this file owns the flags/tokens/audit schema). It is
//! host configuration and identity: deliberately outside the portable library file and every export
//! (PRODUCT_SPEC §5, ADR 0004 decision 4), so a different library can be opened under the same
//! server without moving its flags or tokens.
//!
//! The in-memory [`FlagState`] is loaded at open and held behind an `RwLock` so a per-request guard
//! reads it cheaply; a `set` writes the row (version-checked) *and* updates the in-memory copy, then
//! appends an audit entry — both control planes (config file, admin API) go through this one path.

use dam_api::admin::*;
use dam_api::service::Scopes;
use dam_api::{internal, now_ms, LibError};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, RwLock};
use std::time::Duration;

/// The live, in-memory flag values + their optimistic-concurrency versions (tech-spec 10 §2.2).
#[derive(Clone, Debug)]
pub struct FlagState {
    pub auth: AuthMode,
    pub mcp: McpMode,
    pub network_writes: bool,
    /// Hosted-mode background pipeline toggles (issue #71). On by default so a serve deployment
    /// proactively warms previews + analysis; these are workload flags, not exposure flags.
    pub auto_thumbnail: bool,
    pub auto_analyze: bool,
    /// Serve as a federation peer (phase 6, issue #39): mounts `GET /api/v1/advertise`. The reads a
    /// peer then performs ride the ordinary `/api/v1` surface under the ordinary auth gate, so this
    /// flag adds discovery, not exposure.
    pub federation: bool,
    /// Full user accounts (phase 6, issue #42): login/session auth, groups, sharing. Off ⇒ the
    /// whole `/api/v1/auth` + accounts admin surface 404s; on ⇒ the effective auth gate is raised
    /// to at least `Token` (see [`ServerStore::effective_auth_mode`]).
    pub user_accounts: bool,
    /// Accept uploads — writes of *new* files into a registered source (issue #80). Off ⇒
    /// `POST /api/v1/upload` 404s. Squarely an exposure flag, not a workload one: it is the only
    /// surface in 3DAM that puts bytes in the user's project folders (tech-spec 08 §5.1).
    pub upload: bool,
    /// Accept OIDC/OAuth2 logins (phase 6, issue #41). Off ⇒ the `/api/v1/auth/oidc` surface 404s.
    /// An exposure flag: it admits identities minted by a third party, so turning it on needs the
    /// same explicit confirm as opening any other door.
    pub oidc: bool,
    versions: HashMap<FlagKey, u64>,
}

impl FlagState {
    /// The built-in safe-by-default floor (ADR 0004 decision 1): no auth, MCP off, read-only. The
    /// hosted-mode pipeline defaults *on* — it's the "fat server" doing its job, and it exposes
    /// nothing to the network (rule 4 governs exposure, not local workload).
    fn defaults() -> Self {
        FlagState {
            auth: AuthMode::Off,
            mcp: McpMode::Off,
            network_writes: false,
            auto_thumbnail: true,
            auto_analyze: true,
            federation: false,
            user_accounts: false,
            upload: false,
            oidc: false,
            versions: HashMap::new(),
        }
    }
    fn value(&self, key: FlagKey) -> FlagValue {
        match key {
            FlagKey::Authentication => FlagValue::Auth(self.auth),
            FlagKey::McpServer => FlagValue::Mcp(self.mcp),
            FlagKey::NetworkWrites => FlagValue::Bool(self.network_writes),
            FlagKey::AutoThumbnail => FlagValue::Bool(self.auto_thumbnail),
            FlagKey::AutoAnalyze => FlagValue::Bool(self.auto_analyze),
            FlagKey::Federation => FlagValue::Bool(self.federation),
            FlagKey::UserAccounts => FlagValue::Bool(self.user_accounts),
            FlagKey::Upload => FlagValue::Bool(self.upload),
            FlagKey::Oidc => FlagValue::Bool(self.oidc),
        }
    }
    /// Apply a typed value under its key. The key disambiguates the `bool` flags, which the value
    /// alone can't (network_writes vs upload vs the two pipeline toggles).
    fn apply(&mut self, key: FlagKey, value: FlagValue) {
        match (key, value) {
            (FlagKey::Authentication, FlagValue::Auth(m)) => self.auth = m,
            (FlagKey::McpServer, FlagValue::Mcp(m)) => self.mcp = m,
            (FlagKey::NetworkWrites, FlagValue::Bool(b)) => self.network_writes = b,
            (FlagKey::AutoThumbnail, FlagValue::Bool(b)) => self.auto_thumbnail = b,
            (FlagKey::AutoAnalyze, FlagValue::Bool(b)) => self.auto_analyze = b,
            (FlagKey::Federation, FlagValue::Bool(b)) => self.federation = b,
            (FlagKey::UserAccounts, FlagValue::Bool(b)) => self.user_accounts = b,
            (FlagKey::Upload, FlagValue::Bool(b)) => self.upload = b,
            (FlagKey::Oidc, FlagValue::Bool(b)) => self.oidc = b,
            // Type-mismatched pairs are rejected before this point (`FlagValue::matches`).
            _ => {}
        }
    }
    fn version(&self, key: FlagKey) -> u64 {
        self.versions.get(&key).copied().unwrap_or(0)
    }
}

/// Whether applying `new` (over `old`) increases the server's exposure and so needs an explicit
/// `confirm` (tech-spec 10 §5, DESIGN_GUIDELINES §3.6): removing auth, enabling MCP write tools,
/// opening network writes, or dropping the accounts gate that was standing in for auth. The `key`
/// disambiguates the `bool` flags — the hosted-mode pipeline toggles are workload, never exposure,
/// so they never require confirmation.
///
/// `raw_auth` is the *stored* `Authentication` value, which the `UserAccounts` arm needs: accounts-on
/// raises the effective mode from `Off` to `Token` ([`ServerStore::effective_auth_mode`]), so on an
/// instance configured `authentication = off` the accounts flag is the **only** thing demanding a
/// credential. Turning it off there drops the effective mode back to `Off`, which resolves every
/// anonymous request to `Scopes::owner()` with `Visibility::Full` — the catalog *and* the
/// `/admin/api` plane world-open in one unconfirmed flip.
pub fn is_exposure_increasing(
    key: FlagKey,
    new: FlagValue,
    old: FlagValue,
    raw_auth: AuthMode,
) -> bool {
    match (key, new, old) {
        (FlagKey::Authentication, FlagValue::Auth(AuthMode::Off), FlagValue::Auth(o)) => {
            o != AuthMode::Off
        }
        (FlagKey::McpServer, FlagValue::Mcp(McpMode::ReadWrite), FlagValue::Mcp(o)) => {
            o != McpMode::ReadWrite
        }
        (FlagKey::NetworkWrites, FlagValue::Bool(true), FlagValue::Bool(false)) => true,
        // Upload *on* opens the only path by which a remote caller can put bytes in the user's
        // project folders (issue #80). Confirmed like network writes, and for a stronger reason:
        // the blast radius is files on disk rather than rows in the catalog.
        (FlagKey::Upload, FlagValue::Bool(true), FlagValue::Bool(false)) => true,
        // OIDC *on* delegates part of "who may sign in here" to a third party (issue #41). Even
        // configured correctly it is exposure: for a public issuer the audience is the whole
        // internet, and the answer to that is `provisioning`, which an operator has to have chosen
        // deliberately. Confirm-on-enable is how they are made to.
        (FlagKey::Oidc, FlagValue::Bool(true), FlagValue::Bool(false)) => true,
        // Accounts *off* is the exposure-increasing direction, and only while the raise is
        // load-bearing (raw auth `Off`). With auth already at `Anonymous`/`Token` the gate survives
        // the flip, so it stays an ordinary toggle.
        (FlagKey::UserAccounts, FlagValue::Bool(false), FlagValue::Bool(true)) => {
            raw_auth == AuthMode::Off
        }
        _ => false,
    }
}

/// Does flipping this flag ever increase exposure? A UI hint (`FlagInfo::exposure_increasing`) — true
/// for the auth/MCP/network flags, for `UserAccounts` (whose *off* direction can remove the only
/// gate on the instance), for `Upload`, and for `Oidc`; false for the hosted-mode workload toggles.
///
/// Exhaustive on purpose, like [`FlagValue::matches`]. As a `matches!` list this silently answered
/// `false` for any newly-added key, which does not fail a build and does not fail a `set` — the
/// *enforcement* in [`is_exposure_increasing`] would still demand `confirm` while this hint told
/// the UI there was nothing to warn about. Two answers to one question, disagreeing quietly.
fn flag_can_increase_exposure(key: FlagKey) -> bool {
    match key {
        FlagKey::Authentication
        | FlagKey::McpServer
        | FlagKey::NetworkWrites
        | FlagKey::UserAccounts
        | FlagKey::Upload
        | FlagKey::Oidc => true,
        FlagKey::AutoThumbnail | FlagKey::AutoAnalyze => false,
        // Discovery, not exposure: the reads a peer performs still ride the ordinary auth gate.
        FlagKey::Federation => false,
    }
}

pub struct ServerStore {
    conn: Mutex<Connection>,
    flags: RwLock<FlagState>,
    /// The claim window re-opened by the config-file escape hatch (ADR 0009 §3, issue #42):
    /// per-boot, in-memory only — the next successful claim closes it. With zero accounts the
    /// instance is unclaimed regardless of this bit.
    claim_reopened: std::sync::atomic::AtomicBool,
    /// Bumped on every account/group/share mutation, so a long-lived subscriber (the WS loop) can
    /// notice its resolved visibility ceiling is stale and re-resolve (issue #42).
    visibility_gen: std::sync::atomic::AtomicU64,
}

impl ServerStore {
    /// Open (creating + migrating) the server store at `path`, loading the flag state into memory.
    pub fn open(path: &Path) -> Result<ServerStore, LibError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(internal)?;
        }
        let conn = Connection::open(path).map_err(internal)?;
        Self::from_conn(conn, Some(path))
    }

    /// Open an in-memory store (tests, and the CLI's embedded no-serve-store path).
    pub fn open_in_memory() -> Result<ServerStore, LibError> {
        let conn = Connection::open_in_memory().map_err(internal)?;
        Self::from_conn(conn, None)
    }

    fn from_conn(mut conn: Connection, path: Option<&Path>) -> Result<ServerStore, LibError> {
        // A second process opening the same data directory should wait for the short, serialized
        // migration transaction and then observe its version, not fail spuriously with SQLITE_BUSY.
        conn.busy_timeout(Duration::from_secs(30))
            .map_err(internal)?;
        // The accounts tables lean on cascading deletes (sessions/memberships/shares follow their
        // account or group); rusqlite leaves foreign keys off per SQLite default, so opt in.
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(internal)?;
        schema::migrate(&mut conn, path)?;
        let store = ServerStore {
            conn: Mutex::new(conn),
            flags: RwLock::new(FlagState::defaults()),
            claim_reopened: std::sync::atomic::AtomicBool::new(false),
            visibility_gen: std::sync::atomic::AtomicU64::new(0),
        };
        store.load_flags()?;
        Ok(store)
    }

    fn load_flags(&self) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let mut state = FlagState::defaults();
        let mut stmt = conn
            .prepare("SELECT key, value, version FROM feature_flag")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(internal)?;
        for row in rows {
            let (key_s, val_s, version) = row.map_err(internal)?;
            let Some(key) = FlagKey::parse(&key_s) else {
                continue; // unknown key from a newer build — ignore, forward-compatible
            };
            if let Ok(value) = serde_json::from_str::<FlagValue>(&val_s) {
                if value.matches(key) {
                    state.apply(key, value);
                    state.versions.insert(key, version as u64);
                }
            }
        }
        *self.flags.write().unwrap() = state;
        Ok(())
    }

    // ── flag reads (cheap, in-memory) ────────────────────────────────────────

    pub fn auth_mode(&self) -> AuthMode {
        self.flags.read().unwrap().auth
    }
    pub fn mcp_mode(&self) -> McpMode {
        self.flags.read().unwrap().mcp
    }
    pub fn network_writes(&self) -> bool {
        self.flags.read().unwrap().network_writes
    }
    /// Hosted-mode: auto-generate thumbnails/previews on ingest (issue #71).
    pub fn auto_thumbnail(&self) -> bool {
        self.flags.read().unwrap().auto_thumbnail
    }
    /// Hosted-mode: auto-run the analysis pass on ingest (issue #71).
    pub fn auto_analyze(&self) -> bool {
        self.flags.read().unwrap().auto_analyze
    }
    /// Federation peer mode (phase 6, issue #39): serve `GET /api/v1/advertise`.
    pub fn federation(&self) -> bool {
        self.flags.read().unwrap().federation
    }
    /// Full user accounts (phase 6, issue #42): the `/api/v1/auth` + accounts admin surface.
    pub fn user_accounts(&self) -> bool {
        self.flags.read().unwrap().user_accounts
    }
    /// Accept uploads (issue #80): mount `POST /api/v1/upload`.
    pub fn upload(&self) -> bool {
        self.flags.read().unwrap().upload
    }

    /// Accept OIDC logins (issue #41): mount the `/api/v1/auth/oidc` surface.
    pub fn oidc(&self) -> bool {
        self.flags.read().unwrap().oidc
    }

    /// The single `Oidc` guard, behind the router's `route_layer` on `/api/v1/auth/oidc`.
    ///
    /// Requires **both** flags. `UserAccounts` is not implied by turning `oidc` on, and an OIDC
    /// login has nowhere to land without accounts — a verified subject resolves to an account or to
    /// nothing. Checking only `oidc` would leave a login flow that authenticates successfully and
    /// then fails at the last step, which reads as a broken provider rather than a missing switch.
    /// `NotFound` for the same reason as every other gate: off ⇒ the surface is absent (ADR 0004).
    pub fn require_oidc(&self) -> Result<(), LibError> {
        if !self.user_accounts() {
            return Err(LibError::NotFound(
                "OIDC login needs user accounts, which are disabled".into(),
            ));
        }
        if !self.oidc() {
            return Err(LibError::NotFound("OIDC login is disabled".into()));
        }
        Ok(())
    }

    /// The single `Upload` guard, behind the router's `route_layer` on `/api/v1/upload`.
    /// `NotFound`, not `Forbidden` — off ⇒ the surface is *absent* (ADR 0004), the same answer
    /// `/mcp` and the accounts surface give, so a disabled capability cannot be probed for.
    pub fn require_upload(&self) -> Result<(), LibError> {
        if self.upload() {
            Ok(())
        } else {
            Err(LibError::NotFound("uploads are disabled".into()))
        }
    }

    /// The single `UserAccounts` guard, shared by every gated surface: the HTTP router's one
    /// `route_layer` (`/api/v1/auth` and the accounts block of `/admin/api`) and the CLI's embedded
    /// accounts verbs. `NotFound`, not `Forbidden` — off ⇒ the surface is *absent* (ADR 0004).
    pub fn require_user_accounts(&self) -> Result<(), LibError> {
        if self.user_accounts() {
            Ok(())
        } else {
            Err(LibError::NotFound("user accounts are disabled".into()))
        }
    }

    /// The auth mode requests are actually resolved under: accounts-on raises `Off` to `Token`
    /// (real identities imply a gate — issue #42), otherwise the configured mode stands.
    /// `Anonymous` is left alone: public-read + login-to-elevate is a coherent posture.
    pub fn effective_auth_mode(&self) -> AuthMode {
        let f = self.flags.read().unwrap();
        if f.user_accounts && f.auth == AuthMode::Off {
            AuthMode::Token
        } else {
            f.auth
        }
    }

    pub fn flag_info(&self, key: FlagKey) -> FlagInfo {
        let f = self.flags.read().unwrap();
        FlagInfo {
            key,
            value: f.value(key),
            version: f.version(key),
            live: true, // every flag here applies live (ADR 0009 §2)
            exposure_increasing: flag_can_increase_exposure(key),
        }
    }
    pub fn all_flags(&self) -> Vec<FlagInfo> {
        FlagKey::ALL
            .into_iter()
            .map(|k| self.flag_info(k))
            .collect()
    }

    // ── flag writes (config seed + admin set), both audited (tech-spec 10 §2.3) ──

    /// Seed a flag **only if absent** (`config_authority = seed-only`, the default — ADR 0009 §2):
    /// the config file sets the initial value; the admin UI/CLI owns it thereafter. Returns `true`
    /// if the seed was written.
    pub fn seed_flag(&self, key: FlagKey, value: FlagValue) -> Result<bool, LibError> {
        if !value.matches(key) {
            return Err(LibError::BadRequest(format!(
                "flag '{key}' value has the wrong type"
            )));
        }
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM feature_flag WHERE key = ?1",
                params![key.as_str()],
                |_| Ok(true),
            )
            .optional()
            .map_err(internal)?
            .unwrap_or(false);
        if exists {
            return Ok(false);
        }
        let val_s = serde_json::to_string(&value).map_err(internal)?;
        let now = now_ms();
        conn.execute(
            "INSERT INTO feature_flag (key, value, version, updated_at, updated_by)
             VALUES (?1, ?2, 1, ?3, 'config-file')",
            params![key.as_str(), val_s, now],
        )
        .map_err(internal)?;
        Self::audit_row(
            &conn,
            "config-file",
            "flag.seed",
            Some(key.as_str()),
            Some(serde_json::json!({ "value": value })),
        )?;
        drop(conn);
        {
            let mut f = self.flags.write().unwrap();
            f.apply(key, value);
            f.versions.insert(key, 1);
        }
        Ok(true)
    }

    /// Set a flag through the audited path (tech-spec 10 §2.3, §5): optimistic-concurrency check,
    /// exposure confirmation, store write, in-memory update, audit append. Returns the applied info
    /// — plus the bootstrap owner token when this set turned authentication on with zero admin
    /// credentials in the store (never locked out: the gate always comes with a key).
    pub fn set_flag(
        &self,
        key: FlagKey,
        req: SetFlag,
        actor: &str,
    ) -> Result<SetFlagReply, LibError> {
        if !req.value.matches(key) {
            return Err(LibError::BadRequest(format!(
                "flag '{key}' value has the wrong type"
            )));
        }
        let (old_value, cur_version, raw_auth) = {
            let f = self.flags.read().unwrap();
            (f.value(key), f.version(key), f.auth)
        };
        if let Some(expected) = req.expected_version {
            if expected != cur_version {
                return Err(LibError::Conflict(format!(
                    "flag '{key}' changed underneath you (have v{expected}, current v{cur_version})"
                )));
            }
        }
        if is_exposure_increasing(key, req.value, old_value, raw_auth) && !req.confirm {
            return Err(LibError::BadRequest(format!(
                "setting '{key}' increases exposure — resend with confirm=true"
            )));
        }
        let new_version = cur_version + 1;
        let val_s = serde_json::to_string(&req.value).map_err(internal)?;
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO feature_flag (key, value, version, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(key) DO UPDATE SET value=?2, version=?3, updated_at=?4, updated_by=?5",
                params![key.as_str(), val_s, new_version as i64, now, actor],
            )
            .map_err(internal)?;
            Self::audit_row(
                &conn,
                actor,
                "flag.set",
                Some(key.as_str()),
                Some(serde_json::json!({ "before": old_value, "after": req.value })),
            )?;
        }
        {
            let mut f = self.flags.write().unwrap();
            f.apply(key, req.value);
            f.versions.insert(key, new_version);
        }
        // Turning authentication on must hand the operator a key in the same motion: with zero
        // admin-scoped tokens the instance would be gated with no holder of a credential. The
        // `UserAccounts` flag raises the *effective* gate the same way (accounts imply ≥ Token),
        // so it gets the same guarantee — the claim flow then supersedes the token.
        let bootstrap_token = match (key, req.value) {
            (FlagKey::Authentication, FlagValue::Auth(mode)) if mode != AuthMode::Off => {
                self.bootstrap_owner_token_if_needed(actor)?
            }
            (FlagKey::UserAccounts, FlagValue::Bool(true)) => {
                self.bootstrap_owner_token_if_needed(actor)?
            }
            _ => None,
        };
        Ok(SetFlagReply {
            flag: self.flag_info(key),
            bootstrap_token,
        })
    }

    /// Mint the **bootstrap owner token** (label `owner`, full owner scopes, no expiry) iff no
    /// admin-scoped token exists — the "never locked out" guarantee behind [`Self::set_flag`] and
    /// serve startup (a config file can seed a credentialed mode on first boot). No-op otherwise, so
    /// it never re-mints or spams the audit log.
    pub fn bootstrap_owner_token_if_needed(
        &self,
        actor: &str,
    ) -> Result<Option<NewTokenReply>, LibError> {
        let has_admin = self
            .list_tokens()?
            .iter()
            .any(|t| t.scopes.has(dam_api::service::Scope::Admin));
        if has_admin {
            return Ok(None);
        }
        self.create_token(
            NewToken {
                label: "owner".into(),
                scopes: Scopes::owner(),
                expires: None,
            },
            actor,
        )
        .map(Some)
    }

    // ── tokens (tech-spec 10 §1.4) ───────────────────────────────────────────

    /// Issue a scoped API key. The plaintext secret is returned **once** and only its hash stored.
    pub fn create_token(&self, req: NewToken, actor: &str) -> Result<NewTokenReply, LibError> {
        if req.label.trim().is_empty() {
            return Err(LibError::BadRequest("token label is required".into()));
        }
        // A scope-less token authenticates yet can do nothing — under `Token` mode it 403s every
        // route, which reads as a broken credential. Reject it at mint time rather than hand back a
        // confusing dud.
        if req.scopes.to_vec().is_empty() {
            return Err(LibError::BadRequest(
                "a token needs at least one scope (read, write, admin, mcp_use, or federate)"
                    .into(),
            ));
        }
        // High-entropy secret: two time-ordered v7 UUIDs (each carries OS-random bits) → 128 hex
        // chars behind a `dam_` prefix. Stored only as its blake3 hash.
        let secret = format!(
            "dam_{}{}",
            uuid::Uuid::now_v7().simple(),
            uuid::Uuid::now_v7().simple()
        );
        let token_id = uuid::Uuid::now_v7().simple().to_string();
        let hash = hash_secret(&secret);
        let scopes_s = serde_json::to_string(&req.scopes).map_err(internal)?;
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO token (token_id, label, secret_hash, scopes, created, expires, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![token_id, req.label, hash, scopes_s, now, req.expires],
            )
            .map_err(internal)?;
            Self::audit_row(
                &conn,
                actor,
                "token.create",
                Some(&token_id),
                Some(serde_json::json!({ "label": req.label, "scopes": req.scopes })),
            )?;
        }
        Ok(NewTokenReply {
            token_id,
            label: req.label,
            scopes: req.scopes,
            secret,
        })
    }

    pub fn list_tokens(&self) -> Result<Vec<TokenInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT token_id, label, scopes, created, expires, last_used
                 FROM token ORDER BY created DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TokenInfo {
                    token_id: r.get(0)?,
                    label: r.get(1)?,
                    scopes: serde_json::from_str(&r.get::<_, String>(2)?)
                        .unwrap_or_else(|_| Scopes::none()),
                    created: r.get(3)?,
                    expires: r.get(4)?,
                    last_used: r.get(5)?,
                })
            })
            .map_err(internal)?;
        rows.collect::<Result<_, _>>().map_err(internal)
    }

    pub fn revoke_token(&self, token_id: &str, actor: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("DELETE FROM token WHERE token_id = ?1", params![token_id])
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound(format!("token {token_id}")));
        }
        Self::audit_row(&conn, actor, "token.revoke", Some(token_id), None)?;
        Ok(())
    }

    /// Verify a presented bearer secret (tech-spec 10 §1.2). Returns the token's `(label, scopes)`
    /// on a hit (stamping `last_used`), `None` on miss/expiry.
    pub fn verify_token(&self, secret: &str) -> Result<Option<(String, Scopes)>, LibError> {
        let hash = hash_secret(secret);
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT token_id, label, scopes, expires FROM token WHERE secret_hash = ?1",
                params![hash],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?;
        let Some((token_id, label, scopes_s, expires)) = row else {
            return Ok(None);
        };
        if let Some(exp) = expires {
            if now_ms() >= exp {
                return Ok(None); // expired credential is a miss, not an error
            }
        }
        let scopes: Scopes = serde_json::from_str(&scopes_s).unwrap_or_else(|_| Scopes::none());
        conn.execute(
            "UPDATE token SET last_used = ?1 WHERE token_id = ?2",
            params![now_ms(), token_id],
        )
        .map_err(internal)?;
        Ok(Some((label, scopes)))
    }

    // ── factory reset (Settings §Storage → Factory reset) ────────────────────

    /// Erase the server config — every token, feature flag, and audit entry — and drop the in-memory
    /// flag state back to the safe-by-default floor (tech-spec 10 §2). Returns the number of tokens
    /// removed. This revokes the caller's own admin token and returns `authentication` to `Off`
    /// (localhost implicit trust); it is the `server.db` half of a first-run factory reset and the
    /// UI warns before invoking it. The catalog + caches are wiped separately by the engine.
    pub fn factory_reset(&self) -> Result<u64, LibError> {
        let removed = {
            let conn = self.conn.lock().unwrap();
            let removed: i64 = conn
                .query_row("SELECT COUNT(*) FROM token", [], |r| r.get(0))
                .map_err(internal)?;
            conn.execute("DELETE FROM token", []).map_err(internal)?;
            conn.execute("DELETE FROM feature_flag", [])
                .map_err(internal)?;
            conn.execute("DELETE FROM audit_log", [])
                .map_err(internal)?;
            // The accounts plane resets too (sessions/memberships/shares cascade; failures also
            // cleared) — a factory reset returns the instance to the unclaimed first-run state.
            conn.execute("DELETE FROM share", []).map_err(internal)?;
            conn.execute("DELETE FROM group_", []).map_err(internal)?;
            conn.execute("DELETE FROM account", []).map_err(internal)?;
            conn.execute("DELETE FROM login_failure", [])
                .map_err(internal)?;
            removed as u64
        };
        *self.flags.write().unwrap() = FlagState::defaults();
        Ok(removed)
    }

    // ── audit (tech-spec 10 §4.5) ────────────────────────────────────────────

    /// Append a required audit record. Callers must propagate failure: most audited mutations live
    /// in `library.db` or a source backend and cannot join this `server.db` transaction, so the
    /// defined fallback is fail-after-mutation and explicit client reconciliation, never false
    /// success or an invented cross-database rollback.
    pub fn audit(
        &self,
        actor: &str,
        action: &str,
        target: Option<&str>,
        detail: Option<serde_json::Value>,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        Self::audit_row(&conn, actor, action, target, detail)
    }

    fn audit_row(
        conn: &Connection,
        actor: &str,
        action: &str,
        target: Option<&str>,
        detail: Option<serde_json::Value>,
    ) -> Result<(), LibError> {
        let detail_s = detail
            .map(|d| serde_json::to_string(&d))
            .transpose()
            .map_err(internal)?;
        conn.execute(
            "INSERT INTO audit_log (at, actor, action, target, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now_ms(), actor, action, target, detail_s],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn list_audit(&self, limit: u32) -> Result<Vec<AuditEntry>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT at, actor, action, target, detail
                 FROM audit_log ORDER BY id DESC LIMIT ?1",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![limit], |r| {
                let detail: Option<String> = r.get(4)?;
                Ok(AuditEntry {
                    at: r.get(0)?,
                    actor: r.get(1)?,
                    action: r.get(2)?,
                    target: r.get(3)?,
                    detail: detail.and_then(|s| serde_json::from_str(&s).ok()),
                })
            })
            .map_err(internal)?;
        rows.collect::<Result<_, _>>().map_err(internal)
    }

    // ── status (tech-spec 10 §5) ─────────────────────────────────────────────

    pub fn status(&self, bind: &str, localhost_only: bool, tls: bool) -> AdminStatus {
        let (auth, mcp, network_writes, accounts_enabled, upload_enabled) = {
            let f = self.flags.read().unwrap();
            (f.auth, f.mcp, f.network_writes, f.user_accounts, f.upload)
        };
        // "Exposed" = reachable off-box, no transport encryption, and no credential demanded of an
        // unauthenticated caller. Both `Off` (everyone is owner) and `Anonymous` (everyone gets read
        // unauthenticated) qualify — `Anonymous` on `0.0.0.0` is still a world-readable catalog, so
        // it must trip the same warning rather than reporting itself safe. Accounts-on raises the
        // effective gate, so it is judged on the effective mode, not the raw flag.
        let effective = self.effective_auth_mode();
        let unauthenticated_reachable = matches!(effective, AuthMode::Off | AuthMode::Anonymous);
        let exposed_without_auth = !localhost_only && unauthenticated_reachable && !tls;
        AdminStatus {
            bind: bind.to_string(),
            localhost_only,
            tls,
            auth,
            mcp,
            network_writes,
            exposed_without_auth,
            token_count: self.list_tokens().map(|t| t.len()).unwrap_or(0),
            accounts_enabled,
            upload_enabled,
            unclaimed: self.unclaimed(),
            account_count: self.count_accounts().unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod audit_failure_tests {
    use super::*;

    #[test]
    fn injected_sqlite_audit_failure_is_returned() {
        let store = ServerStore::open_in_memory().unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute("DROP TABLE audit_log", [])
            .unwrap();

        let error = store
            .audit("test", "required.mutation", None, None)
            .unwrap_err();
        assert!(error.to_string().contains("audit_log"));
    }
}

/// Hash a token secret. blake3 of the high-entropy random secret — not a password KDF, because an
/// API key is not a low-entropy user secret (tech-spec 10 §1.4; account passwords, which *do* use
/// argon2id, are phase 6). Rendered as lowercase hex for a stable indexed lookup column.
fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

mod schema;

// The accounts/sessions/groups/shares surface (issue #42) — a child module so it can reach the
// private `conn`/`flags` fields while keeping this file to flags/tokens/audit.
pub(crate) mod accounts;

// The OIDC/OAuth2 login surface (issue #41) — provider config, in-flight authorization requests,
// and identity links. Same child-module reasoning as `accounts`.
pub(crate) mod oidc;
