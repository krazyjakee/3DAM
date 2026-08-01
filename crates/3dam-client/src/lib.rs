//! `dam-client` — the API-client `LibraryService`. Implements the *same* trait as the embedded
//! engine by calling a remote `3dam serve` over `/api/v1` (tech-spec 03 §8). A front-end holding a
//! `Box<dyn LibraryService>` cannot tell this from the in-process engine — that is the whole point
//! of the seam. Phase 1 covers the REST slice; the WS live-update transport lands with the server WS.

use async_trait::async_trait;
use dam_api::accounts::{
    AccountInfo, GroupInfo, GroupMembers, NewAccount, NewGroup, NewShare, ShareInfo, UpdateAccount,
};
use dam_api::admin::{
    AdminStatus, AuditEntry, CacheTarget, ClearAnalysisReport, ClearCacheReport, ClearCacheRequest,
    ConfirmRequest, FactoryResetReport, FlagInfo, LinkOidcIdentity, NewToken, NewTokenReply,
    OidcConfigInfo, OidcIdentity, SetFlag, SetFlagReply, SetOidcConfig, StorageUsage, TokenInfo,
    VacuumReport, WipeReport,
};
use dam_api::dto::*;
use dam_api::event::EventTopic;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, CollectionId, CommentId, ContentHash, JobId, SourceId};
use dam_api::page::{Page, PageParams};
use dam_api::service::{AuthContext, EventStream, LibraryService, WhoAmI};
use dam_api::{ErrorBody, LibError, PeerAdvertise};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::time::Duration;
use url::Url;

#[derive(serde::Deserialize)]
struct IdReply {
    id: SourceId,
}
#[derive(serde::Deserialize)]
struct CollectionIdReply {
    id: CollectionId,
}
#[derive(serde::Deserialize)]
struct JobIdReply {
    job_id: JobId,
}

pub struct ApiClient {
    base: Url,
    http: reqwest::Client,
    /// Bearer token, kept alongside the reqwest default header so the WebSocket handshake
    /// (`subscribe`, tech-spec 09 §A.3) can present the same credential (issue #36).
    token: Option<String>,
}

/// Recover `(media, format)` from a `Content-Type` — the inverse of `dam_api::dto::content_type_for`.
/// Only informational on the client side (the bytes are what matter); unknown types default to model.
fn media_from_content_type(ct: &str) -> (MediaType, String) {
    let main = ct.split(';').next().unwrap_or(ct).trim();
    match main {
        "model/gltf-binary" => (MediaType::Model, "glb".to_string()),
        "model/gltf+json" => (MediaType::Model, "gltf".to_string()),
        "model/obj" => (MediaType::Model, "obj".to_string()),
        "model/ply" => (MediaType::Model, "ply".to_string()),
        "model/stl" => (MediaType::Model, "stl".to_string()),
        "audio/wav" => (MediaType::Audio, "wav".to_string()),
        "audio/mpeg" => (MediaType::Audio, "mp3".to_string()),
        "audio/flac" => (MediaType::Audio, "flac".to_string()),
        "audio/ogg" => (MediaType::Audio, "ogg".to_string()),
        "audio/mp4" => (MediaType::Audio, "aac".to_string()),
        // video — the `.mp4`/`.mov` container extensions are shared with audio, and the server
        // already resolved which one this is (`content_type_for` consults the media type first),
        // so the MIME is the authority here.
        "video/mp4" => (MediaType::Video, "mp4".to_string()),
        "video/quicktime" => (MediaType::Video, "mov".to_string()),
        "video/webm" => (MediaType::Video, "webm".to_string()),
        "video/x-matroska" => (MediaType::Video, "mkv".to_string()),
        "video/x-msvideo" => (MediaType::Video, "avi".to_string()),
        // documents
        "application/pdf" => (MediaType::Document, "pdf".to_string()),
        "text/markdown" => (MediaType::Document, "md".to_string()),
        "text/plain" => (MediaType::Document, "txt".to_string()),
        "application/rtf" => (MediaType::Document, "rtf".to_string()),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            (MediaType::Document, "docx".to_string())
        }
        "application/vnd.oasis.opendocument.text" => (MediaType::Document, "odt".to_string()),
        m if m.starts_with("image/") => {
            (MediaType::Image, m.trim_start_matches("image/").to_string())
        }
        m if m.starts_with("audio/") => (MediaType::Audio, String::new()),
        // `video/ogg` and anything else in the family: the class is unambiguous even when the
        // format token isn't.
        m if m.starts_with("video/") => (MediaType::Video, String::new()),
        m if m.starts_with("text/") => (MediaType::Document, String::new()),
        _ => (MediaType::Model, String::new()),
    }
}

/// Open the WebSocket, presenting the bearer token on the handshake when the peer is token-gated.
async fn connect_ws(
    url: &Url,
    token: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    LibError,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| LibError::BadRequest(format!("bad ws url: {e}")))?;
    if let Some(t) = token {
        let value = format!("Bearer {t}")
            .parse()
            .map_err(|e| LibError::BadRequest(format!("invalid token: {e}")))?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION.as_str(), value);
    }
    let (ws, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
    Ok(ws)
}

/// Whether an event passes the subscription's topic filter. An empty topic list means "everything"
/// (matches the embedded engine, which streams the whole firehose).
fn topic_matches(topics: &[EventTopic], ev: &LibraryEvent) -> bool {
    if topics.is_empty() {
        return true;
    }
    let topic = match ev {
        LibraryEvent::AssetAdded(_)
        | LibraryEvent::AssetChanged { .. }
        | LibraryEvent::AssetRemoved { .. }
        // A catalog-wide reset is an asset-topic event: subscribers watching assets must drop
        // their caches and refetch (the whole catalog just changed underneath them).
        | LibraryEvent::CatalogReset => EventTopic::Assets,
        LibraryEvent::SourceState { .. } => EventTopic::Sources,
        LibraryEvent::JobProgress(_) => EventTopic::Jobs,
    };
    topics.contains(&topic)
}

impl ApiClient {
    /// Connect to a remote server without a credential (auth `Off`/`Anonymous` peers).
    pub async fn connect(endpoint: Url) -> Result<ApiClient, LibError> {
        Self::connect_with_token(endpoint, None).await
    }

    /// Connect presenting a bearer `token` on every request (required for a Token-mode peer). The
    /// credential is set as a default header on the reqwest client so it rides every call uniformly.
    pub async fn connect_with_token(
        endpoint: Url,
        token: Option<String>,
    ) -> Result<ApiClient, LibError> {
        let mut builder = reqwest::Client::builder();
        if let Some(t) = &token {
            let mut headers = reqwest::header::HeaderMap::new();
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
                .map_err(|e| LibError::BadRequest(format!("invalid token: {e}")))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
        let http = builder
            .build()
            .map_err(|e| LibError::Internal(e.to_string()))?;
        Ok(ApiClient {
            base: endpoint,
            http,
            token,
        })
    }

    fn url(&self, path: &str) -> Result<Url, LibError> {
        self.base
            .join(path)
            .map_err(|e| LibError::BadRequest(e.to_string()))
    }

    /// The `ws://` / `wss://` URL for the live-event endpoint, derived from the http(s) base.
    fn ws_url(&self) -> Result<Url, LibError> {
        let mut u = self.url("/api/v1/ws")?;
        let ws_scheme = if u.scheme() == "https" { "wss" } else { "ws" };
        u.set_scheme(ws_scheme)
            .map_err(|_| LibError::Internal("cannot derive ws scheme".into()))?;
        Ok(u)
    }

    async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, LibError> {
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LibError::Upstream(e.to_string()))?;
        if status.is_success() {
            serde_json::from_slice(&bytes).map_err(|e| LibError::Internal(e.to_string()))
        } else {
            match serde_json::from_slice::<ErrorBody>(&bytes) {
                Ok(body) => Err(LibError::from_body(body)),
                Err(_) => Err(LibError::Upstream(format!("HTTP {}", status.as_u16()))),
            }
        }
    }

    async fn expect_no_content(resp: reqwest::Response) -> Result<(), LibError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = resp.bytes().await.unwrap_or_default();
        match serde_json::from_slice::<ErrorBody>(&bytes) {
            Ok(body) => Err(LibError::from_body(body)),
            Err(_) => Err(LibError::Upstream(format!("HTTP {}", status.as_u16()))),
        }
    }

    /// Send a GET expecting raw bytes (not JSON): map a transport failure to `SourceUnavailable`, an
    /// error body / non-2xx to the right `LibError`, and on success return the response
    /// `Content-Type` (if present) with the body. Callers reconstruct the typed `AssetContent`.
    async fn fetch_bytes(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<(Option<String>, Vec<u8>), LibError> {
        let resp = req
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let bytes = resp.bytes().await.unwrap_or_default();
            return match serde_json::from_slice::<ErrorBody>(&bytes) {
                Ok(body) => Err(LibError::from_body(body)),
                Err(_) => Err(LibError::Upstream(format!("HTTP {}", status.as_u16()))),
            };
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LibError::Upstream(e.to_string()))?
            .to_vec();
        Ok((content_type, bytes))
    }

    /// Materialise a raw response while enforcing the cap against both the declared and actual
    /// bytes. The prior `HEAD` is only an optimisation: a changed or dishonest peer cannot turn it
    /// into an unbounded `Response::bytes()` allocation.
    async fn fetch_bytes_bounded(
        &self,
        req: reqwest::RequestBuilder,
        ceiling: u64,
    ) -> Result<(Option<String>, Vec<u8>), LibError> {
        let response = req
            .send()
            .await
            .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            // Do not materialise an untrusted error body on the content path. The status is enough
            // to fail this compatibility read; structured errors remain available on JSON calls.
            return Err(LibError::Upstream(format!("HTTP {}", status.as_u16())));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let announced = response.content_length();
        if announced.is_some_and(|length| length > ceiling) {
            return Err(LibError::Unsupported(format!(
                "asset is larger than the {ceiling}-byte preview content cap"
            )));
        }
        let capacity = announced
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| LibError::Upstream(error.to_string()))?;
            let actual = (bytes.len() as u64)
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| LibError::Unsupported("preview content is too large".into()))?;
            if actual > ceiling {
                return Err(LibError::Unsupported(format!(
                    "asset is larger than the {ceiling}-byte preview content cap"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((content_type, bytes))
    }

    fn content_metadata_from_headers(
        headers: &reqwest::header::HeaderMap,
        len: u64,
    ) -> AssetContentMetadata {
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let (media, format) = media_from_content_type(&content_type);
        let etag = headers
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        AssetContentMetadata {
            len,
            content_type,
            format,
            media,
            etag,
        }
    }

    fn response_content_len(headers: &reqwest::header::HeaderMap) -> Result<u64, LibError> {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| LibError::Upstream("content response omitted Content-Length".into()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, LibError> {
        let resp = self
            .http
            .get(self.url(path)?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, LibError> {
        let resp = self
            .http
            .post(self.url(path)?)
            .json(body)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, LibError> {
        let resp = self
            .http
            .put(self.url(path)?)
            .json(body)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    /// DELETE with no reply body (the common case; a DELETE that answers JSON decodes inline).
    async fn delete(&self, path: &str) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(path)?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    // ── federation (phase 6, issue #39) — this client is also the peer transport ──

    /// `GET /api/v1/advertise` — the peer's self-description (protocol version, catalog weight,
    /// embedding spaces). 404 while the peer's `federation` flag is off, which callers surface as
    /// "this instance is not serving federation".
    pub async fn advertise(&self) -> Result<PeerAdvertise, LibError> {
        self.get("/api/v1/advertise").await
    }

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

#[async_trait]
impl LibraryService for ApiClient {
    async fn query(
        &self,
        _ctx: &AuthContext,
        req: QueryRequest,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.post("/api/v1/query", &req).await
    }

    async fn get_asset(&self, _ctx: &AuthContext, id: &AssetId) -> Result<Asset, LibError> {
        self.get(&format!("/api/v1/assets/{id}")).await
    }

    async fn read_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        let metadata = self.content_metadata(ctx, id).await?;
        if metadata.len > MAX_MATERIALIZED_CONTENT_BYTES {
            return Err(LibError::Unsupported(format!(
                "asset is {} bytes; preview content is capped at {MAX_MATERIALIZED_CONTENT_BYTES} bytes",
                metadata.len
            )));
        }
        // Raw bytes, not JSON — reconstruct `AssetContent` from the HTTP response. Media/format are
        // recovered from the `Content-Type` header (the server sets it via `content_type_for`).
        let (ct, bytes) = self
            .fetch_bytes_bounded(
                self.http
                    .get(self.url(&format!("/api/v1/assets/{id}/content"))?),
                MAX_MATERIALIZED_CONTENT_BYTES,
            )
            .await?;
        let content_type = ct.unwrap_or_else(|| "application/octet-stream".to_string());
        let (media, format) = media_from_content_type(&content_type);
        Ok(AssetContent {
            bytes,
            content_type,
            format,
            media,
        })
    }

    async fn content_metadata(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContentMetadata, LibError> {
        let response = self
            .http
            .head(self.url(&format!("/api/v1/assets/{id}/content"))?)
            .send()
            .await
            .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        if !response.status().is_success() {
            return Err(LibError::Upstream(format!(
                "HTTP {}",
                response.status().as_u16()
            )));
        }
        let len = Self::response_content_len(response.headers())?;
        Ok(Self::content_metadata_from_headers(response.headers(), len))
    }

    async fn stream_content(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
    ) -> Result<AssetContentStream, LibError> {
        let response = self
            .http
            .get(self.url(&format!("/api/v1/assets/{id}/content"))?)
            .header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", range.first(), range.last()),
            )
            .send()
            .await
            .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(LibError::Upstream(format!(
                "range endpoint returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| LibError::Upstream("range response omitted Content-Range".into()))?;
        let (bounds, total_len) = content_range
            .strip_prefix("bytes ")
            .and_then(|value| value.split_once('/'))
            .ok_or_else(|| LibError::Upstream("range response has invalid Content-Range".into()))?;
        let (first, last) = bounds
            .split_once('-')
            .ok_or_else(|| LibError::Upstream("range response has invalid Content-Range".into()))?;
        let first = first
            .parse::<u64>()
            .map_err(|_| LibError::Upstream("range response has invalid Content-Range".into()))?;
        let last = last
            .parse::<u64>()
            .map_err(|_| LibError::Upstream("range response has invalid Content-Range".into()))?;
        let total_len = total_len
            .parse::<u64>()
            .map_err(|_| LibError::Upstream("range response has invalid Content-Range".into()))?;
        if first != range.first() || last != range.last() {
            return Err(LibError::Upstream(
                "range response bounds do not match the request".into(),
            ));
        }
        if range.last() >= total_len {
            return Err(LibError::Upstream(
                "range response bounds exceed the representation length".into(),
            ));
        }
        if Self::response_content_len(response.headers())? != range.len() {
            return Err(LibError::Upstream(
                "range response Content-Length does not match the request".into(),
            ));
        }
        let metadata = Self::content_metadata_from_headers(response.headers(), total_len);
        let bytes = response.bytes_stream().map(|item| {
            item.map(|chunk| chunk.to_vec())
                .map_err(|error| LibError::Upstream(error.to_string()))
        });
        Ok(AssetContentStream {
            metadata,
            range,
            bytes: Box::pin(bytes),
        })
    }

    async fn read_related_content(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
    ) -> Result<AssetContent, LibError> {
        let (ct, bytes) = self
            .fetch_bytes(
                self.http
                    .get(self.url(&format!("/api/v1/assets/{id}/related"))?)
                    .query(&[("path", rel)]),
            )
            .await?;
        let content_type = ct.unwrap_or_else(|| "application/octet-stream".to_string());
        let (media, format) = media_from_content_type(&content_type);
        Ok(AssetContent {
            bytes,
            content_type,
            format,
            media,
        })
    }

    async fn prefetch(&self, _ctx: &AuthContext, req: PrefetchRequest) -> Result<(), LibError> {
        // A lightweight hint (the assets + edge), not the bytes — those stay on HTTP/2 (ADR 0012).
        // The server warms the derivatives; we don't wait on the render.
        let resp = self
            .http
            .post(self.url("/api/v1/prefetch")?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn read_thumbnail(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
    ) -> Result<AssetContent, LibError> {
        let (ct, bytes) = self
            .fetch_bytes(
                self.http
                    .get(self.url(&format!("/api/v1/assets/{id}/thumbnail?edge={max_edge}"))?),
            )
            .await?;
        Ok(AssetContent {
            bytes,
            content_type: ct.unwrap_or_else(|| "image/png".to_string()),
            format: "png".to_string(),
            media: MediaType::Image,
        })
    }

    async fn read_model_preview(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        // Raw `DMSH` bytes, not JSON — reconstruct `AssetContent` from the HTTP response.
        let (ct, bytes) = self
            .fetch_bytes(
                self.http
                    .get(self.url(&format!("/api/v1/assets/{id}/preview-mesh"))?),
            )
            .await?;
        Ok(AssetContent {
            bytes,
            content_type: ct.unwrap_or_else(|| "model/x-dam-preview".to_string()),
            format: "dmsh".to_string(),
            media: MediaType::Model,
        })
    }

    async fn whoami(&self, _ctx: &AuthContext) -> Result<WhoAmI, LibError> {
        // The server resolves the presented token; the local advisory ctx is ignored (as with every
        // connected call — the boundary, not the client, decides scopes).
        self.get("/api/v1/whoami").await
    }

    async fn library_stats(
        &self,
        _ctx: &AuthContext,
        source: Option<SourceId>,
    ) -> Result<LibraryStats, LibError> {
        match source {
            Some(sid) => self.get(&format!("/api/v1/stats?source={sid}")).await,
            None => self.get("/api/v1/stats").await,
        }
    }

    async fn convert(
        &self,
        _ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError> {
        self.post("/api/v1/convert", &req).await
    }

    /// Forward the staged bytes to the server's upload route.
    ///
    /// The body is the file *handle*, not its contents: a connected CLI or desktop shell pushing a
    /// 2 GB model must not need 2 GB of RAM to do it. Everything else about the request — the
    /// destination, the name, the collision rule — rides as query parameters, which keeps the body
    /// a pure byte stream and means the server can start writing before the upload finishes.
    async fn upload(
        &self,
        _ctx: &AuthContext,
        req: UploadRequest,
        staged: &std::path::Path,
    ) -> Result<UploadOutcome, LibError> {
        let file = tokio::fs::File::open(staged)
            .await
            .map_err(|e| LibError::Internal(format!("staged upload: {e}")))?;
        let mut url = self.url("/api/v1/upload")?;
        url.query_pairs_mut()
            .append_pair("source", &req.source.to_string())
            .append_pair("folder", &req.folder)
            .append_pair("name", &req.name)
            // Spelled by the DTO's own serde naming rather than a local match, so the wire value
            // cannot drift from what the server deserialises.
            .append_pair(
                "collision",
                serde_json::to_value(req.collision)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "fail".into())
                    .as_str(),
            );
        let resp = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(reqwest::Body::from(file))
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn list_sources(&self, _ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError> {
        self.get("/api/v1/sources").await
    }

    async fn get_source(&self, _ctx: &AuthContext, id: &SourceId) -> Result<SourceInfo, LibError> {
        self.get(&format!("/api/v1/sources/{id}")).await
    }

    async fn list_folders(
        &self,
        _ctx: &AuthContext,
        req: FolderListing,
    ) -> Result<Vec<FolderEntry>, LibError> {
        self.post("/api/v1/folders", &req).await
    }

    async fn add_source(&self, _ctx: &AuthContext, req: AddSource) -> Result<SourceId, LibError> {
        let reply: IdReply = self.post("/api/v1/sources", &req).await?;
        Ok(reply.id)
    }

    async fn remove_source(
        &self,
        _ctx: &AuthContext,
        id: &SourceId,
        req: RemoveSource,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/sources/{id}"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn remove_asset(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        req: RemoveAsset,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/assets/{id}"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn list_blocklist(&self, _ctx: &AuthContext) -> Result<Vec<BlockEntry>, LibError> {
        self.get("/api/v1/blocklist").await
    }

    async fn unblock(&self, _ctx: &AuthContext, hash: &ContentHash) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/blocklist/{}", hash.to_hex()))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn submit_analyze(
        &self,
        _ctx: &AuthContext,
        req: AnalyzeRequest,
    ) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/analyze", &req).await?;
        Ok(reply.job_id)
    }

    async fn regenerate_thumbnails(
        &self,
        _ctx: &AuthContext,
        req: ThumbnailRegenRequest,
    ) -> Result<ThumbnailRegenReport, LibError> {
        self.post("/api/v1/thumbnails/regenerate", &req).await
    }

    async fn find_similar(
        &self,
        _ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        self.post("/api/v1/similar", &req).await
    }

    async fn find_similar_by_vector(
        &self,
        _ctx: &AuthContext,
        req: dam_api::VectorSimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        self.post("/api/v1/similar-by-vector", &req).await
    }

    async fn list_duplicates(
        &self,
        _ctx: &AuthContext,
        req: DupRequest,
    ) -> Result<Vec<DupGroup>, LibError> {
        self.post("/api/v1/duplicates", &req).await
    }

    async fn review_suggestion(
        &self,
        _ctx: &AuthContext,
        req: SuggestionReview,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .post(self.url("/api/v1/suggestions/review")?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn set_favorite(&self, _ctx: &AuthContext, req: FavoriteRequest) -> Result<(), LibError> {
        let resp = self
            .http
            .post(self.url("/api/v1/assets/favorite")?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn get_note(&self, _ctx: &AuthContext, id: &AssetId) -> Result<Option<Note>, LibError> {
        self.get(&format!("/api/v1/assets/{id}/note")).await
    }

    async fn set_note(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        req: NoteRequest,
    ) -> Result<Option<Note>, LibError> {
        let resp = self
            .http
            .put(self.url(&format!("/api/v1/assets/{id}/note"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn list_comments(
        &self,
        _ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Vec<Comment>, LibError> {
        self.get(&format!("/api/v1/assets/{asset}/comments")).await
    }

    async fn post_comment(
        &self,
        _ctx: &AuthContext,
        asset: &AssetId,
        req: NewComment,
    ) -> Result<Comment, LibError> {
        self.post(&format!("/api/v1/assets/{asset}/comments"), &req)
            .await
    }

    async fn edit_comment(
        &self,
        _ctx: &AuthContext,
        id: &CommentId,
        req: EditComment,
    ) -> Result<Comment, LibError> {
        let resp = self
            .http
            .put(self.url(&format!("/api/v1/comments/{id}"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::decode(resp).await
    }

    async fn delete_comment(&self, _ctx: &AuthContext, id: &CommentId) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/comments/{id}"))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn list_collections(&self, _ctx: &AuthContext) -> Result<Vec<Collection>, LibError> {
        self.get("/api/v1/collections").await
    }

    async fn get_collection(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<Collection, LibError> {
        self.get(&format!("/api/v1/collections/{id}")).await
    }

    async fn create_collection(
        &self,
        _ctx: &AuthContext,
        req: NewCollection,
    ) -> Result<CollectionId, LibError> {
        let reply: CollectionIdReply = self.post("/api/v1/collections", &req).await?;
        Ok(reply.id)
    }

    async fn update_collection(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
        req: UpdateCollection,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .put(self.url(&format!("/api/v1/collections/{id}"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn delete_collection(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .delete(self.url(&format!("/api/v1/collections/{id}"))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn modify_collection_members(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
        req: CollectionMembers,
    ) -> Result<(), LibError> {
        let resp = self
            .http
            .post(self.url(&format!("/api/v1/collections/{id}/members"))?)
            .json(&req)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn collection_assets(
        &self,
        _ctx: &AuthContext,
        id: &CollectionId,
        page: PageParams,
    ) -> Result<Page<AssetSummary>, LibError> {
        self.post(&format!("/api/v1/collections/{id}/assets"), &page)
            .await
    }

    async fn export(
        &self,
        _ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<ExportReport, LibError> {
        self.post("/api/v1/export", &req).await
    }

    async fn submit_scan(&self, _ctx: &AuthContext, req: ScanRequest) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/scan", &req).await?;
        Ok(reply.job_id)
    }

    async fn get_job(&self, _ctx: &AuthContext, id: &JobId) -> Result<JobStatus, LibError> {
        self.get(&format!("/api/v1/jobs/{id}")).await
    }

    async fn list_jobs(
        &self,
        _ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError> {
        self.post("/api/v1/jobs/list", &req).await
    }

    async fn cancel_job(&self, _ctx: &AuthContext, id: &JobId) -> Result<(), LibError> {
        let resp = self
            .http
            .post(self.url(&format!("/api/v1/jobs/{id}/cancel"))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
    }

    async fn subscribe(
        &self,
        _ctx: &AuthContext,
        req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError> {
        // Connect to the server WS firehose (`/api/v1/ws`, tech-spec 09 §A.3) and pump the
        // `LibraryEvent` stream into a channel, reconnecting with backoff so a connected frontend's
        // live updates survive a transient drop (mirrors the web client's ws.ts — issue #36, #25).
        let ws_url = self.ws_url()?;
        let token = self.token.clone();
        let topics = req.topics;
        let (tx, rx) = futures::channel::mpsc::unbounded::<LibraryEvent>();

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            loop {
                match connect_ws(&ws_url, token.as_deref()).await {
                    Ok(mut ws) => {
                        backoff = Duration::from_millis(500); // reset once connected
                        while let Some(msg) = ws.next().await {
                            match msg {
                                Ok(tokio_tungstenite::tungstenite::Message::Text(txt)) => {
                                    match serde_json::from_str::<LibraryEvent>(txt.as_str()) {
                                        Ok(ev) if topic_matches(&topics, &ev) => {
                                            // Receiver dropped → the subscription is gone; stop.
                                            if tx.unbounded_send(ev).is_err() {
                                                return;
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(e) => tracing::debug!(error = %e, "bad ws event frame"),
                                    }
                                }
                                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => {
                                    break
                                }
                                Ok(_) => {} // ping/pong/binary — ignore
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "ws subscribe connect failed"),
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
        });

        Ok(Box::pin(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn range_server(
        headers: &'static str,
        body: &'static [u8],
    ) -> (Url, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            socket
                .write_all(format!("HTTP/1.1 206 Partial Content\r\n{headers}\r\n").as_bytes())
                .await
                .unwrap();
            for chunk in body.chunks(3) {
                if socket.write_all(chunk).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            request
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    async fn raw_server(response: &'static [u8]) -> Url {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(response).await;
        });
        Url::parse(&format!("http://{address}/")).unwrap()
    }

    /// [`media_from_content_type`] is the documented inverse of `dto::content_type_for`, and the
    /// two live in different crates — nothing but this test stops them drifting. Only the media
    /// *class* has to survive the round trip: the format token is informational, and a MIME shared
    /// by several extensions (`audio/mp4` covers `aac`/`m4a`/`mp4`) cannot name which one it was.
    #[test]
    fn content_type_round_trips_to_the_same_media_class() {
        let matrix: &[(MediaType, &[&str])] = &[
            (
                MediaType::Model,
                &["glb", "gltf", "obj", "ply", "stl", "fbx"],
            ),
            (
                MediaType::Audio,
                // `mov`/`m4v` appear here too: the content probe reclassifies an audio-only
                // ISO-BMFF file while keeping its original extension.
                &[
                    "wav", "mp3", "flac", "ogg", "aac", "m4a", "mp4", "mov", "m4v",
                ],
            ),
            (
                MediaType::Image,
                &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff"],
            ),
            (
                MediaType::Video,
                &["mp4", "m4v", "mov", "webm", "mkv", "avi", "ogv"],
            ),
            (
                MediaType::Document,
                &["pdf", "md", "txt", "rtf", "docx", "odt"],
            ),
        ];
        for (media, formats) in matrix {
            for f in *formats {
                let ct = content_type_for(*media, f);
                let (got, _) = media_from_content_type(ct);
                assert_eq!(got, *media, "{f} served as {ct} came back as {got:?}");
            }
        }
    }

    #[tokio::test]
    async fn client_range_stream_validates_wire_bounds_without_materialising_the_total() {
        let (endpoint, request) = range_server(
            "Content-Type: video/mp4\r\nContent-Length: 6\r\nContent-Range: bytes 10-15/300000000\r\nETag: \"peer-hash\"\r\nConnection: close\r\n",
            b"abcdef",
        )
        .await;
        let client = ApiClient::connect(endpoint).await.unwrap();
        let range = ContentRange::new(10, 15).unwrap();
        let mut content = client
            .stream_content(&AuthContext::embedded(), &AssetId::new(), range)
            .await
            .unwrap();
        assert_eq!(content.metadata.len, 300_000_000);
        assert_eq!(content.metadata.media, MediaType::Video);
        let mut received = Vec::new();
        while let Some(chunk) = content.bytes.next().await {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(received, b"abcdef");
        let request = request.await.unwrap();
        assert!(request
            .to_ascii_lowercase()
            .contains("range: bytes=10-15\r\n"));

        let (endpoint, request) = range_server(
            "Content-Type: video/mp4\r\nContent-Length: 5\r\nContent-Range: bytes 10-15/300000000\r\nConnection: close\r\n",
            b"abcde",
        )
        .await;
        let client = ApiClient::connect(endpoint).await.unwrap();
        let error = match client
            .stream_content(&AuthContext::embedded(), &AssetId::new(), range)
            .await
        {
            Ok(_) => panic!("mismatched Content-Length must fail before exposing a stream"),
            Err(error) => error,
        };
        assert!(matches!(error, LibError::Upstream(_)));
        request.await.unwrap();

        let endpoint = raw_server(
            b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n",
        )
        .await;
        let client = ApiClient::connect(endpoint).await.unwrap();
        let error = client
            .fetch_bytes_bounded(client.http.get(client.url("content").unwrap()), 4)
            .await
            .expect_err("actual streamed bytes must enforce the materialisation cap");
        assert!(matches!(error, LibError::Unsupported(_)));
    }
}
