//! Password authentication, lockout enforcement, and session lifecycle operations.

use super::*;
use argon2::password_hash::{PasswordHash, PasswordVerifier};
use argon2::Argon2;
use dam_api::accounts::{AccountIdentity, Role, SessionInfo};

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

impl ServerStore {
    /// Verify a username/password and mint a session. Lockout applies per username; a failure is
    /// recorded whether the username exists or not, so probing behaves identically either way.
    ///
    /// **The argon2id verify runs outside the connection mutex** (three short critical sections:
    /// lockout+row read, hash, record outcome). Holding the server's single `server.db` lock across
    /// a deliberately-expensive KDF turned a burst of wrong-password attempts into a whole-server
    /// stall — and the lockout counter is per *username*, so varying the username defeats it. The
    /// callers additionally run this on `spawn_blocking` so the KDF never occupies a tokio worker
    /// (CLAUDE.md golden rule 5). `create_account` already hashed outside the lock; this mirrors it.
    pub fn login(
        &self,
        username: &str,
        password: &str,
        user_agent: Option<&str>,
    ) -> Result<(NewSession, AccountIdentity), LibError> {
        let now = now_ms();
        // ── 1. Locked out? Which credential row? (lock held briefly) ──
        let row: Option<(String, Option<String>, String, i64)> = {
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
            conn.query_row(
                "SELECT account_id, password_hash, role, disabled FROM account WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(internal)?
        };
        // ── 2. The KDF, with no lock held ──
        let ok = match &row {
            Some((_, Some(phc), _, 0)) => verify_password(password, phc),
            // Unknown user / OIDC-only / disabled: burning a comparable amount of time is overkill
            // at this scale; the lockout counter is the meaningful defence.
            _ => false,
        };
        // ── 3. Record the outcome (lock re-acquired) ──
        let conn = self.conn.lock().unwrap();
        if !ok {
            conn.execute(
                "INSERT INTO login_failure (username, at) VALUES (?1, ?2)",
                params![username, now],
            )
            .map_err(internal)?;
            // Opportunistic GC of stale failure rows.
            if let Err(error) = conn.execute(
                "DELETE FROM login_failure WHERE at < ?1",
                params![now - LOCKOUT_WINDOW_MS],
            ) {
                tracing::warn!(%error, "opportunistic login-failure cleanup failed");
            }
            return Err(LibError::Unauthorized);
        }
        let (account_id, _, role_s, _) = row.unwrap();
        conn.execute(
            "DELETE FROM login_failure WHERE username = ?1",
            params![username],
        )
        .map_err(internal)?;
        // `last_login` is stamped by `insert_session`, which every login path goes through.
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
        // Stamping `last_login` here rather than in each caller is the point: minting a session
        // *is* signing in, and this is the one place all three ways of doing that meet (password
        // login, the first-run claim, and OIDC). It was previously done only by `login`, so a
        // claimed admin showed "never signed in" until their second visit — and every future login
        // path would have had the same trap waiting.
        conn.execute(
            "UPDATE account SET last_login = ?2 WHERE account_id = ?1",
            params![account_id, now],
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
            conn.execute(
                "DELETE FROM session WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(internal)?;
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
            if let Err(error) = conn.execute(
                "UPDATE session SET last_seen = ?2 WHERE session_id = ?1",
                params![session_id, now],
            ) {
                tracing::warn!(%error, "session last-seen touch failed");
            }
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
}
