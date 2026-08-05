//! Process lifecycle for the long-running roles: bring the engine, server store, and router up,
//! then run them until shutdown — `3dam serve` (plaintext or TLS), the in-process desktop server
//! behind the Tauri shell (ADR 0013), and the MCP stdio transport.

use crate::tickets::{empty_media_tickets, empty_ws_tickets};
use crate::{
    auth_rate, build_router, ectx, oidc_bearer, AppState, McpAdapter, ServeConfig, ServeFile,
    ServerStore, WriteGate,
};
use axum::Router;
use dam_api::service::LibraryService;
use dam_api::LibError;
use dam_core::EmbeddedLibrary;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

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
    let cache_options = dam_core::CacheOptions::from_mebibytes(
        file.resources.derivative_cache_mb,
        file.resources.peer_cache_mb,
    );
    let lib =
        Arc::new(EmbeddedLibrary::open_with_cache(&cfg.data_dir, resources, cache_options).await?);
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
        auth_protection: Arc::new(auth_rate::AuthProtection::new(
            file.server.trusted_proxies.iter().copied(),
        )),
        oidc_bearer: Arc::new(oidc_bearer::Verifier::new()),
        shutdown: shutdown_rx,
        ready: ready.clone(),
        max_upload_bytes: file.upload.max_bytes(),
        artifacts_dir: Arc::new(cfg.data_dir.join("artifacts")),
        ws_tickets: empty_ws_tickets(),
        media_tickets: empty_media_tickets(),
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
        auth_protection: Arc::new(auth_rate::AuthProtection::new([])),
        oidc_bearer: Arc::new(oidc_bearer::Verifier::new()),
        shutdown,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        // The desktop shell reads no serve config file, so the built-in ceiling stands. A local
        // drag-and-drop is the *least* constrained case anyway: no network hop to protect.
        max_upload_bytes: crate::config::UploadBlock::default().max_bytes(),
        artifacts_dir: Arc::new(data_dir.join("artifacts")),
        ws_tickets: empty_ws_tickets(),
        media_tickets: empty_media_tickets(),
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
