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

/// The live, in-memory flag values + their optimistic-concurrency versions (tech-spec 10 §2.2).
#[derive(Clone, Debug)]
pub struct FlagState {
    pub auth: AuthMode,
    pub mcp: McpMode,
    pub network_writes: bool,
    versions: HashMap<FlagKey, u64>,
}

impl FlagState {
    /// The built-in safe-by-default floor (ADR 0004 decision 1): no auth, MCP off, read-only.
    fn defaults() -> Self {
        FlagState {
            auth: AuthMode::Off,
            mcp: McpMode::Off,
            network_writes: false,
            versions: HashMap::new(),
        }
    }
    fn value(&self, key: FlagKey) -> FlagValue {
        match key {
            FlagKey::Authentication => FlagValue::Auth(self.auth),
            FlagKey::McpServer => FlagValue::Mcp(self.mcp),
            FlagKey::NetworkWrites => FlagValue::Bool(self.network_writes),
        }
    }
    fn apply(&mut self, value: FlagValue) {
        match value {
            FlagValue::Auth(m) => self.auth = m,
            FlagValue::Mcp(m) => self.mcp = m,
            FlagValue::Bool(b) => self.network_writes = b,
        }
    }
    fn version(&self, key: FlagKey) -> u64 {
        self.versions.get(&key).copied().unwrap_or(0)
    }
}

/// Whether applying `new` (over `old`) increases the server's exposure and so needs an explicit
/// `confirm` (tech-spec 10 §5, DESIGN_GUIDELINES §3.6): removing auth, enabling MCP write tools, or
/// opening network writes.
pub fn is_exposure_increasing(new: FlagValue, old: FlagValue) -> bool {
    match (new, old) {
        (FlagValue::Auth(AuthMode::Off), FlagValue::Auth(o)) => o != AuthMode::Off,
        (FlagValue::Mcp(McpMode::ReadWrite), FlagValue::Mcp(o)) => o != McpMode::ReadWrite,
        (FlagValue::Bool(true), FlagValue::Bool(false)) => true,
        _ => false,
    }
}

pub struct ServerStore {
    conn: Mutex<Connection>,
    flags: RwLock<FlagState>,
}

impl ServerStore {
    /// Open (creating + migrating) the server store at `path`, loading the flag state into memory.
    pub fn open(path: &Path) -> Result<ServerStore, LibError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path).map_err(internal)?;
        conn.execute_batch(SCHEMA).map_err(internal)?;
        let store = ServerStore {
            conn: Mutex::new(conn),
            flags: RwLock::new(FlagState::defaults()),
        };
        store.load_flags()?;
        Ok(store)
    }

    /// Open an in-memory store (tests, and the CLI's embedded no-serve-store path).
    pub fn open_in_memory() -> Result<ServerStore, LibError> {
        let conn = Connection::open_in_memory().map_err(internal)?;
        conn.execute_batch(SCHEMA).map_err(internal)?;
        let store = ServerStore {
            conn: Mutex::new(conn),
            flags: RwLock::new(FlagState::defaults()),
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
                    state.apply(value);
                    state.versions.insert(key, version as u64);
                }
            }
        }
        *self.flags.write().unwrap() = state;
        Ok(())
    }

    // ── flag reads (cheap, in-memory) ────────────────────────────────────────

    pub fn flag_state(&self) -> FlagState {
        self.flags.read().unwrap().clone()
    }
    pub fn auth_mode(&self) -> AuthMode {
        self.flags.read().unwrap().auth
    }
    pub fn mcp_mode(&self) -> McpMode {
        self.flags.read().unwrap().mcp
    }
    pub fn network_writes(&self) -> bool {
        self.flags.read().unwrap().network_writes
    }

    pub fn flag_info(&self, key: FlagKey) -> FlagInfo {
        let f = self.flags.read().unwrap();
        FlagInfo {
            key,
            value: f.value(key),
            version: f.version(key),
            live: true,                // every phase-5 flag applies live (ADR 0009 §2)
            exposure_increasing: true, // all three can increase exposure — a UI hint (§5)
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
        self.flags.write().unwrap().apply(value);
        self.flags.write().unwrap().versions.insert(key, 1);
        Ok(true)
    }

    /// Set a flag through the audited path (tech-spec 10 §2.3, §5): optimistic-concurrency check,
    /// exposure confirmation, store write, in-memory update, audit append. Returns the applied info.
    pub fn set_flag(&self, key: FlagKey, req: SetFlag, actor: &str) -> Result<FlagInfo, LibError> {
        if !req.value.matches(key) {
            return Err(LibError::BadRequest(format!(
                "flag '{key}' value has the wrong type"
            )));
        }
        let old_value = self.flags.read().unwrap().value(key);
        let cur_version = self.flags.read().unwrap().version(key);
        if let Some(expected) = req.expected_version {
            if expected != cur_version {
                return Err(LibError::Conflict(format!(
                    "flag '{key}' changed underneath you (have v{expected}, current v{cur_version})"
                )));
            }
        }
        if is_exposure_increasing(req.value, old_value) && !req.confirm {
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
            f.apply(req.value);
            f.versions.insert(key, new_version);
        }
        Ok(self.flag_info(key))
    }

    // ── tokens (tech-spec 10 §1.4) ───────────────────────────────────────────

    /// Issue a scoped API key. The plaintext secret is returned **once** and only its hash stored.
    pub fn create_token(&self, req: NewToken, actor: &str) -> Result<NewTokenReply, LibError> {
        if req.label.trim().is_empty() {
            return Err(LibError::BadRequest("token label is required".into()));
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
            removed as u64
        };
        *self.flags.write().unwrap() = FlagState::defaults();
        Ok(removed)
    }

    // ── audit (tech-spec 10 §4.5) ────────────────────────────────────────────

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
        let f = self.flags.read().unwrap();
        let exposed_without_auth = !localhost_only && matches!(f.auth, AuthMode::Off) && !tls;
        AdminStatus {
            bind: bind.to_string(),
            localhost_only,
            tls,
            auth: f.auth,
            mcp: f.mcp,
            network_writes: f.network_writes,
            exposed_without_auth,
            token_count: self.list_tokens().map(|t| t.len()).unwrap_or(0),
        }
    }
}

/// Hash a token secret. blake3 of the high-entropy random secret — not a password KDF, because an
/// API key is not a low-entropy user secret (tech-spec 10 §1.4; account passwords, which *do* use
/// argon2id, are phase 6). Rendered as lowercase hex for a stable indexed lookup column.
fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS feature_flag (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  version    INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by TEXT
);
CREATE TABLE IF NOT EXISTS token (
  token_id    TEXT PRIMARY KEY,
  label       TEXT NOT NULL,
  secret_hash TEXT NOT NULL UNIQUE,
  scopes      TEXT NOT NULL,
  created     INTEGER NOT NULL,
  expires     INTEGER,
  last_used   INTEGER
);
CREATE INDEX IF NOT EXISTS token_secret ON token(secret_hash);
CREATE TABLE IF NOT EXISTS audit_log (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  at     INTEGER NOT NULL,
  actor  TEXT NOT NULL,
  action TEXT NOT NULL,
  target TEXT,
  detail TEXT
);
";
