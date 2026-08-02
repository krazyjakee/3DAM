//! Stateless OIDC bearer-JWT authentication (issue #100), against the real loopback issuer.

mod support;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use dam_api::accounts::{NewAccount, Role};
use dam_api::admin::{FlagKey, FlagValue, OidcConfig, OidcProvisioning, SetFlag, SetOidcConfig};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use futures::future::join_all;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use support::oidc_issuer::TestIssuer;
use tower::ServiceExt;

const CLIENT_ID: &str = "3dam-test-client";
const REDIRECT_URL: &str = "http://127.0.0.1:7878/api/v1/auth/oidc/callback";

fn unique_tmp() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("3dam-oidc-bearer-{}-{n}", std::process::id()))
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

struct Harness {
    app: axum::Router,
    store: Arc<ServerStore>,
}

async fn harness(issuer: &TestIssuer, oidc_enabled: bool) -> Harness {
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    if oidc_enabled {
        store
            .set_flag(
                FlagKey::Oidc,
                SetFlag {
                    value: FlagValue::Bool(true),
                    expected_version: None,
                    confirm: true,
                },
                "test",
            )
            .unwrap();
    }
    store
        .set_flag(
            FlagKey::UserAccounts,
            SetFlag {
                value: FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
    store
        .set_oidc_config(
            &SetOidcConfig {
                config: OidcConfig {
                    issuer: issuer.issuer_url(),
                    client_id: CLIENT_ID.into(),
                    redirect_url: REDIRECT_URL.into(),
                    scopes: vec![],
                    provisioning: OidcProvisioning::Linked,
                },
                client_secret: None,
            },
            "test",
        )
        .unwrap();

    let peer: SocketAddr = "127.0.0.1:54321".parse().unwrap();
    let app = router(lib, store.clone(), "127.0.0.1:7878", true)
        .layer(axum::Extension(ConnectInfo(peer)));
    let (status, owner) = send(
        &app,
        "POST",
        "/api/v1/auth/claim",
        None,
        Some(json!({"username": "owner", "password": "password123"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim failed: {owner}");
    let owner_id = owner["account"]["account_id"].as_str().unwrap();
    store
        .link_oidc_identity(&issuer.issuer_url(), "owner-sub", owner_id, "test")
        .unwrap();
    let agent = store
        .create_account(
            &NewAccount {
                username: "agent".into(),
                password: "password123".into(),
                display_name: None,
                role: Role::Viewer,
            },
            "test",
        )
        .unwrap();
    store
        .link_oidc_identity(&issuer.issuer_url(), "agent-sub", &agent.account_id, "test")
        .unwrap();
    Harness { app, store }
}

#[tokio::test]
async fn a_linked_bearer_uses_the_accounts_scope_and_visibility_seam() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, true).await;
    let token = issuer.bearer_token("agent-sub", CLIENT_ID);
    let (status, body) = send(&h.app, "GET", "/api/v1/whoami", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["username"], "agent");
    assert_eq!(body["restricted"], true);
    let scopes = body["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|scope| scope == "read"));
    assert!(!scopes.iter().any(|scope| scope == "write"));

    let admin = issuer.bearer_token("owner-sub", CLIENT_ID);
    let (status, body) = send(
        &h.app,
        "PUT",
        "/admin/api/flags/auto_thumbnail",
        Some(&admin),
        Some(json!({"value": true, "expected_version": null, "confirm": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(h
        .store
        .list_audit(20)
        .unwrap()
        .iter()
        .any(|entry| entry.actor == "oidc-bearer:owner"));
}

#[tokio::test]
async fn unknown_issuer_and_subject_are_rejected() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, true).await;
    let wrong_issuer =
        issuer.bearer_token_with_issuer("agent-sub", CLIENT_ID, "https://issuer.example.invalid");
    let (status, _) = send(&h.app, "GET", "/api/v1/whoami", Some(&wrong_issuer), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(issuer.jwks_requests(), 0, "unknown issuer must not fetch");

    let unknown_subject = issuer.bearer_token("stranger", CLIENT_ID);
    let (status, _) = send(
        &h.app,
        "GET",
        "/api/v1/whoami",
        Some(&unknown_subject),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn audience_expiry_and_signature_are_verified() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, true).await;
    let tokens = [
        issuer.bearer_token_with_audience("agent-sub", "some-other-service"),
        issuer.expired_bearer_token("agent-sub", CLIENT_ID),
        issuer.future_bearer_token("agent-sub", CLIENT_ID),
        issuer.bad_signature_bearer_token("agent-sub", CLIENT_ID),
    ];
    for token in tokens {
        let (status, _) = send(&h.app, "GET", "/api/v1/whoami", Some(&token), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn an_unknown_kid_refresh_is_singleflight_and_accepts_rotation() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, true).await;
    let warm = issuer.bearer_token("agent-sub", CLIENT_ID);
    assert_eq!(
        send(&h.app, "GET", "/api/v1/whoami", Some(&warm), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(issuer.jwks_requests(), 1);

    issuer.publish_rotated_key();
    let rotated = issuer.rotated_bearer_token("agent-sub", CLIENT_ID);
    let calls = (0..12).map(|_| send(&h.app, "GET", "/api/v1/whoami", Some(&rotated), None));
    for (status, body) in join_all(calls).await {
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(
        issuer.jwks_requests(),
        2,
        "concurrent unknown-kid requests must coalesce into one refresh"
    );
}

#[tokio::test]
async fn failed_unknown_kid_refreshes_are_rate_bounded() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, true).await;
    let warm = issuer.bearer_token("agent-sub", CLIENT_ID);
    assert_eq!(
        send(&h.app, "GET", "/api/v1/whoami", Some(&warm), None)
            .await
            .0,
        StatusCode::OK
    );
    issuer.fail_jwks(true);
    let unknown = issuer.unknown_key_bearer_token("agent-sub", CLIENT_ID);
    for _ in 0..8 {
        assert_eq!(
            send(&h.app, "GET", "/api/v1/whoami", Some(&unknown), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        issuer.jwks_requests(),
        2,
        "only one failed refresh is allowed"
    );
}

#[tokio::test]
async fn an_initial_jwks_outage_is_rate_bounded() {
    let issuer = TestIssuer::start().await;
    issuer.fail_jwks(true);
    let h = harness(&issuer, true).await;
    let token = issuer.bearer_token("agent-sub", CLIENT_ID);

    let calls = (0..12).map(|_| send(&h.app, "GET", "/api/v1/whoami", Some(&token), None));
    for (status, _) in join_all(calls).await {
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
    assert_eq!(
        issuer.jwks_requests(),
        1,
        "a failed cold-cache fetch must start the same retry cooldown as a stale-cache fetch"
    );
}

#[tokio::test]
async fn oidc_flag_off_and_native_token_precedence_never_fetch_jwks() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, false).await;
    let token = issuer.bearer_token("agent-sub", CLIENT_ID);
    let (status, _) = send(&h.app, "GET", "/api/v1/whoami", Some(&token), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(issuer.jwks_requests(), 0);

    let h = harness(&issuer, true).await;
    let (status, _) = send(
        &h.app,
        "GET",
        "/api/v1/whoami",
        Some("dam_invalid.looks.jwt"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(issuer.jwks_requests(), 0);
}
