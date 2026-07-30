//! Phase-6 user-accounts coverage (issue #42): the first-run claim, cookie sessions + CSRF, role
//! scope gating, groups + shares, and — most importantly — the **leak audit**: one test per read
//! path proving an unshared asset is *absent* (not forbidden) for a restricted identity.
//!
//! Driven through the real axum router in-process (`ServiceExt::oneshot`, no socket). The store is
//! shared so flag flips and share edits are visible to the next request — the same live path the
//! admin UI uses. There is no ConnectInfo in the oneshot seam, so the claim gate resolves loopback
//! from the bind posture: `harness(true)` behaves like a loopback bind, `harness(false)` like an
//! exposed one with an unknown peer.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
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

async fn harness(localhost_only: bool) -> (axum::Router, Arc<ServerStore>, Arc<EmbeddedLibrary>) {
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", localhost_only);
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

#[tokio::test]
async fn accounts_surface_absent_when_flag_off() {
    let (app, _store, _lib) = harness(true).await;
    for (method, uri) in [
        ("GET", "/api/v1/auth/status"),
        ("POST", "/api/v1/auth/claim"),
        ("POST", "/api/v1/auth/login"),
        ("GET", "/admin/api/accounts"),
        ("GET", "/admin/api/groups"),
        ("GET", "/admin/api/shares"),
    ] {
        let body = (method == "POST").then(|| json!({"username": "x", "password": "yyyyyyyy"}));
        let (st, _) = call(&app, method, uri, None, body).await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "{method} {uri} must 404 while off"
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
    // Exposed bind, unknown peer (no ConnectInfo in the oneshot seam) — not loopback.
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
    let (app, _s, lib, admin, vera, _shared, _secret, _vid) = leak_world().await;
    // The seeding scan/analyze jobs exist and name paths — the admin can list them…
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
    let restricted = AuthContext::connected(Some("vera".into()), dam_api::Role::Editor.scopes())
        .with_visibility(dam_api::Visibility::Restricted(scope));

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
