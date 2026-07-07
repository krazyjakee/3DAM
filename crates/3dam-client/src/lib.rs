//! `dam-client` — the API-client `LibraryService`. Implements the *same* trait as the embedded
//! engine by calling a remote `3dam serve` over `/api/v1` (tech-spec 03 §8). A front-end holding a
//! `Box<dyn LibraryService>` cannot tell this from the in-process engine — that is the whole point
//! of the seam. Phase 1 covers the REST slice; the WS live-update transport lands with the server WS.

use async_trait::async_trait;
use dam_api::admin::{
    AdminStatus, AuditEntry, CacheTarget, ClearAnalysisReport, ClearCacheReport, ClearCacheRequest,
    ConfirmRequest, FactoryResetReport, FlagInfo, NewToken, NewTokenReply, SetFlag, StorageUsage,
    TokenInfo, VacuumReport, WipeReport,
};
use dam_api::dto::*;
use dam_api::event::EventTopic;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use dam_api::page::{Page, PageParams};
use dam_api::service::{AuthContext, EventStream, LibraryService};
use dam_api::{ErrorBody, LibError};
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
        m if m.starts_with("image/") => {
            (MediaType::Image, m.trim_start_matches("image/").to_string())
        }
        m if m.starts_with("audio/") => (MediaType::Audio, String::new()),
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
        | LibraryEvent::AssetRemoved(_)
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

    // ── admin API (tech-spec 10 §5) — the CLI's `--connect` admin path ────────

    /// `GET /admin/api/status`.
    pub async fn admin_status(&self) -> Result<AdminStatus, LibError> {
        self.get("/admin/api/status").await
    }
    /// `GET /admin/api/flags`.
    pub async fn admin_flags(&self) -> Result<Vec<FlagInfo>, LibError> {
        self.get("/admin/api/flags").await
    }
    /// `PUT /admin/api/flags/{key}`.
    pub async fn admin_set_flag(&self, key: &str, req: &SetFlag) -> Result<FlagInfo, LibError> {
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
        let resp = self
            .http
            .delete(self.url(&format!("/admin/api/tokens/{id}"))?)
            .send()
            .await
            .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
        Self::expect_no_content(resp).await
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
        _ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        // Raw bytes, not JSON — reconstruct `AssetContent` from the HTTP response. Media/format are
        // recovered from the `Content-Type` header (the server sets it via `content_type_for`).
        let (ct, bytes) = self
            .fetch_bytes(
                self.http
                    .get(self.url(&format!("/api/v1/assets/{id}/content"))?),
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

    async fn library_stats(&self, _ctx: &AuthContext) -> Result<LibraryStats, LibError> {
        self.get("/api/v1/stats").await
    }

    async fn convert(
        &self,
        _ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError> {
        self.post("/api/v1/convert", &req).await
    }

    async fn list_sources(&self, _ctx: &AuthContext) -> Result<Vec<SourceInfo>, LibError> {
        self.get("/api/v1/sources").await
    }

    async fn get_source(&self, _ctx: &AuthContext, id: &SourceId) -> Result<SourceInfo, LibError> {
        self.get(&format!("/api/v1/sources/{id}")).await
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

    async fn find_similar(
        &self,
        _ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        self.post("/api/v1/similar", &req).await
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
