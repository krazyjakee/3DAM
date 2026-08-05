//! The server's out-of-the-box posture (tech-spec 09/10): what a fresh instance exposes under the
//! default Off mode, the response headers that deny worker/service-worker creation, the
//! unauthenticated ops health probes (issue #75), and the exposure warning an anonymous non-local
//! bind must trip.
//!
//! The router is exercised with `ServiceExt::oneshot` (no socket bind); the server store is shared
//! (`Arc`) with the test so a flag flip is visible to the next request.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use support::{call_token, harness};
use tower::ServiceExt;

// ── safe-by-default posture ──────────────────────────────────────────────────

#[tokio::test]
async fn defaults_are_safe_and_admin_reachable_under_off() {
    let (app, _store, _lib) = harness(true).await;

    // Off mode: the local owner reaches the admin surface with no credential.
    let (st, body) = call_token(&app, "GET", "/admin/api/status", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["auth"], "off");
    assert_eq!(body["mcp"], "off");
    assert_eq!(body["network_writes"], false);
    assert_eq!(body["exposed_without_auth"], false);

    // Nine live flags, all at version 0 (unset → defaults): the three exposure flags, the two
    // hosted-mode pipeline toggles (issue #71) which default *on*, the federation peer flag
    // (phase 6, issue #39) which defaults *off* — off means the advertise surface is absent —
    // user accounts (phase 6, issue #42), also off, which keeps the whole accounts/groups/shares
    // surface absent, uploads (issue #80), off, which keeps the one write-into-source route
    // absent, and OIDC login (issue #41), off, which keeps `/api/v1/auth/oidc` absent.
    let (st, flags) = call_token(&app, "GET", "/admin/api/flags", None, None).await;
    assert_eq!(st, StatusCode::OK);
    let flags = flags.as_array().unwrap();
    assert_eq!(flags.len(), 9);
    let flag = |key: &str| flags.iter().find(|f| f["key"] == key).unwrap();
    assert_eq!(flag("auto_thumbnail")["value"], true);
    assert_eq!(flag("auto_analyze")["value"], true);
    assert_eq!(flag("federation")["value"], false);
    assert_eq!(flag("user_accounts")["value"], false);
    assert_eq!(flag("upload")["value"], false);
    assert_eq!(flag("oidc")["value"], false);
    // Admitting identities minted by a third party is exposure, so it needs the same explicit
    // confirm as opening network writes or uploads.
    assert_eq!(flag("oidc")["exposure_increasing"], true);
    // Workload toggles never raise exposure, so they need no confirm.
    assert_eq!(flag("auto_thumbnail")["exposure_increasing"], false);
    // Uploads do: on is the only way bytes enter a source over the network.
    assert_eq!(flag("upload")["exposure_increasing"], true);

    // Federation off ⇒ the advertise surface is absent (404, same mechanism as /mcp).
    let (st, _) = call_token(&app, "GET", "/api/v1/advertise", None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Upload off ⇒ so is the one write-into-source route (issue #80, same mechanism again).
    let (st, _) = call_token(
        &app,
        "POST",
        "/api/v1/upload?source=x&name=y.png",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Reads work under Off (owner holds Read).
    let (st, _) = call_token(&app, "GET", "/api/v1/stats", None, None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn responses_deny_worker_and_service_worker_creation() {
    let (app, _store, _lib) = harness(true).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok()),
        Some("worker-src 'none'")
    );
}

// ── ops health probes (issue #75) ────────────────────────────────────────────

#[tokio::test]
async fn health_probes_are_unauthenticated_and_ready() {
    // Token mode gates the API, but the ops probes must answer with no credential (for a load
    // balancer / systemd watchdog) and are distinct from the versioned API surface.
    let (app, store, _lib) = harness(true).await;
    store
        .set_flag(
            dam_api::admin::FlagKey::Authentication,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Auth(dam_api::admin::AuthMode::Token),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();

    let (st, _) = call_token(&app, "GET", "/healthz", None, None).await;
    assert_eq!(st, StatusCode::OK, "liveness needs no token");
    let (st, _) = call_token(&app, "GET", "/readyz", None, None).await;
    assert_eq!(st, StatusCode::OK, "readiness needs no token and is ready");

    // A gated API route still refuses the anonymous caller — the probes are the only open surface.
    let (st, _) = call_token(&app, "GET", "/api/v1/stats", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

// ── exposure warnings (ADR 0004) ─────────────────────────────────────────────

#[tokio::test]
async fn anonymous_off_localhost_is_flagged_exposed() {
    // Anonymous on a non-localhost bind with no TLS is still world-readable — it must trip the same
    // exposure warning as Off, not report itself safe.
    let (_app, store, _lib) = harness(false).await;
    store
        .set_flag(
            dam_api::admin::FlagKey::Authentication,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Auth(dam_api::admin::AuthMode::Anonymous),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    let status = store.status("0.0.0.0:7878", false, false);
    assert!(
        status.exposed_without_auth,
        "anonymous on 0.0.0.0 without TLS is exposed"
    );
}
