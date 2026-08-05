//! Feature-flag write semantics (ADR 0004, tech-spec 10): the confirmation gate on a flag whose
//! flip widens exposure, and optimistic concurrency on `expected_version` — over the admin API and
//! at the store level, where no auth stands in the way.
//!
//! The router is exercised with `ServiceExt::oneshot` (no socket bind); the server store is shared
//! (`Arc`) with the test so a flag flip is visible to the next request — the same live-toggle path
//! the admin UI uses.

mod support;

use axum::http::StatusCode;
use dam_server::ServerStore;
use serde_json::json;
use support::{call_token, harness};

// ── flag write semantics ─────────────────────────────────────────────────────

#[tokio::test]
async fn flag_set_confirm_and_optimistic_concurrency() {
    // Stay in Off mode throughout so the owner keeps admin access across requests.
    let (app, _store, _lib) = harness(true).await;

    // Enabling network writes is exposure-increasing → without `confirm`, 400.
    let (st, _) = call_token(
        &app,
        "PUT",
        "/admin/api/flags/network_writes",
        None,
        Some(json!({"value": true})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // With `confirm`, it applies and bumps the version.
    let (st, info) = call_token(
        &app,
        "PUT",
        "/admin/api/flags/network_writes",
        None,
        Some(json!({"value": true, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(info["value"], true);
    assert_eq!(info["version"], 1);

    // A stale `expected_version` is detected, not silently last-write-wins → 409.
    let (st, _) = call_token(
        &app,
        "PUT",
        "/admin/api/flags/network_writes",
        None,
        Some(json!({"value": false, "expected_version": 99})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn confirm_gate_and_concurrency_on_store() {
    // Store-level checks (no auth in the way) for the two guard rails.
    let store = ServerStore::open_in_memory().unwrap();
    use dam_api::admin::{AuthMode, FlagKey, FlagValue, McpMode, SetFlag};

    // Enable MCP read-write is exposure-increasing → confirm required.
    let err = store
        .set_flag(
            FlagKey::McpServer,
            SetFlag {
                value: FlagValue::Mcp(McpMode::ReadWrite),
                expected_version: None,
                confirm: false,
            },
            "t",
        )
        .unwrap_err();
    assert!(matches!(err, dam_api::LibError::BadRequest(_)));
    // With confirm it applies.
    let info = store
        .set_flag(
            FlagKey::McpServer,
            SetFlag {
                value: FlagValue::Mcp(McpMode::ReadWrite),
                expected_version: None,
                confirm: true,
            },
            "t",
        )
        .unwrap();
    assert_eq!(info.flag.version, 1);

    // Stale expected_version → Conflict.
    let err = store
        .set_flag(
            FlagKey::Authentication,
            SetFlag {
                value: FlagValue::Auth(AuthMode::Token),
                expected_version: Some(5),
                confirm: false,
            },
            "t",
        )
        .unwrap_err();
    assert!(matches!(err, dam_api::LibError::Conflict(_)));

    // The audit log recorded the applied change.
    let audit = store.list_audit(10).unwrap();
    assert!(audit
        .iter()
        .any(|e| e.action == "flag.set" && e.target.as_deref() == Some("mcp_server")));
}
