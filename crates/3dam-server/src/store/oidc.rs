//! The OIDC/OAuth2 surface of [`ServerStore`] (phase 6, issue #41; tech-spec 10 §1.5).
//!
//! Three pieces of state, each with a reason to be server-side:
//! - **the provider config** — one row, because two providers disagreeing about who may sign in is
//!   not a configuration anyone wants. The client secret is write-only (tech-spec 10 §5).
//! - **in-flight logins** — the `state`, `nonce` and PKCE verifier held across the redirect. The
//!   verifier especially: PKCE's whole value is that the code is useless without a secret the
//!   browser never saw, so putting it in a cookie would give it away.
//! - **identity links** — which `(issuer, subject)` is which local account.
//!
//! Everything here is storage. Token validation lives in `crate::oidc`, which owns the flow.

use super::*;
use dam_api::accounts::{AccountInfo, Role};

/// How long an unfinished authorization request stays redeemable. Long enough for a human to read
/// a consent screen and type a password; short enough that an abandoned login is not a lasting row.
/// Also the replay window for a `state`, which is why it is minutes rather than hours.
const LOGIN_TTL_MS: i64 = 10 * 60 * 1000;

/// Ceiling on unfinished authorization requests held at once. See [`ServerStore::begin_oidc_login`]
/// — this bounds an unauthenticated write, it is not a concurrency limit anyone should ever meet.
const MAX_PENDING_LOGINS: i64 = 10_000;

/// The provider config as the flow needs it — including the secret, which never leaves this crate.
#[derive(Clone, Debug)]
pub struct StoredOidc {
    pub config: OidcConfig,
    pub client_secret: Option<String>,
}

/// Outcome of redeeming an authorization request. Three answers, not two, because "no such login"
/// and "not your login" need different handling: only the first is safe to treat as spent.
#[derive(Debug)]
pub(crate) enum TakeLogin {
    Redeemed(Box<PendingLogin>),
    /// Unknown, expired, or already redeemed.
    Unknown,
    /// The row exists and is live, but this browser did not start it. Left in place.
    WrongBrowser,
}

/// Compare two byte strings without an early exit. Both operands here are fixed-length blake3 hex,
/// so this is belt-and-braces — but a length-dependent or short-circuiting compare on anything
/// derived from a credential is the kind of thing that quietly becomes exploitable later.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An authorization request read back at the callback.
#[derive(Clone, Debug)]
pub struct PendingLogin {
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: Option<String>,
    /// blake3 of the value in the browser's `dam_oidc` cookie when this login began. The callback
    /// must present a cookie hashing to this — see [`ServerStore::take_oidc_login`].
    pub browser_hash: String,
}

impl ServerStore {
    // ── provider config ──────────────────────────────────────────────────────

    /// Write the provider config (upsert of the single row).
    ///
    /// An absent `client_secret` **keeps** the stored one, so an operator can adjust the issuer or
    /// scopes without re-typing a value the admin API would never show them. Passing an empty
    /// string is the explicit way to clear it (a public client).
    pub fn set_oidc_config(&self, req: &SetOidcConfig, actor: &str) -> Result<(), LibError> {
        let cfg = &req.config;
        // Validate here rather than at the first login: a misconfigured issuer should fail at the
        // moment an operator sets it, not silently at the moment a user tries to sign in.
        let issuer = cfg.issuer.trim_end_matches('/');
        if issuer.is_empty() {
            return Err(LibError::BadRequest("issuer is required".into()));
        }
        let url = url::Url::parse(issuer)
            .map_err(|e| LibError::BadRequest(format!("issuer is not a URL: {e}")))?;
        // An `http://` issuer would send the authorization code, and the ID token, in clear text.
        // Loopback is exempt so a local dev provider works without a certificate.
        //
        // Matched on `Host`, not `host_str()`: the latter returns the *serialisation*, so an IPv6
        // literal arrives as `[::1]` with the brackets and a `Some("::1")` arm never fires. That
        // failed closed rather than open, but it made the rule quietly narrower than it reads.
        let host_is_local = match url.host() {
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(a)) => a.is_loopback(),
            Some(url::Host::Ipv6(a)) => a.is_loopback(),
            None => false,
        };
        if url.scheme() != "https" && !host_is_local {
            return Err(LibError::BadRequest(
                "issuer must be https (http is allowed only for localhost)".into(),
            ));
        }
        if cfg.client_id.trim().is_empty() {
            return Err(LibError::BadRequest("client_id is required".into()));
        }
        url::Url::parse(&cfg.redirect_url)
            .map_err(|e| LibError::BadRequest(format!("redirect_url is not a URL: {e}")))?;

        let scopes = serde_json::to_string(&cfg.scopes).map_err(internal)?;
        let provisioning = serde_json::to_value(cfg.provisioning)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "linked".into());
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        match &req.client_secret {
            // `COALESCE(excluded.client_secret, oidc_provider.client_secret)` would be neater, but
            // reads worse than being explicit about which of the two statements ran.
            Some(secret) => {
                let secret = if secret.is_empty() {
                    None
                } else {
                    Some(secret.as_str())
                };
                conn.execute(
                    "INSERT INTO oidc_provider
                       (id, issuer, client_id, client_secret, redirect_url, scopes, provisioning,
                        updated_at, updated_by)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(id) DO UPDATE SET
                       issuer=?1, client_id=?2, client_secret=?3, redirect_url=?4, scopes=?5,
                       provisioning=?6, updated_at=?7, updated_by=?8",
                    params![
                        issuer,
                        cfg.client_id,
                        secret,
                        cfg.redirect_url,
                        scopes,
                        provisioning,
                        now,
                        actor
                    ],
                )
                .map_err(internal)?;
            }
            None => {
                conn.execute(
                    "INSERT INTO oidc_provider
                       (id, issuer, client_id, client_secret, redirect_url, scopes, provisioning,
                        updated_at, updated_by)
                     VALUES (1, ?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(id) DO UPDATE SET
                       issuer=?1, client_id=?2, redirect_url=?3, scopes=?4, provisioning=?5,
                       updated_at=?6, updated_by=?7",
                    params![
                        issuer,
                        cfg.client_id,
                        cfg.redirect_url,
                        scopes,
                        provisioning,
                        now,
                        actor
                    ],
                )
                .map_err(internal)?;
            }
        }
        // The secret is deliberately absent from the audit detail, for the same reason it is absent
        // from the GET: the audit log is readable by any admin and exportable.
        Self::audit_row(
            &conn,
            actor,
            "oidc.configure",
            None,
            Some(serde_json::json!({
                "issuer": issuer,
                "client_id": cfg.client_id,
                "provisioning": provisioning,
                "secret_changed": req.client_secret.is_some(),
            })),
        )?;
        Ok(())
    }

    /// The stored provider config including its secret. Crate-internal — the admin API reads
    /// [`ServerStore::oidc_config_info`] instead.
    pub(crate) fn oidc_provider(&self) -> Result<Option<StoredOidc>, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT issuer, client_id, client_secret, redirect_url, scopes, provisioning
             FROM oidc_provider WHERE id = 1",
            [],
            |r| {
                let scopes: String = r.get(4)?;
                let provisioning: String = r.get(5)?;
                Ok(StoredOidc {
                    config: OidcConfig {
                        issuer: r.get(0)?,
                        client_id: r.get(1)?,
                        redirect_url: r.get(3)?,
                        scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                        provisioning: serde_json::from_value(serde_json::Value::String(
                            provisioning,
                        ))
                        .unwrap_or_default(),
                    },
                    client_secret: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(internal)
    }

    /// The provider config for the admin API — same shape minus the secret, plus whether one is on
    /// file. There is no code path that can put the secret into this type.
    pub fn oidc_config_info(&self) -> Result<Option<OidcConfigInfo>, LibError> {
        Ok(self.oidc_provider()?.map(|s| OidcConfigInfo {
            config: s.config,
            client_secret_set: s.client_secret.is_some(),
        }))
    }

    // ── in-flight authorization requests ─────────────────────────────────────

    /// Sweep expired authorization requests and refuse if the table is at capacity.
    ///
    /// Called at the *top* of `/start`, before the outbound discovery request. `begin_oidc_login`
    /// re-checks — that one is authoritative and race-safe — but doing it only there would mean a
    /// flood still bought one untimed round trip to the identity provider per request, which is the
    /// expensive half. This makes the refusal cheap.
    pub(crate) fn check_oidc_login_capacity(&self) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM oidc_login WHERE created < ?1",
            params![now_ms() - LOGIN_TTL_MS],
        )
        .map_err(internal)?;
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM oidc_login", [], |r| r.get(0))
            .map_err(internal)?;
        if pending >= MAX_PENDING_LOGINS {
            return Err(LibError::Internal(
                "too many OIDC logins in flight; try again shortly".into(),
            ));
        }
        Ok(())
    }

    /// Record an authorization request, and sweep expired ones while we hold the lock.
    ///
    /// Capped, because `/api/v1/auth/oidc/start` is necessarily unauthenticated — it is the entry
    /// to a login — so anyone who can reach the server can ask it to write one of these rows. The
    /// TTL sweep alone bounds that only by "however many arrive in ten minutes". The ceiling is far
    /// above any real concurrent-login count, so it never interferes with use; it exists so a flood
    /// stops at a refusal instead of growing `server.db` without limit.
    pub(crate) fn begin_oidc_login(
        &self,
        state: &str,
        nonce: &str,
        pkce_verifier: &str,
        return_to: Option<&str>,
        browser_hash: &str,
    ) -> Result<(), LibError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM oidc_login WHERE created < ?1",
            params![now - LOGIN_TTL_MS],
        )
        .map_err(internal)?;
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM oidc_login", [], |r| r.get(0))
            .map_err(internal)?;
        if pending >= MAX_PENDING_LOGINS {
            return Err(LibError::Internal(
                "too many OIDC logins in flight; try again shortly".into(),
            ));
        }
        conn.execute(
            "INSERT INTO oidc_login (state, nonce, pkce_verifier, return_to, browser_hash, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![state, nonce, pkce_verifier, return_to, browser_hash, now],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Redeem an authorization request, but only for the browser that started it.
    ///
    /// Ordering is the point. The obvious implementation deletes by `state` and compares the
    /// browser hash afterwards — but then a callback with a valid `state` and the *wrong* cookie
    /// consumes the row, so the legitimate browser can never finish, and "wrong cookie" becomes
    /// indistinguishable from "already used". The check therefore happens first, and the row is
    /// only spent once it is going to be honoured.
    ///
    /// `SELECT` and `DELETE` are separate statements but share one held `Mutex<Connection>`, so no
    /// second caller can interleave; the `rows == 1` check turns a lost race (another process on
    /// the same file) into a refusal rather than a double-redeem.
    pub(crate) fn take_oidc_login(
        &self,
        state: &str,
        browser_hash: &str,
    ) -> Result<TakeLogin, LibError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT nonce, pkce_verifier, return_to, browser_hash, created
                 FROM oidc_login WHERE state = ?1",
                params![state],
                |r| {
                    Ok((
                        PendingLogin {
                            nonce: r.get(0)?,
                            pkce_verifier: r.get(1)?,
                            return_to: r.get(2)?,
                            browser_hash: r.get(3)?,
                        },
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(internal)?;
        let Some((login, created)) = row else {
            return Ok(TakeLogin::Unknown);
        };
        if now - created > LOGIN_TTL_MS {
            conn.execute("DELETE FROM oidc_login WHERE state = ?1", params![state])
                .map_err(internal)?;
            return Ok(TakeLogin::Unknown);
        }
        if !constant_time_eq(login.browser_hash.as_bytes(), browser_hash.as_bytes()) {
            // Deliberately leaves the row alone: this callback is not the one we are waiting for.
            return Ok(TakeLogin::WrongBrowser);
        }
        let rows = conn
            .execute("DELETE FROM oidc_login WHERE state = ?1", params![state])
            .map_err(internal)?;
        if rows != 1 {
            return Ok(TakeLogin::Unknown);
        }
        Ok(TakeLogin::Redeemed(Box::new(login)))
    }

    // ── identity links ───────────────────────────────────────────────────────

    /// The account a verified `(issuer, subject)` is linked to, if any.
    pub(crate) fn oidc_account(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<String>, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT account_id FROM oidc_identity WHERE issuer = ?1 AND subject = ?2",
            params![issuer, subject],
            |r| r.get(0),
        )
        .optional()
        .map_err(internal)
    }

    /// Link an external identity to an existing account.
    pub fn link_oidc_identity(
        &self,
        issuer: &str,
        subject: &str,
        account_id: &str,
        actor: &str,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "INSERT OR IGNORE INTO oidc_identity (issuer, subject, account_id, linked_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![issuer, subject, account_id, now_ms()],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::Conflict(
                "that provider subject is already linked to an account".into(),
            ));
        }
        Self::audit_row(
            &conn,
            actor,
            "oidc.link",
            Some(account_id),
            Some(serde_json::json!({ "issuer": issuer, "subject": subject })),
        )?;
        Ok(())
    }

    /// Create an account for a verified subject and link it, under the `auto_*` provisioning
    /// policies. The account has **no password hash** — it can only ever be signed into through the
    /// provider, which is why `account.password_hash` is nullable (it was reserved for this).
    ///
    /// `username` is derived by the caller from the token's claims and may collide with an existing
    /// local account; that is a conflict, not a merge. Silently adopting a same-named account would
    /// let anyone who can get a chosen `preferred_username` from the issuer take over a local one.
    pub(crate) fn provision_oidc_account(
        &self,
        issuer: &str,
        subject: &str,
        username: &str,
        display_name: Option<&str>,
        role: Role,
        actor: &str,
    ) -> Result<AccountInfo, LibError> {
        let account_id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        {
            let conn = self.conn.lock().unwrap();
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO account
                     (account_id, username, display_name, password_hash, role, disabled, created)
                     VALUES (?1, ?2, ?3, NULL, ?4, 0, ?5)",
                    params![account_id, username, display_name, role.as_str(), now],
                )
                .map_err(internal)?;
            if n == 0 {
                return Err(LibError::Conflict(format!(
                    "cannot provision '{username}': a local account already has that username"
                )));
            }
            conn.execute(
                "INSERT INTO oidc_identity (issuer, subject, account_id, linked_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![issuer, subject, account_id, now],
            )
            .map_err(internal)?;
            Self::audit_row(
                &conn,
                actor,
                "oidc.provision",
                Some(&account_id),
                Some(serde_json::json!({
                    "issuer": issuer, "subject": subject, "username": username, "role": role,
                })),
            )?;
        }
        self.bump_visibility_gen();
        Ok(AccountInfo {
            account_id,
            username: username.to_string(),
            display_name: display_name.map(str::to_string),
            role,
            disabled: false,
            created: now,
            last_login: None,
        })
    }
}

impl ServerStore {
    /// Every provider identity link, newest first, with the account's current username.
    pub fn list_oidc_identities(&self) -> Result<Vec<OidcIdentity>, LibError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT i.issuer, i.subject, i.account_id, a.username, i.linked_at
                 FROM oidc_identity i JOIN account a ON a.account_id = i.account_id
                 ORDER BY i.linked_at DESC",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(OidcIdentity {
                    issuer: r.get(0)?,
                    subject: r.get(1)?,
                    account_id: r.get(2)?,
                    username: r.get(3)?,
                    linked_at: r.get(4)?,
                })
            })
            .map_err(internal)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
    }

    /// Remove a link. The account is untouched — this revokes the *provider's* ability to sign in
    /// as it, which is a different act from disabling or deleting the account.
    pub fn unlink_oidc_identity(
        &self,
        issuer: &str,
        subject: &str,
        actor: &str,
    ) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "DELETE FROM oidc_identity WHERE issuer = ?1 AND subject = ?2",
                params![issuer, subject],
            )
            .map_err(internal)?;
        if n == 0 {
            return Err(LibError::NotFound("no such provider identity link".into()));
        }
        Self::audit_row(
            &conn,
            actor,
            "oidc.unlink",
            None,
            Some(serde_json::json!({ "issuer": issuer, "subject": subject })),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ServerStore {
        ServerStore::open_in_memory().unwrap()
    }

    fn cfg(issuer: &str) -> SetOidcConfig {
        SetOidcConfig {
            config: OidcConfig {
                issuer: issuer.into(),
                client_id: "client-abc".into(),
                redirect_url: "https://dam.example/api/v1/auth/oidc/callback".into(),
                scopes: vec!["email".into()],
                provisioning: OidcProvisioning::Linked,
            },
            client_secret: Some("s3cret".into()),
        }
    }

    #[test]
    fn the_client_secret_never_comes_back_out_of_the_read_path() {
        let st = store();
        st.set_oidc_config(&cfg("https://issuer.example"), "test")
            .unwrap();
        let info = st.oidc_config_info().unwrap().expect("configured");
        assert!(
            info.client_secret_set,
            "the operator must see that one is set"
        );
        // The read shape has nowhere to put a secret, so the guarantee is structural — this asserts
        // the serialized form too, which is what actually crosses the wire.
        let json = serde_json::to_string(&info).unwrap();
        assert!(
            !json.contains("s3cret"),
            "a secret reached the admin read path: {json}"
        );
        // …and the flow path still gets it.
        assert_eq!(
            st.oidc_provider()
                .unwrap()
                .unwrap()
                .client_secret
                .as_deref(),
            Some("s3cret")
        );
    }

    #[test]
    fn omitting_the_secret_on_update_keeps_the_stored_one() {
        let st = store();
        st.set_oidc_config(&cfg("https://issuer.example"), "test")
            .unwrap();
        let mut update = cfg("https://issuer.example");
        update.config.scopes = vec!["email".into(), "profile".into()];
        update.client_secret = None;
        st.set_oidc_config(&update, "test").unwrap();

        let stored = st.oidc_provider().unwrap().unwrap();
        assert_eq!(stored.client_secret.as_deref(), Some("s3cret"));
        assert_eq!(stored.config.scopes, vec!["email", "profile"]);

        // An explicit empty string is the way to clear it (a public client).
        let mut clear = cfg("https://issuer.example");
        clear.client_secret = Some(String::new());
        st.set_oidc_config(&clear, "test").unwrap();
        assert_eq!(st.oidc_provider().unwrap().unwrap().client_secret, None);
    }

    #[test]
    fn an_http_issuer_is_refused_unless_it_is_loopback() {
        let st = store();
        let err = st
            .set_oidc_config(&cfg("http://issuer.example"), "test")
            .expect_err("plaintext issuer must be refused");
        assert!(matches!(err, LibError::BadRequest(_)), "{err:?}");
        // Loopback is exempt so a local dev provider needs no certificate — in every spelling,
        // including the IPv6 literal, whose brackets used to make its arm unreachable.
        for local in [
            "http://localhost:9999",
            "http://127.0.0.1:9999",
            "http://[::1]:9999",
            "http://LOCALHOST:9999",
        ] {
            st.set_oidc_config(&cfg(local), "test")
                .unwrap_or_else(|e| panic!("{local} should be allowed: {e:?}"));
        }
        // A hostname that merely *starts* with a loopback name is not loopback.
        for hostile in [
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
        ] {
            assert!(
                st.set_oidc_config(&cfg(hostile), "test").is_err(),
                "{hostile} must not be treated as loopback"
            );
        }
    }

    #[test]
    fn only_one_provider_row_can_ever_exist() {
        let st = store();
        st.set_oidc_config(&cfg("https://one.example"), "test")
            .unwrap();
        st.set_oidc_config(&cfg("https://two.example"), "test")
            .unwrap();
        assert_eq!(
            st.oidc_provider().unwrap().unwrap().config.issuer,
            "https://two.example",
            "a second configure must replace, not add"
        );
    }

    /// The replay guard plus the browser binding, which are one operation.
    ///
    /// `state` is single-use, so a captured callback URL cannot be walked through the flow twice —
    /// and it is only spendable by the browser that started it, which is what stops login CSRF.
    #[test]
    fn an_authorization_request_is_single_use_and_bound_to_its_browser() {
        let st = store();
        let hash = |v: &str| blake3::hash(v.as_bytes()).to_hex().to_string();
        st.begin_oidc_login(
            "state-1",
            "nonce-1",
            "verifier-1",
            Some("/browse"),
            &hash("browser-token"),
        )
        .unwrap();

        // A different browser is refused — and, critically, does NOT consume the row. Deleting it
        // here would let anyone holding a leaked `state` lock the real user out of finishing.
        assert!(matches!(
            st.take_oidc_login("state-1", &hash("someone-else"))
                .unwrap(),
            TakeLogin::WrongBrowser
        ));

        let TakeLogin::Redeemed(first) = st
            .take_oidc_login("state-1", &hash("browser-token"))
            .unwrap()
        else {
            panic!("the right browser must still be able to redeem after a wrong-cookie attempt");
        };
        assert_eq!(first.nonce, "nonce-1");
        assert_eq!(first.pkce_verifier, "verifier-1");
        assert_eq!(first.return_to.as_deref(), Some("/browse"));

        assert!(
            matches!(
                st.take_oidc_login("state-1", &hash("browser-token"))
                    .unwrap(),
                TakeLogin::Unknown
            ),
            "a redeemed state must not be redeemable again"
        );
        assert!(matches!(
            st.take_oidc_login("never-issued", &hash("browser-token"))
                .unwrap(),
            TakeLogin::Unknown
        ));
    }

    #[test]
    fn a_subject_is_scoped_to_its_issuer() {
        let st = store();
        let acct = st
            .provision_oidc_account(
                "https://one.example",
                "subject-42",
                "alice",
                None,
                Role::Viewer,
                "test",
            )
            .unwrap();
        assert_eq!(
            st.oidc_account("https://one.example", "subject-42")
                .unwrap(),
            Some(acct.account_id.clone())
        );
        // The same `sub` from a different issuer is a different person. If this ever resolves, a
        // second configured provider can mint subjects that inherit the first's accounts.
        assert_eq!(
            st.oidc_account("https://two.example", "subject-42")
                .unwrap(),
            None,
            "subject must not resolve across issuers"
        );
    }

    #[test]
    fn provisioning_will_not_silently_adopt_an_existing_local_account() {
        let st = store();
        st.create_account(
            &dam_api::accounts::NewAccount {
                username: "alice".into(),
                password: "hunter2hunter2".into(),
                display_name: None,
                role: Role::Admin,
            },
            "test",
        )
        .unwrap();
        // Anyone who can get `preferred_username: alice` out of the issuer would otherwise take
        // over the local admin.
        let err = st
            .provision_oidc_account(
                "https://one.example",
                "subject-9",
                "alice",
                None,
                Role::Viewer,
                "test",
            )
            .expect_err("must not adopt a same-named local account");
        assert!(matches!(err, LibError::Conflict(_)), "{err:?}");
    }
}
