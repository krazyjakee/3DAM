//! Getting *in*: the first-run claim, cookie sessions + CSRF, login lockout, role→scope gating,
//! and the network posture the claim window rests on (issue #42; ADR 0014).
//!
//! Driven through the real axum router in-process (`ServiceExt::oneshot`, no socket). The oneshot
//! seam crosses no socket, so the harness injects the `ConnectInfo` extension the serve stack would
//! register: `harness(true)` presents a loopback peer, and `harness(false)` a public one. The claim
//! gate reads that peer and nothing else about the bind (ADR 0014 — a loopback *bind* is not
//! evidence of a local caller behind a same-host proxy).

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{
    call, claim, create_account, enable_accounts, harness, login, send, session_from_reply,
};

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
