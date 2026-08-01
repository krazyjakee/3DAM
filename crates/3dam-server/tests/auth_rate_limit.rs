//! Wire-level regression for the unauthenticated auth limiter (issue #126).

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use dam_api::admin::{FlagKey, FlagValue, SetFlag};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn unique_tmp() -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "3dam-auth-rate-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

#[tokio::test]
async fn rotating_usernames_and_forwarded_headers_end_in_429_with_retry_after() {
    let lib = Arc::new(
        EmbeddedLibrary::open_with(&unique_tmp(), dam_core::ResourceOptions::ungoverned())
            .await
            .unwrap(),
    );
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
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

    let peer: SocketAddr = "203.0.113.9:54321".parse().unwrap();
    let app = router(lib, store, "0.0.0.0:7878", false).layer(axum::Extension(ConnectInfo(peer)));

    for attempt in 0..20 {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/login")
            // This is an untrusted direct peer. Rotating a forged forwarding value must not rotate
            // the limiter key.
            .header("x-forwarded-for", format!("198.51.100.{attempt}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "username": format!("rotated-{attempt}"),
                    "password": "not-a-password",
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("x-forwarded-for", "192.0.2.200")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"username": "one-more-name", "password": "not-a-password"}).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .expect("429 must tell generic clients when to retry")
        .to_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(retry_after >= 1);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "rate_limited");
    assert_eq!(body["detail"]["retry_after"], retry_after);
}
