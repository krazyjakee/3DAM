//! Test-only support for the `dam-server` integration tests.
//!
//! Everything under `tests/support/` is compiled **only** into the test binaries — nothing here
//! ships. The house rule these modules follow (set by `crates/3dam-sources/tests/support/mod.rs`,
//! which runs a real SSH+SFTP server on a loopback port rather than mocking `FileSource`) is that a
//! protocol is exercised against a **real implementation of the other side**, over a real socket.
//! A mock proves only that our code calls the functions we told it to call.
//!
//! ## What belongs here
//!
//! Only **setup and transport**: build a router, fire a request, sign a browser in, seed fixtures,
//! wait for a job. Nothing here asserts a property of the system under test — a helper that hid an
//! `assert_eq!` would silently make a scenario pass or fail for a reason its own body never names.
//! Setup that cannot proceed panics with the status and body that stopped it, which reads as
//! "the fixture is broken", not as "the behaviour under test is wrong". Deliberately **no
//! `assert!`/`assert_eq!` appears below**, and that is a checked property of this module.
//!
//! Each integration test file is its own crate, so every binary compiles the whole module and uses
//! a slice of it — hence the blanket `dead_code` allow, exactly as the sources crate does.

#![allow(dead_code)] // a shared support module is used piecemeal by each test binary

pub mod oidc_issuer;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use dam_api::dto::{
    AddSource, AnalyzeRequest, JobState, ScanMode, ScanRequest, SourceKind, SourceOptions,
};
use dam_api::event::LibraryEvent;
use dam_api::service::{AuthContext, EventStream, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
use tower::ServiceExt;

// ── data directories ─────────────────────────────────────────────────────────

/// A fresh, never-reused data directory under the system temp dir, tagged with `label`.
///
/// Per-process atomic counter **as well as** a timestamp, for the reason `dam-core/tests/scan.rs`
/// documents: these tests run in parallel within one process and `as_nanos()` can coincide for two
/// that start in the same clock tick, silently sharing a data dir — which surfaces as a migration
/// failure ("table already exists") from whichever harness loses the race. The window is only as
/// narrow as `open` is fast, so it widens whenever a migration is added.
pub fn unique_tmp(label: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-{label}-{}-{nanos}-{n}", std::process::id()))
}

// ── the router under test ────────────────────────────────────────────────────

/// Build the in-process router over a fresh library + in-memory server store, with a synthetic
/// peer address. `local_peer` chooses between a loopback client (the desktop / `curl localhost`
/// case) and an off-box one, and is also what the router is told about its bind.
///
/// `Extension` as a layer inserts into request extensions, which is exactly where
/// `into_make_service_with_connect_info` puts `ConnectInfo` on the real serve path. The claim gate
/// reads that peer and nothing else about the bind (ADR 0014 — a loopback *bind* is not evidence
/// of a local caller behind a same-host proxy).
pub async fn harness(local_peer: bool) -> (axum::Router, Arc<ServerStore>, Arc<EmbeddedLibrary>) {
    let lib = Arc::new(
        EmbeddedLibrary::open_with(
            &unique_tmp("harness"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", local_peer)
        .layer(axum::Extension(ConnectInfo(peer_addr(local_peer))));
    (app, store, lib)
}

/// The synthetic peer a [`harness`] presents: loopback, or TEST-NET-3 (RFC 5737) — a range that is
/// unambiguously not loopback.
pub fn peer_addr(local_peer: bool) -> SocketAddr {
    if local_peer {
        "127.0.0.1:54321".parse().unwrap()
    } else {
        "203.0.113.9:54321".parse().unwrap()
    }
}

// ── requests ─────────────────────────────────────────────────────────────────

/// A signed-in browser: the session cookie + the CSRF token the login reply carried.
#[derive(Clone, Debug)]
pub struct Session {
    pub cookie: String,
    pub csrf: String,
}

impl Session {
    pub fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("cookie".into(), format!("dam_session={}", self.cookie)),
            ("x-dam-csrf".into(), self.csrf.clone()),
        ]
    }

    /// Cookie only — for exercising the CSRF gate itself.
    pub fn cookie_only(&self) -> Vec<(String, String)> {
        vec![("cookie".into(), format!("dam_session={}", self.cookie))]
    }
}

/// Fire one request at the router; returns `(status, response headers, json-or-null)`.
///
/// This is the one primitive: [`call`] and [`call_token`] only choose which credential headers to
/// attach, and a test that needs the reply headers (a `Set-Cookie`, a `Location`, a `Retry-After`)
/// reaches for this directly.
pub async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(String, String)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(k, v);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let hdrs = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, hdrs, val)
}

/// Fire one request as a signed-in browser (cookie + CSRF), or as nobody.
pub async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    session: Option<&Session>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let headers = session.map(|s| s.headers()).unwrap_or_default();
    let (st, _h, v) = send(app, method, uri, &headers, body).await;
    (st, v)
}

/// Post one upload body as a signed-in browser. Upload deliberately uses a raw streaming body, not
/// JSON, so the ordinary [`call`] helper cannot exercise its auth/share boundary.
pub async fn upload_call(
    app: &axum::Router,
    session: &Session,
    source: &str,
    name: &str,
    bytes: Vec<u8>,
) -> (StatusCode, Value) {
    upload_body_call(app, session, source, name, Body::from(bytes)).await
}

/// As [`upload_call`], but the caller supplies the body — a `Body` that stalls or over-runs its
/// declared length is how the streaming guards get exercised.
pub async fn upload_body_call(
    app: &axum::Router,
    session: &Session,
    source: &str,
    name: &str,
    body: Body,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/upload?source={source}&name={name}"))
        .header("content-type", "application/octet-stream");
    for (name, value) in session.headers() {
        request = request.header(name, value);
    }
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// One JSON-RPC call against the mounted `/mcp` endpoint, as a token bearer or as nobody.
pub async fn rpc(
    app: &axum::Router,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> (StatusCode, Value) {
    call_token(
        app,
        "POST",
        "/mcp",
        token,
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})),
    )
    .await
}

/// Fire one request bearing a token (an API token or an OIDC access token), or none.
pub async fn call_token(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let headers = token
        .map(|t| vec![("authorization".to_string(), format!("Bearer {t}"))])
        .unwrap_or_default();
    let (st, _h, v) = send(app, method, uri, &headers, body).await;
    (st, v)
}

/// Read one cookie out of a reply's `Set-Cookie` headers.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|h| {
            let s = h.to_str().ok()?;
            s.strip_prefix(&prefix)
                .and_then(|rest| rest.split(';').next())
                .map(str::to_string)
        })
}

/// Pair the `dam_session` cookie a reply set with the CSRF token its body carried.
pub fn session_from_reply(headers: &HeaderMap, body: &Value) -> Session {
    Session {
        cookie: cookie(headers, "dam_session").expect("login/claim reply sets the session cookie"),
        csrf: body["csrf"].as_str().expect("csrf in reply").to_string(),
    }
}

// ── signing in ───────────────────────────────────────────────────────────────

/// Flip the `user_accounts` flag on (as the pre-accounts local owner, auth Off) and return the
/// bootstrap owner token the reply minted (the never-locked-out guarantee).
pub async fn enable_accounts(app: &axum::Router) -> Option<String> {
    let (st, body) = call(
        app,
        "PUT",
        "/admin/api/flags/user_accounts",
        None,
        Some(json!({"value": true, "expected_version": null, "confirm": true})),
    )
    .await;
    expect_ok(st, &body, "flipping user_accounts on");
    body["bootstrap_token"]["secret"].as_str().map(String::from)
}

/// Claim the instance as `username` (loopback harness) and return the admin session.
pub async fn claim(app: &axum::Router, username: &str) -> Session {
    let (st, headers, body) = send(
        app,
        "POST",
        "/api/v1/auth/claim",
        &[],
        Some(json!({"username": username, "password": "password123"})),
    )
    .await;
    expect_ok(st, &body, "the first-run claim");
    session_from_reply(&headers, &body)
}

/// Sign in over the password endpoint and return the session.
pub async fn login(app: &axum::Router, username: &str, password: &str) -> Session {
    let (st, headers, body) = send(
        app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"username": username, "password": password})),
    )
    .await;
    expect_ok(st, &body, "login");
    session_from_reply(&headers, &body)
}

/// Create an account via the admin API; returns its `account_id`.
pub async fn create_account(
    app: &axum::Router,
    admin: &Session,
    username: &str,
    role: &str,
) -> String {
    let (st, body) = call(
        app,
        "POST",
        "/admin/api/accounts",
        Some(admin),
        Some(json!({"username": username, "password": "password123", "role": role})),
    )
    .await;
    expect_ok(st, &body, "creating an account");
    body["account_id"].as_str().unwrap().to_string()
}

/// Share a resource to an account (or group) and return the share id. `target` is
/// `("account_id" | "group_id", id)`.
pub async fn share(
    app: &axum::Router,
    admin: &Session,
    resource: &str,
    resource_id: &str,
    target: (&str, &str),
    access: &str,
) -> String {
    let (st, body) = call(
        app,
        "POST",
        "/admin/api/shares",
        Some(admin),
        Some({
            let mut req = json!({
                "resource": resource, "resource_id": resource_id, "access": access,
            });
            req[target.0] = json!(target.1);
            req
        }),
    )
    .await;
    expect_ok(st, &body, "creating a share");
    body["share_id"].as_str().unwrap().to_string()
}

// ── fixture seeding (engine-side, before the gate goes up) ───────────────────

/// Write a tiny solid-colour PNG (analysable by the model-free pipeline).
pub fn write_png(path: &Path, rgb: [u8; 3]) {
    let img = image::RgbImage::from_pixel(24, 24, image::Rgb(rgb));
    img.save(path).unwrap();
}

/// Two local sources: `shared/` (brick_red, brick_blue) and `secret/` (secret_wall + a byte-perfect
/// twin of brick_red for the dedup/similar leak tests). Scanned **and** analysed, so every image
/// has an embedding and `similar`/near-dup have something to leak. Returns
/// `(shared_sid, secret_sid)`.
pub async fn seed_two_sources(lib: &EmbeddedLibrary) -> (dam_api::SourceId, dam_api::SourceId) {
    let ctx = AuthContext::embedded();
    let base = unique_tmp("seed");
    let shared = base.join("shared");
    let secret = base.join("secret");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(&secret).unwrap();
    write_png(&shared.join("brick_red.png"), [200, 40, 40]);
    write_png(&shared.join("brick_blue.png"), [40, 40, 200]);
    write_png(&secret.join("secret_wall.png"), [10, 200, 10]);
    // The cross-source exact duplicate: identical bytes to brick_red.png.
    std::fs::copy(
        shared.join("brick_red.png"),
        secret.join("brick_red_copy.png"),
    )
    .unwrap();

    let add = |uri: String, name: &str| AddSource {
        kind: SourceKind::LocalFs,
        uri,
        name: Some(name.into()),
        options: SourceOptions::default(),
    };
    let shared_sid = lib
        .add_source(
            &ctx,
            add(shared.to_string_lossy().into_owned(), "shared-src"),
        )
        .await
        .unwrap();
    let secret_sid = lib
        .add_source(
            &ctx,
            add(secret.to_string_lossy().into_owned(), "secret-src"),
        )
        .await
        .unwrap();
    let job = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(lib, &ctx, &job).await;
    // Analysis gives every image an embedding (model-free v1 space) → similar/near-dup work.
    let job = lib
        .submit_analyze(
            &ctx,
            AnalyzeRequest {
                assets: vec![],
                force: false,
            },
        )
        .await
        .unwrap();
    wait_job(lib, &ctx, &job).await;
    (shared_sid, secret_sid)
}

/// The world every accounts leak-audit scenario starts from: [`seed_two_sources`], accounts on, an
/// admin (`owner`) holding the instance, and viewer `vera` given a **read** share on `shared-src`
/// only — `secret-src` is shared with nobody, so anything of its that reaches `vera` is a leak.
///
/// Returns `(app, store, lib, admin, vera, shared_source_id, secret_source_id, vera_account_id)`.
#[allow(clippy::type_complexity)]
pub async fn leak_world() -> (
    axum::Router,
    Arc<ServerStore>,
    Arc<EmbeddedLibrary>,
    Session, // admin
    Session, // vera (viewer, shared-src read)
    String,  // shared source id
    String,  // secret source id
    String,  // vera account id
) {
    let (app, store, lib) = harness(true).await;
    let (shared_sid, secret_sid) = seed_two_sources(&lib).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared_sid.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;
    (
        app,
        store,
        lib,
        admin,
        vera,
        shared_sid.to_string(),
        secret_sid.to_string(),
        vera_id,
    )
}

// ── the server store, reached behind the router ──────────────────────────────

/// Mint a token with the given scopes directly on the store — the setup a test needs *before* it
/// raises the auth mode, since minting over the admin API would then need a credential it lacks.
pub fn mint(store: &ServerStore, label: &str, scopes: dam_api::service::Scopes) -> String {
    store
        .create_token(
            dam_api::admin::NewToken {
                label: label.into(),
                scopes,
                expires: None,
            },
            "test",
        )
        .unwrap()
        .secret
}

/// Set the authentication mode on the store, bypassing the admin API's own gates.
pub fn set_auth(store: &ServerStore, mode: dam_api::admin::AuthMode) {
    store
        .set_flag(
            dam_api::admin::FlagKey::Authentication,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Auth(mode),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
}

/// Block until an engine-side job reaches a terminal state.
pub async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &dam_api::JobId) {
    loop {
        let j = lib.get_job(ctx, job).await.unwrap();
        if matches!(
            j.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── reading the catalog back over the wire ───────────────────────────────────

/// The sorted asset names an unfiltered query returns for this session.
pub async fn query_names(app: &axum::Router, s: &Session) -> Vec<String> {
    let (st, body) = call(app, "POST", "/api/v1/query", Some(s), Some(json!({}))).await;
    expect_ok(st, &body, "querying assets");
    let mut names: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

/// Find one asset id by name via an admin session.
pub async fn asset_id_by_name(app: &axum::Router, admin: &Session, name: &str) -> String {
    let (st, body) = call(app, "POST", "/api/v1/query", Some(admin), Some(json!({}))).await;
    expect_ok(st, &body, "querying assets");
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == name)
        .unwrap_or_else(|| panic!("asset {name} not found"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Deadline-bounded read of the next event — a filter bug that withholds everything would otherwise
/// hang the test rather than fail it.
pub async fn next_event(stream: &mut EventStream<LibraryEvent>) -> LibraryEvent {
    use futures::StreamExt;
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("an allowed event should arrive before the timeout")
        .expect("the stream should still be open")
}

// ── internals ────────────────────────────────────────────────────────────────

/// Fail a *setup* step loudly, naming the status and body that stopped it.
///
/// Deliberately a panic rather than an assertion: nothing in this module is under test, so a
/// failure here means the fixture could not be built — a different thing from the scenario's own
/// expectations, which stay in the test body where a reader can see them.
fn expect_ok(status: StatusCode, body: &Value, what: &str) {
    if status != StatusCode::OK {
        panic!("test setup: {what} failed with {status}: {body}");
    }
}
