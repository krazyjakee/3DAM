//! Stateless OIDC bearer-JWT authentication (issue #100), against the real loopback issuer.

mod support;

use axum::http::StatusCode;
use dam_api::accounts::{NewAccount, Role};
use dam_api::admin::{FlagKey, FlagValue, OidcConfig, OidcProvisioning, SetFlag, SetOidcConfig};
use dam_server::ServerStore;
use futures::future::join_all;
use serde_json::json;
use std::sync::Arc;
use support::call_token;
use support::oidc_issuer::TestIssuer;

const CLIENT_ID: &str = "3dam-test-client";
const REDIRECT_URL: &str = "http://127.0.0.1:7878/api/v1/auth/oidc/callback";

struct Harness {
    app: axum::Router,
    store: Arc<ServerStore>,
}

async fn harness(issuer: &TestIssuer, oidc_enabled: bool) -> Harness {
    let (app, store, _lib) = support::harness(true).await;
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

    let (status, owner) = call_token(
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
    let (status, body) = call_token(&h.app, "GET", "/api/v1/whoami", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["username"], "agent");
    assert_eq!(body["restricted"], true);
    let scopes = body["scopes"].as_array().unwrap();
    assert!(scopes.iter().any(|scope| scope == "read"));
    assert!(!scopes.iter().any(|scope| scope == "write"));

    let admin = issuer.bearer_token("owner-sub", CLIENT_ID);
    let (status, body) = call_token(
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
    let (status, _) = call_token(&h.app, "GET", "/api/v1/whoami", Some(&wrong_issuer), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(issuer.jwks_requests(), 0, "unknown issuer must not fetch");

    let unknown_subject = issuer.bearer_token("stranger", CLIENT_ID);
    let (status, _) = call_token(
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
        let (status, _) = call_token(&h.app, "GET", "/api/v1/whoami", Some(&token), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn an_unknown_kid_refresh_is_singleflight_and_accepts_rotation() {
    let issuer = TestIssuer::start().await;
    let h = harness(&issuer, true).await;
    let warm = issuer.bearer_token("agent-sub", CLIENT_ID);
    assert_eq!(
        call_token(&h.app, "GET", "/api/v1/whoami", Some(&warm), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(issuer.jwks_requests(), 1);

    issuer.publish_rotated_key();
    let rotated = issuer.rotated_bearer_token("agent-sub", CLIENT_ID);
    let calls = (0..12).map(|_| call_token(&h.app, "GET", "/api/v1/whoami", Some(&rotated), None));
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
        call_token(&h.app, "GET", "/api/v1/whoami", Some(&warm), None)
            .await
            .0,
        StatusCode::OK
    );
    issuer.fail_jwks(true);
    let unknown = issuer.unknown_key_bearer_token("agent-sub", CLIENT_ID);
    for _ in 0..8 {
        assert_eq!(
            call_token(&h.app, "GET", "/api/v1/whoami", Some(&unknown), None)
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

    let calls = (0..12).map(|_| call_token(&h.app, "GET", "/api/v1/whoami", Some(&token), None));
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
    let (status, _) = call_token(&h.app, "GET", "/api/v1/whoami", Some(&token), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(issuer.jwks_requests(), 0);

    let h = harness(&issuer, true).await;
    let (status, _) = call_token(
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
