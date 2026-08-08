//! Account claiming and CRUD operations for the server store.

use super::*;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::Argon2;
use dam_api::accounts::*;
use std::sync::atomic::Ordering;

// Preserve the established crate-visible path used by the authentication transport.
pub(crate) use super::sessions::NewSession;
#[cfg(test)]
use super::sessions::SESSION_IDLE_MS;

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

    /// Redeem the claim: create the first (admin) account and close the window. The *caller* (the
    /// auth route) owns the loopback / admin-bearer gate — this is the state transition only.
    ///
    /// **Atomic by construction.** The unclaimed check, the account insert, and the closing of the
    /// re-opened window all happen inside one `BEGIN IMMEDIATE` transaction, held across a single
    /// acquisition of the connection mutex. The earlier shape re-checked through `unclaimed()`
    /// (which takes and releases the lock), then hashed the password for tens of milliseconds, then
    /// re-acquired to insert — a window wide enough for two concurrent claims with different
    /// usernames to both succeed and both become admin. The argon2 hash stays *outside* the
    /// transaction (below), so the KDF never holds the server's only DB connection.
    pub fn claim(&self, req: &ClaimRequest, actor: &str) -> Result<AccountInfo, LibError> {
        if !self.user_accounts() {
            return Err(LibError::NotFound("user accounts are disabled".into()));
        }
        if !valid_username(&req.username) {
            return Err(LibError::BadRequest(
                "username must be 1–64 chars of letters, digits, '-', '_', '.', '@'".into(),
            ));
        }
        // Hash first, lock second: an expensive KDF must never run under the connection mutex.
        // A hash computed for a claim that then loses the race is simply discarded.
        let phc = hash_password(&req.password)?;
        let account_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(internal)?;
            let n: i64 = tx
                .query_row("SELECT COUNT(*) FROM account", [], |r| r.get(0))
                .map_err(internal)?;
            // The config escape hatch re-opens the window for one claim; the mutex serialises us,
            // so clearing it here (before commit) is what makes it one-shot.
            let reopened = self.claim_reopened.load(Ordering::Relaxed);
            if n != 0 && !reopened {
                return Err(LibError::Conflict(
                    "this instance is already claimed".into(),
                ));
            }
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO account
                     (account_id, username, display_name, password_hash, role, disabled, created)
                     VALUES (?1, ?2, ?3, ?4, 'admin', 0, ?5)",
                    params![account_id, req.username, req.display_name, phc, now],
                )
                .map_err(internal)?;
            if inserted == 0 {
                return Err(LibError::Conflict(format!(
                    "username '{}' is taken",
                    req.username
                )));
            }
            Self::audit_row(
                &tx,
                actor,
                "account.create",
                Some(&account_id),
                Some(serde_json::json!({ "username": req.username, "role": Role::Admin })),
            )?;
            Self::audit_row(&tx, actor, "account.claim", Some(&account_id), None)?;
            tx.commit().map_err(internal)?;
            self.claim_reopened.store(false, Ordering::Relaxed);
        }
        self.bump_visibility_gen();
        Ok(AccountInfo {
            account_id,
            username: req.username.clone(),
            display_name: req.display_name.clone(),
            role: Role::Admin,
            disabled: false,
            created: now,
            last_login: None,
        })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::id::SourceId;
    use dam_api::service::Visibility;

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

    /// Two claims racing with *different* usernames: exactly one may win. The username uniqueness
    /// constraint cannot settle this (the usernames differ), so the guarantee rests entirely on the
    /// `BEGIN IMMEDIATE` transaction around the count-check + insert. The previous shape re-checked
    /// through `unclaimed()` (lock taken and released), then spent tens of milliseconds in argon2,
    /// then re-acquired to insert — and both racers became admin.
    #[test]
    fn concurrent_claims_cannot_both_win() {
        let s = std::sync::Arc::new(store());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let winners = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..8 {
            let (s, barrier, winners) = (s.clone(), barrier.clone(), winners.clone());
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let r = s.claim(
                    &ClaimRequest {
                        username: format!("racer{i}"),
                        password: "password123".into(),
                        display_name: None,
                    },
                    "test",
                );
                match r {
                    Ok(a) => {
                        assert_eq!(a.role, Role::Admin);
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(LibError::Conflict(_)) => {}
                    Err(e) => panic!("unexpected claim error: {e:?}"),
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::Relaxed), 1, "the claim is one-shot");
        assert_eq!(s.count_accounts().unwrap(), 1);
        assert!(!s.unclaimed());
    }

    /// The re-opened window (recovery hatch) is one-shot too: it survives the lock/hash/insert
    /// sequence exactly once, then closes.
    #[test]
    fn reopened_claim_window_admits_exactly_one() {
        let s = store();
        acct(&s, "root", Role::Admin);
        assert!(!s.unclaimed());
        s.reopen_claim("config-file").unwrap();
        assert!(s.unclaimed());
        let mk = |n: &str| ClaimRequest {
            username: n.into(),
            password: "password123".into(),
            display_name: None,
        };
        assert_eq!(s.claim(&mk("rescue"), "test").unwrap().role, Role::Admin);
        assert!(matches!(
            s.claim(&mk("second"), "test"),
            Err(LibError::Conflict(_))
        ));
        assert_eq!(s.count_accounts().unwrap(), 2);
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
