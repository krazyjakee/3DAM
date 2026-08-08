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
mod auth_rate;
mod authn;
mod comments;
mod config;
mod http;
mod mcp;
mod oidc;
mod oidc_bearer;
mod runtime;
mod static_assets;
mod store;
mod tickets;
mod upload;
mod ws;

use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, Extensions, HeaderMap, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_api::dto::*;
use dam_api::id::AssetId;
use dam_api::service::LibraryService;
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;
use tower_http::compression::predicate::{Predicate, SizeAbove};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use auth::{Reader, Writer};
pub use config::ServeFile;
pub use mcp::{McpAdapter, WriteGate};
pub use runtime::{mcp_stdio, serve, serve_desktop, DesktopServer};
use static_assets::static_handler;
pub use store::ServerStore;
use tickets::{
    empty_media_tickets, empty_ws_tickets, mint_media_ticket, mint_ws_ticket, MediaTicket, WsTicket,
};
use ws::ws_handler;

const WORKER_CSP: &str = "worker-src 'none'";

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
    /// Force `Secure` on session/CSRF cookies regardless of the local TLS posture — the operator's
    /// declaration that a TLS-terminating proxy sits in front (`[server] secure_cookies`).
    pub secure_cookies: bool,
    /// Refuse the *open* (loopback-peer) first-run claim path outright, so only bootstrap-token
    /// redemption can claim (`[accounts] require_claim_token`; ADR 0014).
    pub require_claim_token: bool,
    /// Process-local request and expensive-work bounds for the unauthenticated auth surfaces.
    pub auth_protection: Arc<auth_rate::AuthProtection>,
    /// Single-entry, bounded OIDC metadata/JWKS cache for stateless bearer verification.
    pub oidc_bearer: Arc<oidc_bearer::Verifier>,
    /// Flips `false → true` once when shutdown begins, so long-lived handlers (the `/api/v1/ws`
    /// loop) can stop awaiting and close cleanly instead of pinning the graceful drain open.
    pub shutdown: watch::Receiver<bool>,
    /// Readiness for `/readyz` (issue #75): `false` until the engine, stores, and background job
    /// pipeline are fully wired, then `true`. A load balancer routes traffic only once this holds.
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    /// Per-file upload ceiling in bytes (`[upload] max_file_mb`, issue #80).
    pub max_upload_bytes: u64,
    /// Server-owned job outputs which may be retrieved through the authenticated artifact route.
    /// No request path is ever resolved relative to this directory; download targets come only
    /// from a visible completed job's structured result.
    pub artifacts_dir: Arc<PathBuf>,
    /// One-use, 30-second WebSocket handshakes. Keys are hashes so even a process diagnostic never
    /// contains the browser-visible ticket. Each value carries the already-resolved context plus
    /// the hidden parent credential needed to re-resolve visibility on a long-lived connection.
    ws_tickets: Arc<std::sync::Mutex<HashMap<[u8; 32], WsTicket>>>,
    /// Renewable range-stream tickets for audio/video. They are exact-target bound and retain the
    /// parent credential only server-side so every use observes token/session revocation.
    media_tickets: Arc<std::sync::Mutex<HashMap<[u8; 32], MediaTicket>>>,
}

fn managed_artifacts_dir(lib: &EmbeddedLibrary) -> PathBuf {
    // `scratch` is always the data directory's direct child (dam-core's public transport staging
    // contract). Keeping artifacts as a sibling means they survive scratch cleanup and remain on
    // the user-selected real disk rather than an OS tmpfs.
    lib.scratch_dir()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("artifacts")
}

/// The audit actor string for a request (tech-spec 10 §4.5). A session records as
/// `account:<username>`, a stateless provider credential as `oidc-bearer:<username>`, a
/// store-verified token as `token:<label>`, and an unauthenticated localhost owner (under
/// `Authentication = Off`) as `local-owner`. The prefixes keep attribution unambiguous without
/// retaining a token, session id, or OIDC subject.
///
/// Lives here rather than in one route module because every module that audits needs the identical
/// string: the log's value is that an actor is greppable across `admin.*`, `comment.*`, and
/// `source.upload` alike, which stops being true the moment two copies disagree.
pub(crate) fn actor_of(ctx: &dam_api::service::AuthContext) -> String {
    use dam_api::service::AuthSource;
    match ctx.auth_source {
        AuthSource::Session => ctx
            .account
            .as_ref()
            .map(|account| format!("account:{}", account.username))
            .unwrap_or_else(|| "account:unknown".into()),
        AuthSource::OidcBearer => ctx
            .account
            .as_ref()
            .map(|account| format!("oidc-bearer:{}", account.username))
            .unwrap_or_else(|| "oidc-bearer:unknown".into()),
        AuthSource::ApiToken => ctx
            .identity
            .as_ref()
            .map(|label| format!("token:{label}"))
            .unwrap_or_else(|| "token:unknown".into()),
        AuthSource::Embedded => "embedded".into(),
        AuthSource::LocalOwner => "local-owner".into(),
        AuthSource::Anonymous => "anonymous".into(),
    }
}

/// Full-scope context for the in-process engine call (the engine trusts its caller; the boundary is
/// guarded by the auth extractors before we get here).
pub(crate) fn ectx() -> dam_api::service::AuthContext {
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
        let retry_after = match &self.0 {
            LibError::RateLimited { retry_after } => Some(*retry_after),
            _ => None,
        };
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = (status, Json(self.0.to_body())).into_response();
        // A 401 SHOULD advertise the scheme it wants (RFC 7235 §3.1). We use bearer tokens, so say
        // so — tools and generic HTTP clients key off this header, and it costs nothing.
        if status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        // The typed body is useful to 3DAM clients; the standard header lets browsers, proxies,
        // generic HTTP clients, and operators apply the same backoff without parsing JSON.
        if let Some(seconds) = retry_after {
            if let Ok(value) = header::HeaderValue::from_str(&seconds.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        resp
    }
}

fn parse_id<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, ApiError> {
    s.parse::<T>()
        .map_err(|_| ApiError(LibError::BadRequest(format!("invalid {what} id"))))
}

/// Wrap a sub-router in the **one** `UserAccounts` flag gate (ADR 0004: off *unmounts* the route —
/// a 404, not a 403, decided before auth so a disabled surface cannot be probed). Both gated blocks
/// — all of `/api/v1/auth` and the accounts/groups/shares block of `/admin/api` — pass through here,
/// so the check exists once instead of being pasted into eighteen handler bodies where the
/// nineteenth would forget it.
pub(crate) fn gate_accounts(router: Router<AppState>, state: AppState) -> Router<AppState> {
    router.route_layer(axum::middleware::from_fn_with_state(state, accounts_gate))
}

async fn accounts_gate(
    State(st): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match st.store.require_user_accounts() {
        Ok(()) => next.run(req).await,
        Err(e) => ApiError(e).into_response(),
    }
}

/// Assemble the router over a fully-built [`AppState`].
pub(crate) fn build_router(state: AppState) -> Router {
    // Compression remains a body stream: tower-http encodes frames as handlers produce them and
    // never collects a large query/duplicates result just to compress it. Its response wrapper also
    // refuses Content-Range and pre-encoded responses. The MIME allow-list below is the final guard
    // against spending CPU on media, thumbnails, fonts, archives, and opaque binary payloads.
    let asset_routes = http::assets::routes();
    let source_routes = http::sources::routes();
    let collection_routes = http::collections::routes();
    let job_routes = http::jobs::routes();

    Router::new()
        .route("/api/version", get(version))
        // Ops health probes (issue #75), unauthenticated + distinct from the versioned API so a load
        // balancer / systemd watchdog can poll them without a token: liveness vs readiness.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/query", post(query))
        .route(
            "/api/v1/assets/{id}",
            source_routes.with_remove_asset(get(get_asset)),
        )
        .route("/api/v1/assets/{id}/content", asset_routes.content)
        .route("/api/v1/assets/{id}/related", asset_routes.related)
        .route(
            "/api/v1/assets/{id}/preview-mesh",
            asset_routes.preview_mesh,
        )
        .route("/api/v1/assets/{id}/thumbnail", asset_routes.thumbnail)
        .route("/api/v1/assets/{id}/note", get(get_note).put(set_note))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/convert", post(convert))
        .route("/api/v1/similar", post(find_similar))
        // Federation surface (phase 6, issue #39): statically mounted, 404 while the `federation`
        // flag is off — the same route-level-guard mechanism as `/mcp` ("off ⇒ absent", ADR 0004).
        .route("/api/v1/advertise", get(advertise))
        .route("/api/v1/similar-by-vector", post(similar_by_vector))
        .route("/api/v1/duplicates", post(list_duplicates))
        .route("/api/v1/duplicates/review", post(review_duplicate))
        .route("/api/v1/duplicates/membership", post(duplicate_membership))
        .route(
            "/api/v1/duplicates/group-members",
            post(duplicate_group_members),
        )
        .route("/api/v1/assets/{id}/duplicates", get(duplicate_group))
        .route("/api/v1/suggestions/review", post(review_suggestion))
        .route("/api/v1/tags/edit", post(edit_tags))
        .route("/api/v1/tags/list", post(list_tags))
        .route("/api/v1/assets/license", post(set_license))
        .route("/api/v1/assets/favorite", post(set_favorite))
        .route("/api/v1/jobs/analyze", post(submit_analyze))
        .route("/api/v1/thumbnails/regenerate", post(regenerate_thumbnails))
        // Prefetch hint (issue #72): warm thumbnails/preview meshes ahead of the client's HTTP fetch.
        .route("/api/v1/prefetch", post(prefetch))
        .route("/api/v1/sources", source_routes.sources)
        .route("/api/v1/folders", source_routes.folders)
        .route("/api/v1/sources/{id}", source_routes.source)
        .route("/api/v1/collections", collection_routes.collections)
        .route("/api/v1/collections/{id}", collection_routes.collection)
        .route(
            "/api/v1/collections/{id}/members",
            collection_routes.members,
        )
        .route("/api/v1/collections/{id}/assets", collection_routes.assets)
        .route("/api/v1/export", collection_routes.export)
        .route("/api/v1/blocklist", source_routes.blocklist)
        .route("/api/v1/blocklist/{hash}", source_routes.unblock)
        .route("/api/v1/jobs/scan", job_routes.scan)
        .route("/api/v1/jobs/convert", job_routes.convert)
        .route("/api/v1/jobs/export", job_routes.export)
        .route("/api/v1/jobs/convert-artifact", job_routes.managed_convert)
        .route("/api/v1/jobs/export-artifact", job_routes.managed_export)
        .route("/api/v1/jobs/list", job_routes.list)
        .route("/api/v1/jobs/{id}", job_routes.get)
        .route("/api/v1/jobs/{id}/artifact", job_routes.artifact)
        .route("/api/v1/jobs/{id}/cancel", job_routes.cancel)
        .route("/api/v1/media-ticket", post(mint_media_ticket))
        .route("/api/v1/ws-ticket", post(mint_ws_ticket))
        .route("/api/v1/ws", get(ws_handler))
        // The MCP endpoint (tech-spec 11): present, but the handler 404s when the flag is Off.
        .route("/mcp", post(mcp_http))
        // User accounts: claim/login/sessions (phase 6, issue #42) — 404 while the flag is off.
        .merge(authn::routes(state.clone()))
        .merge(comments::routes(state.clone()))
        // The one write-into-source path (issue #80) — behind the `Upload` flag, so the route is
        // absent (404) until an operator turns uploads on. Past that gate the `Writer` extractor
        // still answers the per-*caller* question with a 401/403 that says why.
        .merge(upload::routes(state.clone()))
        // OIDC/OAuth2 login (issue #41) — behind the `Oidc` flag *and* `UserAccounts`, since a
        // verified subject resolves to an account or to nothing. Absent (404) until both are on.
        .merge(oidc::routes(state.clone()))
        // The admin API (tech-spec 10 §5), guarded by the AdminAuth extractor.
        .merge(admin::routes(state.clone()))
        // SPA fallback: any non-API GET serves the embedded web client (tech-spec 09 §A.4).
        .fallback(static_handler)
        // Record only the path, never the query. A WebSocket ticket is short-lived and one-use,
        // but defense in depth still keeps it out of debug spans and downstream diagnostics.
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<Body>| {
                tracing::debug_span!(
                    "request",
                    method = %request.method(),
                    uri = %trace_path(request.uri()),
                    version = ?request.version(),
                )
            }),
        )
        .layer(CorsLayer::permissive())
        // Media has no URL credential and WebSockets carry only a derived one-use ticket. Keep the
        // no-referrer policy as another boundary against future sensitive URL material.
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            header::HeaderValue::from_static("no-referrer"),
        ))
        // No worker may sit below the page's fetch boundary and inspect Authorization. This also
        // denies ServiceWorker registration; the Tauri shell additionally uses an incognito
        // renderer partition so a worker from a previous release cannot survive into this launch.
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(WORKER_CSP),
        ))
        .layer(response_compression())
        .with_state(state)
}

fn trace_path(uri: &Uri) -> &str {
    uri.path()
}

fn response_compression() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new()
        .br(true)
        .gzip(true)
        .compress_when(SizeAbove::new(256).and(compressible_response))
}

/// MIME policy for on-the-fly compression. This is intentionally an allow-list: new binary media
/// types remain cheap by default, while textual API/web formats and WASM opt in explicitly.
fn compressible_response(
    _status: StatusCode,
    _version: Version,
    headers: &HeaderMap,
    _extensions: &Extensions,
) -> bool {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    (content_type.starts_with("text/") && content_type != "text/event-stream")
        || matches!(
            content_type.as_str(),
            "application/json"
                | "application/javascript"
                | "application/x-javascript"
                | "application/wasm"
                | "application/xml"
                | "application/graphql-response+json"
                | "image/svg+xml"
        )
        || content_type.ends_with("+json")
        || content_type.ends_with("+xml")
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
    let artifacts_dir = managed_artifacts_dir(&lib);
    build_router(AppState {
        lib,
        store,
        bind,
        localhost_only,
        tls: false,
        secure_cookies: false,
        require_claim_token: false,
        auth_protection: Arc::new(auth_rate::AuthProtection::new([])),
        oidc_bearer: Arc::new(oidc_bearer::Verifier::new()),
        shutdown,
        // The test seam is ready the moment it's built (no async pipeline warm-up to await).
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        max_upload_bytes: crate::config::UploadBlock::default().max_bytes(),
        artifacts_dir: Arc::new(artifacts_dir),
        ws_tickets: empty_ws_tickets(),
        media_tickets: empty_media_tickets(),
    })
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn version(State(st): State<AppState>) -> Json<serde_json::Value> {
    let mut capabilities = vec![
        "query",
        "sources",
        "scan",
        "stats",
        "ws",
        "web",
        "thumbnail",
        "convert",
        "analyze",
        "similar",
        "duplicates",
        "suggestions",
        "collections",
        "export",
        "auth",
        "flags",
        "mcp",
        "accounts",
    ];
    // Video decode is a *discovered* ffmpeg, not a compiled-in feature (ADR 0015), so whether this
    // server can describe a video or render its poster frame is a property of the machine it runs
    // on — something no client can infer from the build. Advertising it here lets the UI say
    // "no decoder installed" instead of leaving an empty tile that reads as a bug.
    if dam_core::video_probe_available() {
        capabilities.push("video_probe");
    }
    Json(serde_json::json!({
        "api": "v1",
        "server": concat!("3dam ", env!("CARGO_PKG_VERSION")),
        // The auth posture, on the one public route: lets a client render its login gate up front
        // instead of provoking 401s (the same fact a 401 would reveal anyway). The *effective*
        // mode — `UserAccounts` on raises `Off` to `Token` (issue #42).
        "auth": st.store.effective_auth_mode(),
        // Accounts posture (issue #42): lets the client offer username/password login (and the
        // first-run claim screen while unclaimed) instead of the bare token prompt.
        "accounts": st.store.user_accounts(),
        // Upload posture (issue #80): the route is absent while the flag is off, so the client hides
        // its Upload view rather than offering one whose every request 404s. Reported here for the
        // same reason `accounts` is — a capability the build has but this deployment may not run.
        "upload": st.store.upload(),
        // OIDC posture (issue #41): whether this deployment offers a "sign in with…" button. True
        // only when the login surface would actually answer — the flag *and* accounts *and* a
        // configured provider — because a button that leads to a 404 or a "no provider configured"
        // error is worse than no button. Deliberately says nothing about *which* provider: this
        // endpoint is unauthenticated, and the issuer can name an organisation.
        "oidc": st.store.oidc()
            && st.store.user_accounts()
            && st.store.oidc_config_info().ok().flatten().is_some(),
        "unclaimed": st.store.unclaimed(),
        // How many accounts exist. `unclaimed` alone can't distinguish "brand new instance" from
        // "the operator re-opened the claim window to recover a lost admin" — and in the second
        // case every existing user can still sign in, so the client must not replace the login
        // screen with a claim form. Reveals only a cardinality the claim screen itself implies.
        "account_count": st.store.count_accounts().unwrap_or(0),
        "capabilities": capabilities,
    }))
}

/// `GET /api/v1/whoami` — the caller's resolved identity + effective scopes (tech-spec 10 §1.2).
/// Deliberately requires no scope of its own: it reports whatever the presented credential resolves
/// to, so a client can shape its UI to the granted scopes. Under `Token` mode with no credential it
/// still `401`s (via `resolve`), which is the signal to show a login gate.
async fn whoami(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<dam_api::WhoAmI>, ApiError> {
    let resolved = auth::resolve_request(
        &st,
        auth::bearer_header(&headers),
        auth::cookie_value(&headers, auth::SESSION_COOKIE),
    )
    .await?;
    Ok(Json(resolved.ctx.whoami()))
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
    Query(q): Query<OwnerQuery>,
) -> Result<Json<Asset>, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    Ok(Json(st.lib.get_asset_from(&ctx, &id, q.source).await?))
}

#[derive(serde::Deserialize)]
struct OwnerQuery {
    /// Locally-issued federated source id from a query/detail result. It is resolved only against
    /// the server's registry and can never select a caller-controlled endpoint.
    source: Option<dam_api::id::SourceId>,
}

/// Build a binary asset response: `Content-Type` from the payload, plus a private cache directive.
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
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
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
) -> Result<Json<dam_api::Page<DupGroup>>, ApiError> {
    Ok(Json(st.lib.list_duplicates(&ctx, req).await?))
}

async fn duplicate_membership(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<DupMembershipRequest>,
) -> Result<Json<Vec<DupMembership>>, ApiError> {
    Ok(Json(st.lib.duplicate_membership(&ctx, req).await?))
}

async fn duplicate_group_members(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<DupGroupMembersRequest>,
) -> Result<Json<dam_api::Page<DupMember>>, ApiError> {
    Ok(Json(st.lib.duplicate_group_members(&ctx, req).await?))
}

async fn review_duplicate(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<DupReviewRequest>,
) -> Result<StatusCode, ApiError> {
    st.lib.review_duplicate(&ctx, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn duplicate_group(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Option<DupGroup>>, ApiError> {
    let id = parse_id(&id, "asset")?;
    Ok(Json(st.lib.duplicate_group(&ctx, &id).await?))
}

async fn review_suggestion(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<SuggestionReview>,
) -> Result<StatusCode, ApiError> {
    st.lib.review_suggestion(&ctx, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn edit_tags(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<TagEditRequest>,
) -> Result<Json<TagEditResult>, ApiError> {
    Ok(Json(st.lib.edit_tags(&ctx, req).await?))
}

async fn list_tags(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<TagListRequest>,
) -> Result<Json<Vec<TagInfo>>, ApiError> {
    Ok(Json(st.lib.list_tags(&ctx, req).await?))
}

/// Bulk rights edit (issue #106). `Writer` because a licence is a claim about how the asset may be
/// used — the same write gate as any other metadata mutation (scope + network-writes flag + CSRF).
/// Summary-shaped: the result carries counts and bounded warnings, never one row per target.
async fn set_license(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<SetLicenseRequest>,
) -> Result<Json<LicenseEditResult>, ApiError> {
    Ok(Json(st.lib.set_license(&ctx, req).await?))
}

async fn set_favorite(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<FavoriteRequest>,
) -> Result<StatusCode, ApiError> {
    st.lib.set_favorite(&ctx, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The asset's note (issue #81). `null` when there isn't one — an absent note is a normal state,
/// not a 404, so a client can render the empty editor without special-casing an error.
async fn get_note(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Option<Note>>, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    Ok(Json(st.lib.get_note(&ctx, &id).await?))
}

/// Set or clear the asset's note. Returns the stored value so the client's "saved" state comes from
/// what the server actually kept (including the trim), not from what it optimistically sent.
async fn set_note(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<NoteRequest>,
) -> Result<Json<Option<Note>>, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    Ok(Json(st.lib.set_note(&ctx, &id, req).await?))
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

/// `POST /mcp` — the Streamable-HTTP MCP transport (tech-spec 11 §2.1). The `McpServer` flag is read
/// live: `Off` ⇒ `404` (route absent, checked *before* auth so a disabled endpoint cannot even be
/// probed). Otherwise auth is resolved on the shared surface and one JSON-RPC message is dispatched.
///
/// A cookie-authenticated call passes the same CSRF gate as any other write: this is a POST that can
/// reach write tools, and while `SameSite=Strict` already stops a cross-site cookie from riding
/// along, the double-submit token is the second fence (ADR 0009 §4). Bearer-credentialled agents —
/// the normal MCP client — are unaffected.
async fn mcp_http(State(st): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    use dam_api::admin::McpMode;
    let mcp = st.store.mcp_mode();
    if matches!(mcp, McpMode::Off) {
        return ApiError(LibError::NotFound("mcp is disabled".into())).into_response();
    }
    let resolved = match auth::resolve_request(
        &st,
        auth::bearer_header(&headers),
        auth::cookie_value(&headers, auth::SESSION_COOKIE),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return ApiError(e).into_response(),
    };
    if let Some(sess) = &resolved.session {
        let presented = headers.get(auth::CSRF_HEADER).and_then(|v| v.to_str().ok());
        if presented != Some(sess.csrf.as_str()) {
            return ApiError(LibError::Forbidden(
                "missing or invalid CSRF token (send the login csrf in x-dam-csrf)".into(),
            ))
            .into_response();
        }
    }
    let ctx = resolved.ctx;
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

#[cfg(test)]
mod tests {
    use super::http::{managed_path_belongs_to, parse_range, safe_archive_name, RangeSpec};
    use super::{response_compression, trace_path, WORKER_CSP};
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn sat(first: u64, last: u64) -> RangeSpec {
        RangeSpec::Satisfiable(first, last)
    }

    #[test]
    fn archive_names_are_portable_single_components() {
        assert_eq!(safe_archive_name("mesh.bin".as_ref()).unwrap(), "mesh.bin");
        for unsafe_name in [
            "..",
            ".",
            "../secret",
            "..\\secret",
            "dir/file",
            "dir\\file",
        ] {
            assert!(
                safe_archive_name(unsafe_name.as_ref()).is_err(),
                "{unsafe_name}"
            );
        }
    }

    #[test]
    fn managed_result_decoration_rejects_lexical_parent_components() {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("artifacts");
        std::fs::create_dir_all(artifacts.join("export")).unwrap();
        assert!(managed_path_belongs_to(
            &artifacts,
            &artifacts.join("export/manifest.json")
        ));
        assert!(!managed_path_belongs_to(
            &artifacts,
            &artifacts.join("export/../manifest.json")
        ));
    }

    #[test]
    fn credential_material_is_absent_from_traced_targets() {
        let sentinel = "dam_XSS_SENTINEL_DO_NOT_LOG";
        let uri: axum::http::Uri =
            format!("/api/v1/ws?ticket=dam_ws_short&token={sentinel}&diagnostic={sentinel}")
                .parse()
                .unwrap();
        assert_eq!(trace_path(&uri), "/api/v1/ws");
        assert!(!trace_path(&uri).contains(sentinel));
    }

    #[test]
    fn worker_policy_denies_service_worker_registration() {
        assert_eq!(WORKER_CSP, "worker-src 'none'");
    }

    #[tokio::test]
    async fn preencoded_body_is_not_recompressed() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let mut response = (
                        [(header::CONTENT_TYPE, "application/json")],
                        vec![b'x'; 1024],
                    )
                        .into_response();
                    response.headers_mut().insert(
                        header::CONTENT_ENCODING,
                        header::HeaderValue::from_static("gzip"),
                    );
                    response
                }),
            )
            .layer(response_compression());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), vec![b'x'; 1024]);
    }

    /// RFC 9110 §14.1.2 range forms, plus the ones a `<video>` element actually sends when it
    /// seeks. Getting these wrong is not a subtle bug — a browser that asks for `bytes=500-` and
    /// gets the whole file back either replays from the start or refuses to scrub.
    #[test]
    fn parses_the_range_forms_a_video_element_sends() {
        // Opening probe: some bytes from the front.
        assert_eq!(parse_range("bytes=0-1023", 4096), sat(0, 1023));
        // Seek: open-ended from an offset.
        assert_eq!(parse_range("bytes=500-", 4096), sat(500, 4095));
        // Suffix: the last N bytes (MP4 `moov` atom at the tail).
        assert_eq!(parse_range("bytes=-500", 4096), sat(3596, 4095));
        // A suffix longer than the file clamps to the whole file rather than underflowing.
        assert_eq!(parse_range("bytes=-9999", 100), sat(0, 99));
        // An end past EOF clamps to the last byte.
        assert_eq!(parse_range("bytes=0-99999", 100), sat(0, 99));
        // Whole file, explicitly.
        assert_eq!(parse_range("bytes=0-", 10), sat(0, 9));
        // Single byte.
        assert_eq!(parse_range("bytes=5-5", 10), sat(5, 5));
        // Tolerate the optional whitespace the grammar allows.
        assert_eq!(parse_range(" bytes=0-1 ", 10), sat(0, 1));
    }

    /// The distinction RFC 9110 §14.2 draws and that a single `Option` cannot express: a *valid*
    /// range past the end of the representation is a 416; a spec we cannot parse must be ignored
    /// (answered with the whole representation), because 416ing a typo strands a client that would
    /// have been perfectly happy with a 200.
    #[test]
    fn separates_unsatisfiable_from_malformed_ranges() {
        // Valid syntax, entirely past the end — the caller turns this into a 416.
        assert_eq!(parse_range("bytes=100-200", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=100-", 100), RangeSpec::Unsatisfiable);
        // A zero-length suffix is well-formed and requests nothing.
        assert_eq!(parse_range("bytes=-0", 100), RangeSpec::Unsatisfiable);
        // Any range against an empty representation. Non-panicking: every offset saturates.
        assert_eq!(parse_range("bytes=0-", 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), RangeSpec::Unsatisfiable);

        // Malformed specs are ignored, not rejected.
        assert_eq!(parse_range("bytes=50-10", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=abc-def", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=", 100), RangeSpec::Ignore);
        assert_eq!(parse_range("", 100), RangeSpec::Ignore);
        // Not a byte range unit at all.
        assert_eq!(parse_range("items=0-9", 100), RangeSpec::Ignore);
        // Overflows a u64 rather than wrapping into a plausible offset.
        assert_eq!(
            parse_range("bytes=99999999999999999999999-", 100),
            RangeSpec::Ignore
        );
        // Multi-range: legal to decline, so we fall back to the full representation.
        assert_eq!(parse_range("bytes=0-9,20-29", 100), RangeSpec::Ignore);
    }
}
