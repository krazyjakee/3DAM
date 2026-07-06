//! `dam-server` — the axum host. Exposes the embedded engine over `/api/v1` (tech-spec 03 §8),
//! serves live updates over a WebSocket, and hosts the embedded React web client (tech-spec 09 §A.4)
//! with SPA-fallback routing. `serve` starts the long-lived service; `mcp_stdio` is the local-agent
//! MCP transport (stub).

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_api::dto::*;
use dam_api::event::SubscribeRequest;
use dam_api::id::{AssetId, JobId, SourceId};
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
use rust_embed::RustEmbed;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// The built React web client (tech-spec 09 §A.4). `pnpm build` emits content-hashed assets into
/// `web/dist/`; this bakes them into the `3dam` binary so one file serves the whole UI with no
/// separate deploy. In debug builds rust-embed reads from disk (fast iteration); release embeds.
/// If `web/dist` is empty (web client not yet built), the handler serves a build hint instead.
#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct WebAssets;

/// Server configuration (a subset of the eventual serve config file, tech-spec 09).
pub struct ServeConfig {
    pub addr: SocketAddr,
    pub data_dir: PathBuf,
}

#[derive(Clone)]
struct AppState {
    lib: Arc<EmbeddedLibrary>,
}

fn ctx() -> AuthContext {
    AuthContext::embedded()
}

/// Wraps `LibError` so handlers can `?` and get the right status + wire body (tech-spec 03 §5.2).
struct ApiError(LibError);
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

/// Build the router for an already-open engine (also the seam a test harness targets).
pub fn router(lib: Arc<EmbeddedLibrary>) -> Router {
    let state = AppState { lib };
    Router::new()
        .route("/api/version", get(version))
        .route("/api/v1/query", post(query))
        .route("/api/v1/assets/{id}", get(get_asset))
        .route("/api/v1/assets/{id}/content", get(asset_content))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/sources", get(list_sources).post(add_source))
        .route(
            "/api/v1/sources/{id}",
            get(get_source).delete(remove_source),
        )
        .route("/api/v1/jobs/scan", post(submit_scan))
        .route("/api/v1/jobs/list", post(list_jobs))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/cancel", post(cancel_job))
        .route("/api/v1/ws", get(ws_handler))
        // SPA fallback: any non-API GET serves the embedded web client (tech-spec 09 §A.4).
        // API prefixes are matched above, so a mistyped `/api/...` still returns a JSON 404.
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Open the library and run the server until shutdown.
pub async fn serve(cfg: ServeConfig) -> anyhow::Result<()> {
    let lib = Arc::new(EmbeddedLibrary::open(&cfg.data_dir).await?);
    let app = router(lib);
    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    let actual = listener.local_addr()?;
    tracing::info!(%actual, data_dir = %cfg.data_dir.display(), "3dam serve listening");
    eprintln!(
        "3dam serve → http://{actual}  (data: {})",
        cfg.data_dir.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

/// The MCP stdio transport (local agents). Stub until the MCP phase (tech-spec 11, ADR 0003).
pub async fn mcp_stdio(_data_dir: PathBuf) -> anyhow::Result<()> {
    eprintln!("3dam mcp (stdio) is not implemented yet (tech-spec 11 / ADR 0003).");
    Ok(())
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// Serve the embedded web client with SPA-fallback semantics (tech-spec 09 §A.4):
/// a hashed asset path returns that asset with a long immutable cache; any other GET returns
/// `index.html` (no-cache) so the client router owns in-app navigation. A stray `/api/*` that
/// fell through returns a JSON 404 (file-03 error shape), never the HTML shell.
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    if path.starts_with("api/") || path == "api" {
        return ApiError(LibError::NotFound(format!("no route: /{path}"))).into_response();
    }

    if let Some(resp) = serve_embedded(path) {
        return resp;
    }
    // Unknown non-asset path → SPA shell (client-side route), or a build hint if unbuilt.
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

/// Look up one embedded file and build its response (content-type + cache policy).
fn serve_embedded(path: &str) -> Option<Response> {
    let file = WebAssets::get(path)?;
    let mime = file.metadata.mimetype();
    // Vite emits content-hashed asset filenames → safe to cache forever; the HTML shell must not.
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
        "capabilities": ["query", "sources", "scan", "stats", "ws", "web"],
    }))
}

async fn query(
    State(st): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<dam_api::page::Page<AssetSummary>>, ApiError> {
    Ok(Json(st.lib.query(&ctx(), req).await?))
}

async fn get_asset(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Asset>, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    Ok(Json(st.lib.get_asset(&ctx(), &id).await?))
}

/// Raw asset bytes for the WASM viewer islands (tech-spec 09 §B.3). Streams the file with its MIME
/// `Content-Type` so the DOM can `load_model` a GLB or WebAudio-decode an audio file. Private cache:
/// bytes can change on re-scan, so this is not the immutable-forever policy the hashed web assets use.
async fn asset_content(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_content(&ctx(), &id).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content.content_type),
            (header::CACHE_CONTROL, "private, max-age=60".to_string()),
        ],
        Body::from(content.bytes),
    )
        .into_response())
}

async fn stats(State(st): State<AppState>) -> Result<Json<LibraryStats>, ApiError> {
    Ok(Json(st.lib.library_stats(&ctx()).await?))
}

async fn list_sources(State(st): State<AppState>) -> Result<Json<Vec<SourceInfo>>, ApiError> {
    Ok(Json(st.lib.list_sources(&ctx()).await?))
}

async fn get_source(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<SourceInfo>, ApiError> {
    let id: SourceId = parse_id(&id, "source")?;
    Ok(Json(st.lib.get_source(&ctx(), &id).await?))
}

async fn add_source(
    State(st): State<AppState>,
    Json(req): Json<AddSource>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = st.lib.add_source(&ctx(), req).await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

async fn remove_source(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RemoveSource>,
) -> Result<StatusCode, ApiError> {
    let id: SourceId = parse_id(&id, "source")?;
    st.lib.remove_source(&ctx(), &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn submit_scan(
    State(st): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let job_id = st.lib.submit_scan(&ctx(), req).await?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn get_job(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<JobStatus>, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    Ok(Json(st.lib.get_job(&ctx(), &id).await?))
}

async fn list_jobs(
    State(st): State<AppState>,
    Json(req): Json<JobListRequest>,
) -> Result<Json<dam_api::page::Page<JobStatus>>, ApiError> {
    Ok(Json(st.lib.list_jobs(&ctx(), req).await?))
}

async fn cancel_job(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    st.lib.cancel_job(&ctx(), &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

async fn ws_loop(mut socket: WebSocket, st: AppState) {
    let mut stream = match st.lib.subscribe(&ctx(), SubscribeRequest::default()).await {
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
