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
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use dam_api::dto::*;
use dam_api::event::SubscribeRequest;
use dam_api::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use dam_api::service::LibraryService;
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use futures::StreamExt;
use rust_embed::RustEmbed;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
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
    /// PEM certificate chain for in-process TLS (issue #75). With `tls_key`, `serve` speaks HTTPS and
    /// may bind beyond localhost with no `--insecure`. `None` → plaintext (localhost-only by default).
    pub tls_cert: Option<PathBuf>,
    /// PEM private key paired with `tls_cert`.
    pub tls_key: Option<PathBuf>,
}

/// The one state every handler and auth extractor reads. Cheap to clone (two `Arc`s + small fields).
#[derive(Clone)]
pub(crate) struct AppState {
    pub lib: Arc<EmbeddedLibrary>,
    pub store: Arc<ServerStore>,
    pub bind: String,
    pub localhost_only: bool,
    pub tls: bool,
    /// Flips `false → true` once when shutdown begins, so long-lived handlers (the `/api/v1/ws`
    /// loop) can stop awaiting and close cleanly instead of pinning the graceful drain open.
    pub shutdown: watch::Receiver<bool>,
    /// Readiness for `/readyz` (issue #75): `false` until the engine, stores, and background job
    /// pipeline are fully wired, then `true`. A load balancer routes traffic only once this holds.
    pub ready: Arc<std::sync::atomic::AtomicBool>,
}

/// Bridges the server's live feature flags to the engine's background pipeline (issue #71). The
/// engine asks, on each drain, whether it should auto-generate — we answer from the flag store, so a
/// runtime admin toggle changes behaviour on the next scan with no restart.
struct ServerPipelinePolicy {
    store: Arc<ServerStore>,
}

impl dam_core::PipelinePolicy for ServerPipelinePolicy {
    fn auto_thumbnail(&self) -> bool {
        self.store.auto_thumbnail()
    }
    fn auto_analyze(&self) -> bool {
        self.store.auto_analyze()
    }
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
        // Ops health probes (issue #75), unauthenticated + distinct from the versioned API so a load
        // balancer / systemd watchdog can poll them without a token: liveness vs readiness.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/query", post(query))
        .route("/api/v1/assets/{id}", get(get_asset).delete(remove_asset))
        .route("/api/v1/assets/{id}/content", get(asset_content))
        .route("/api/v1/assets/{id}/related", get(asset_related))
        .route("/api/v1/assets/{id}/preview-mesh", get(asset_preview_mesh))
        .route("/api/v1/assets/{id}/thumbnail", get(asset_thumbnail))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/convert", post(convert))
        .route("/api/v1/similar", post(find_similar))
        // Federation surface (phase 6, issue #39): statically mounted, 404 while the `federation`
        // flag is off — the same route-level-guard mechanism as `/mcp` ("off ⇒ absent", ADR 0004).
        .route("/api/v1/advertise", get(advertise))
        .route("/api/v1/similar-by-vector", post(similar_by_vector))
        .route("/api/v1/duplicates", post(list_duplicates))
        .route("/api/v1/suggestions/review", post(review_suggestion))
        .route("/api/v1/assets/favorite", post(set_favorite))
        .route("/api/v1/jobs/analyze", post(submit_analyze))
        .route("/api/v1/thumbnails/regenerate", post(regenerate_thumbnails))
        // Prefetch hint (issue #72): warm thumbnails/preview meshes ahead of the client's HTTP fetch.
        .route("/api/v1/prefetch", post(prefetch))
        .route("/api/v1/sources", get(list_sources).post(add_source))
        .route("/api/v1/folders", post(list_folders))
        .route(
            "/api/v1/sources/{id}",
            get(get_source).delete(remove_source),
        )
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
        .route("/api/v1/blocklist", get(list_blocklist))
        .route("/api/v1/blocklist/{hash}", delete(unblock))
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
    // No signal plumbing in the test seam; leak the sender so the receiver never observes a change
    // (a dropped sender would make `changed()` resolve and close WS loops immediately).
    let (tx, shutdown) = watch::channel(false);
    Box::leak(Box::new(tx));
    build_router(AppState {
        lib,
        store,
        bind,
        localhost_only,
        tls: false,
        shutdown,
        // The test seam is ready the moment it's built (no async pipeline warm-up to await).
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
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
    let addr = cfg
        .addr
        .or_else(|| file.socket_addr())
        .unwrap_or(default_addr);
    let localhost_only = addr.ip().is_loopback();
    // In-process TLS (issue #75): CLI flags win over the config file; both cert *and* key must be
    // present to enable HTTPS. With TLS on, exposing beyond localhost needs no `--insecure`.
    let tls_paths: Option<(PathBuf, PathBuf)> = match (
        cfg.tls_cert
            .clone()
            .or_else(|| file.server.tls_cert.clone()),
        cfg.tls_key.clone().or_else(|| file.server.tls_key.clone()),
    ) {
        (Some(cert), Some(key)) => Some((cert, key)),
        _ => None,
    };
    let tls = tls_paths.is_some();

    // Refuse to expose beyond localhost without TLS unless explicitly overridden (ADR 0009 §4).
    if !localhost_only && !tls && !cfg.insecure {
        anyhow::bail!(
            "refusing to bind {addr} (beyond localhost) without TLS. Provide --tls-cert/--tls-key \
             for HTTPS, pass --insecure to override for a trusted network, or terminate TLS in front."
        );
    }

    // 2. Stores: open the engine and the server store; seed flags from the config file (seed-only).
    let lib = Arc::new(EmbeddedLibrary::open(&cfg.data_dir).await?);
    lib.start_watchers(); // long-running role: resume auto-rescan for watch-enabled sources.
    let store = Arc::new(ServerStore::open(&cfg.data_dir.join("server.db"))?);
    for (key, value) in file.flag_seeds() {
        store.seed_flag(key, value)?;
    }

    // Hosted-mode background pipeline (issue #71): proactively drain thumbnails + analysis on ingest
    // so a freshly-connected client hits ready data instead of paying first-look render latency.
    // Flag-gated through the server store (`auto_thumbnail` / `auto_analyze`), read fresh on every
    // drain so a live admin toggle takes effect on the next scan.
    lib.start_background_pipeline(Arc::new(ServerPipelinePolicy {
        store: store.clone(),
    }));

    // Shutdown fan-out: the signal task flips this once, and every long-lived handler watches it.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Readiness (issue #75): the engine, stores, and background pipeline are wired; flip to ready
    // just before we start accepting so `/readyz` only goes 200 once the server can actually serve.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let state = AppState {
        lib: lib.clone(),
        store: store.clone(),
        bind: addr.to_string(),
        localhost_only,
        tls,
        shutdown: shutdown_rx,
        ready: ready.clone(),
    };
    let app = build_router(state);

    // 3. Bind + log the posture so an operator sees "am I safe to expose?" at a glance (tech-spec 15).
    // Bind a std listener up front so the actual addr (and any :0 port) is known for logging, then
    // hand it to whichever server the TLS posture selects.
    let std_listener = std::net::TcpListener::bind(addr)?;
    std_listener.set_nonblocking(true)?;
    let actual = std_listener.local_addr()?;
    let scheme = if tls { "https" } else { "http" };
    let s = store.status(&actual.to_string(), localhost_only, tls);
    tracing::info!(
        %actual, tls, scheme, auth = ?s.auth, mcp = ?s.mcp, network_writes = s.network_writes,
        exposed_without_auth = s.exposed_without_auth, "3dam serve listening"
    );
    eprintln!(
        "3dam serve → {scheme}://{actual}  (tls: {tls}, auth: {:?}, mcp: {:?}, writes: {})  data: {}",
        s.auth,
        s.mcp,
        s.network_writes,
        cfg.data_dir.display()
    );
    if s.exposed_without_auth {
        eprintln!(
            "  ⚠ exposed beyond localhost with no auth and no TLS — set the authentication flag"
        );
    }
    ready.store(true, std::sync::atomic::Ordering::Relaxed);

    // TLS deployment: serve HTTPS via axum-server (its own graceful-shutdown handle), still flipping
    // the WS watch so idle sockets close. Plaintext keeps the axum::serve path below.
    if let Some((cert, key)) = tls_paths {
        return serve_tls(std_listener, app, cert, key, shutdown_tx, actual).await;
    }

    let listener = tokio::net::TcpListener::from_std(std_listener)?;
    // Graceful shutdown, in three moves:
    //   1. `wait_for_signal()` resolves on Ctrl-C / SIGTERM,
    //   2. we flip the watch so the WS loops send a Close frame and return (otherwise an idle
    //      browser tab would pin the drain open forever),
    //   3. axum stops accepting and drains in-flight requests. A grace timer is the backstop so a
    //      wedged connection can't hang the process — past the window we stop waiting and exit.
    let shutdown_rx_backstop = shutdown_tx.subscribe();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        wait_for_signal().await;
        tracing::info!("shutdown signal received; draining connections");
        eprintln!(
            "3dam serve → shutting down (draining connections, ≤{}s)",
            SHUTDOWN_GRACE.as_secs()
        );
        let _ = shutdown_tx.send(true);
    });
    tokio::select! {
        res = serve => {
            res?;
            tracing::info!("shutdown complete; all connections drained");
        }
        _ = force_after_grace(shutdown_rx_backstop) => {
            tracing::warn!(
                grace = ?SHUTDOWN_GRACE,
                "grace period elapsed with connections still open; exiting anyway"
            );
        }
    }
    Ok(())
}

/// Serve HTTPS from a PEM cert/key over an already-bound listener (issue #75). Uses `axum-server`'s
/// rustls integration and its `Handle` for graceful shutdown, mirroring the plaintext path: on a
/// stop signal we flip the WS watch (so idle sockets close) then drain within [`SHUTDOWN_GRACE`].
async fn serve_tls(
    std_listener: std::net::TcpListener,
    app: Router,
    cert: PathBuf,
    key: PathBuf,
    shutdown_tx: watch::Sender<bool>,
    actual: SocketAddr,
) -> anyhow::Result<()> {
    // rustls 0.23 needs a process-level crypto provider; install ring's (idempotent — ignore if a
    // provider is already set, e.g. by a linked reqwest).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to load TLS cert '{}' / key '{}': {e}",
                cert.display(),
                key.display()
            )
        })?;

    let handle = axum_server::Handle::new();
    let handle_for_signal = handle.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("shutdown signal received; draining TLS connections");
        eprintln!(
            "3dam serve → shutting down (draining connections, ≤{}s)",
            SHUTDOWN_GRACE.as_secs()
        );
        let _ = shutdown_tx.send(true); // close idle WS loops
        handle_for_signal.graceful_shutdown(Some(SHUTDOWN_GRACE));
    });

    axum_server::from_tcp_rustls(std_listener, config)
        .handle(handle)
        .serve(app.into_make_service())
        .await?;
    tracing::info!(%actual, "TLS shutdown complete; all connections drained");
    Ok(())
}

/// How long the graceful drain may run before we stop waiting on stragglers and exit.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Resolves only once shutdown has begun *and* the grace window has then elapsed — the backstop that
/// bounds the drain. Idle until the watch flips, so it never fires during normal operation.
async fn force_after_grace(mut shutdown: watch::Receiver<bool>) {
    // `wait_for` returns immediately if the value is already `true`, else awaits the flip.
    if shutdown.wait_for(|v| *v).await.is_err() {
        // Sender dropped without signalling — nothing to bound; idle forever so `serve` wins.
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(SHUTDOWN_GRACE).await;
}

/// Resolve when the OS asks us to stop. Ctrl-C (SIGINT) on every platform, **plus** SIGTERM on Unix —
/// what `systemd`, `docker stop`, and a bare `kill` send. Without the SIGTERM arm those would bypass
/// the drain and hard-kill the process mid-request.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not install SIGTERM handler; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The MCP stdio transport (`3dam mcp`, tech-spec 11 §2.2). Opens an embedded engine — no network,
/// no running server, no auth surface — and serves the identical adapter over stdio. Locally trusted:
/// writes are allowed (still non-destructive). Unaffected by the served `McpServer` flag (§7).
pub async fn mcp_stdio(data_dir: PathBuf) -> anyhow::Result<()> {
    let lib = Arc::new(EmbeddedLibrary::open(&data_dir).await?);
    lib.start_watchers(); // long-running role: resume auto-rescan for watch-enabled sources.
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

/// Liveness probe (issue #75): the process is up and the axum stack is answering. Always `200 ok`
/// once the listener is bound — deliberately does no work, so it never false-negatives under load.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness probe (issue #75): `200 ready` once the engine, stores, and background job pipeline are
/// fully wired; `503` before that. A load balancer should route traffic only when this returns 200.
async fn readyz(State(st): State<AppState>) -> Response {
    if st.ready.load(std::sync::atomic::Ordering::Relaxed) {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting").into_response()
    }
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

/// Build a binary asset response: `Content-Type` from the payload, plus a private cache directive.
fn content_response(content: AssetContent, cache_control: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content.content_type),
            (header::CACHE_CONTROL, cache_control.to_string()),
        ],
        Body::from(content.bytes),
    )
        .into_response()
}

async fn asset_content(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_content(&ctx, &id).await?;
    Ok(content_response(content, "private, max-age=60"))
}

async fn asset_related(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<RelatedQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_related_content(&ctx, &id, &q.path).await?;
    Ok(content_response(content, "private, max-age=60"))
}

/// Serve the interactive 3D preview mesh (`DMSH` blob) the WASM viewer island uploads directly.
/// Generated + cached server-side from the same Assimp decode as the turntable thumbnail, so it
/// covers the full professional format range with textures. `Unsupported` (415) for non-models.
async fn asset_preview_mesh(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_model_preview(&ctx, &id).await?;
    Ok(content_response(content, "private, max-age=300"))
}

#[derive(serde::Deserialize)]
struct RelatedQuery {
    /// The glTF-relative URI of the sibling file (`.bin` / texture), resolved against the asset dir.
    path: String,
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
    Ok(content_response(content, "private, max-age=300"))
}

#[derive(serde::Deserialize)]
struct ThumbQuery {
    edge: Option<u32>,
}

#[derive(serde::Deserialize)]
struct StatsQuery {
    /// Scope the aggregates to one source; a federated source proxies to the peer (phase 6).
    source: Option<dam_api::id::SourceId>,
}

async fn stats(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Query(q): Query<StatsQuery>,
) -> Result<Json<LibraryStats>, ApiError> {
    Ok(Json(st.lib.library_stats(&ctx, q.source).await?))
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

async fn regenerate_thumbnails(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ThumbnailRegenRequest>,
) -> Result<Json<ThumbnailRegenReport>, ApiError> {
    Ok(Json(st.lib.regenerate_thumbnails(&ctx, req).await?))
}

async fn find_similar(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<SimilarRequest>,
) -> Result<Json<dam_api::page::Page<SimilarHit>>, ApiError> {
    Ok(Json(st.lib.find_similar(&ctx, req).await?))
}

/// Peer self-description (phase 6, issue #39): federation protocol version, catalog weight, and the
/// embedding space per media type — what a caller needs to register this instance as a federated
/// source and gate cross-peer similarity (issue #40). 404 while the `federation` flag is off.
async fn advertise(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<dam_api::PeerAdvertise>, ApiError> {
    if !st.store.federation() {
        return Err(ApiError(LibError::NotFound(
            "federation is disabled".into(),
        )));
    }
    let stats = st.lib.library_stats(&ctx, None).await?;
    Ok(Json(dam_api::PeerAdvertise {
        protocol_version: dam_api::FEDERATION_PROTOCOL_VERSION.to_string(),
        instance: st.bind.clone(),
        assets: stats.total,
        spaces: st.lib.embedding_spaces(),
    }))
}

/// The federated form of `/similar`: rank a caller-supplied vector against this instance's own
/// index (issue #40). Part of the federation surface, so it shares the flag gate.
async fn similar_by_vector(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<dam_api::VectorSimilarRequest>,
) -> Result<Json<dam_api::page::Page<SimilarHit>>, ApiError> {
    if !st.store.federation() {
        return Err(ApiError(LibError::NotFound(
            "federation is disabled".into(),
        )));
    }
    Ok(Json(st.lib.find_similar_by_vector(&ctx, req).await?))
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

async fn set_favorite(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<FavoriteRequest>,
) -> Result<StatusCode, ApiError> {
    st.lib.set_favorite(&ctx, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Prefetch hint (issue #72): a read-scoped client names the assets it's about to render; the server
/// warms their thumbnails + preview meshes so the following HTTP GETs are cache hits. Fire-and-forget
/// — returns as soon as the warm is scheduled, not when it finishes.
async fn prefetch(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<PrefetchRequest>,
) -> Result<StatusCode, ApiError> {
    st.lib.prefetch(&ctx, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sources(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<SourceInfo>>, ApiError> {
    Ok(Json(st.lib.list_sources(&ctx).await?))
}

async fn list_folders(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<FolderListing>,
) -> Result<Json<Vec<FolderEntry>>, ApiError> {
    Ok(Json(st.lib.list_folders(&ctx, req).await?))
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

async fn remove_asset(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RemoveAsset>,
) -> Result<StatusCode, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    st.lib.remove_asset(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_blocklist(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<BlockEntry>>, ApiError> {
    Ok(Json(st.lib.list_blocklist(&ctx).await?))
}

async fn unblock(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(hash): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let hash = ContentHash::from_hex(&hash)
        .ok_or_else(|| ApiError(LibError::BadRequest("invalid content hash".into())))?;
    st.lib.unblock(&ctx, &hash).await?;
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

/// Auth for the WebSocket firehose: a browser can't set the `Authorization` header on a WS, so we
/// also accept the bearer secret as `?token=` (issue #74).
#[derive(serde::Deserialize)]
struct WsAuthQuery {
    #[serde(default)]
    token: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<WsAuthQuery>,
    headers: HeaderMap,
    State(st): State<AppState>,
) -> Response {
    if let Err(e) = auth::resolve_ws(&st.store, &headers, q.token) {
        return ApiError(e).into_response();
    }
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

async fn ws_loop(mut socket: WebSocket, st: AppState) {
    let mut stream = match st.lib.subscribe(&ectx(), SubscribeRequest::default()).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut shutdown = st.shutdown.clone();
    loop {
        tokio::select! {
            // Server is stopping: send a courteous Close frame and let the drain complete.
            _ = shutdown.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            ev = stream.next() => {
                let Some(ev) = ev else { break }; // event bus closed (engine shutting down)
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
