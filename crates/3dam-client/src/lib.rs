//! `dam-client` — the API-client `LibraryService`. Implements the *same* trait as the embedded
//! engine by calling a remote `3dam serve` over `/api/v1` (tech-spec 03 §8). A front-end holding a
//! `Box<dyn LibraryService>` cannot tell this from the in-process engine — that is the whole point
//! of the seam. Phase 1 covers the REST slice; the WS live-update transport lands with the server WS.
//!
//! The trait implementation lives here; the HTTP plumbing it is written in terms of is in
//! [`mod@http`], and the `/admin/api` surface is in [`mod@admin`].

mod admin;
mod http;

use async_trait::async_trait;
use dam_api::dto::*;
use dam_api::event::EventTopic;
use dam_api::event::{LibraryEvent, SubscribeRequest};
use dam_api::id::{AssetId, CollectionId, CommentId, ContentHash, JobId, SourceId};
use dam_api::page::{Page, PageParams};
use dam_api::service::{AuthContext, EventStream, LibraryService, WhoAmI};
use dam_api::LibError;
use futures::{SinkExt, StreamExt};
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

#[derive(serde::Deserialize)]
struct WsTicketReply {
    ticket: String,
}

/// Mint a short-lived, one-use ticket over authenticated HTTP, then open the WebSocket with only
/// that ticket in the query. Browser and native clients therefore share the same handshake and a
/// long-lived bearer credential never enters an upgrade request, URL, or proxy log.
async fn connect_ws(
    http: &reqwest::Client,
    base: &Url,
    url: &Url,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    LibError,
> {
    let ticket_url = base
        .join("/api/v1/ws-ticket")
        .map_err(|e| LibError::BadRequest(e.to_string()))?;
    let response = http
        .post(ticket_url)
        .send()
        .await
        .map_err(|e| LibError::SourceUnavailable(e.to_string()))?;
    let ticket: WsTicketReply = ApiClient::decode(response).await?;
    let mut ticketed = url.clone();
    ticketed
        .query_pairs_mut()
        .append_pair("ticket", &ticket.ticket);
    let (ws, _resp) = tokio_tungstenite::connect_async(ticketed.as_str())
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
        // Lag spans unknown topics, so every filtered subscriber must see the resync marker.
        LibraryEvent::StreamLagged => return true,
        LibraryEvent::SourceState { .. } => EventTopic::Sources,
        LibraryEvent::JobProgress(_) => EventTopic::Jobs,
    };
    topics.contains(&topic)
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

    async fn get_asset_from(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<Asset, LibError> {
        let response = self
            .http
            .get(self.routed_url(&format!("/api/v1/assets/{id}"), source)?)
            .send()
            .await
            .map_err(|error| LibError::SourceUnavailable(error.to_string()))?;
        Self::decode(response).await
    }

    async fn read_content(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<AssetContent, LibError> {
        self.read_content_from(ctx, id, None).await
    }

    async fn read_content_from(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let metadata = self.content_metadata_from(ctx, id, source).await?;
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
                    .get(self.routed_url(&format!("/api/v1/assets/{id}/content"), source)?),
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
        self.content_metadata_from(_ctx, id, None).await
    }

    async fn content_metadata_from(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContentMetadata, LibError> {
        let response = self
            .http
            .head(self.routed_url(&format!("/api/v1/assets/{id}/content"), source)?)
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
        self.stream_content_from(_ctx, id, range, None).await
    }

    async fn stream_content_from(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        range: ContentRange,
        source: Option<SourceId>,
    ) -> Result<AssetContentStream, LibError> {
        let response = self
            .http
            .get(self.routed_url(&format!("/api/v1/assets/{id}/content"), source)?)
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
        self.read_related_content_from(_ctx, id, rel, None).await
    }

    async fn read_related_content_from(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        rel: &str,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let (ct, bytes) = self
            .fetch_bytes(
                self.http
                    .get(self.routed_url(&format!("/api/v1/assets/{id}/related"), source)?)
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
        self.read_thumbnail_from(_ctx, id, max_edge, None).await
    }

    async fn read_thumbnail_from(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        max_edge: u32,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        let (ct, bytes) = self
            .fetch_bytes(self.http.get(self.routed_url(
                &format!("/api/v1/assets/{id}/thumbnail?edge={max_edge}"),
                source,
            )?))
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
        self.read_model_preview_from(_ctx, id, None).await
    }

    async fn read_model_preview_from(
        &self,
        _ctx: &AuthContext,
        id: &AssetId,
        source: Option<SourceId>,
    ) -> Result<AssetContent, LibError> {
        // Raw `DMSH` bytes, not JSON — reconstruct `AssetContent` from the HTTP response.
        let (ct, bytes) = self
            .fetch_bytes(
                self.http
                    .get(self.routed_url(&format!("/api/v1/assets/{id}/preview-mesh"), source)?),
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

    async fn submit_convert(
        &self,
        _ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/convert", &req).await?;
        Ok(reply.job_id)
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
    ) -> Result<Page<DupGroup>, LibError> {
        self.post("/api/v1/duplicates", &req).await
    }

    async fn duplicate_membership(
        &self,
        _ctx: &AuthContext,
        req: DupMembershipRequest,
    ) -> Result<Vec<DupMembership>, LibError> {
        self.post("/api/v1/duplicates/membership", &req).await
    }

    async fn duplicate_group(
        &self,
        _ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Option<DupGroup>, LibError> {
        self.get(&format!("/api/v1/assets/{asset}/duplicates"))
            .await
    }

    async fn duplicate_group_members(
        &self,
        _ctx: &AuthContext,
        req: DupGroupMembersRequest,
    ) -> Result<Page<DupMember>, LibError> {
        self.post("/api/v1/duplicates/group-members", &req).await
    }

    async fn review_duplicate(
        &self,
        _ctx: &AuthContext,
        req: DupReviewRequest,
    ) -> Result<(), LibError> {
        self.post("/api/v1/duplicates/review", &req).await
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

    async fn edit_tags(
        &self,
        _ctx: &AuthContext,
        req: TagEditRequest,
    ) -> Result<TagEditResult, LibError> {
        self.post("/api/v1/tags/edit", &req).await
    }

    async fn list_tags(
        &self,
        _ctx: &AuthContext,
        req: TagListRequest,
    ) -> Result<Vec<TagInfo>, LibError> {
        self.post("/api/v1/tags/list", &req).await
    }

    async fn set_license(
        &self,
        _ctx: &AuthContext,
        req: SetLicenseRequest,
    ) -> Result<LicenseEditResult, LibError> {
        self.post("/api/v1/assets/license", &req).await
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

    async fn submit_export(
        &self,
        _ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<JobId, LibError> {
        let reply: JobIdReply = self.post("/api/v1/jobs/export", &req).await?;
        Ok(reply.job_id)
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
        let base = self.base.clone();
        let http = self.http.clone();
        let topics = req.topics;
        const DELIVERY_CAPACITY: usize = 256;
        let (mut tx, rx) = futures::channel::mpsc::channel::<LibraryEvent>(DELIVERY_CAPACITY);

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            let mut needs_resync = false;
            loop {
                match connect_ws(&http, &base, &ws_url).await {
                    Ok(mut ws) => {
                        // A reconnect has an unknowable event gap even if the local queue never
                        // filled. Make the resync contract explicit before accepting fresh frames.
                        if needs_resync && tx.send(LibraryEvent::StreamLagged).await.is_err() {
                            return;
                        }
                        needs_resync = false;
                        let mut gap_signalled = false;
                        backoff = Duration::from_millis(500); // reset once connected
                        while let Some(msg) = ws.next().await {
                            match msg {
                                Ok(tokio_tungstenite::tungstenite::Message::Text(txt)) => {
                                    match serde_json::from_str::<LibraryEvent>(txt.as_str()) {
                                        Ok(ev) if topic_matches(&topics, &ev) => {
                                            if let Err(error) = tx.try_send(ev) {
                                                if error.is_disconnected() {
                                                    return;
                                                }
                                                // Delivery is deliberately bounded. Disconnect so
                                                // upstream applies backpressure, then place exactly
                                                // one resync marker behind the retained backlog.
                                                let _ = ws.close(None).await;
                                                if tx
                                                    .send(LibraryEvent::StreamLagged)
                                                    .await
                                                    .is_err()
                                                {
                                                    return;
                                                }
                                                gap_signalled = true;
                                                break;
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
                        // Overflow already placed its marker behind the retained queue. Every other
                        // disconnect may have missed remote frames before the next handshake.
                        if !gap_signalled {
                            needs_resync = true;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ws subscribe connect failed");
                        needs_resync = true;
                    }
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
