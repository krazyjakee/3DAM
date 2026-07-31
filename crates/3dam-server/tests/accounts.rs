//! Phase-6 user-accounts coverage (issue #42): the first-run claim, cookie sessions + CSRF, role
//! scope gating, groups + shares, and — most importantly — the **leak audit**: one test per read
//! path proving an unshared asset is *absent* (not forbidden) for a restricted identity.
//!
//! Driven through the real axum router in-process (`ServiceExt::oneshot`, no socket). The store is
//! shared so flag flips and share edits are visible to the next request — the same live path the
//! admin UI uses. The oneshot seam crosses no socket, so the harness injects the `ConnectInfo`
//! extension the serve stack would register: `harness(true)` presents a loopback peer, and
//! `harness(false)` a public one. The claim gate reads that peer and nothing else about the bind
//! (ADR 0014 — a loopback *bind* is not evidence of a local caller behind a same-host proxy).

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use dam_api::dto::*;
use dam_api::event::LibraryEvent;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn unique_tmp() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-accounts-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    ))
}

/// Build the in-process router with a synthetic peer address. `local_peer` chooses between a
/// loopback client (the desktop / `curl localhost` case) and an off-box one.
async fn harness(local_peer: bool) -> (axum::Router, Arc<ServerStore>, Arc<EmbeddedLibrary>) {
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let peer: SocketAddr = if local_peer {
        "127.0.0.1:54321".parse().unwrap()
    } else {
        // TEST-NET-3 (RFC 5737) — unambiguously not loopback.
        "203.0.113.9:54321".parse().unwrap()
    };
    // `Extension` as a layer inserts into request extensions, which is exactly where
    // `into_make_service_with_connect_info` puts `ConnectInfo` on the real serve path.
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", local_peer)
        .layer(axum::Extension(ConnectInfo(peer)));
    (app, store, lib)
}

/// A signed-in browser: the session cookie + the CSRF token the login reply carried.
#[derive(Clone)]
struct Session {
    cookie: String,
    csrf: String,
}

impl Session {
    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("cookie".into(), format!("dam_session={}", self.cookie)),
            ("x-dam-csrf".into(), self.csrf.clone()),
        ]
    }
    /// Cookie only — for asserting the CSRF gate itself.
    fn cookie_only(&self) -> Vec<(String, String)> {
        vec![("cookie".into(), format!("dam_session={}", self.cookie))]
    }
}

/// Fire one request; returns `(status, response headers, json-or-null)`.
async fn send(
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

async fn call(
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

/// Parse `dam_session` out of the reply's Set-Cookie headers and pair it with the body's csrf.
fn session_from_reply(headers: &HeaderMap, body: &Value) -> Session {
    let cookie = headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|h| {
            let s = h.to_str().ok()?;
            s.strip_prefix("dam_session=")
                .and_then(|rest| rest.split(';').next())
                .map(|v| v.to_string())
        })
        .expect("login/claim reply sets the session cookie");
    Session {
        cookie,
        csrf: body["csrf"].as_str().expect("csrf in reply").to_string(),
    }
}

/// Flip the `user_accounts` flag on (as the pre-accounts local owner, auth Off) and return the
/// bootstrap owner token the reply minted (the never-locked-out guarantee).
async fn enable_accounts(app: &axum::Router) -> Option<String> {
    let (st, body) = call(
        app,
        "PUT",
        "/admin/api/flags/user_accounts",
        None,
        Some(json!({"value": true, "expected_version": null, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "flag flip failed: {body}");
    body["bootstrap_token"]["secret"].as_str().map(String::from)
}

/// Claim the instance as `username` (loopback harness) and return the admin session.
async fn claim(app: &axum::Router, username: &str) -> Session {
    let (st, headers, body) = send(
        app,
        "POST",
        "/api/v1/auth/claim",
        &[],
        Some(json!({"username": username, "password": "password123"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "claim failed: {body}");
    assert_eq!(body["account"]["role"], "admin");
    session_from_reply(&headers, &body)
}

async fn login(app: &axum::Router, username: &str, password: &str) -> Session {
    let (st, headers, body) = send(
        app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "login failed: {body}");
    session_from_reply(&headers, &body)
}

/// Create an account via the admin API; returns its account_id.
async fn create_account(app: &axum::Router, admin: &Session, username: &str, role: &str) -> String {
    let (st, body) = call(
        app,
        "POST",
        "/admin/api/accounts",
        Some(admin),
        Some(json!({"username": username, "password": "password123", "role": role})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "create account failed: {body}");
    body["account_id"].as_str().unwrap().to_string()
}

/// Share a resource to an account (or group) and return the share id.
async fn share(
    app: &axum::Router,
    admin: &Session,
    resource: &str,
    resource_id: &str,
    target: (&str, &str), // ("account_id" | "group_id", id)
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
    assert_eq!(st, StatusCode::OK, "share failed: {body}");
    body["share_id"].as_str().unwrap().to_string()
}

// ── fixture seeding (engine-side, before the gate goes up) ───────────────────

/// Write a tiny solid-colour PNG (analysable by the model-free pipeline).
fn write_png(path: &Path, rgb: [u8; 3]) {
    let img = image::RgbImage::from_pixel(24, 24, image::Rgb(rgb));
    img.save(path).unwrap();
}

/// Two local sources: `shared/` (brick_red, brick_blue) and `secret/` (secret_wall + a byte-perfect
/// twin of brick_red for the dedup/similar leak tests). Returns (shared_sid, secret_sid).
async fn seed_two_sources(lib: &EmbeddedLibrary) -> (dam_api::SourceId, dam_api::SourceId) {
    let ctx = AuthContext::embedded();
    let base = unique_tmp();
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

async fn wait_job(lib: &EmbeddedLibrary, ctx: &AuthContext, job: &dam_api::JobId) {
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

/// Asset names a query returns for this session.
async fn query_names(app: &axum::Router, s: &Session) -> Vec<String> {
    let (st, body) = call(app, "POST", "/api/v1/query", Some(s), Some(json!({}))).await;
    assert_eq!(st, StatusCode::OK, "query failed: {body}");
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
async fn asset_id_by_name(app: &axum::Router, admin: &Session, name: &str) -> String {
    let (st, body) = call(app, "POST", "/api/v1/query", Some(admin), Some(json!({}))).await;
    assert_eq!(st, StatusCode::OK);
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

// ── the surface is absent while the flag is off ──────────────────────────────

/// **Every** gated route, not a sample: the guard is one `route_layer` per block now, and the point
/// of this test is to notice if a route ever escapes its block. Mutating endpoints matter most — a
/// reachable `POST /admin/api/shares` with the flag off would write grants nothing can display.
#[tokio::test]
async fn accounts_surface_absent_when_flag_off() {
    let (app, _store, _lib) = harness(true).await;
    let id = "00000000-0000-0000-0000-000000000000";
    for (method, uri) in [
        // /api/v1/auth — the whole public accounts surface
        ("GET", "/api/v1/auth/status".to_string()),
        ("POST", "/api/v1/auth/claim".to_string()),
        ("POST", "/api/v1/auth/login".to_string()),
        ("POST", "/api/v1/auth/logout".to_string()),
        ("GET", "/api/v1/auth/sessions".to_string()),
        ("DELETE", format!("/api/v1/auth/sessions/{id}")),
        // /admin/api accounts block — reads *and* every mutation
        ("GET", "/admin/api/accounts".to_string()),
        ("POST", "/admin/api/accounts".to_string()),
        ("PUT", format!("/admin/api/accounts/{id}")),
        ("DELETE", format!("/admin/api/accounts/{id}")),
        ("DELETE", format!("/admin/api/accounts/{id}/sessions")),
        ("GET", "/admin/api/groups".to_string()),
        ("POST", "/admin/api/groups".to_string()),
        ("DELETE", format!("/admin/api/groups/{id}")),
        ("PUT", format!("/admin/api/groups/{id}/members")),
        ("GET", "/admin/api/shares".to_string()),
        ("POST", "/admin/api/shares".to_string()),
        ("DELETE", format!("/admin/api/shares/{id}")),
    ] {
        let body = (method != "GET" && method != "DELETE").then(|| {
            json!({
                "username": "x", "password": "yyyyyyyy", "name": "x", "account_ids": [],
                "resource": "source", "resource_id": id, "account_id": id, "access": "read",
            })
        });
        let (st, _) = call(&app, method, &uri, None, body).await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "{method} {uri} must 404 while the flag is off"
        );
    }
}

// ── first-run claim (issue #42 §2; ADR 0014) ─────────────────────────────────

#[tokio::test]
async fn claim_flow_first_account_becomes_admin_and_window_closes() {
    let (app, _store, _lib) = harness(true).await;
    enable_accounts(&app).await;

    // Accounts-on raises the effective gate: an unauthenticated read now 401s…
    let (st, _) = call(&app, "POST", "/api/v1/query", None, Some(json!({}))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // …while the public auth status reports the unclaimed window.
    let (st, body) = call(&app, "GET", "/api/v1/auth/status", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["unclaimed"], true);

    let admin = claim(&app, "owner").await;

    // The session authenticates; whoami reports the account and full (admin) scopes.
    let (st, body) = call(&app, "GET", "/api/v1/whoami", Some(&admin), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["account"]["username"], "owner");
    assert_eq!(body["restricted"], false);
    assert!(body["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "admin"));

    // The window is closed: a second claim conflicts, and a plain signup route doesn't exist.
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/auth/claim",
        None,
        Some(json!({"username": "late", "password": "password123"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (_, body) = call(&app, "GET", "/api/v1/auth/status", None, None).await;
    assert_eq!(body["unclaimed"], false);

    // Admin status reflects the accounts posture.
    let (st, body) = call(&app, "GET", "/admin/api/status", Some(&admin), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["accounts_enabled"], true);
    assert_eq!(body["unclaimed"], false);
    assert_eq!(body["account_count"], 1);
}

#[tokio::test]
async fn remote_claim_refused_without_bootstrap_token() {
    // Off-box peer — not loopback.
    let (app, _store, _lib) = harness(false).await;
    let bootstrap = enable_accounts(&app).await.expect("bootstrap token minted");

    // A naked remote claim is refused — an unclaimed instance is not a signup form.
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/auth/claim",
        None,
        Some(json!({"username": "attacker", "password": "password123"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Presenting the bootstrap owner token as a bearer authorises the off-box claim.
    let (st, headers, body) = send(
        &app,
        "POST",
        "/api/v1/auth/claim",
        &[("authorization".into(), format!("Bearer {bootstrap}"))],
        Some(json!({"username": "owner", "password": "password123"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "token-redemption claim failed: {body}");
    assert_eq!(body["account"]["role"], "admin");
    let _ = session_from_reply(&headers, &body);
}

// ── sessions: CSRF, logout, self-service list/revoke ─────────────────────────

#[tokio::test]
async fn csrf_gates_cookie_writes_and_sessions_are_self_serviceable() {
    let (app, _store, _lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;

    // A cookie-authenticated mutation without the CSRF header is refused…
    let (st, _h, body) = send(
        &app,
        "PUT",
        "/admin/api/flags/auto_thumbnail",
        &admin.cookie_only(),
        Some(json!({"value": false, "expected_version": null, "confirm": false})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "missing csrf must 403: {body}");
    // …and passes with it.
    let (st, _) = call(
        &app,
        "PUT",
        "/admin/api/flags/auto_thumbnail",
        Some(&admin),
        Some(json!({"value": false, "expected_version": null, "confirm": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Two sessions exist after a second login; the list marks the current one.
    let second = login(&app, "owner", "password123").await;
    let (st, body) = call(&app, "GET", "/api/v1/auth/sessions", Some(&second), None).await;
    assert_eq!(st, StatusCode::OK);
    let sessions = body.as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions.iter().filter(|s| s["current"] == true).count(), 1);

    // Revoke the first from the second; the first is signed out on its next request.
    let first_id = sessions.iter().find(|s| s["current"] == false).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{first_id}"),
        Some(&second),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(&app, "GET", "/api/v1/whoami", Some(&admin), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Logout revokes the current session.
    let (st, _) = call(&app, "POST", "/api/v1/auth/logout", Some(&second), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(&app, "GET", "/api/v1/whoami", Some(&second), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_lockout_after_repeated_failures() {
    let (app, _store, _lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    create_account(&app, &admin, "vera", "viewer").await;

    for _ in 0..10 {
        let (st, _) = call(
            &app,
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"username": "vera", "password": "wrong-wrong"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    // Locked: even the correct password is refused with 429.
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "vera", "password": "password123"})),
    )
    .await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
}

// ── roles gate handlers by scope (never by role) ─────────────────────────────

#[tokio::test]
async fn role_scopes_gate_write_and_admin_surfaces() {
    let (app, _store, _lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    create_account(&app, &admin, "vera", "viewer").await;
    create_account(&app, &admin, "erin", "editor").await;
    let vera = login(&app, "vera", "password123").await;
    let erin = login(&app, "erin", "password123").await;

    // Viewer: read ok, write 403 (missing Write scope), admin 403.
    let (st, _) = call(&app, "POST", "/api/v1/query", Some(&vera), Some(json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/jobs/scan",
        Some(&vera),
        Some(json!({"sources": []})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = call(&app, "GET", "/admin/api/accounts", Some(&vera), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Editor: holds Write, but a restricted (no shares) identity cannot run library-wide ops.
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/jobs/scan",
        Some(&erin),
        Some(json!({"sources": []})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // And no admin surface either.
    let (st, _) = call(&app, "GET", "/admin/api/accounts", Some(&erin), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

// ── the leak audit (issue #42): every read path hides unshared content ───────

/// Everything shares one seeded world: `shared-src` (brick_red, brick_blue) is shared read to
/// viewer `vera`; `secret-src` (secret_wall + an exact twin of brick_red) is not shared at all.
async fn leak_world() -> (
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

#[tokio::test]
async fn leak_audit_query_get_content_thumbnail() {
    let (app, _s, _l, admin, vera, _shared, _secret, _vid) = leak_world().await;

    // Query: only the shared source's assets. The admin sees all four.
    assert_eq!(
        query_names(&app, &vera).await,
        ["brick_blue.png", "brick_red.png"]
    );
    assert_eq!(query_names(&app, &admin).await.len(), 4);

    // Direct-by-id reads on a hidden asset: absent (404), for detail, bytes, and thumbnail alike.
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    for uri in [
        format!("/api/v1/assets/{secret_id}"),
        format!("/api/v1/assets/{secret_id}/content"),
        format!("/api/v1/assets/{secret_id}/thumbnail"),
        format!("/api/v1/assets/{secret_id}/preview-mesh"),
    ] {
        let (st, _) = call(&app, "GET", &uri, Some(&vera), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri} must be absent");
    }
    // A shared asset's bytes flow normally.
    let ok_id = asset_id_by_name(&app, &admin, "brick_red.png").await;
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{ok_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn leak_audit_stats_sources_folders() {
    let (app, _s, _l, admin, vera, shared, secret, _vid) = leak_world().await;

    // Aggregates are no oracle: totals and breakdowns count only reachable assets, and the
    // unshared source's name never appears.
    let (st, body) = call(&app, "GET", "/api/v1/stats", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["total"], 2);
    assert_eq!(body["sources"], 1);
    assert!(body["by_source"].get("secret-src").is_none());
    let (_, admin_stats) = call(&app, "GET", "/api/v1/stats", Some(&admin), None).await;
    assert_eq!(admin_stats["total"], 4);

    // Source-scoped stats + the source record + folder tree of the hidden source: absent.
    for uri in [
        format!("/api/v1/stats?source={secret}"),
        format!("/api/v1/sources/{secret}"),
    ] {
        let (st, _) = call(&app, "GET", &uri, Some(&vera), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri} must be absent");
    }
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/folders",
        Some(&vera),
        Some(json!({"source": secret, "prefix": ""})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The sources list contains exactly the shared one.
    let (st, body) = call(&app, "GET", "/api/v1/sources", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![shared.as_str()]);
}

#[tokio::test]
async fn leak_audit_similar_and_duplicates() {
    let (app, _s, _l, admin, vera, _shared, _secret, _vid) = leak_world().await;
    let red_id = asset_id_by_name(&app, &admin, "brick_red.png").await;

    // Similar: the byte-identical twin lives in the unshared source. The admin sees it as the top
    // neighbour; the viewer must not see it at any rank.
    let sim = |body: Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["asset"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/similar",
        Some(&admin),
        Some(json!({"asset": red_id, "k": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        sim(body).contains(&"brick_red_copy.png".to_string()),
        "admin similar should surface the twin"
    );
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/similar",
        Some(&vera),
        Some(json!({"asset": red_id, "k": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !sim(body).contains(&"brick_red_copy.png".to_string()),
        "viewer similar leaked an unshared neighbour"
    );

    // A hidden seed asset must not rank at all.
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/similar",
        Some(&vera),
        Some(json!({"asset": secret_id, "k": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Exact duplicates: the red pair spans shared+secret. Admin sees the group; for the viewer it
    // collapses to one visible member — and a group of one is not a duplicate, so it vanishes.
    let dup_req = json!({"kind": "exact", "limit": 50});
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/duplicates",
        Some(&admin),
        Some(dup_req.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !body.as_array().unwrap().is_empty(),
        "admin should see the cross-source duplicate group"
    );
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/duplicates",
        Some(&vera),
        Some(dup_req),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body.as_array().unwrap().is_empty(),
        "viewer duplicates leaked a group spanning an unshared source: {body}"
    );
}

#[tokio::test]
async fn leak_audit_collections_and_shared_collection_grant() {
    let (app, _s, _l, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;

    // Admin builds a collection holding only the hidden asset.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/collections",
        Some(&admin),
        Some(json!({"name": "Hidden picks", "kind": "manual"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cid = body["id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{cid}/members"),
        Some(&admin),
        Some(json!({"add": [secret_id]})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Unshared: absent from the list, and its record/assets 404 by id.
    let (st, body) = call(&app, "GET", "/api/v1/collections", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body.as_array().unwrap().is_empty(),
        "collection leaked: {body}"
    );
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{cid}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Sharing the collection grants exactly its members (rule 5): the collection appears, its
    // member asset resolves, but the rest of the secret source stays hidden.
    share(
        &app,
        &admin,
        "collection",
        &cid,
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let (st, body) = call(&app, "GET", "/api/v1/collections", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{cid}/assets"),
        Some(&vera),
        // `PageParams::limit` has no serde default — the wire form always names it.
        Some(json!({"limit": 50})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{secret_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a shared collection's member must resolve"
    );
    // Collection ⊅ source: the twin in the same source is still absent.
    let twin_id = asset_id_by_name(&app, &admin, "brick_red_copy.png").await;
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{twin_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // And the query surface now includes the granted member.
    assert_eq!(
        query_names(&app, &vera).await,
        ["brick_blue.png", "brick_red.png", "secret_wall.png"]
    );
}

#[tokio::test]
async fn leak_audit_jobs_are_absent_for_restricted_identities() {
    let (app, _s, lib, admin, vera, shared, _secret, _vid) = leak_world().await;
    // The seeding scan/analyze jobs span *both* sources, so every one of them names paths vera
    // cannot reach. A job is observable only when its whole source set is inside the ceiling
    // (`Visibility::allows_job`) — "all", not "any", precisely so `progress.current` can never carry
    // a path out of an unshared source.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/list",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let admin_jobs = body["items"].as_array().unwrap().len();
    assert!(admin_jobs >= 2);
    // …the restricted viewer gets an empty page, and a direct get is absent.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/list",
        Some(&vera),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());
    let ctx = AuthContext::embedded();
    let job = lib
        .list_jobs(&ctx, JobListRequest::default())
        .await
        .unwrap()
        .items[0]
        .id;
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{job}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // …but a job confined to the source she *does* hold is hers to watch. This is the other half of
    // the rule: absence is driven by attribution, not by a blanket "restricted identities get no
    // jobs" — otherwise her status bar could never show her own scan running (issue #42).
    let mine = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![shared.parse().unwrap()],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(&lib, &ctx, &mine).await;
    let (st, body) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{mine}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a scan of her own source is visible: {body}"
    );
    assert_eq!(body["sources"], json!([shared]));

    // Visible is not cancellable: watching a job is a read, stopping one is a library-wide act.
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{mine}/cancel"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// The restricted-subscriber contract (issue #42): a share-based identity must get a *live* stream
/// for the sources it holds, not a socket that never speaks. Every `LibraryEvent` now carries the
/// attribution the ceiling is evaluated against, so this asserts both directions at once — the
/// shared source's events arrive, the unshared source's events never do.
///
/// Each pair below emits the **secret** event first. If the filter were merely slow rather than
/// exclusive, the secret event would arrive first and fail the assertion; ordering is what turns
/// "we saw the shared one" into "we saw *only* the shared one".
#[tokio::test]
async fn restricted_subscriber_receives_events_only_for_shared_sources() {
    let (app, store, lib, admin, _vera, shared, secret, vera_id) = leak_world().await;
    let shared_sid: dam_api::SourceId = shared.parse().unwrap();
    let secret_sid: dam_api::SourceId = secret.parse().unwrap();

    // Vera's ceiling, resolved from her real share rows the same way the auth layer resolves it for
    // a session — not a hand-built scope, so the share→visibility mapping is under test too.
    let acct = store.get_account(&vera_id).unwrap();
    let vis = store
        .resolve_visibility(&dam_api::AccountIdentity {
            account_id: acct.account_id.clone(),
            username: acct.username.clone(),
            role: acct.role,
        })
        .unwrap();
    assert!(
        !vis.is_full(),
        "vera must be restricted or this test proves nothing"
    );
    let vera_ctx = AuthContext::connected(Some(acct.username.clone()), acct.role.scopes(), vis);
    let ctx = AuthContext::embedded();

    let mut stream = lib
        .subscribe(&vera_ctx, dam_api::SubscribeRequest::default())
        .await
        .unwrap();

    // ── AssetChanged: a favourite toggle on each source ──────────────────────
    let secret_asset: dam_api::AssetId = asset_id_by_name(&app, &admin, "secret_wall.png")
        .await
        .parse()
        .unwrap();
    let shared_asset: dam_api::AssetId = asset_id_by_name(&app, &admin, "brick_red.png")
        .await
        .parse()
        .unwrap();
    for asset in [secret_asset, shared_asset] {
        lib.set_favorite(
            &ctx,
            FavoriteRequest {
                asset,
                favorite: true,
            },
        )
        .await
        .unwrap();
    }
    match next_event(&mut stream).await {
        LibraryEvent::AssetChanged { id, source_id, .. } => {
            assert_eq!(
                id, shared_asset,
                "the secret source's change must be dropped"
            );
            assert_eq!(source_id, Some(shared_sid));
        }
        other => panic!("expected AssetChanged for the shared asset, got {other:?}"),
    }

    // ── AssetRemoved: attribution captured before the row disappears ─────────
    let secret_dup: dam_api::AssetId = asset_id_by_name(&app, &admin, "brick_red_copy.png")
        .await
        .parse()
        .unwrap();
    let shared_other: dam_api::AssetId = asset_id_by_name(&app, &admin, "brick_blue.png")
        .await
        .parse()
        .unwrap();
    for asset in [secret_dup, shared_other] {
        lib.remove_asset(&ctx, &asset, RemoveAsset { block: false })
            .await
            .unwrap();
    }
    match next_event(&mut stream).await {
        LibraryEvent::AssetRemoved { id, source_id } => {
            assert_eq!(
                id, shared_other,
                "the secret source's removal must be dropped"
            );
            assert_eq!(
                source_id,
                Some(shared_sid),
                "a removal must still name its source after the row is gone"
            );
        }
        other => panic!("expected AssetRemoved for the shared asset, got {other:?}"),
    }

    // ── JobProgress: a scan of each source in turn ───────────────────────────
    for sid in [secret_sid, shared_sid] {
        let job = lib
            .submit_scan(
                &ctx,
                ScanRequest {
                    sources: vec![sid],
                    mode: ScanMode::Full,
                },
            )
            .await
            .unwrap();
        wait_job(&lib, &ctx, &job).await;
    }
    // Scans also re-add the two assets removed above, so drain to the first job event.
    let progress = loop {
        match next_event(&mut stream).await {
            LibraryEvent::JobProgress(js) => break js,
            LibraryEvent::AssetAdded(a) => {
                assert_eq!(
                    a.source_id,
                    Some(shared_sid),
                    "a re-scan must not announce the secret source's assets"
                );
            }
            other => panic!("unexpected event while waiting for job progress: {other:?}"),
        }
    };
    assert_eq!(
        progress.sources,
        vec![shared_sid],
        "only the scan confined to her own source may surface"
    );
}

/// Deadline-bounded read of the next event — a filter bug that withholds everything would otherwise
/// hang the test rather than fail it.
async fn next_event(stream: &mut dam_api::service::EventStream<LibraryEvent>) -> LibraryEvent {
    use futures::StreamExt;
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("an allowed event should arrive before the timeout")
        .expect("the stream should still be open")
}

#[tokio::test]
async fn leak_audit_mcp_tools_inherit_the_ceiling() {
    // The MCP surface is the same engine over a different transport: it gets the visibility filter
    // for free *only* if enforcement really lives in the query path (issue #42 leak audit).
    let (app, _s, _l, admin, vera, _shared, _secret, _vid) = leak_world().await;
    let (st, _) = call(
        &app,
        "PUT",
        "/admin/api/flags/mcp_server",
        Some(&admin),
        Some(json!({"value": "read_only", "expected_version": null, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let rpc = |session: Session, tool: &str, args: Value| {
        let app = app.clone();
        let tool = tool.to_string();
        async move {
            let (st, body) = call(
                &app,
                "POST",
                "/mcp",
                Some(&session),
                Some(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": tool, "arguments": args},
                })),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "mcp call failed: {body}");
            // Tool payloads ride as a JSON text block in `content[0].text`.
            let text = body["result"]["content"][0]["text"].as_str().unwrap_or("");
            serde_json::from_str::<Value>(text).unwrap_or(Value::Null)
        }
    };

    // `search` shows only the shared source's assets…
    let out = rpc(vera.clone(), "search", json!({"limit": 50})).await;
    let names: Vec<&str> = out["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(names.len(), 2, "mcp search leaked: {out}");
    assert!(
        !names.contains(&"secret_wall.png"),
        "mcp search leaked: {out}"
    );
    // …and `library_stats` is no aggregate oracle either.
    let stats = rpc(vera, "library_stats", json!({})).await;
    assert_eq!(stats["total"], 2, "mcp stats leaked a total: {stats}");
    // The admin, over the same transport, still sees everything.
    let all = rpc(admin, "search", json!({"limit": 50})).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 4);
}

// ── groups + write shares (resolution rules) ─────────────────────────────────

#[tokio::test]
async fn group_shares_grant_members_and_write_needs_both_gates() {
    let (app, _s, _l, admin, _vera, shared, _secret, _vid) = leak_world().await;
    // A group with a viewer and an editor; the *group* gets a write share on shared-src.
    let viewer2 = create_account(&app, &admin, "viewer2", "viewer").await;
    let editor2 = create_account(&app, &admin, "editor2", "editor").await;
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/groups",
        Some(&admin),
        Some(json!({"name": "Team"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let gid = body["group_id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/admin/api/groups/{gid}/members"),
        Some(&admin),
        Some(json!({"account_ids": [viewer2, editor2]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let share_id = share(&app, &admin, "source", &shared, ("group_id", &gid), "write").await;

    let v2 = login(&app, "viewer2", "password123").await;
    let e2 = login(&app, "editor2", "password123").await;

    // Both members reach the source through the group grant.
    assert_eq!(
        query_names(&app, &v2).await,
        ["brick_blue.png", "brick_red.png"]
    );
    assert_eq!(
        query_names(&app, &e2).await,
        ["brick_blue.png", "brick_red.png"]
    );

    // Write needs *both* gates: the editor (Write scope + write share) succeeds; the viewer with
    // the very same write share still lacks the scope → 403 (issue #42 resolution rule 4).
    let red = asset_id_by_name(&app, &admin, "brick_red.png").await;
    let fav = json!({"asset": red, "favorite": true});
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&e2),
        Some(fav.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "editor + write share must pass");
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&v2),
        Some(fav.clone()),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "viewer + write share must still fail"
    );

    // Revoking the share removes access on the next request — for every member.
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{share_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(query_names(&app, &v2).await.is_empty());
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&e2),
        Some(fav),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "revoked share: the asset is absent again"
    );
}

#[tokio::test]
async fn read_share_does_not_grant_write() {
    let (app, _s, _l, admin, _vera, shared, _secret, _vid) = leak_world().await;
    let erin = create_account(&app, &admin, "erin", "editor").await;
    share(
        &app,
        &admin,
        "source",
        &shared,
        ("account_id", &erin),
        "read",
    )
    .await;
    let e = login(&app, "erin", "password123").await;
    let red = asset_id_by_name(&app, &admin, "brick_red.png").await;
    // Editor scope + read-only share: visible, but writes are Forbidden (share level, not absence).
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{red}"),
        Some(&e),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&e),
        Some(json!({"asset": red, "favorite": true})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn leak_audit_export_respects_the_ceiling() {
    // Engine-level: export (bulk egress) composes the same predicate as browse. A restricted
    // context exporting "the whole library" gets only its reachable slice; explicit hidden ids
    // are silently filtered out.
    let (_app, _s, lib, _admin, _vera, shared, _secret, _vid) = leak_world().await;
    let ectx = AuthContext::embedded();
    let all = lib.query(&ectx, QueryRequest::default()).await.unwrap();
    let secret_id = all
        .items
        .iter()
        .find(|a| a.name == "secret_wall.png")
        .unwrap()
        .id;

    let mut scope = dam_api::VisibilityScope::default();
    scope.sources.insert(shared.parse().unwrap());
    let restricted = AuthContext::connected(
        Some("vera".into()),
        dam_api::Role::Editor.scopes(),
        dam_api::Visibility::Restricted(scope),
    );

    let out = unique_tmp();
    std::fs::create_dir_all(&out).unwrap();
    let whole = lib
        .export(
            &restricted,
            ExportRequest {
                assets: vec![],
                collection: None,
                query: None,
                format: ExportFormat::Json,
                output: out.join("manifest.json").to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        whole.assets, 2,
        "whole-library export must be ceiling-sized"
    );

    let by_id = lib
        .export(
            &restricted,
            ExportRequest {
                assets: vec![secret_id],
                collection: None,
                query: None,
                format: ExportFormat::Json,
                output: out.join("by-id.json").to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(by_id.assets, 0, "a hidden id must not export");
}

// ── admin guard rails ────────────────────────────────────────────────────────

#[tokio::test]
async fn last_admin_cannot_be_demoted_and_share_gc_runs_on_source_removal() {
    let (app, store, _l, admin, _vera, shared, _secret, vera_id) = leak_world().await;

    // The sole admin account resists demotion/deletion (409).
    let (st, body) = call(&app, "GET", "/admin/api/accounts", Some(&admin), None).await;
    assert_eq!(st, StatusCode::OK);
    let admin_id = body
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["username"] == "owner")
        .unwrap()["account_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/admin/api/accounts/{admin_id}"),
        Some(&admin),
        Some(json!({"role": "viewer"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/accounts/{admin_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // Removing the shared source garbage-collects its share rows (cross-DB soft refs).
    assert!(store
        .list_shares()
        .unwrap()
        .iter()
        .any(|s| s.resource_id == shared));
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/sources/{shared}"),
        Some(&admin),
        Some(json!({"keep_metadata": false})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(
        !store
            .list_shares()
            .unwrap()
            .iter()
            .any(|s| s.resource_id == shared),
        "orphaned share rows must be GC'd"
    );
    let _ = vera_id; // world fixture
}

// ── the claim gate rests on the peer socket, not the bind (ADR 0014) ─────────

#[tokio::test]
async fn forwarded_headers_disqualify_the_open_claim() {
    // The dangerous shape: a loopback *peer* (exactly what a same-host reverse proxy presents) on a
    // loopback bind. Everything the server can observe about the socket says "local", so the only
    // remaining signal is the forwarding header the proxy added.
    let (app, _store, _lib) = harness(true).await;
    let bootstrap = enable_accounts(&app).await.expect("bootstrap token minted");
    let body = json!({"username": "attacker", "password": "password123"});

    for header in ["x-forwarded-for", "x-real-ip", "forwarded"] {
        let value = if header == "forwarded" {
            "for=203.0.113.9"
        } else {
            "203.0.113.9"
        };
        let (st, _h, reply) = send(
            &app,
            "POST",
            "/api/v1/auth/claim",
            &[(header.into(), value.into())],
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "a request carrying {header} must not take the open claim path: {reply}"
        );
    }
    // The instance is still unclaimed — nothing was created by the refused attempts.
    let (_, status) = call(&app, "GET", "/api/v1/auth/status", None, None).await;
    assert_eq!(status["unclaimed"], true);

    // The documented off-box route still works *through the proxy*: an Admin-scoped bearer claims
    // from anywhere, forwarded header or not.
    let (st, headers, reply) = send(
        &app,
        "POST",
        "/api/v1/auth/claim",
        &[
            ("x-forwarded-for".into(), "203.0.113.9".into()),
            ("authorization".into(), format!("Bearer {bootstrap}")),
        ],
        Some(json!({"username": "owner", "password": "password123"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "token redemption must survive a proxy: {reply}"
    );
    assert_eq!(reply["account"]["role"], "admin");
    let _ = session_from_reply(&headers, &reply);
}

#[tokio::test]
async fn direct_loopback_peer_still_claims_openly() {
    // The friendly path is untouched: no forwarding header, real loopback peer → open claim.
    let (app, _store, _lib) = harness(true).await;
    enable_accounts(&app).await;
    let _admin = claim(&app, "owner").await;
    let (_, status) = call(&app, "GET", "/api/v1/auth/status", None, None).await;
    assert_eq!(status["unclaimed"], false);
}

// ── flag exposure confirmation (ADR 0004) ───────────────────────────────────

#[tokio::test]
async fn turning_accounts_off_needs_confirmation_while_it_is_the_only_gate() {
    let (app, store, _lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    // `authentication` is still Off — accounts alone raise the effective gate to Token, so dropping
    // them world-opens the catalog *and* the admin plane in one flip.
    assert_eq!(store.auth_mode(), dam_api::admin::AuthMode::Off);

    let off = |confirm: bool| json!({"value": false, "expected_version": null, "confirm": confirm});
    let (st, body) = call(
        &app,
        "PUT",
        "/admin/api/flags/user_accounts",
        Some(&admin),
        Some(off(false)),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "unconfirmed accounts-off must be refused: {body}"
    );
    assert!(store.user_accounts(), "the flag must not have moved");

    // The UI hint says so too.
    let (_, flags) = call(&app, "GET", "/admin/api/flags", Some(&admin), None).await;
    let accounts_flag = flags
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "user_accounts")
        .unwrap();
    assert_eq!(accounts_flag["exposure_increasing"], true);

    // Confirmed, it goes through — and the surface disappears with it.
    let (st, body) = call(
        &app,
        "PUT",
        "/admin/api/flags/user_accounts",
        Some(&admin),
        Some(off(true)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "confirmed flip must succeed: {body}");
    assert!(!store.user_accounts());
}

#[tokio::test]
async fn accounts_off_is_an_ordinary_toggle_once_auth_stands_on_its_own() {
    let (app, store, _lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    // Raise the *raw* auth flag: now the token gate survives an accounts-off flip, so the flip
    // stops being exposure-increasing.
    let (st, _) = call(
        &app,
        "PUT",
        "/admin/api/flags/authentication",
        Some(&admin),
        Some(json!({"value": "token", "expected_version": null, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = call(
        &app,
        "PUT",
        "/admin/api/flags/user_accounts",
        Some(&admin),
        Some(json!({"value": false, "expected_version": null, "confirm": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "no confirmation needed here: {body}");
    assert!(!store.user_accounts());
}

// ── collection membership on the asset record honours the ceiling ────────────

#[tokio::test]
async fn asset_record_hides_unreachable_collection_ids() {
    let (app, _s, _l, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;

    // Two collections both holding the hidden asset; only the first is shared with vera.
    let mut ids = Vec::new();
    for name in ["Shared picks", "Private picks"] {
        let (st, body) = call(
            &app,
            "POST",
            "/api/v1/collections",
            Some(&admin),
            Some(json!({"name": name, "kind": "manual"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let cid = body["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "POST",
            &format!("/api/v1/collections/{cid}/members"),
            Some(&admin),
            Some(json!({"add": [secret_id]})),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        ids.push(cid);
    }
    let (shared_cid, private_cid) = (ids[0].clone(), ids[1].clone());
    share(
        &app,
        &admin,
        "collection",
        &shared_cid,
        ("account_id", &vera_id),
        "read",
    )
    .await;

    // Vera reaches the asset through the shared collection…
    let (st, body) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{secret_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let listed: Vec<&str> = body["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    // …and learns about that collection only. The other one 404s by id, so naming it on the record
    // would be the record contradicting the ceiling.
    assert_eq!(listed, vec![shared_cid.as_str()], "leaked: {body}");
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{private_cid}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The admin, unrestricted, still sees both.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{secret_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body["collections"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn manual_collection_count_is_ceiling_filtered() {
    let (app, _s, _l, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    let visible_id = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/collections",
        Some(&admin),
        Some(json!({"name": "Mixed", "kind": "manual"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cid = body["id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{cid}/members"),
        Some(&admin),
        Some(json!({"add": [secret_id, visible_id]})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    share(
        &app,
        &admin,
        "collection",
        &cid,
        ("account_id", &vera_id),
        "read",
    )
    .await;

    // The collection share grants its members, so vera reaches both — count 2, matching the grid.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{cid}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(body["count"], 2);

    // Now revoke the collection share and grant the source instead: vera reaches only brick_red,
    // and the count must follow the grid rather than announce the hidden member.
    let shares = call(&app, "GET", "/admin/api/shares", Some(&admin), None)
        .await
        .1;
    let sid = shares
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["resource"] == "collection")
        .unwrap()["share_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{sid}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/api/v1/collections", Some(&vera), None).await;
    let seen = list.as_array().unwrap();
    assert_eq!(seen.len(), 1, "reachable as a view over the shared source");
    assert_eq!(
        seen[0]["count"], 1,
        "count must not be a cardinality oracle"
    );
}

// ── share ids are canonical, so the UI and the orphan GC agree ───────────────

#[tokio::test]
async fn share_resource_id_is_canonicalised() {
    let (app, store, _l, admin, _vera, shared, _secret, vera_id) = leak_world().await;
    // The same source id in its unhyphenated ("simple") spelling — `Uuid::parse_str` accepts it.
    let simple = shared.replace('-', "");
    assert_ne!(simple, shared);
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "source", "resource_id": simple,
            "account_id": vera_id, "access": "read",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // Stored and echoed canonically — string-equality consumers (the web share list, the orphan GC)
    // would otherwise never match it, leaving a live grant that cannot be seen or revoked.
    assert_eq!(body["resource_id"], shared);
    assert!(store
        .list_shares()
        .unwrap()
        .iter()
        .all(|s| s.resource_id == shared));

    // And the GC, which binds the canonical form, actually reaches it.
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/sources/{shared}"),
        Some(&admin),
        Some(json!({"keep_metadata": false})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(
        store.list_shares().unwrap().is_empty(),
        "a non-canonically-spelled share must still be GC-able"
    );
}

// ── shares that would grant nothing are refused, not accepted-and-inert ──────

#[tokio::test]
async fn smart_collection_cannot_be_shared() {
    let (app, _s, lib, admin, _vera, _shared, _secret, vera_id) = leak_world().await;
    // Smart folders are CLI-created in v1 — go through the engine directly.
    let smart = lib
        .create_collection(
            &AuthContext::embedded(),
            NewCollection {
                name: "All images".into(),
                kind: CollectionKind::Smart,
                query: Some(QueryRequest::default()),
            },
        )
        .await
        .unwrap();
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "collection", "resource_id": smart.to_string(),
            "account_id": vera_id, "access": "read",
        })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "a smart folder grants nothing — refuse rather than record an inert share: {body}"
    );
    assert!(body["message"].as_str().unwrap().contains("smart folder"));
}

#[tokio::test]
async fn federated_peer_source_cannot_be_shared() {
    // Needs a *real* peer: the federated `add_source` handshakes over HTTP, so the oneshot seam
    // can't stand in. Bind a second server on an ephemeral port with the federation flag on.
    let peer_lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let peer_store = Arc::new(ServerStore::open_in_memory().unwrap());
    peer_store
        .set_flag(
            dam_api::admin::FlagKey::Federation,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
    let peer_app = router(peer_lib, peer_store, "127.0.0.1:0", true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let peer_task = tokio::spawn(async move { axum::serve(listener, peer_app).await.unwrap() });

    let (app, _s, lib, admin, _vera, _shared, _secret, vera_id) = leak_world().await;
    let peer_sid = lib
        .add_source(
            &AuthContext::embedded(),
            AddSource {
                kind: SourceKind::Federated,
                uri: format!("http://{peer_addr}"),
                name: Some("peer".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();

    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "source", "resource_id": peer_sid.to_string(),
            "account_id": vera_id, "access": "read",
        })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "peer reads bypass the ceiling, so the grant would be an empty grid under a real count: {body}"
    );
    assert!(body["message"].as_str().unwrap().contains("federated peer"));
    peer_task.abort();
}

// ── per-asset discussion (issue #82) ────────────────────────────────────────

/// The headline acceptance: two accounts talking on one asset, each message correctly attributed —
/// and a **viewer** among them. Requiring `Scope::Write` to post would have locked out exactly the
/// reviewing art director this feature exists for, so this asserts the looser rule holds.
#[tokio::test]
async fn two_accounts_hold_a_conversation_correctly_attributed() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    // A non-admin account reaches nothing until something is shared with it (issue #42's fail-safe
    // ceiling), so the read share is what puts vera in the room at all.
    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;

    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        Some(json!({"body": "Is this the final bake?"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "owner could not post: {body}");

    // A viewer holds Read but not Write — and must still be able to join the discussion.
    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        Some(json!({"body": "No, see the -v3."})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "a viewer must be able to post: {body}"
    );

    let (st, thread) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let msgs = thread.as_array().unwrap();
    assert_eq!(msgs.len(), 2, "both messages present: {thread}");
    // Oldest first, and each resolved to its author's name across the database boundary.
    assert_eq!(msgs[0]["body"], "Is this the final bake?");
    assert_eq!(msgs[0]["author"]["display"], "owner");
    assert_eq!(msgs[1]["body"], "No, see the -v3.");
    assert_eq!(msgs[1]["author"]["display"], "vera");
}

/// The leak audit entry for discussion: an asset you cannot see has no thread you can read, and no
/// thread you can post to. A message body can quote a hidden path, so this is the same trap the
/// rest of issue #42's read paths guard.
#[tokio::test]
async fn discussion_on_an_unreachable_asset_is_absent() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let hidden = asset_id_by_name(&app, &admin, "secret_wall.png").await;

    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;

    // Something *is* there — the owner can see it — so this is absence, not emptiness.
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{hidden}/comments"),
        Some(&admin),
        Some(json!({"body": "internal only"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{hidden}/comments"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "an unreachable asset's thread must be absent, not forbidden"
    );

    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{hidden}/comments"),
        Some(&vera),
        Some(json!({"body": "nope"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Editing marks the message; deleting leaves a tombstone that keeps a reply's parent intact.
#[tokio::test]
async fn editing_marks_and_deleting_tombstones_without_breaking_replies() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let (_, first) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        Some(json!({"body": "original"})),
    )
    .await;
    let first_id = first["id"].as_str().unwrap().to_string();

    let (st, reply) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        Some(json!({"body": "a reply", "reply_to": first_id})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "reply failed: {reply}");

    let (st, edited) = call(
        &app,
        "PUT",
        &format!("/api/v1/comments/{first_id}"),
        Some(&admin),
        Some(json!({"body": "corrected"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(edited["body"], "corrected");
    assert!(edited["edited_at"].is_i64(), "an edit is marked: {edited}");

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/comments/{first_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (_, thread) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        None,
    )
    .await;
    let msgs = thread.as_array().unwrap();
    assert_eq!(msgs.len(), 2, "the tombstone stays in the thread: {thread}");
    assert!(
        msgs[0]["deleted_at"].is_i64(),
        "deleted message is a tombstone"
    );
    assert_eq!(msgs[0]["body"], "", "a tombstone carries no text");
    assert_eq!(
        msgs[1]["reply_to"], first_id,
        "the reply still points at its (now deleted) parent"
    );
}

/// Moderation boundaries: nobody edits someone else's words — not even an admin — but an admin may
/// remove a message. The asymmetry is the point: an edited message still carries its author's name.
#[tokio::test]
async fn only_the_author_edits_but_an_admin_may_delete() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;

    let (_, posted) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        Some(json!({"body": "vera's words"})),
    )
    .await;
    let id = posted["id"].as_str().unwrap().to_string();

    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/v1/comments/{id}"),
        Some(&admin),
        Some(json!({"body": "put words in her mouth"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "an admin must not rewrite someone's message"
    );

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/comments/{id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NO_CONTENT,
        "an admin may moderate by removing"
    );
}

/// A deleted account's messages survive, rendered as an unresolved author — cascade-deleting them
/// would silently rewrite the project's history.
#[tokio::test]
async fn a_deleted_account_leaves_its_messages_intact() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let vera_id = create_account(&app, &admin, "vera", "editor").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;
    call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        Some(json!({"body": "archiving this one"})),
    )
    .await;

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/accounts/{vera_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert!(st.is_success(), "account delete failed: {st}");

    let (st, thread) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let msgs = thread.as_array().unwrap();
    assert_eq!(msgs.len(), 1, "the message survived its author: {thread}");
    assert_eq!(msgs[0]["body"], "archiving this one");
    assert!(
        msgs[0]["author"]["display"].is_null(),
        "an unresolvable author renders as absent, not as a fabricated name: {thread}"
    );
    assert_eq!(msgs[0]["author"]["id"], vera_id, "the id is kept forever");
}

/// Flag off ⇒ the surface is absent (ADR 0004), not merely empty or forbidden.
#[tokio::test]
async fn the_discussion_surface_404s_while_user_accounts_is_off() {
    let (app, _store, lib) = harness(true).await;
    seed_two_sources(&lib).await;
    // No `enable_accounts` — the flag is off, which is the default posture.
    let assets = lib
        .query(&AuthContext::embedded(), QueryRequest::default())
        .await
        .unwrap();
    let asset = assets.items[0].id;

    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        None,
        Some(json!({"body": "hello?"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
