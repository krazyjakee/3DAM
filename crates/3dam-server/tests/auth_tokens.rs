//! Token auth at the front door (tech-spec 10 §4): token mode gating reads while scopes gate
//! writes, the network ceiling capping *implicit* trust but never a verified credential, the
//! bootstrap owner token minted when authentication is switched on, and `whoami` reporting the
//! caller's own scopes to a scope-aware UI.
//!
//! The router is exercised with `ServiceExt::oneshot` (no socket bind); the server store is shared
//! (`Arc`) with the test so a token mint or flag flip is visible to the next request.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use support::{call_token, harness, mint, set_auth};
use tower::ServiceExt;

// ── token auth ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn token_mode_gates_reads_and_scopes_gate_writes() {
    let (app, store, _lib) = harness(true).await;

    // Mint a read-only token *before* turning auth on (bootstrap under Off), then require tokens.
    let reply = store
        .create_token(
            dam_api::admin::NewToken {
                label: "reader".into(),
                scopes: dam_api::service::Scopes::none().with(dam_api::service::Scope::Read),
                expires: None,
            },
            "test",
        )
        .unwrap();
    let secret = reply.secret;
    store
        .set_flag(
            dam_api::admin::FlagKey::Authentication,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Auth(dam_api::admin::AuthMode::Token),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();

    // No credential → 401.
    let (st, _) = call_token(&app, "GET", "/api/v1/stats", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Valid read token → 200 on a read.
    let (st, _) = call_token(&app, "GET", "/api/v1/stats", Some(&secret), None).await;
    assert_eq!(st, StatusCode::OK);

    // Derived credentials never echo the parent and cannot cross their transport/path boundary.
    let (st, ws_ticket) = call_token(&app, "POST", "/api/v1/ws-ticket", Some(&secret), None).await;
    assert_eq!(st, StatusCode::OK);
    let ws_ticket = ws_ticket["ticket"].as_str().unwrap();
    assert!(!ws_ticket.contains(&secret));
    let (st, _) = call_token(
        &app,
        "GET",
        &format!("/api/v1/stats?ticket={ws_ticket}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "WS ticket cannot call JSON APIs"
    );

    let asset = uuid::Uuid::now_v7();
    let other = uuid::Uuid::now_v7();
    let (st, media_ticket) = call_token(
        &app,
        "POST",
        "/api/v1/media-ticket",
        Some(&secret),
        Some(json!({"target": format!("/api/v1/assets/{asset}/content")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let media_ticket = media_ticket["ticket"].as_str().unwrap();
    assert!(!media_ticket.contains(&secret));
    let (st, _) = call_token(
        &app,
        "GET",
        &format!("/api/v1/assets/{other}/content?ticket={media_ticket}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "media ticket is exact-asset bound"
    );

    // Read token on a write route → 403 (missing Write scope). Localhost, so the network ceiling
    // is not the blocker — the identity scope is.
    let (st, _) = call_token(
        &app,
        "POST",
        "/api/v1/jobs/scan",
        Some(&secret),
        Some(json!({"sources": [], "mode": "full"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // A bogus token → 401.
    let (st, _) = call_token(&app, "GET", "/api/v1/stats", Some("dam_nope"), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Long-lived bearers are never URI credentials (issue #128). Even a valid sentinel in the
    // query is ignored; browser media/WS use narrow derived tickets minted via a header request.
    let (st, _) = call_token(
        &app,
        "GET",
        &format!("/api/v1/stats?token={secret}"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = call_token(&app, "GET", "/api/v1/stats?token=dam_nope", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

// ── front-door auth: the network ceiling caps implicit trust, not identities ─

#[tokio::test]
async fn verified_write_token_bypasses_network_ceiling() {
    // Bound beyond localhost, network_writes off: a *verified* write-scoped token still writes —
    // past the front door, the credential's scopes alone decide (tech-spec 10 §4.2).
    let (app, store, _lib) = harness(false).await;
    use dam_api::service::Scope;
    let secret = mint(
        &store,
        "writer",
        dam_api::service::Scopes::none()
            .with(Scope::Read)
            .with(Scope::Write),
    );
    set_auth(&store, dam_api::admin::AuthMode::Token);

    let (st, body) = call_token(
        &app,
        "POST",
        "/api/v1/collections",
        Some(&secret),
        Some(json!({"name": "from-the-network"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "verified write token must not be capped by network_writes: {body}"
    );
}

#[tokio::test]
async fn implicit_trust_still_capped_by_network_ceiling() {
    // Auth off + non-loopback bind: the owner-posture caller has Write scope but no verified
    // identity, so the network ceiling still applies until the flag opens it.
    let (app, store, _lib) = harness(false).await;

    let (st, body) = call_token(
        &app,
        "POST",
        "/api/v1/collections",
        None,
        Some(json!({"name": "blocked"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(
        body["code"], "disabled",
        "implicit trust is read-only to the network"
    );

    store
        .set_flag(
            dam_api::admin::FlagKey::NetworkWrites,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
    let (st, _) = call_token(
        &app,
        "POST",
        "/api/v1/collections",
        None,
        Some(json!({"name": "now-allowed"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "network_writes opens implicit-trust writes"
    );
}

#[tokio::test]
async fn read_token_never_gains_write_from_ceiling_bypass() {
    // The bypass is about the ceiling, not the scopes: a verified read-only token still lacks Write.
    let (app, store, _lib) = harness(false).await;
    let secret = mint(
        &store,
        "reader",
        dam_api::service::Scopes::none().with(dam_api::service::Scope::Read),
    );
    set_auth(&store, dam_api::admin::AuthMode::Token);

    let (st, body) = call_token(
        &app,
        "POST",
        "/api/v1/jobs/scan",
        Some(&secret),
        Some(json!({"sources": [], "mode": "full"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(
        body["code"], "forbidden",
        "scope miss, not the ceiling: {body}"
    );
}

#[tokio::test]
async fn version_reports_auth_mode_publicly() {
    // The one public route carries the auth posture so a client can render its login gate without
    // provoking 401s.
    let (app, store, _lib) = harness(true).await;

    let (st, body) = call_token(&app, "GET", "/api/version", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["auth"], "off");

    set_auth(&store, dam_api::admin::AuthMode::Token);
    let (st, body) = call_token(&app, "GET", "/api/version", None, None).await;
    assert_eq!(st, StatusCode::OK, "version stays public in token mode");
    assert_eq!(body["auth"], "token");
}

#[tokio::test]
async fn enabling_auth_mints_a_bootstrap_owner_token() {
    // Turning authentication on with zero admin credentials must hand the operator a key in the
    // same motion — an instance is never gated with no holder of a credential.
    let (app, store, _lib) = harness(true).await;

    // The owner (auth off) enables token mode over the admin API with an empty token store.
    let (st, body) = call_token(
        &app,
        "PUT",
        "/admin/api/flags/authentication",
        None,
        Some(json!({"value": "token", "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        body["value"], "token",
        "flag fields stay flattened in the reply"
    );
    let secret = body["bootstrap_token"]["secret"]
        .as_str()
        .expect("enabling auth on an empty token store mints the owner token")
        .to_string();
    assert_eq!(body["bootstrap_token"]["label"], "owner");

    // The minted secret is a working admin credential — the flip never locks the operator out.
    let (st, status) = call_token(&app, "GET", "/admin/api/status", Some(&secret), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(status["auth"], "token");

    // Flipping again mints nothing: an admin credential now exists.
    let (st, body) = call_token(
        &app,
        "PUT",
        "/admin/api/flags/authentication",
        Some(&secret),
        Some(json!({"value": "anonymous", "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body.get("bootstrap_token").is_none() || body["bootstrap_token"].is_null(),
        "no re-mint once an admin token exists: {body}"
    );

    // And the startup path is idempotent too.
    assert!(store
        .bootstrap_owner_token_if_needed("startup")
        .unwrap()
        .is_none());
}

// ── whoami: the caller's own scopes, for a scope-aware UI ─────────────────────

#[tokio::test]
async fn whoami_reports_the_callers_scopes() {
    let (app, store, _lib) = harness(true).await;

    // Under Off, the unauthenticated local owner is a full-trust, non-anonymous identity.
    let (st, body) = call_token(&app, "GET", "/api/v1/whoami", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["anonymous"], false);
    let scopes = body["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|s| s == "write"));
    assert!(scopes.iter().any(|s| s == "admin"));

    // Mint a read-only token, require tokens, then whoami reflects exactly that token's scopes.
    let secret = store
        .create_token(
            dam_api::admin::NewToken {
                label: "reader".into(),
                scopes: dam_api::service::Scopes::none().with(dam_api::service::Scope::Read),
                expires: None,
            },
            "test",
        )
        .unwrap()
        .secret;
    store
        .set_flag(
            dam_api::admin::FlagKey::Authentication,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Auth(dam_api::admin::AuthMode::Token),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();

    // No credential under Token mode → 401 (the client's cue to show a login gate).
    let (st, _) = call_token(&app, "GET", "/api/v1/whoami", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // The read token sees itself: read but not write, non-anonymous, its label as identity.
    let (st, body) = call_token(&app, "GET", "/api/v1/whoami", Some(&secret), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["anonymous"], false);
    assert_eq!(body["identity"], "reader");
    let scopes = body["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|s| s == "read"));
    assert!(!scopes.iter().any(|s| s == "write"));
}

#[tokio::test]
async fn unauthorized_carries_www_authenticate() {
    // A 401 advertises the bearer scheme (RFC 7235 §3.1) so generic HTTP clients know how to auth.
    let (app, store, _lib) = harness(true).await;
    store
        .set_flag(
            dam_api::admin::FlagKey::Authentication,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Auth(dam_api::admin::AuthMode::Token),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/stats")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
}

#[tokio::test]
async fn scopeless_token_is_rejected_at_mint() {
    // A token with no scopes authenticates yet can do nothing — reject it rather than hand back a dud.
    let (_app, store, _lib) = harness(true).await;
    let err = store
        .create_token(
            dam_api::admin::NewToken {
                label: "empty".into(),
                scopes: dam_api::service::Scopes::none(),
                expires: None,
            },
            "test",
        )
        .unwrap_err();
    assert!(matches!(err, dam_api::LibError::BadRequest(_)), "{err:?}");
}
