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
mod authn;
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
use tower_http::set_header::SetResponseHeaderLayer;
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
    /// Force `Secure` on session/CSRF cookies regardless of the local TLS posture — the operator's
    /// declaration that a TLS-terminating proxy sits in front (`[server] secure_cookies`).
    pub secure_cookies: bool,
    /// Refuse the *open* (loopback-peer) first-run claim path outright, so only bootstrap-token
    /// redemption can claim (`[accounts] require_claim_token`; ADR 0014).
    pub require_claim_token: bool,
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
        let mut resp = (status, Json(self.0.to_body())).into_response();
        // A 401 SHOULD advertise the scheme it wants (RFC 7235 §3.1). We use bearer tokens, so say
        // so — tools and generic HTTP clients key off this header, and it costs nothing.
        if status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        resp
    }
}

fn parse_id<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, ApiError> {
    s.parse::<T>()
        .map_err(|_| ApiError(LibError::BadRequest(format!("invalid {what} id"))))
}

/// Write a secret (the bootstrap owner token) to a file readable only by the owner. On Unix the file
/// is created with `0600` before any bytes are written, so the secret is never briefly world-readable;
/// elsewhere it falls back to a plain write (best effort). Kept out of the logs on purpose.
fn write_secret_file(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        writeln!(f, "{secret}")
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(path)?;
        writeln!(f, "{secret}")
    }
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
        // User accounts: claim/login/sessions (phase 6, issue #42) — 404 while the flag is off.
        .merge(authn::routes(state.clone()))
        // The admin API (tech-spec 10 §5), guarded by the AdminAuth extractor.
        .merge(admin::routes(state.clone()))
        // SPA fallback: any non-API GET serves the embedded web client (tech-spec 09 §A.4).
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        // Reads carry the bearer as a `?token=` query param on `<img>`/`<audio>`/WS loads (browsers
        // can't header-auth those). `no-referrer` stops that token leaking onward via the `Referer`
        // header when a preview or the page links out. Applied to every response, cheaply.
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            header::HeaderValue::from_static("no-referrer"),
        ))
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
        secure_cookies: false,
        require_claim_token: false,
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
    // The `[resources]` block bounds the engine's background appetite (tech-spec 14 §5).
    let resources = dam_core::ResourceOptions {
        background_threads: file.resources.background_threads,
        min_free_memory_mb: file.resources.min_free_memory_mb,
        max_io_stall_pct: file.resources.max_io_stall_pct,
    };
    let lib = Arc::new(EmbeddedLibrary::open_with(&cfg.data_dir, resources).await?);
    lib.start_watchers(); // long-running role: resume auto-rescan for watch-enabled sources.
    let store = Arc::new(ServerStore::open(&cfg.data_dir.join("server.db"))?);
    for (key, value) in file.flag_seeds() {
        store.seed_flag(key, value)?;
    }
    // Config-plane recovery hatch (ADR 0009 §3, issue #42): `[accounts] reopen_claim = true`
    // re-opens the first-run claim window for this boot, so a lost sole admin can be recovered
    // from the machine that owns the config file. Audited, and loud below via the unclaimed beat.
    if file.accounts.reopen_claim == Some(true) {
        store.reopen_claim("config-file")?;
        eprintln!("  ⚠ [accounts] reopen_claim: the claim window is OPEN — the next signup from this machine becomes admin");
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
        secure_cookies: file.server.secure_cookies == Some(true),
        require_claim_token: file.accounts.require_claim_token == Some(true),
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
    // Never locked out: a config file can seed a credentialed mode on first boot (the hardened
    // template does), which would gate the instance with zero key holders. Mint the bootstrap owner
    // token — but the secret must NOT land in stderr/journald/docker logs, which persist and are
    // widely readable. Write it to a 0600 file the operator collects once and then deletes; log only
    // the path.
    if !matches!(s.auth, dam_api::admin::AuthMode::Off) {
        if let Some(t) = store.bootstrap_owner_token_if_needed("startup")? {
            let path = cfg.data_dir.join("bootstrap-owner-token.txt");
            write_secret_file(&path, &t.secret)?;
            eprintln!(
                "  authentication is on and no admin credential existed — minted the owner token\n  \
                 secret written to {} (owner-only; save it, then delete the file)",
                path.display()
            );
            tracing::warn!(
                path = %path.display(),
                "minted bootstrap owner token; secret is in the file, kept out of the logs"
            );
        }
    }
    ready.store(true, std::sync::atomic::Ordering::Relaxed);

    // Unclaimed state is loud (issue #42 §2): while accounts are on with no admin claimed, warn on
    // a beat so an exposed unclaimed instance is never silent. The task ends itself once claimed.
    {
        let store = store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                if !store.unclaimed() {
                    break;
                }
                tracing::warn!(
                    "user accounts are on but UNCLAIMED — the first signup from a process on this \
                     machine becomes admin; claim it now (web UI) or via POST /api/v1/auth/claim. \
                     Behind a reverse proxy set [accounts] require_claim_token and redeem the \
                     bootstrap owner token instead"
                );
            }
        });
    }

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
    // ConnectInfo gives handlers the peer address — the first-run claim gate binds acceptance to a
    // loopback peer (issue #42 §2), which needs more than the bind posture when listening wide.
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
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
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
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

/// What [`serve_desktop`] reports back to the desktop shell (ADR 0013): where the in-process server
/// listens and the credential the webview should present.
pub struct DesktopServer {
    /// `http://127.0.0.1:<port>` — loopback only, ephemeral port.
    pub url: String,
    /// A fresh owner-scoped shell token when the `Authentication` flag is on; `None` when it is Off
    /// (an Off-mode server already resolves the unauthenticated local caller to owner trust).
    pub token: Option<String>,
}

/// Run the server for the native desktop shell (ADR 0013): the same engine + store + router as
/// [`serve`], but bound to `127.0.0.1:0` (loopback, ephemeral port), defaults-only config (no config
/// file, no TLS), and no signal handling — the shell owns the process lifetime and this future is
/// simply dropped (or the process exits) when the window closes. The bound URL (and shell credential)
/// is sent through `ready` once the listener is accepting.
pub async fn serve_desktop(
    data_dir: PathBuf,
    ready: tokio::sync::oneshot::Sender<anyhow::Result<DesktopServer>>,
) {
    let (app, listener, info) = match desktop_setup(&data_dir).await {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(info));
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "desktop server exited with an error");
    }
}

/// Wire the desktop server: open the stores, start watchers + the background pipeline, mint the
/// shell credential, and bind. Split from [`serve_desktop`] so setup failures funnel to one `?` path.
async fn desktop_setup(
    data_dir: &std::path::Path,
) -> anyhow::Result<(Router, tokio::net::TcpListener, DesktopServer)> {
    let lib = Arc::new(EmbeddedLibrary::open(data_dir).await?);
    lib.start_watchers();
    let store = Arc::new(ServerStore::open(&data_dir.join("server.db"))?);
    lib.start_background_pipeline(Arc::new(ServerPipelinePolicy {
        store: store.clone(),
    }));
    let token = desktop_shell_token(&store)?;

    // No graceful drain for the desktop role — leak the sender (as the test seam does) so the WS
    // loops never observe a spurious shutdown flip from a dropped channel.
    let (tx, shutdown) = watch::channel(false);
    Box::leak(Box::new(tx));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let actual = listener.local_addr()?;
    let state = AppState {
        lib,
        store,
        bind: actual.to_string(),
        localhost_only: true,
        tls: false,
        // The desktop shell is a webview on a genuinely local loopback socket: no proxy in front
        // (so no forced `Secure` over plaintext http://127.0.0.1, which browsers would then drop)
        // and the open claim path is exactly right for the solo-dev first run (ADR 0013/0014).
        secure_cookies: false,
        require_claim_token: false,
        shutdown,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    tracing::info!(%actual, "3dam desktop server listening");
    Ok((
        build_router(state),
        listener,
        DesktopServer {
            url: format!("http://{actual}"),
            token,
        },
    ))
}

/// The desktop shell's credential when the `Authentication` flag is on: a fresh owner-scoped token
/// per launch, with the previous launch's revoked by label so they don't accumulate in `server.db`.
/// (Two concurrent shells on one data dir will fight over this label — the second launch signs the
/// first out; an accepted edge, same as today's concurrent-session behaviour.)
fn desktop_shell_token(store: &ServerStore) -> Result<Option<String>, LibError> {
    use dam_api::admin::{AuthMode, NewToken};
    // The *effective* mode: `UserAccounts` on gates the surface even with the auth flag Off, and
    // the shell webview still needs a credential to reach its own in-process server (ADR 0013).
    if matches!(store.effective_auth_mode(), AuthMode::Off) {
        return Ok(None);
    }
    const LABEL: &str = "desktop shell";
    for t in store.list_tokens()? {
        if t.label == LABEL {
            store.revoke_token(&t.token_id, "desktop")?;
        }
    }
    let minted = store.create_token(
        NewToken {
            label: LABEL.into(),
            scopes: dam_api::service::Scopes::owner(),
            expires: None,
        },
        "desktop",
    )?;
    Ok(Some(minted.secret))
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
    let resolved = auth::resolve(
        &st.store,
        auth::bearer_header(&headers),
        auth::cookie_value(&headers, auth::SESSION_COOKIE),
    )?;
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
/// Note this slices bytes already resident in memory: `read_content` materialises the whole asset,
/// per the transport design in ADR 0012. Range therefore buys correct seek semantics and bounded
/// *response* size, not bounded server memory — streaming a large file straight from the source
/// would be a change to the `LibraryService` seam, and is the natural follow-up if video libraries
/// get big.
fn ranged_content_response(
    content: AssetContent,
    cache_control: &'static str,
    range_header: Option<&str>,
) -> Response {
    let len = content.bytes.len() as u64;
    let Some(spec) = range_header else {
        let mut res = content_response(content, cache_control);
        res.headers_mut().insert(
            header::ACCEPT_RANGES,
            header::HeaderValue::from_static("bytes"),
        );
        return res;
    };

    match parse_range(spec, len) {
        RangeSpec::Satisfiable(first, last) => {
            let slice = content.bytes[first as usize..=last as usize].to_vec();
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, content.content_type),
                    (header::CACHE_CONTROL, cache_control.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_RANGE, format!("bytes {first}-{last}/{len}")),
                ],
                Body::from(slice),
            )
                .into_response()
        }
        // Valid but past the end (including any range against an empty body) is the one case 416
        // describes; a malformed or unimplemented spec is ignored, per `RangeSpec`.
        RangeSpec::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
        )
            .into_response(),
        RangeSpec::Ignore => {
            let mut res = content_response(content, cache_control);
            res.headers_mut().insert(
                header::ACCEPT_RANGES,
                header::HeaderValue::from_static("bytes"),
            );
            res
        }
    }
}

async fn asset_content(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    let content = st.lib.read_content(&ctx, &id).await?;
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    Ok(ranged_content_response(
        content,
        "private, max-age=60",
        range,
    ))
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
    // Orphan-GC the cross-database soft references (issue #42): shares of a deleted source are
    // dead grants. Ids are uuids (never recycled), so a failure here is clutter, not exposure.
    let _ = st.store.remove_shares_for_resource(
        dam_api::accounts::ShareResource::Source,
        &id.to_string(),
        "system",
    );
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
    let _ = st.store.remove_shares_for_resource(
        dam_api::accounts::ShareResource::Collection,
        &id.to_string(),
        "system",
    );
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
    let resolved = match auth::resolve_ws(&st.store, &headers, q.token.clone()) {
        Ok(r) => r,
        Err(e) => return ApiError(e).into_response(),
    };
    // Keep the raw credentials so the long-lived loop can *re-resolve* when shares/groups change
    // (issue #42): a revoked share must not keep feeding a stale ceiling to an open socket.
    let token = auth::bearer_header(&headers).or(q.token);
    let cookie = auth::cookie_value(&headers, auth::SESSION_COOKIE);
    ws.on_upgrade(move |socket| ws_loop(socket, st, resolved.ctx, token, cookie))
}

async fn ws_loop(
    mut socket: WebSocket,
    st: AppState,
    ctx: dam_api::service::AuthContext,
    token: Option<String>,
    cookie: Option<String>,
) {
    // Subscribe under the *caller's* context, not the embedded owner — the engine filters the
    // stream to the ceiling (restricted subscribers get no job/asset payloads; issue #42).
    let mut vis_gen = st.store.visibility_generation();
    let mut stream = match st.lib.subscribe(&ctx, SubscribeRequest::default()).await {
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
                // A share/group/account mutation since we subscribed: re-resolve the credential
                // and re-subscribe under the fresh ceiling; a now-invalid credential closes.
                let gen_now = st.store.visibility_generation();
                if gen_now != vis_gen {
                    vis_gen = gen_now;
                    match auth::resolve(&st.store, token.clone(), cookie.clone()) {
                        Ok(r) if r.ctx.scopes.has(dam_api::service::Scope::Read) => {
                            match st.lib.subscribe(&r.ctx, SubscribeRequest::default()).await {
                                Ok(s) => stream = s,
                                Err(_) => break,
                            }
                        }
                        _ => {
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    continue; // the in-flight event predates the fresh ceiling — drop it
                }
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
    let resolved = match auth::resolve(
        &st.store,
        auth::bearer_header(&headers),
        auth::cookie_value(&headers, auth::SESSION_COOKIE),
    ) {
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
    use super::{parse_range, RangeSpec};

    fn sat(first: u64, last: u64) -> RangeSpec {
        RangeSpec::Satisfiable(first, last)
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
