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
use axum::http::{header, Extensions, HeaderMap, Method, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use dam_api::dto::*;
use dam_api::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use dam_api::service::LibraryService;
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;
use tokio_util::io::ReaderStream;
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
    Router::new()
        .route("/api/version", get(version))
        // Ops health probes (issue #75), unauthenticated + distinct from the versioned API so a load
        // balancer / systemd watchdog can poll them without a token: liveness vs readiness.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/whoami", get(whoami))
        .route("/api/v1/query", post(query))
        .route("/api/v1/assets/{id}", get(get_asset).delete(remove_asset))
        .route("/api/v1/assets/{id}/content", get(asset_content))
        .route("/api/v1/assets/{id}/related", get(asset_related))
        .route("/api/v1/assets/{id}/preview-mesh", get(asset_preview_mesh))
        .route("/api/v1/assets/{id}/thumbnail", get(asset_thumbnail))
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
        .route("/api/v1/jobs/convert", post(submit_convert))
        .route("/api/v1/jobs/export", post(submit_export))
        .route(
            "/api/v1/jobs/convert-artifact",
            post(submit_managed_convert),
        )
        .route("/api/v1/jobs/export-artifact", post(submit_managed_export))
        .route("/api/v1/jobs/list", post(list_jobs))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route(
            "/api/v1/jobs/{id}/artifact",
            get(download_job_artifact).head(download_job_artifact),
        )
        .route("/api/v1/jobs/{id}/cancel", post(cancel_job))
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

/// What a `Range` header resolves to against a known length — the three outcomes RFC 9110 §14.2
/// distinguishes, which are *not* two: "I can't parse this" and "this asks for bytes you don't
/// have" get different answers, and conflating them 416s a client that only sent us a typo.
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    /// A valid single range that overlaps the representation: serve `206` with these bounds.
    Satisfiable(u64, u64),
    /// Syntactically valid but starting past the end of the representation: `416`, the one case
    /// the status is for.
    Unsatisfiable,
    /// Unparsable (`bytes=abc-def`, `bytes=50-10`, an overflowing number), or a form we don't
    /// implement. RFC 9110 requires an unsatisfiable-*looking* but invalid spec to be **ignored**
    /// — "a server MUST ignore a Range header field that contains a range unit it does not
    /// understand" — so these fall through to the full `200`.
    Ignore,
}

/// Resolve a single byte range against a known length.
///
/// Only the single-range form is supported. Multi-range (`bytes=0-99,200-299`) requires a
/// `multipart/byteranges` body, is not used by any media element, and is explicitly optional in
/// RFC 9110 §14.2 — a server may answer it with the whole representation, which is what
/// [`RangeSpec::Ignore`] does.
///
/// Total: every arithmetic path saturates, so a zero-length representation or an absurd offset
/// returns an answer rather than panicking.
fn parse_range(spec: &str, len: u64) -> RangeSpec {
    let Some(spec) = spec.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (start, end) = (start.trim(), end.trim());
    let (first, last) = if start.is_empty() {
        // Suffix form `bytes=-N`: the final N bytes. `-0` asks for nothing, which is valid syntax
        // and cannot be satisfied.
        let Ok(n) = end.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        if n == 0 {
            return RangeSpec::Unsatisfiable;
        }
        (len.saturating_sub(n), len.saturating_sub(1))
    } else {
        let Ok(first) = start.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        let last = if end.is_empty() {
            len.saturating_sub(1)
        } else {
            let Ok(last) = end.parse::<u64>() else {
                return RangeSpec::Ignore;
            };
            // A last-byte-pos below first-byte-pos makes the spec invalid, not unsatisfiable.
            // Checked before clamping, so `bytes=150-140` against a short body is still a typo.
            if last < first {
                return RangeSpec::Ignore;
            }
            last.min(len.saturating_sub(1))
        };
        (first, last)
    };
    if len == 0 || first >= len {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Satisfiable(first, last)
}

/// Serve asset bytes with **range support**.
///
/// Range matters far more here than it did for images and meshes: a browser `<video>` element
/// issues a range request to seek, and several will refuse to show a scrub bar (or to play at all,
/// on Safari) against a server that answers `200` to every request. Video is served as original
/// bytes through this one route — there is no transcoding and no WASM island — so this is what
/// makes video preview work at all (PRODUCT_SPEC §9 phase 2b).
///
/// `Accept-Ranges: bytes` is advertised on every response, including the unranged `200`, so the
/// client knows seeking is available before it tries.
///
/// Bytes are pulled directly from the capability-confined local handle or the remote source range
/// API into a two-chunk producer window. Axum polls that stream as the socket accepts data; a
/// disconnect drops it, which stops the source loop instead of finishing a gigabyte transfer.
fn streamed_content_response(
    content: AssetContentStream,
    status: StatusCode,
    cache_control: &'static str,
) -> Response {
    let mut response = Response::new(Body::from_stream(content.bytes));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        content
            .metadata
            .content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&content.range.len().to_string()).unwrap(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                content.range.first(),
                content.range.last(),
                content.metadata.len
            ))
            .unwrap(),
        );
    }
    if let Some(etag) = content.metadata.etag {
        if let Ok(etag) = header::HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, etag);
        }
    }
    response
}

fn metadata_only_response(
    metadata: &AssetContentMetadata,
    status: StatusCode,
    cache_control: &'static str,
    content_range: Option<String>,
) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        metadata
            .content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(
            &if status == StatusCode::OK {
                metadata.len
            } else {
                0
            }
            .to_string(),
        )
        .unwrap(),
    );
    if let Some(value) = content_range {
        headers.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&value).unwrap(),
        );
    }
    if let Some(etag) = metadata.etag.as_deref() {
        if let Ok(etag) = header::HeaderValue::from_str(etag) {
            headers.insert(header::ETAG, etag);
        }
    }
    response
}

/// `If-Range` is deliberately strict. Only the strong content-hash tag emitted by this route can
/// prove the requested range belongs to the current representation; weak tags, dates, malformed
/// values, and assets without a hash all fall back to a complete `200` stream.
fn if_range_matches(headers: &HeaderMap, metadata: &AssetContentMetadata) -> bool {
    let Some(value) = headers.get(header::IF_RANGE) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    !value.starts_with("W/") && metadata.etag.as_deref() == Some(value.trim())
}

async fn asset_content(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<OwnerQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let metadata = st.lib.content_metadata_from(&ctx, &id, q.source).await?;
    const CACHE_CONTROL: &str = "private, max-age=60";

    // HEAD describes the complete selected representation. Range is defined for GET; more
    // importantly this branch never starts a source producer, so an availability probe is cheap.
    if method == Method::HEAD {
        return Ok(metadata_only_response(
            &metadata,
            StatusCode::OK,
            CACHE_CONTROL,
            None,
        ));
    }

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| if_range_matches(&headers, &metadata));
    let (status, range) = match range.map(|value| parse_range(value, metadata.len)) {
        Some(RangeSpec::Satisfiable(first, last)) => (
            StatusCode::PARTIAL_CONTENT,
            ContentRange::new(first, last).expect("a satisfiable range is ordered"),
        ),
        Some(RangeSpec::Unsatisfiable) => {
            return Ok(metadata_only_response(
                &metadata,
                StatusCode::RANGE_NOT_SATISFIABLE,
                CACHE_CONTROL,
                Some(format!("bytes */{}", metadata.len)),
            ));
        }
        Some(RangeSpec::Ignore) | None if metadata.len > 0 => (
            StatusCode::OK,
            ContentRange::new(0, metadata.len - 1).expect("non-empty full range is ordered"),
        ),
        Some(RangeSpec::Ignore) | None => {
            return Ok(metadata_only_response(
                &metadata,
                StatusCode::OK,
                CACHE_CONTROL,
                None,
            ));
        }
    };
    let content = st
        .lib
        .stream_content_from(&ctx, &id, range, q.source)
        .await?;
    Ok(streamed_content_response(content, status, CACHE_CONTROL))
}

async fn asset_related(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<RelatedQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st
        .lib
        .read_related_content_from(&ctx, &id, &q.path, q.source)
        .await?;
    Ok(content_response(content, "private, max-age=60"))
}

/// Serve the interactive 3D preview mesh (`DMSH` blob) the WASM viewer island uploads directly.
/// Generated + cached server-side from the same Assimp decode as the turntable thumbnail, so it
/// covers the full professional format range with textures. `Unsupported` (415) for non-models.
async fn asset_preview_mesh(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<OwnerQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_model_preview_from(&ctx, &id, q.source).await?;
    Ok(content_response(content, "private, max-age=300"))
}

#[derive(serde::Deserialize)]
struct RelatedQuery {
    /// The glTF-relative URI of the sibling file (`.bin` / texture), resolved against the asset dir.
    path: String,
    source: Option<dam_api::id::SourceId>,
}

async fn asset_thumbnail(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let edge = q.edge.unwrap_or(256);
    let content = st
        .lib
        .read_thumbnail_from(&ctx, &id, edge, q.source)
        .await?;
    Ok(content_response(content, "private, max-age=300"))
}

#[derive(serde::Deserialize)]
struct ThumbQuery {
    edge: Option<u32>,
    source: Option<dam_api::id::SourceId>,
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
    // Orphan-GC the cross-database soft references (issue #42): shares of a deleted source are
    // dead grants. This is a required durable cleanup; the resource mutation lives in the other
    // database, so a cleanup failure fails the request and is safe to retry.
    st.store.remove_shares_for_resource(
        dam_api::accounts::ShareResource::Source,
        &id.to_string(),
        "system",
    )?;
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
    // Orphan-GC shares pointing at the deleted collection (issue #42; see remove_source).
    st.store.remove_shares_for_resource(
        dam_api::accounts::ShareResource::Collection,
        &id.to_string(),
        "system",
    )?;
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
    // The engine/store seam intentionally knows nothing about accounts or token labels. Attribute
    // the durable history row here, where the authenticated actor is available. A failure is
    // returned rather than silently accepting an unattributed job.
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok(Json(serde_json::json!({ "job_id": job_id })))
}

async fn submit_convert(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ConvertRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let job_id = st.lib.submit_convert(&ctx, req).await?;
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

/// Allocate an opaque, server-owned output directory for a hosted convert. The browser submits no
/// filesystem path; completion is retrieved through `download_job_artifact`, which re-resolves the
/// visible job and validates its structured result against this root.
async fn submit_managed_convert(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ManagedConvertRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let output = allocate_managed_output(&st.artifacts_dir, "convert", None, true)?;
    let submitted = st
        .lib
        .submit_convert(
            &ctx,
            req.with_output_dir(output.to_string_lossy().into_owned()),
        )
        .await;
    let job_id = match submitted {
        Ok(job) => job,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&output);
            return Err(ApiError(error));
        }
    };
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

async fn submit_export(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let job_id = st.lib.submit_export(&ctx, req).await?;
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

/// Allocate a server-owned manifest destination. JSON/CSV land as one exact file; sidecars land in
/// a fresh directory and are packaged only when the authenticated user asks to download the
/// completed job. The public request intentionally cannot supply or influence the path.
async fn submit_managed_export(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ManagedExportRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let extension = match req.format {
        ExportFormat::Json => Some("json"),
        ExportFormat::Csv => Some("csv"),
        ExportFormat::Sidecar => None,
    };
    let output = allocate_managed_output(
        &st.artifacts_dir,
        "export",
        extension,
        matches!(req.format, ExportFormat::Sidecar),
    )?;
    let submitted = st
        .lib
        .submit_export(&ctx, req.with_output(output.to_string_lossy().into_owned()))
        .await;
    let job_id = match submitted {
        Ok(job) => job,
        Err(error) => {
            if output.is_dir() {
                let _ = std::fs::remove_dir_all(&output);
            } else {
                let _ = std::fs::remove_file(&output);
            }
            return Err(ApiError(error));
        }
    };
    st.lib.set_job_initiator(&job_id, actor_of(&ctx)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "job_id": job_id })),
    ))
}

fn allocate_managed_output(
    root: &std::path::Path,
    kind: &str,
    extension: Option<&str>,
    directory: bool,
) -> Result<PathBuf, ApiError> {
    let parent = root.join(kind);
    std::fs::create_dir_all(&parent).map_err(|error| {
        ApiError(LibError::Internal(format!(
            "create managed artifact directory: {error}"
        )))
    })?;
    // A fresh UUIDv7 is an allocation token, not the eventual job id. It makes the destination
    // unguessable enough to avoid collisions while the authenticated route still keys by job id.
    let token = JobId::new().to_string();
    let output = match extension {
        Some(extension) => parent.join(format!("{token}.{extension}")),
        None => parent.join(token),
    };
    if directory {
        std::fs::create_dir(&output).map_err(|error| {
            ApiError(LibError::Internal(format!(
                "create managed artifact output: {error}"
            )))
        })?;
    }
    Ok(output)
}

async fn get_job(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<JobStatus>, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    let mut job = st.lib.get_job(&ctx, &id).await?;
    decorate_managed_job(&st.artifacts_dir, &mut job);
    Ok(Json(job))
}

async fn list_jobs(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<JobListRequest>,
) -> Result<Json<dam_api::page::Page<JobStatus>>, ApiError> {
    let mut page = st.lib.list_jobs(&ctx, req).await?;
    for job in &mut page.items {
        decorate_managed_job(&st.artifacts_dir, job);
    }
    Ok(Json(page))
}

fn decorate_managed_job(root: &std::path::Path, job: &mut JobStatus) {
    if job.state != JobState::Done {
        return;
    }
    let Some(result) = job.result.as_deref_mut() else {
        return;
    };
    let (reported, downloadable, label) = match result {
        JobResult::Export(report) => (&report.output, true, "Download manifest"),
        JobResult::Convert(report) => (
            &report.output_dir,
            !report.dry_run && report.done > 0,
            "Download converted files",
        ),
    };
    if !managed_path_belongs_to(root, std::path::Path::new(reported)) {
        return;
    }
    let route = format!("/api/v1/jobs/{}/artifact", job.id);
    if downloadable
        && !job
            .result_artifacts
            .iter()
            .any(|artifact| artifact.route.as_deref() == Some(route.as_str()))
    {
        job.result_artifacts.push(JobArtifact {
            label: label.into(),
            route: Some(route),
        });
    }

    // A managed path is an implementation detail, not useful locality information. Keep only file
    // names in item rows and use an explicit delivery label for the destination.
    match result {
        JobResult::Export(report) => report.output = "Server-managed download".into(),
        JobResult::Convert(report) => {
            report.output_dir = if report.dry_run {
                "No output (dry run)".into()
            } else {
                "Server-managed download".into()
            };
            for item in &mut report.items {
                if let Some(name) = std::path::Path::new(&item.planned_output).file_name() {
                    item.planned_output = name.to_string_lossy().into_owned();
                }
            }
        }
    }
}

/// Containment for result decoration also works before a file exists (for example an empty convert
/// or a dry run): canonicalize the managed root and the nearest existing parent, then append only a
/// single final file name. A parent component, absolute replacement, or symlink escape fails shut.
fn managed_path_belongs_to(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let Ok(relative) = candidate.strip_prefix(root) else {
        return false;
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    if let Ok(candidate) = std::fs::canonicalize(candidate) {
        return candidate != root && candidate.starts_with(&root);
    }
    let Some(parent) = candidate.parent() else {
        return false;
    };
    let Ok(parent) = std::fs::canonicalize(parent) else {
        return false;
    };
    candidate.file_name().is_some() && parent.starts_with(root)
}

async fn download_job_artifact(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: JobId = parse_id(&id, "job")?;
    // This is the authorization and visibility choke point. The route never accepts a path and
    // never trusts a client copy of the report: it re-reads the durable job for this caller.
    let job = st.lib.get_job(&ctx, &id).await?;
    if job.state != JobState::Done {
        return Err(ApiError(LibError::Conflict(
            "the job has no completed artifact".into(),
        )));
    }
    let result = job
        .result
        .as_deref()
        .ok_or_else(|| ApiError(LibError::NotFound("job artifact".into())))?;
    let (reported, filename, content_type, package) = match result {
        JobResult::Export(report) => match report.format {
            ExportFormat::Json => (
                std::path::Path::new(&report.output),
                format!("3dam-manifest-{id}.json"),
                "application/json",
                false,
            ),
            ExportFormat::Csv => (
                std::path::Path::new(&report.output),
                format!("3dam-manifest-{id}.csv"),
                "text/csv; charset=utf-8",
                false,
            ),
            ExportFormat::Sidecar => (
                std::path::Path::new(&report.output),
                format!("3dam-sidecars-{id}.zip"),
                "application/zip",
                true,
            ),
        },
        JobResult::Convert(report) if !report.dry_run && report.done > 0 => (
            std::path::Path::new(&report.output_dir),
            format!("3dam-convert-{id}.zip"),
            "application/zip",
            true,
        ),
        JobResult::Convert(_) => return Err(ApiError(LibError::NotFound("job artifact".into()))),
    };
    let source = canonical_managed_artifact(&st.artifacts_dir, reported)?;
    let file = if package {
        package_managed_directory(&st.artifacts_dir, &source, &id).await?
    } else {
        if !std::fs::metadata(&source)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Err(ApiError(LibError::NotFound("job artifact".into())));
        }
        source
    };
    stream_artifact(file, filename, content_type, id, method, headers).await
}

fn canonical_managed_artifact(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> Result<PathBuf, ApiError> {
    let root = std::fs::canonicalize(root)
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?;
    let candidate = std::fs::canonicalize(candidate)
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?;
    if candidate == root || !candidate.starts_with(&root) {
        return Err(ApiError(LibError::NotFound("job artifact".into())));
    }
    Ok(candidate)
}

async fn package_managed_directory(
    root: &std::path::Path,
    directory: &std::path::Path,
    job: &JobId,
) -> Result<PathBuf, ApiError> {
    if !std::fs::metadata(directory)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(ApiError(LibError::NotFound("job artifact".into())));
    }
    let packages = root.join("packages");
    std::fs::create_dir_all(&packages).map_err(|error| {
        ApiError(LibError::Internal(format!(
            "create artifact package directory: {error}"
        )))
    })?;
    let destination = packages.join(format!("{job}.zip"));
    if destination.is_file() {
        return canonical_managed_artifact(root, &destination);
    }
    let directory = directory.to_path_buf();
    let destination_for_task = destination.clone();
    tokio::task::spawn_blocking(move || -> Result<(), LibError> {
        let temp = tempfile::Builder::new()
            .prefix(".3dam-package-")
            .tempfile_in(
                destination_for_task
                    .parent()
                    .expect("managed package has a parent"),
            )
            .map_err(|error| LibError::Internal(format!("stage artifact package: {error}")))?;
        let mut archive = zip::ZipWriter::new(temp);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| LibError::Internal(format!("read artifact directory: {error}")))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| LibError::Internal(format!("read artifact entry: {error}")))?;
            let file_type = entry
                .file_type()
                .map_err(|error| LibError::Internal(format!("inspect artifact entry: {error}")))?;
            // Managed multi-file outputs are flat today. Refuse symlinks, directories, devices,
            // and anything else rather than recursively acquiring unrelated server content.
            if !file_type.is_file() {
                return Err(LibError::NotFound("job artifact".into()));
            }
            let name = safe_archive_name(&entry.file_name())?;
            archive.start_file(name, options).map_err(|error| {
                LibError::Internal(format!("start artifact zip entry: {error}"))
            })?;
            let mut input = std::fs::File::open(entry.path())
                .map_err(|error| LibError::Internal(format!("open artifact entry: {error}")))?;
            std::io::copy(&mut input, &mut archive).map_err(|error| {
                LibError::Internal(format!("write artifact zip entry: {error}"))
            })?;
        }
        let temp = archive
            .finish()
            .map_err(|error| LibError::Internal(format!("finish artifact package: {error}")))?;
        if let Err(error) = temp.persist(&destination_for_task) {
            // Two authenticated downloads may build the same immutable package concurrently. The
            // winner is already the complete representation; losing an atomic publish race is
            // success once the destination is a regular file (not a 500 on double-click).
            if !destination_for_task.is_file() {
                return Err(LibError::Internal(format!(
                    "publish artifact package: {}",
                    error.error
                )));
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| {
        ApiError(LibError::Internal(format!(
            "package artifact task: {error}"
        )))
    })??;
    canonical_managed_artifact(root, &destination)
}

fn safe_archive_name(name: &std::ffi::OsStr) -> Result<String, LibError> {
    let Some(name) = name.to_str() else {
        return Err(LibError::NotFound("job artifact".into()));
    };
    // ZIP consumers disagree on whether `\` is data or a separator. Restrict names to a portable
    // single component so an archive created on Unix cannot traverse when extracted on Windows.
    let portable = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    portable
        .then_some(name.to_owned())
        .ok_or_else(|| LibError::NotFound("job artifact".into()))
}

async fn stream_artifact(
    path: PathBuf,
    filename: String,
    content_type: &'static str,
    job: JobId,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?;
    let len = file
        .metadata()
        .await
        .map_err(|_| ApiError(LibError::NotFound("job artifact".into())))?
        .len();
    let etag = format!("\"artifact-{job}\"");
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .filter(|_| {
            headers
                .get(header::IF_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|value| value.trim() == etag)
        });
    let (status, first, last) = if method == Method::HEAD {
        (StatusCode::OK, 0, len.saturating_sub(1))
    } else {
        match range.map(|value| parse_range(value, len)) {
            Some(RangeSpec::Satisfiable(first, last)) => (StatusCode::PARTIAL_CONTENT, first, last),
            Some(RangeSpec::Unsatisfiable) => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    header::HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
                );
                return Ok(response);
            }
            Some(RangeSpec::Ignore) | None => (StatusCode::OK, 0, len.saturating_sub(1)),
        }
    };
    let body_len = if len == 0 { 0 } else { last - first + 1 };
    if method != Method::HEAD && body_len > 0 {
        file.seek(std::io::SeekFrom::Start(first))
            .await
            .map_err(|error| ApiError(LibError::Internal(format!("seek artifact: {error}"))))?;
    }
    let body = if method == Method::HEAD || body_len == 0 {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(body_len)))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .expect("job-derived filename is a safe header value"),
    );
    h.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    h.insert(header::ETAG, header::HeaderValue::from_str(&etag).unwrap());
    h.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&body_len.to_string()).unwrap(),
    );
    h.insert(
        header::HeaderName::from_static("x-content-type-options"),
        header::HeaderValue::from_static("nosniff"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&format!("bytes {first}-{last}/{len}")).unwrap(),
        );
    }
    Ok(response)
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
    use super::{
        managed_path_belongs_to, parse_range, response_compression, safe_archive_name, trace_path,
        RangeSpec, WORKER_CSP,
    };
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
