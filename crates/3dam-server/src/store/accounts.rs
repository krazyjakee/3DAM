//! The accounts / sessions / groups / shares surface of [`ServerStore`] (phase 6, issue #42;
//! tech-spec 10 §4). A child of `store` so it reaches the private connection; the schema lives in
//! the parent's `SCHEMA` batch (server-store convention: `CREATE TABLE IF NOT EXISTS`, not the
//! catalog's numbered migrations).
//!
//! Security posture, all decided in ADR 0009 §3–§4 and implemented here:
//! - argon2id password hashes (PHC strings); session cookie secrets hashed with blake3 (they are
//!   high-entropy, so no KDF needed — same reasoning as API tokens).
//! - account lockout: 10 failed attempts / 15-minute window per username.
//! - sessions: 14-day inactivity expiry + 90-day absolute ceiling; revocable individually.
//! - every mutation writes an `audit_log` row with actor + before/after.

use super::*;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use dam_api::accounts::*;
use dam_api::id::{CollectionId, SourceId};
use dam_api::service::{Visibility, VisibilityScope};
use std::sync::atomic::Ordering;

/// Session inactivity expiry (ADR 0009 §3): 14 days.
pub const SESSION_IDLE_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// Session absolute ceiling (ADR 0009 §3): 90 days.
pub const SESSION_ABS_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Lockout threshold (ADR 0009 §4): 10 failures…
const LOCKOUT_MAX_FAILURES: i64 = 10;
/// …within a 15-minute window.
const LOCKOUT_WINDOW_MS: i64 = 15 * 60 * 1000;
/// `last_seen` write throttle: a busy client shouldn't rewrite the row on every request.
const TOUCH_INTERVAL_MS: i64 = 60 * 1000;

/// A freshly minted session: what becomes the cookie (`session_id.secret`) plus the CSRF token.
pub struct NewSession {
    pub session_id: String,
    /// The full cookie value, `<session_id>.<secret>` — the secret half is never stored.
    pub cookie_value: String,
    pub csrf: String,
}

fn hash_password(pw: &str) -> Result<String, LibError> {
    if pw.len() < 8 {
        return Err(LibError::BadRequest(
            "password must be at least 8 characters".into(),
        ));
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(internal)
}

fn verify_password(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

fn new_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    )
}

fn valid_username(u: &str) -> bool {
    let n = u.chars().count();
    (1..=64).contains(&n)
        && u.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

fn row_to_account(r: &rusqlite::Row) -> rusqlite::Result<AccountInfo> {
    let role_s: String = r.get(3)?;
    Ok(AccountInfo {
        account_id: r.get(0)?,
        username: r.get(1)?,
        display_name: r.get(2)?,
        role: Role::parse(&role_s).unwrap_or(Role::Viewer),
        disabled: r.get::<_, i64>(4)? != 0,
        created: r.get(5)?,
        last_login: r.get(6)?,
    })
}

const ACCOUNT_COLS: &str =
    "account_id, username, display_name, role, disabled, created, last_login";

impl ServerStore {
    // ── claim state (issue #42 §2) ───────────────────────────────────────────

    pub fn count_accounts(&self) -> Result<usize, LibError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM account", [], |r| r.get(0))
            .map_err(internal)?;
        Ok(n as usize)
    }

    /// Unclaimed = accounts are on and either no account exists or the config escape hatch
    /// re-opened the window this boot. While true, the next authorised claim becomes admin.
    pub fn unclaimed(&self) -> bool {
        if !self.user_accounts() {
            return false;
        }
        self.claim_reopened.load(Ordering::Relaxed) || self.count_accounts().unwrap_or(0) == 0
    }

    /// Config-file escape hatch (ADR 0009 §3): re-open the claim window for this boot so a lost
    /// sole admin can be recovered from the config plane. Audited loudly.
    pub fn reopen_claim(&self, actor: &str) -> Result<(), LibError> {
        self.claim_reopened.store(true, Ordering::Relaxed);
        self.audit(actor, "account.claim_reopened", None, None)
    }

    /// Redeem the claim: create the first (admin) account and close the window. The *caller*
    /// (the auth route) is responsible for the loopback / admin-bearer gate — this is the state
    /// transition only, and it re-checks unclaimed under the lock so two racing claims can't both
    /// win.
    pub fn claim(&self, req: &ClaimRequest, actor: &str) -> Result<AccountInfo, LibError> {
        if !self.unclaimed() {
            return Err(LibError::Conflict(
                "this instance is already claimed".into(),
            ));
        }
        let info = self.create_account(
            &NewAccount {
                username: req.username.clone(),
                password: req.password.clone(),
                display_name: req.display_name.clone(),
                role: Role::Admin,
            },
            actor,
        )?;
        self.claim_reopened.store(false, Ordering::Relaxed);
        self.audit(actor, "account.claim", Some(&info.account_id), None)?;
        Ok(info)
    }

    // ── account CRUD (tech-spec 10 §5) ───────────────────────────────────────

    pub fn create_account(&self, req: &NewAccount, actor: &str) -> Result<AccountInfo, LibError> {
        if !valid_username(&req.username) {
            return Err(LibError::BadRequest(
                "username must be 1–64 chars of letters, digits, '-', '_', '.', '@'".into(),
            ));
        }
        let phc = hash_password(&req.password)?;
        let account_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO account
                     (account_id, username, display_name, password_hash, role, disabled, created)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
                    params![
                        account_id,
                        req.username,
                        req.display_name,
                        phc,
                        req.role.as_str(),
                        now
                    ],
                )
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::Conflict(format!(
                    "username '{}' is taken",
                    req.username
                )));
            }
            Self::audit_row(
                &conn,
                actor,
                "account.create",
                Some(&account_id),
                Some(serde_json::json!({ "username": req.username, "role": req.role })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(AccountInfo {
            account_id,
            username: req.username.clone(),
            display_name: req.display_name.clone(),
            role: req.role,
            disabled: false,
            created: now,
            last_login: None,
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ACCOUNT_COLS} FROM account ORDER BY username COLLATE NOCASE"
            ))
            .map_err(internal)?;
        let rows = stmt.query_map([], row_to_account).map_err(internal)?;
        rows.collect::<Result<_, _>>().map_err(internal)
    }

    pub fn get_account(&self, account_id: &str) -> Result<AccountInfo, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("SELECT {ACCOUNT_COLS} FROM account WHERE account_id = ?1"),
            params![account_id],
            row_to_account,
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| LibError::NotFound(format!("account {account_id}")))
    }

    /// How many *enabled admin* accounts exist — the guard that keeps the last admin standing.
    fn enabled_admin_count(conn: &Connection) -> Result<i64, LibError> {
        conn.query_row(
            "SELECT COUNT(*) FROM account WHERE role = 'admin' AND disabled = 0",
            [],
            |r| r.get(0),
        )
        .map_err(internal)
    }

    pub fn update_account(
        &self,
        account_id: &str,
        req: &UpdateAccount,
        actor: &str,
    ) -> Result<AccountInfo, LibError> {
        let before = self.get_account(account_id)?;
        // Never demote or disable the last enabled admin — the instance must always have one
        // (recovery otherwise falls to the config escape hatch, which should stay exceptional).
        let demotes = matches!(req.role, Some(r) if r != Role::Admin) && before.role == Role::Admin;
        let disables = req.disabled == Some(true) && !before.disabled;
        let new_phc = req.password.as_deref().map(hash_password).transpose()?;
        {
            let conn = self.conn.lock().unwrap();
            if (demotes || disables)
                && before.role == Role::Admin
                && !before.disabled
                && Self::enabled_admin_count(&conn)? <= 1
            {
                return Err(LibError::Conflict(
                    "cannot demote or disable the last admin account".into(),
                ));
            }
            conn.execute(
                "UPDATE account SET
                   display_name  = COALESCE(?2, display_name),
                   role          = COALESCE(?3, role),
                   disabled      = COALESCE(?4, disabled),
                   password_hash = COALESCE(?5, password_hash)
                 WHERE account_id = ?1",
                params![
                    account_id,
                    req.display_name,
                    req.role.map(|r| r.as_str()),
                    req.disabled.map(|d| d as i64),
                    new_phc,
                ],
            )
            .map_err(internal)?;
            // A changed password or a disable signs the account's sessions out everywhere.
            if new_phc.is_some() || req.disabled == Some(true) {
                conn.execute(
                    "DELETE FROM session WHERE account_id = ?1",
                    params![account_id],
                )
                .map_err(internal)?;
            }
            Self::audit_row(
                &conn,
                actor,
                "account.update",
                Some(account_id),
                Some(serde_json::json!({
                    "before": { "role": before.role, "disabled": before.disabled },
                    "after": {
                        "role": req.role.unwrap_or(before.role),
                        "disabled": req.disabled.unwrap_or(before.disabled),
                        "password_changed": new_phc.is_some(),
                    },
                })),
            )?;
        }
        self.bump_visibility_gen();
        self.get_account(account_id)
    }

    pub fn delete_account(&self, account_id: &str, actor: &str) -> Result<(), LibError> {
        let before = self.get_account(account_id)?;
        {
            let conn = self.conn.lock().unwrap();
            if before.role == Role::Admin
                && !before.disabled
                && Self::enabled_admin_count(&conn)? <= 1
            {
                return Err(LibError::Conflict(
                    "cannot delete the last admin account".into(),
                ));
            }
            // Sessions, group memberships, and direct shares cascade (schema FKs).
            conn.execute(
                "DELETE FROM account WHERE account_id = ?1",
                params![account_id],
            )
            .map_err(internal)?;
            Self::audit_row(
                &conn,
                actor,
                "account.delete",
                Some(account_id),
                Some(serde_json::json!({ "username": before.username })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(())
    }

    // ── login / sessions (tech-spec 10 §4.4; ADR 0009 §3–§4) ─────────────────

    /// Verify a username/password and mint a session. Lockout applies per username; a failure is
    /// recorded whether the username exists or not, so probing behaves identically either way.
    pub fn login(
        &self,
        username: &str,
        password: &str,
        user_agent: Option<&str>,
    ) -> Result<(NewSession, AccountIdentity), LibError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        // Lockout check first — a locked account rejects even a correct password.
        let failures: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM login_failure WHERE username = ?1 AND at > ?2",
                params![username, now - LOCKOUT_WINDOW_MS],
                |r| r.get(0),
            )
            .map_err(internal)?;
        if failures >= LOCKOUT_MAX_FAILURES {
            Self::audit_row(&conn, "system", "account.lockout", Some(username), None)?;
            return Err(LibError::RateLimited {
                retry_after: (LOCKOUT_WINDOW_MS / 1000) as u32,
            });
        }
        let row: Option<(String, Option<String>, String, i64)> = conn
            .query_row(
                "SELECT account_id, password_hash, role, disabled FROM account WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(internal)?;
        let ok = match &row {
            Some((_, Some(phc), _, 0)) => verify_password(password, phc),
            // Unknown user / OIDC-only / disabled: burn a comparable amount of time is overkill at
            // this scale; the lockout counter is the meaningful defence.
            _ => false,
        };
        if !ok {
            conn.execute(
                "INSERT INTO login_failure (username, at) VALUES (?1, ?2)",
                params![username, now],
            )
            .map_err(internal)?;
            // Opportunistic GC of stale failure rows.
            let _ = conn.execute(
                "DELETE FROM login_failure WHERE at < ?1",
                params![now - LOCKOUT_WINDOW_MS],
            );
            return Err(LibError::Unauthorized);
        }
        let (account_id, _, role_s, _) = row.unwrap();
        conn.execute(
            "DELETE FROM login_failure WHERE username = ?1",
            params![username],
        )
        .map_err(internal)?;
        conn.execute(
            "UPDATE account SET last_login = ?2 WHERE account_id = ?1",
            params![account_id, now],
        )
        .map_err(internal)?;
        let session = Self::insert_session(&conn, &account_id, user_agent, now)?;
        Self::audit_row(
            &conn,
            &format!("account:{username}"),
            "account.login",
            None,
            None,
        )?;
        Ok((
            session,
            AccountIdentity {
                account_id,
                username: username.to_string(),
                role: Role::parse(&role_s).unwrap_or(Role::Viewer),
            },
        ))
    }

    /// Mint a session row for an already-authenticated account (login and claim share this).
    pub fn mint_session(
        &self,
        account_id: &str,
        user_agent: Option<&str>,
    ) -> Result<NewSession, LibError> {
        let conn = self.conn.lock().unwrap();
        Self::insert_session(&conn, account_id, user_agent, now_ms())
    }

    fn insert_session(
        conn: &Connection,
        account_id: &str,
        user_agent: Option<&str>,
        now: i64,
    ) -> Result<NewSession, LibError> {
        let session_id = uuid::Uuid::now_v7().simple().to_string();
        let secret = new_secret();
        let csrf = new_secret();
        conn.execute(
            "INSERT INTO session
             (session_id, account_id, secret_hash, csrf, created, last_seen, absolute_exp, user_agent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
            params![
                session_id,
                account_id,
                hash_secret(&secret),
                csrf,
                now,
                now + SESSION_ABS_MS,
                user_agent
            ],
        )
        .map_err(internal)?;
        Ok(NewSession {
            cookie_value: format!("{session_id}.{secret}"),
            session_id,
            csrf,
        })
    }

    /// Resolve a presented session cookie (`<session_id>.<secret>`) to its account + CSRF token.
    /// A miss, an expired session, or a disabled account all answer `None` (the caller 401s);
    /// expired rows are deleted on sight. `last_seen` slides, throttled to once a minute.
    pub fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<(AccountIdentity, String, String)>, LibError> {
        let Some((session_id, secret)) = cookie_value.split_once('.') else {
            return Ok(None);
        };
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let row: Option<(String, String, i64, i64, String)> = conn
            .query_row(
                "SELECT s.secret_hash, s.csrf, s.last_seen, s.absolute_exp, s.account_id
                 FROM session s WHERE s.session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((secret_hash, csrf, last_seen, absolute_exp, account_id)) = row else {
            return Ok(None);
        };
        if hash_secret(secret) != secret_hash {
            return Ok(None);
        }
        if now >= absolute_exp || now - last_seen >= SESSION_IDLE_MS {
            let _ = conn.execute(
                "DELETE FROM session WHERE session_id = ?1",
                params![session_id],
            );
            return Ok(None);
        }
        let acct: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT username, role, disabled FROM account WHERE account_id = ?1",
                params![account_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(internal)?;
        let Some((username, role_s, disabled)) = acct else {
            return Ok(None);
        };
        if disabled != 0 {
            return Ok(None);
        }
        if now - last_seen > TOUCH_INTERVAL_MS {
            let _ = conn.execute(
                "UPDATE session SET last_seen = ?2 WHERE session_id = ?1",
                params![session_id, now],
            );
        }
        Ok(Some((
            AccountIdentity {
                account_id,
                username,
                role: Role::parse(&role_s).unwrap_or(Role::Viewer),
            },
            csrf,
            session_id.to_string(),
        )))
    }

    /// Revoke one session. Scoped to `account_id` so a user can only sign out their own sessions
    /// (the admin plane revokes via [`Self::revoke_account_sessions`]).
    pub fn revoke_session(&self, account_id: &str, session_id: &str) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM session WHERE session_id = ?1 AND account_id = ?2",
                params![session_id, account_id],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound(format!("session {session_id}")));
        }
        Ok(())
    }

    /// Revoke every session of an account (admin plane; audited by the caller's route).
    pub fn revoke_account_sessions(&self, account_id: &str, actor: &str) -> Result<u64, LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM session WHERE account_id = ?1",
                params![account_id],
            )
            .map_err(internal)?;
        Self::audit_row(
            &conn,
            actor,
            "account.sessions_revoked",
            Some(account_id),
            Some(serde_json::json!({ "count": n })),
        )?;
        Ok(n as u64)
    }

    pub fn list_sessions(
        &self,
        account_id: &str,
        current_session: &str,
    ) -> Result<Vec<SessionInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, created, last_seen, user_agent FROM session
                 WHERE account_id = ?1 ORDER BY last_seen DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok(SessionInfo {
                    session_id: r.get(0)?,
                    created: r.get(1)?,
                    last_seen: r.get(2)?,
                    user_agent: r.get(3)?,
                    current: false,
                })
            })
            .map_err(internal)?;
        let mut out: Vec<SessionInfo> = rows.collect::<Result<_, _>>().map_err(internal)?;
        for s in &mut out {
            s.current = s.session_id == current_session;
        }
        Ok(out)
    }

    // ── groups (issue #42 §3) ────────────────────────────────────────────────

    pub fn create_group(&self, req: &NewGroup, actor: &str) -> Result<GroupInfo, LibError> {
        let name = req.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(LibError::BadRequest("group name must be 1–64 chars".into()));
        }
        let group_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO group_ (group_id, name, created) VALUES (?1, ?2, ?3)",
                    params![group_id, name, now],
                )
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::Conflict(format!("group '{name}' already exists")));
            }
            Self::audit_row(
                &conn,
                actor,
                "group.create",
                Some(&group_id),
                Some(serde_json::json!({ "name": name })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(GroupInfo {
            group_id,
            name: name.to_string(),
            created: now,
            members: Vec::new(),
        })
    }

    pub fn list_groups(&self) -> Result<Vec<GroupInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT group_id, name, created FROM group_ ORDER BY name COLLATE NOCASE")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GroupInfo {
                    group_id: r.get(0)?,
                    name: r.get(1)?,
                    created: r.get(2)?,
                    members: Vec::new(),
                })
            })
            .map_err(internal)?;
        let mut groups: Vec<GroupInfo> = rows.collect::<Result<_, _>>().map_err(internal)?;
        let mut mstmt = conn
            .prepare("SELECT account_id FROM group_member WHERE group_id = ?1")
            .map_err(internal)?;
        for g in &mut groups {
            let rows = mstmt
                .query_map(params![g.group_id], |r| r.get::<_, String>(0))
                .map_err(internal)?;
            g.members = rows.collect::<Result<_, _>>().map_err(internal)?;
        }
        Ok(groups)
    }

    pub fn delete_group(&self, group_id: &str, actor: &str) -> Result<(), LibError> {
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute("DELETE FROM group_ WHERE group_id = ?1", params![group_id])
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::NotFound(format!("group {group_id}")));
            }
            Self::audit_row(&conn, actor, "group.delete", Some(group_id), None)?;
        }
        self.bump_visibility_gen();
        Ok(())
    }

    /// Replace a group's membership set (idempotent, whole-set semantics — the admin UI edits the
    /// list and submits it). Unknown account ids are rejected up front.
    pub fn set_group_members(
        &self,
        group_id: &str,
        account_ids: &[String],
        actor: &str,
    ) -> Result<GroupInfo, LibError> {
        {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction().map_err(internal)?;
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM group_ WHERE group_id = ?1",
                    params![group_id],
                    |_| Ok(true),
                )
                .optional()
                .map_err(internal)?
                .unwrap_or(false);
            if !exists {
                return Err(LibError::NotFound(format!("group {group_id}")));
            }
            for a in account_ids {
                let known: bool = tx
                    .query_row(
                        "SELECT 1 FROM account WHERE account_id = ?1",
                        params![a],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(internal)?
                    .unwrap_or(false);
                if !known {
                    return Err(LibError::BadRequest(format!("unknown account {a}")));
                }
            }
            tx.execute(
                "DELETE FROM group_member WHERE group_id = ?1",
                params![group_id],
            )
            .map_err(internal)?;
            for a in account_ids {
                tx.execute(
                    "INSERT OR IGNORE INTO group_member (group_id, account_id) VALUES (?1, ?2)",
                    params![group_id, a],
                )
                .map_err(internal)?;
            }
            Self::audit_row(
                &tx,
                actor,
                "group.members",
                Some(group_id),
                Some(serde_json::json!({ "members": account_ids })),
            )?;
            tx.commit().map_err(internal)?;
        }
        self.bump_visibility_gen();
        let groups = self.list_groups()?;
        groups
            .into_iter()
            .find(|g| g.group_id == group_id)
            .ok_or_else(|| LibError::NotFound(format!("group {group_id}")))
    }

    // ── shares (issue #42 §3) ────────────────────────────────────────────────

    pub fn create_share(&self, req: &NewShare, actor: &str) -> Result<ShareInfo, LibError> {
        // Exactly one target — mirrors the schema CHECK, but with a friendlier error.
        match (&req.account_id, &req.group_id) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(LibError::BadRequest(
                    "a share targets exactly one of account_id or group_id".into(),
                ))
            }
        }
        // The resource id must at least be a well-formed uuid — the resource itself lives across
        // the database boundary in library.db (soft reference; the route layer validates liveness).
        if uuid::Uuid::parse_str(&req.resource_id).is_err() {
            return Err(LibError::BadRequest("invalid resource id".into()));
        }
        let share_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            if let Some(a) = &req.account_id {
                let known: bool = conn
                    .query_row(
                        "SELECT 1 FROM account WHERE account_id = ?1",
                        params![a],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(internal)?
                    .unwrap_or(false);
                if !known {
                    return Err(LibError::BadRequest(format!("unknown account {a}")));
                }
            }
            if let Some(g) = &req.group_id {
                let known: bool = conn
                    .query_row(
                        "SELECT 1 FROM group_ WHERE group_id = ?1",
                        params![g],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(internal)?
                    .unwrap_or(false);
                if !known {
                    return Err(LibError::BadRequest(format!("unknown group {g}")));
                }
            }
            conn.execute(
                "INSERT INTO share
                 (share_id, resource, resource_id, account_id, group_id, access, granted_by, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    share_id,
                    req.resource.as_str(),
                    req.resource_id,
                    req.account_id,
                    req.group_id,
                    req.access.as_str(),
                    actor,
                    now
                ],
            )
            .map_err(internal)?;
            Self::audit_row(
                &conn,
                actor,
                "share.create",
                Some(&share_id),
                Some(serde_json::json!({
                    "resource": req.resource,
                    "resource_id": req.resource_id,
                    "account_id": req.account_id,
                    "group_id": req.group_id,
                    "access": req.access,
                })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(ShareInfo {
            share_id,
            resource: req.resource,
            resource_id: req.resource_id.clone(),
            account_id: req.account_id.clone(),
            group_id: req.group_id.clone(),
            access: req.access,
            granted_by: actor.to_string(),
            created: now,
        })
    }

    pub fn list_shares(&self) -> Result<Vec<ShareInfo>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT share_id, resource, resource_id, account_id, group_id, access,
                        granted_by, created
                 FROM share ORDER BY created DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                let resource_s: String = r.get(1)?;
                let access_s: String = r.get(5)?;
                Ok(ShareInfo {
                    share_id: r.get(0)?,
                    resource: ShareResource::parse(&resource_s).unwrap_or(ShareResource::Source),
                    resource_id: r.get(2)?,
                    account_id: r.get(3)?,
                    group_id: r.get(4)?,
                    access: ShareAccess::parse(&access_s).unwrap_or(ShareAccess::Read),
                    granted_by: r.get(6)?,
                    created: r.get(7)?,
                })
            })
            .map_err(internal)?;
        rows.collect::<Result<_, _>>().map_err(internal)
    }

    pub fn delete_share(&self, share_id: &str, actor: &str) -> Result<(), LibError> {
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute("DELETE FROM share WHERE share_id = ?1", params![share_id])
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::NotFound(format!("share {share_id}")));
            }
            Self::audit_row(&conn, actor, "share.delete", Some(share_id), None)?;
        }
        self.bump_visibility_gen();
        Ok(())
    }

    /// Orphan GC for the cross-database soft reference (issue #42 §3): when a source/collection is
    /// deleted in `library.db`, its share rows here are dead weight — drop them. Ids are uuids and
    /// never recycled, so a missed GC is only clutter, never a grant to a future resource.
    pub fn remove_shares_for_resource(
        &self,
        resource: ShareResource,
        resource_id: &str,
        actor: &str,
    ) -> Result<u64, LibError> {
        let n = {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute(
                    "DELETE FROM share WHERE resource = ?1 AND resource_id = ?2",
                    params![resource.as_str(), resource_id],
                )
                .map_err(internal)?;
            if n > 0 {
                Self::audit_row(
                    &conn,
                    actor,
                    "share.gc",
                    Some(resource_id),
                    Some(serde_json::json!({ "resource": resource, "removed": n })),
                )?;
            }
            n
        };
        if n > 0 {
            self.bump_visibility_gen();
        }
        Ok(n as u64)
    }

    // ── visibility resolution (issue #42 §3 resolution rules) ────────────────

    /// Resolve an account's visibility ceiling — **at auth time, in the server** — so the engine
    /// only ever sees a finished value and never learns what an account or group is:
    /// 1. admins bypass sharing entirely (`Full`);
    /// 2. direct shares ∪ shares to any group the account belongs to;
    /// 3. the most permissive access level wins per resource (`write` ⊃ `read`);
    /// 4. `write` grants land in the `write_*` sets — whether the identity may write *at all*
    ///    stays a scope question (rule 4: two independent gates).
    pub fn resolve_visibility(&self, account: &AccountIdentity) -> Result<Visibility, LibError> {
        if account.role == Role::Admin {
            return Ok(Visibility::Full);
        }
        let conn = self.conn.lock().unwrap();
        let mut scope = VisibilityScope::default();
        let mut stmt = conn
            .prepare(
                "SELECT resource, resource_id, access FROM share
                 WHERE account_id = ?1
                    OR group_id IN (SELECT group_id FROM group_member WHERE account_id = ?1)",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![account.account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(internal)?;
        for row in rows {
            let (resource_s, rid, access_s) = row.map_err(internal)?;
            let write = access_s == "write";
            match ShareResource::parse(&resource_s) {
                Some(ShareResource::Source) => {
                    if let Ok(id) = rid.parse::<SourceId>() {
                        scope.sources.insert(id);
                        if write {
                            scope.write_sources.insert(id);
                        }
                    }
                }
                Some(ShareResource::Collection) => {
                    if let Ok(id) = rid.parse::<CollectionId>() {
                        scope.collections.insert(id);
                        if write {
                            scope.write_collections.insert(id);
                        }
                    }
                }
                None => {}
            }
        }
        Ok(Visibility::Restricted(scope))
    }

    /// The generation counter behind live share/membership edits — a subscriber caches this with
    /// its resolved ceiling and re-resolves when it moves (issue #42: a share change must not keep
    /// serving a stale WS stream).
    pub fn visibility_generation(&self) -> u64 {
        self.visibility_gen.load(Ordering::Relaxed)
    }

    fn bump_visibility_gen(&self) {
        self.visibility_gen.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ServerStore {
        let s = ServerStore::open_in_memory().unwrap();
        s.set_flag(
            dam_api::admin::FlagKey::UserAccounts,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
        s
    }

    fn acct(s: &ServerStore, name: &str, role: Role) -> AccountInfo {
        s.create_account(
            &NewAccount {
                username: name.into(),
                password: "hunter2hunter2".into(),
                display_name: None,
                role,
            },
            "test",
        )
        .unwrap()
    }

    /// The full first-run claim state machine: unclaimed → first claim wins admin → window closes.
    #[test]
    fn claim_window_closes_after_first_account() {
        let s = store();
        assert!(s.unclaimed());
        let a = s
            .claim(
                &ClaimRequest {
                    username: "owner".into(),
                    password: "password123".into(),
                    display_name: None,
                },
                "test",
            )
            .unwrap();
        assert_eq!(a.role, Role::Admin);
        assert!(!s.unclaimed());
        // A second claim is refused outright.
        let err = s
            .claim(
                &ClaimRequest {
                    username: "late".into(),
                    password: "password123".into(),
                    display_name: None,
                },
                "test",
            )
            .unwrap_err();
        assert!(matches!(err, LibError::Conflict(_)));
        // The config escape hatch re-opens it.
        s.reopen_claim("config-file").unwrap();
        assert!(s.unclaimed());
    }

    /// Login round-trip + session cookie verification, wrong-password rejection, and lockout after
    /// 10 failures inside the window (ADR 0009 §4).
    #[test]
    fn login_sessions_and_lockout() {
        let s = store();
        let a = acct(&s, "erin", Role::Editor);
        // Wrong password: unauthorized, and it counts toward lockout.
        for _ in 0..9 {
            assert!(matches!(
                s.login("erin", "wrong", None),
                Err(LibError::Unauthorized)
            ));
        }
        // Correct password still works below the threshold — and clears the counter.
        let (sess, ident) = s.login("erin", "hunter2hunter2", Some("ua")).unwrap();
        assert_eq!(ident.account_id, a.account_id);
        let (ident2, csrf, sid) = s.verify_session(&sess.cookie_value).unwrap().unwrap();
        assert_eq!(ident2.username, "erin");
        assert_eq!(csrf, sess.csrf);
        assert_eq!(sid, sess.session_id);
        // A garbage cookie is a miss, not an error.
        assert!(s.verify_session("nonsense").unwrap().is_none());
        assert!(s
            .verify_session(&format!("{}.badsecret", sess.session_id))
            .unwrap()
            .is_none());
        // Now trip the lockout: 10 failures, then even the right password is refused.
        for _ in 0..10 {
            let _ = s.login("erin", "wrong", None);
        }
        assert!(matches!(
            s.login("erin", "hunter2hunter2", None),
            Err(LibError::RateLimited { .. })
        ));
    }

    /// Session expiry: 14-day inactivity and the 90-day absolute ceiling (ADR 0009 §3), driven by
    /// rewinding the stored timestamps.
    #[test]
    fn session_expiry_inactivity_and_absolute() {
        let s = store();
        acct(&s, "erin", Role::Viewer);
        let (sess, _) = s.login("erin", "hunter2hunter2", None).unwrap();
        // Rewind last_seen past the inactivity window: the session is gone (and deleted).
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE session SET last_seen = last_seen - ?1 WHERE session_id = ?2",
                params![SESSION_IDLE_MS + 1000, sess.session_id],
            )
            .unwrap();
        }
        assert!(s.verify_session(&sess.cookie_value).unwrap().is_none());
        assert!(s.verify_session(&sess.cookie_value).unwrap().is_none());

        // Fresh session, rewind creation past the absolute ceiling while keeping last_seen live.
        let (sess2, _) = s.login("erin", "hunter2hunter2", None).unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE session SET absolute_exp = ?1 WHERE session_id = ?2",
                params![now_ms() - 1, sess2.session_id],
            )
            .unwrap();
        }
        assert!(s.verify_session(&sess2.cookie_value).unwrap().is_none());
    }

    /// The last enabled admin can be neither demoted, disabled, nor deleted; password changes
    /// revoke the account's other sessions.
    #[test]
    fn last_admin_guard_and_password_rotation() {
        let s = store();
        let admin = acct(&s, "root", Role::Admin);
        let err = s
            .update_account(
                &admin.account_id,
                &UpdateAccount {
                    role: Some(Role::Viewer),
                    ..Default::default()
                },
                "test",
            )
            .unwrap_err();
        assert!(matches!(err, LibError::Conflict(_)));
        assert!(matches!(
            s.delete_account(&admin.account_id, "test").unwrap_err(),
            LibError::Conflict(_)
        ));
        // A second admin unlocks the demotion.
        let admin2 = acct(&s, "root2", Role::Admin);
        s.update_account(
            &admin.account_id,
            &UpdateAccount {
                role: Some(Role::Viewer),
                ..Default::default()
            },
            "test",
        )
        .unwrap();
        // Password rotation kills live sessions.
        let (sess, _) = s.login("root2", "hunter2hunter2", None).unwrap();
        s.update_account(
            &admin2.account_id,
            &UpdateAccount {
                password: Some("newpassword9".into()),
                ..Default::default()
            },
            "test",
        )
        .unwrap();
        assert!(s.verify_session(&sess.cookie_value).unwrap().is_none());
    }

    /// Visibility resolution: admin bypass, direct + group union, write beats read, and the
    /// generation counter moves on share mutations.
    #[test]
    fn visibility_resolution_rules() {
        let s = store();
        let admin = acct(&s, "root", Role::Admin);
        let viewer = acct(&s, "vera", Role::Viewer);
        let ident = |a: &AccountInfo| AccountIdentity {
            account_id: a.account_id.clone(),
            username: a.username.clone(),
            role: a.role,
        };
        assert_eq!(
            s.resolve_visibility(&ident(&admin)).unwrap(),
            Visibility::Full
        );
        // No shares: an empty restricted set (sees nothing).
        match s.resolve_visibility(&ident(&viewer)).unwrap() {
            Visibility::Restricted(sc) => {
                assert!(sc.sources.is_empty() && sc.collections.is_empty())
            }
            v => panic!("expected restricted, got {v:?}"),
        }
        // Direct read share on a source + group write share on the same source: union, write wins.
        let src = SourceId::new();
        let g = s
            .create_group(
                &NewGroup {
                    name: "team".into(),
                },
                "test",
            )
            .unwrap();
        s.set_group_members(
            &g.group_id,
            std::slice::from_ref(&viewer.account_id),
            "test",
        )
        .unwrap();
        let gen0 = s.visibility_generation();
        s.create_share(
            &NewShare {
                resource: ShareResource::Source,
                resource_id: src.to_string(),
                account_id: Some(viewer.account_id.clone()),
                group_id: None,
                access: ShareAccess::Read,
            },
            "test",
        )
        .unwrap();
        s.create_share(
            &NewShare {
                resource: ShareResource::Source,
                resource_id: src.to_string(),
                account_id: None,
                group_id: Some(g.group_id.clone()),
                access: ShareAccess::Write,
            },
            "test",
        )
        .unwrap();
        assert!(s.visibility_generation() > gen0);
        match s.resolve_visibility(&ident(&viewer)).unwrap() {
            Visibility::Restricted(sc) => {
                assert!(sc.sources.contains(&src));
                assert!(sc.write_sources.contains(&src), "write beats read");
            }
            v => panic!("expected restricted, got {v:?}"),
        }
        // Deleting the group drops its grant (cascade) — only the direct read remains.
        s.delete_group(&g.group_id, "test").unwrap();
        match s.resolve_visibility(&ident(&viewer)).unwrap() {
            Visibility::Restricted(sc) => {
                assert!(sc.sources.contains(&src));
                assert!(!sc.write_sources.contains(&src));
            }
            v => panic!("expected restricted, got {v:?}"),
        }
    }

    /// Orphan GC drops every share row for a deleted resource.
    #[test]
    fn share_gc_for_removed_resource() {
        let s = store();
        let viewer = acct(&s, "vera", Role::Viewer);
        let src = SourceId::new();
        s.create_share(
            &NewShare {
                resource: ShareResource::Source,
                resource_id: src.to_string(),
                account_id: Some(viewer.account_id.clone()),
                group_id: None,
                access: ShareAccess::Read,
            },
            "test",
        )
        .unwrap();
        assert_eq!(s.list_shares().unwrap().len(), 1);
        let n = s
            .remove_shares_for_resource(ShareResource::Source, &src.to_string(), "test")
            .unwrap();
        assert_eq!(n, 1);
        assert!(s.list_shares().unwrap().is_empty());
    }
}
