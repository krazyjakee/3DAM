//! `dam-server` — the axum host. Exposes the embedded engine over `/api/v1` (tech-spec 03 §8),
//! serves live updates over a WebSocket, hosts the embedded React web client (tech-spec 09 §A.4)
//! with SPA-fallback routing, and — new in phase 5 — wraps the whole surface in the auth layer
//! (tech-spec 10), mounts the admin API (`/admin/api`), and serves MCP over `POST /mcp` + `3dam mcp`
//! stdio (tech-spec 11, ADR 0003).
//!
//! Routing is **flag-aware but statically mounted**: rather than rebuild the router on every flag
//! flip, the `/mcp` route reads the live `McpServer` flag and falls through to `404` when it is
//! `Off` (the route-level-guard mechanism sanctioned by tech-spec 09 §A.2 / 11 §7 — "off ⇒ absent",
//! a 404 not a 403), so a live toggle takes effect immediately with no listener churn.

mod admin;
mod auth;
mod config;
mod mcp;
mod store;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_api::dto::*;
use dam_api::event::SubscribeRequest;
use dam_api::id::{AssetId, CollectionId, JobId, SourceId};
use dam_api::service::LibraryService;
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
use rust_embed::RustEmbed;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use auth::{Reader, Writer};
pub use config::ServeFile;
pub use mcp::{McpAdapter, WriteGate};
pub use store::ServerStore;

/// The built React web client (tech-spec 09 §A.4). `pnpm build` emits content-hashed assets into
/// `web/dist/`; this bakes them into the `3dam` binary so one file serves the whole UI with no
/// separate deploy. In debug builds rust-embed reads from disk (fast iteration); release embeds.
/// If `web/dist` is empty (web client not yet built), the handler serves a build hint instead.
#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct WebAssets;

/// Server configuration. `addr`/`config`/`insecure` come from the CLI (tech-spec 09 §A.1); the flag
/// state, tokens, and audit log live in the server store, seeded from the config file.
pub struct ServeConfig {
    /// Explicit bind from `--addr`. `None` → fall back to the config file, then the safe default.
    pub addr: Option<SocketAddr>,
    pub data_dir: PathBuf,
    /// Path to the serve config file (`--config`); `None` → no file (safe defaults only).
    pub config: Option<PathBuf>,
    /// Allow binding beyond localhost without TLS (ADR 0009 §4 — refused unless overridden).
    pub insecure: bool,
}

/// The one state every handler and auth extractor reads. Cheap to clone (two `Arc`s + small fields).
#[derive(Clone)]
pub(crate) struct AppState {
    pub lib: Arc<EmbeddedLibrary>,
    pub store: Arc<ServerStore>,
    pub bind: String,
    pub localhost_only: bool,
    pub tls: bool,
}

/// Full-scope context for the in-process engine call (the engine trusts its caller; the boundary is
/// guarded by the auth extractors before we get here).
fn ectx() -> dam_api::service::AuthContext {
    dam_api::service::AuthContext::embedded()
}

/// Wraps `LibError` so handlers can `?` and get the right status + wire body (tech-spec 03 §5.2).
pub(crate) struct ApiError(pub LibError);
impl From<LibError> for ApiError {
    fn from(e: LibError) -> Self {
        ApiError(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.to_body())).into_response()
    }
}

fn parse_id<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, ApiError> {
    s.parse::<T>()
        .map_err(|_| ApiError(LibError::BadRequest(format!("invalid {what} id"))))
}

/// Assemble the router over a fully-built [`AppState`].
pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/version", get(version))
        .route("/api/v1/query", post(query))
        .route("/api/v1/assets/{id}", get(get_asset))
        .route("/api/v1/assets/{id}/content", get(asset_content))
        .route("/api/v1/assets/{id}/thumbnail", get(asset_thumbnail))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/convert", post(convert))
        .route("/api/v1/similar", post(find_similar))
        .route("/api/v1/duplicates", post(list_duplicates))
        .route("/api/v1/suggestions/review", post(review_suggestion))
        .route("/api/v1/jobs/analyze", post(submit_analyze))
        .route("/api/v1/sources", get(list_sources).post(add_source))
        .route("/api/v1/sources/{id}", get(get_source).delete(remove_source))
        .route(
            "/api/v1/collections",
            get(list_collections).post(create_collection),
        )
        .route(
            "/api/v1/collections/{id}",
            get(get_collection)
                .put(update_collection)
                .delete(delete_collection),
        )
        .route(
            "/api/v1/collections/{id}/members",
            post(modify_collection_members),
        )
        .route("/api/v1/collections/{id}/assets", post(collection_assets))
        .route("/api/v1/export", post(export))
        .route("/api/v1/jobs/scan", post(submit_scan))
        .route("/api/v1/jobs/list", post(list_jobs))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/cancel", post(cancel_job))
        .route("/api/v1/ws", get(ws_handler))
        // The MCP endpoint (tech-spec 11): present, but the handler 404s when the flag is Off.
        .route("/mcp", post(mcp_http))
        // The admin API (tech-spec 10 §5), guarded by the AdminAuth extractor.
        .merge(admin::routes())
        // SPA fallback: any non-API GET serves the embedded web client (tech-spec 09 §A.4).
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Build the router over an open engine + server store — the seam a test harness targets.
pub fn router(
    lib: Arc<EmbeddedLibrary>,
    store: Arc<ServerStore>,
    bind: impl Into<String>,
    localhost_only: bool,
) -> Router {
    let bind = bind.into();
    build_router(AppState {
        lib,
        store,
        bind,
        localhost_only,
        tls: false,
    })
}

/// Open the library + server store, seed flags from the config file, and run until shutdown.
pub async fn serve(cfg: ServeConfig) -> anyhow::Result<()> {
    // 1. Config: resolve the bind with precedence CLI `--addr` > config `[server]` > safe default.
    let file = match &cfg.config {
        Some(p) => ServeFile::load(p)?,
        None => ServeFile::default(),
    };
    let default_addr: SocketAddr = "127.0.0.1:7878".parse().unwrap();
    let addr = cfg.addr.or_else(|| file.socket_addr()).unwrap_or(default_addr);
    let localhost_only = addr.ip().is_loopback();
    let tls = false; // static rustls cert/key is a phase-6 follow-up (ADR 0009 §4).

    // Refuse to expose beyond localhost without TLS unless explicitly overridden (ADR 0009 §4).
    if !localhost_only && !tls && !cfg.insecure {
        anyhow::bail!(
            "refusing to bind {addr} (beyond localhost) without TLS. Pass --insecure to override \
             for a trusted network, or terminate TLS in front."
        );
    }

    // 2. Stores: open the engine and the server store; seed flags from the config file (seed-only).
    let lib = Arc::new(EmbeddedLibrary::open(&cfg.data_dir).await?);
    let store = Arc::new(ServerStore::open(&cfg.data_dir.join("server.db"))?);
    for (key, value) in file.flag_seeds() {
        store.seed_flag(key, value)?;
    }

    let state = AppState {
        lib,
        store: store.clone(),
        bind: addr.to_string(),
        localhost_only,
        tls,
    };
    let app = build_router(state);

    // 3. Bind + log the posture so an operator sees "am I safe to expose?" at a glance (tech-spec 15).
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    let s = store.status(&actual.to_string(), localhost_only, tls);
    tracing::info!(
        %actual, auth = ?s.auth, mcp = ?s.mcp, network_writes = s.network_writes,
        exposed_without_auth = s.exposed_without_auth, "3dam serve listening"
    );
    eprintln!(
        "3dam serve → http://{actual}  (auth: {:?}, mcp: {:?}, writes: {})  data: {}",
        s.auth,
        s.mcp,
        s.network_writes,
        cfg.data_dir.display()
    );
    if s.exposed_without_auth {
        eprintln!("  ⚠ exposed beyond localhost with no auth and no TLS — set the authentication flag");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

/// The MCP stdio transport (`3dam mcp`, tech-spec 11 §2.2). Opens an embedded engine — no network,
/// no running server, no auth surface — and serves the identical adapter over stdio. Locally trusted:
/// writes are allowed (still non-destructive). Unaffected by the served `McpServer` flag (§7).
pub async fn mcp_stdio(data_dir: PathBuf) -> anyhow::Result<()> {
    let lib = Arc::new(EmbeddedLibrary::open(&data_dir).await?);
    let library: Arc<dyn LibraryService> = lib;
    let adapter = McpAdapter::new(library, WriteGate::local_stdio());
    adapter.serve_stdio(ectx()).await
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// Serve the embedded web client with SPA-fallback semantics (tech-spec 09 §A.4).
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    if path.starts_with("api/") || path == "api" {
        return ApiError(LibError::NotFound(format!("no route: /{path}"))).into_response();
    }

    if let Some(resp) = serve_embedded(path) {
        return resp;
    }
    match serve_embedded("index.html") {
        Some(resp) => resp,
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!doctype html><meta charset=utf-8><title>3dam</title>\
             <h1>3dam serve</h1><p>API is live at <code>/api/v1</code>. The web client bundle is \
             not present — run <code>pnpm --dir web build</code> (or <code>cargo xtask web</code>) \
             and rebuild.</p>",
        )
            .into_response(),
    }
}

fn serve_embedded(path: &str) -> Option<Response> {
    let file = WebAssets::get(path)?;
    let mime = file.metadata.mimetype();
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::CACHE_CONTROL, cache.to_string()),
            ],
            Body::from(file.data.into_owned()),
        )
            .into_response(),
    )
}

async fn version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "api": "v1",
        "server": concat!("3dam ", env!("CARGO_PKG_VERSION")),
        "capabilities": ["query", "sources", "scan", "stats", "ws", "web", "thumbnail", "convert",
                         "analyze", "similar", "duplicates", "suggestions", "collections", "export",
                         "auth", "flags", "mcp"],
    }))
}

async fn query(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<dam_api::page::Page<AssetSummary>>, ApiError> {
    Ok(Json(st.lib.query(&ctx, req).await?))
}

async fn get_asset(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Asset>, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    Ok(Json(st.lib.get_asset(&ctx, &id).await?))
}

async fn asset_content(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_content(&ctx, &id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content.content_type),
            (header::CACHE_CONTROL, "private, max-age=60".to_string()),
        ],
        Body::from(content.bytes),
    )
        .into_response())
}

async fn asset_thumbnail(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let edge = q.edge.unwrap_or(256);
    let content = st.lib.read_thumbnail(&ctx, &id, edge).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content.content_type),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
        ],
        Body::from(content.bytes),
    )
        .into_response())
}

#[derive(serde::Deserialize)]
struct ThumbQuery {
    edge: Option<u32>,
}

async fn stats(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<LibraryStats>, ApiError> {
    Ok(Json(st.lib.library_stats(&ctx).await?))
}

async fn convert(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ConvertRequest>,
) -> Result<Json<ConvertReport>, ApiError> {
    Ok(Json(st.lib.convert(&ctx, req).await?))
}

async fn submit_analyze(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<AnalyzeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job_id = st.lib.submit_analyze(&ctx, req).await?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn find_similar(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<SimilarRequest>,
) -> Result<Json<dam_api::page::Page<SimilarHit>>, ApiError> {
    Ok(Json(st.lib.find_similar(&ctx, req).await?))
}

async fn list_duplicates(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<DupRequest>,
) -> Result<Json<Vec<DupGroup>>, ApiError> {
    Ok(Json(st.lib.list_duplicates(&ctx, req).await?))
}

async fn review_suggestion(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<SuggestionReview>,
) -> Result<StatusCode, ApiError> {
    st.lib.review_suggestion(&ctx, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sources(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<SourceInfo>>, ApiError> {
    Ok(Json(st.lib.list_sources(&ctx).await?))
}

async fn get_source(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<SourceInfo>, ApiError> {
    let id: SourceId = parse_id(&id, "source")?;
    Ok(Json(st.lib.get_source(&ctx, &id).await?))
}

async fn add_source(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<AddSource>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = st.lib.add_source(&ctx, req).await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

async fn remove_source(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RemoveSource>,
) -> Result<StatusCode, ApiError> {
    let id: SourceId = parse_id(&id, "source")?;
    st.lib.remove_source(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_collections(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<Collection>>, ApiError> {
    Ok(Json(st.lib.list_collections(&ctx).await?))
}

async fn get_collection(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Collection>, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    Ok(Json(st.lib.get_collection(&ctx, &id).await?))
}

async fn create_collection(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<NewCollection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = st.lib.create_collection(&ctx, req).await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

async fn update_collection(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<UpdateCollection>,
) -> Result<StatusCode, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    st.lib.update_collection(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_collection(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    st.lib.delete_collection(&ctx, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn modify_collection_members(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<CollectionMembers>,
) -> Result<StatusCode, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    st.lib.modify_collection_members(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn collection_assets(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(page): Json<dam_api::page::PageParams>,
) -> Result<Json<dam_api::page::Page<AssetSummary>>, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    Ok(Json(st.lib.collection_assets(&ctx, &id, page).await?))
}

async fn export(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<Json<ExportReport>, ApiError> {
    Ok(Json(st.lib.export(&ctx, req).await?))
}

async fn submit_scan(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job_id = st.lib.submit_scan(&ctx, req).await?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn get_job(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<JobStatus>, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    Ok(Json(st.lib.get_job(&ctx, &id).await?))
}

async fn list_jobs(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<JobListRequest>,
) -> Result<Json<dam_api::page::Page<JobStatus>>, ApiError> {
    Ok(Json(st.lib.list_jobs(&ctx, req).await?))
}

async fn cancel_job(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    st.lib.cancel_job(&ctx, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ws_handler(_r: Reader, ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

async fn ws_loop(mut socket: WebSocket, st: AppState) {
    let mut stream = match st.lib.subscribe(&ectx(), SubscribeRequest::default()).await {
        Ok(s) => s,
        Err(_) => return,
    };
    while let Some(ev) = stream.next().await {
        match serde_json::to_string(&ev) {
            Ok(txt) => {
                if socket.send(Message::Text(txt.into())).await.is_err() {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
}

/// `POST /mcp` — the Streamable-HTTP MCP transport (tech-spec 11 §2.1). The `McpServer` flag is read
/// live: `Off` ⇒ `404` (route absent, checked *before* auth so a disabled endpoint cannot even be
/// probed). Otherwise auth is resolved on the shared surface and one JSON-RPC message is dispatched.
async fn mcp_http(State(st): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    use dam_api::admin::McpMode;
    let mcp = st.store.mcp_mode();
    if matches!(mcp, McpMode::Off) {
        return ApiError(LibError::NotFound("mcp is disabled".into())).into_response();
    }
    let ctx = match auth::resolve(&st.store, auth::bearer_header(&headers)) {
        Ok(c) => c,
        Err(e) => return ApiError(e).into_response(),
    };
    let bytes = match axum::body::to_bytes(body, 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return ApiError(LibError::BadRequest("body too large".into())).into_response(),
    };
    let msg: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                })),
            )
                .into_response()
        }
    };
    let library: Arc<dyn LibraryService> = st.lib.clone();
    let gate = WriteGate::from_flags(mcp, st.localhost_only, st.store.network_writes());
    let adapter = McpAdapter::new(library, gate);
    match adapter.handle_message(&ctx, msg).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        // A notification has no reply — acknowledge with 202 (Streamable HTTP allows an empty body).
        None => StatusCode::ACCEPTED.into_response(),
    }
}
