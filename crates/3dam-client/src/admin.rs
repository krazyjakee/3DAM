//! The server-administration surface of [`ApiClient`] — the `/admin/api` calls behind the CLI's
//! `--connect` admin path (tech-spec 10 §5): feature flags, tokens, the audit log, storage
//! maintenance, accounts/groups/shares, and OIDC provider configuration. These are inherent
//! methods, not part of `LibraryService`: administering a *server* is meaningless for the embedded
//! engine, so the seam deliberately does not carry them.

use crate::ApiClient;
use dam_api::accounts::{
    AccountInfo, GroupInfo, GroupMembers, NewAccount, NewGroup, NewShare, ShareInfo, UpdateAccount,
};
use dam_api::admin::{
    AdminStatus, AuditEntry, CacheTarget, ClearAnalysisReport, ClearCacheReport, ClearCacheRequest,
    ConfirmRequest, FactoryResetReport, FlagInfo, LinkOidcIdentity, NewToken, NewTokenReply,
    OidcConfigInfo, OidcIdentity, SetFlag, SetFlagReply, SetOidcConfig, StorageUsage, TokenInfo,
    VacuumReport, WipeReport,
};
use dam_api::LibError;

impl ApiClient {
    // ── admin API (tech-spec 10 §5) — the CLI's `--connect` admin path ────────

    /// `GET /admin/api/status`.
    pub async fn admin_status(&self) -> Result<AdminStatus, LibError> {
        self.get("/admin/api/status").await
    }
    /// `GET /admin/api/flags`.
    pub async fn admin_flags(&self) -> Result<Vec<FlagInfo>, LibError> {
        self.get("/admin/api/flags").await
    }
    /// `PUT /admin/api/flags/{key}` — the reply carries the bootstrap owner token when enabling
    /// auth minted the first admin credential.
    pub async fn admin_set_flag(&self, key: &str, req: &SetFlag) -> Result<SetFlagReply, LibError> {
        self.put(&format!("/admin/api/flags/{key}"), req).await
    }
    /// `GET /admin/api/tokens`.
    pub async fn admin_tokens(&self) -> Result<Vec<TokenInfo>, LibError> {
        self.get("/admin/api/tokens").await
    }
    /// `POST /admin/api/tokens` — returns the plaintext secret once.
    pub async fn admin_create_token(&self, req: &NewToken) -> Result<NewTokenReply, LibError> {
        self.post("/admin/api/tokens", req).await
    }
    /// `DELETE /admin/api/tokens/{id}`.
    pub async fn admin_revoke_token(&self, id: &str) -> Result<(), LibError> {
        self.delete(&format!("/admin/api/tokens/{id}")).await
    }
    /// `GET /admin/api/audit?limit=N`.
    pub async fn admin_audit(&self, limit: u32) -> Result<Vec<AuditEntry>, LibError> {
        self.get(&format!("/admin/api/audit?limit={limit}")).await
    }

    // ── storage & maintenance (tech-spec 10 §5) ──────────────────────────────

    /// `GET /admin/api/maintenance/usage`.
    pub async fn admin_storage_usage(&self) -> Result<StorageUsage, LibError> {
        self.get("/admin/api/maintenance/usage").await
    }
    /// `POST /admin/api/maintenance/clear-cache`.
    pub async fn admin_clear_cache(
        &self,
        target: CacheTarget,
    ) -> Result<ClearCacheReport, LibError> {
        self.post(
            "/admin/api/maintenance/clear-cache",
            &ClearCacheRequest { target },
        )
        .await
    }
    /// `POST /admin/api/maintenance/clear-analysis`.
    pub async fn admin_clear_analysis(&self) -> Result<ClearAnalysisReport, LibError> {
        self.post("/admin/api/maintenance/clear-analysis", &())
            .await
    }
    /// `POST /admin/api/maintenance/vacuum`.
    pub async fn admin_vacuum(&self) -> Result<VacuumReport, LibError> {
        self.post("/admin/api/maintenance/vacuum", &()).await
    }
    /// `POST /admin/api/maintenance/wipe` — reset the catalog (requires `confirm`).
    pub async fn admin_wipe(&self, confirm: bool) -> Result<WipeReport, LibError> {
        self.post("/admin/api/maintenance/wipe", &ConfirmRequest { confirm })
            .await
    }
    /// `POST /admin/api/maintenance/factory-reset` — erase everything (requires `confirm`).
    pub async fn admin_factory_reset(&self, confirm: bool) -> Result<FactoryResetReport, LibError> {
        self.post(
            "/admin/api/maintenance/factory-reset",
            &ConfirmRequest { confirm },
        )
        .await
    }

    // ── accounts / groups / shares (phase 6, issue #42) ──────────────────────
    // The whole surface 404s while the server's `user_accounts` flag is off (ADR 0004).

    /// `GET /admin/api/accounts`.
    pub async fn admin_accounts(&self) -> Result<Vec<AccountInfo>, LibError> {
        self.get("/admin/api/accounts").await
    }
    /// `POST /admin/api/accounts`.
    pub async fn admin_create_account(&self, req: &NewAccount) -> Result<AccountInfo, LibError> {
        self.post("/admin/api/accounts", req).await
    }
    /// `PUT /admin/api/accounts/{id}` — partial update; absent fields are left unchanged.
    pub async fn admin_update_account(
        &self,
        id: &str,
        req: &UpdateAccount,
    ) -> Result<AccountInfo, LibError> {
        self.put(&format!("/admin/api/accounts/{id}"), req).await
    }
    /// `DELETE /admin/api/accounts/{id}`.
    pub async fn admin_delete_account(&self, id: &str) -> Result<(), LibError> {
        self.delete(&format!("/admin/api/accounts/{id}")).await
    }
    /// `DELETE /admin/api/accounts/{id}/sessions` — sign the account out everywhere; returns how
    /// many sessions were revoked.
    pub async fn admin_revoke_account_sessions(&self, id: &str) -> Result<u64, LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/admin/api/accounts/{id}/sessions"))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        let reply: serde_json::Value = Self::decode(resp).await?;
        Ok(reply.get("revoked").and_then(|v| v.as_u64()).unwrap_or(0))
    }
    /// `GET /admin/api/groups`.
    pub async fn admin_groups(&self) -> Result<Vec<GroupInfo>, LibError> {
        self.get("/admin/api/groups").await
    }
    /// `POST /admin/api/groups`.
    pub async fn admin_create_group(&self, req: &NewGroup) -> Result<GroupInfo, LibError> {
        self.post("/admin/api/groups", req).await
    }
    /// `DELETE /admin/api/groups/{id}` — its shares and memberships cascade away.
    pub async fn admin_delete_group(&self, id: &str) -> Result<(), LibError> {
        self.delete(&format!("/admin/api/groups/{id}")).await
    }
    /// `PUT /admin/api/groups/{id}/members` — replaces the full membership set.
    pub async fn admin_set_group_members(
        &self,
        id: &str,
        req: &GroupMembers,
    ) -> Result<GroupInfo, LibError> {
        self.put(&format!("/admin/api/groups/{id}/members"), req)
            .await
    }
    /// `GET /admin/api/shares`.
    pub async fn admin_shares(&self) -> Result<Vec<ShareInfo>, LibError> {
        self.get("/admin/api/shares").await
    }
    /// `POST /admin/api/shares`.
    pub async fn admin_create_share(&self, req: &NewShare) -> Result<ShareInfo, LibError> {
        self.post("/admin/api/shares", req).await
    }
    /// `DELETE /admin/api/shares/{id}`.
    pub async fn admin_delete_share(&self, id: &str) -> Result<(), LibError> {
        self.delete(&format!("/admin/api/shares/{id}")).await
    }

    // ── OIDC provider configuration (phase 6, issue #41) ─────────────────────
    // Unlike the block above, this surface is *not* behind the `user_accounts` (or `oidc`) flag:
    // an operator configures the provider and links subjects before switching the flag on.

    /// `GET /admin/api/oidc` — the configured provider, or `None` when none is set. The reply type
    /// has no field for the client secret, so a secret can never come back this way.
    pub async fn admin_oidc(&self) -> Result<Option<OidcConfigInfo>, LibError> {
        self.get("/admin/api/oidc").await
    }
    /// `PUT /admin/api/oidc` — the secret is write-only, and omitting it keeps the stored one.
    pub async fn admin_set_oidc(
        &self,
        req: &SetOidcConfig,
    ) -> Result<Option<OidcConfigInfo>, LibError> {
        self.put("/admin/api/oidc", req).await
    }
    /// `GET /admin/api/oidc/identities`.
    pub async fn admin_oidc_identities(&self) -> Result<Vec<OidcIdentity>, LibError> {
        self.get("/admin/api/oidc/identities").await
    }
    /// `POST /admin/api/oidc/identities` — replies with the whole list, not just the new link.
    pub async fn admin_link_oidc_identity(
        &self,
        req: &LinkOidcIdentity,
    ) -> Result<Vec<OidcIdentity>, LibError> {
        self.post("/admin/api/oidc/identities", req).await
    }
    /// `DELETE /admin/api/oidc/identities/{subject}` — replies with the whole remaining list.
    pub async fn admin_unlink_oidc_identity(
        &self,
        subject: &str,
        issuer: Option<&str>,
    ) -> Result<Vec<OidcIdentity>, LibError> {
        // The subject is the provider's `sub` claim, not an id we mint: it may legally contain
        // `/`, `?`, or `#`. Pushed as a path *segment* (which percent-encodes) rather than
        // formatted into the path, so it can't rewrite the route.
        let mut url = self.url("/admin/api/oidc/identities")?;
        url.path_segments_mut()
            .map_err(|_| LibError::BadRequest("endpoint cannot take a path".into()))?
            .push(subject);
        // Names *which* link when the configured issuer is no longer the one it was made under.
        if let Some(i) = issuer {
            url.query_pairs_mut().append_pair("issuer", i);
        }
        let resp = self
            .http
            .delete(url)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }
}
