//! Storage & maintenance routes (tech-spec 10 §5): cache usage/clear, the wipe gate, the factory
//! reset that must be confirmed and erases tokens — and the audit rows each one leaves behind.
//!
//! The router is exercised with `ServiceExt::oneshot` (no socket bind); the server store is shared
//! (`Arc`) with the test so the audit log is readable straight after the request that wrote it.

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{call_token, harness};

// ── storage & maintenance routes (tech-spec 10 §5) ───────────────────────────

#[tokio::test]
async fn maintenance_usage_wipe_gate_and_audit() {
    let (app, store, _lib) = harness(true).await;

    // Usage is reachable and well-formed on an empty library.
    let (st, usage) = call_token(&app, "GET", "/admin/api/maintenance/usage", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(usage["asset_count"], 0);
    assert!(usage["thumbnails"]["files"].is_number());

    // Clearing caches on a cold cache succeeds (nothing to free).
    let (st, cache) = call_token(
        &app,
        "POST",
        "/admin/api/maintenance/clear-cache",
        None,
        Some(json!({ "target": "all" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(cache["files_deleted"], 0);

    // Wipe without confirm is rejected — the machine form of warn-and-confirm.
    let (st, _) = call_token(
        &app,
        "POST",
        "/admin/api/maintenance/wipe",
        None,
        Some(json!({ "confirm": false })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // With confirm it runs and reports (empty catalog → zero removed).
    let (st, wipe) = call_token(
        &app,
        "POST",
        "/admin/api/maintenance/wipe",
        None,
        Some(json!({ "confirm": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(wipe["assets_removed"], 0);

    // Every mutating maintenance op left an audit trail.
    let audit = store.list_audit(20).unwrap();
    assert!(audit.iter().any(|e| e.action == "maintenance.clear_cache"));
    assert!(audit.iter().any(|e| e.action == "maintenance.wipe"));
}

#[tokio::test]
async fn factory_reset_requires_confirm_and_erases_tokens() {
    let (app, store, _lib) = harness(true).await;

    // A token exists before the reset.
    store
        .create_token(
            dam_api::admin::NewToken {
                label: "doomed".into(),
                scopes: dam_api::service::Scopes::none().with(dam_api::service::Scope::Read),
                expires: None,
            },
            "test",
        )
        .unwrap();
    assert_eq!(store.list_tokens().unwrap().len(), 1);

    // Without confirm → rejected.
    let (st, _) = call_token(
        &app,
        "POST",
        "/admin/api/maintenance/factory-reset",
        None,
        Some(json!({ "confirm": false })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // With confirm → tokens erased and the audit log holds just the reset entry.
    let (st, report) = call_token(
        &app,
        "POST",
        "/admin/api/maintenance/factory-reset",
        None,
        Some(json!({ "confirm": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(report["tokens_removed"], 1);
    assert!(store.list_tokens().unwrap().is_empty());
    let audit = store.list_audit(20).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "maintenance.factory_reset");
}
