//! Phase-5 (Server & web) coverage: the auth layer, the feature-flag store + admin API, and the
//! MCP server surface, driven through the real axum router in-process (tech-spec 09/10/11).
//!
//! The router is exercised with `ServiceExt::oneshot` (no socket bind); the server store is shared
//! (`Arc`) with the test so a flag flip is visible to the next request — the same live-toggle path
//! the admin UI uses.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore, WriteGate};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn unique_tmp() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("3dam-phase5-{}-{}", std::process::id(), nanos))
}

async fn harness(localhost_only: bool) -> (axum::Router, Arc<ServerStore>, Arc<EmbeddedLibrary>) {
    let lib = Arc::new(EmbeddedLibrary::open(&unique_tmp()).await.unwrap());
    let store = Arc::new(ServerStore::open_in_memory().unwrap());
    let app = router(lib.clone(), store.clone(), "127.0.0.1:7878", localhost_only);
    (app, store, lib)
}

/// Fire one request at the router and read back `(status, json-body-or-null)`.
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, val)
}

async fn rpc(
    app: &axum::Router,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/mcp",
        token,
        Some(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})),
    )
    .await
}

// ── safe-by-default posture ──────────────────────────────────────────────────

#[tokio::test]
async fn defaults_are_safe_and_admin_reachable_under_off() {
    let (app, _store, _lib) = harness(true).await;

    // Off mode: the local owner reaches the admin surface with no credential.
    let (st, body) = call(&app, "GET", "/admin/api/status", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["auth"], "off");
    assert_eq!(body["mcp"], "off");
    assert_eq!(body["network_writes"], false);
    assert_eq!(body["exposed_without_auth"], false);

    // Three live flags, all at version 0 (unset → defaults).
    let (st, flags) = call(&app, "GET", "/admin/api/flags", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(flags.as_array().unwrap().len(), 3);

    // Reads work under Off (owner holds Read).
    let (st, _) = call(&app, "GET", "/api/v1/stats", None, None).await;
    assert_eq!(st, StatusCode::OK);
}

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
    let (st, _) = call(&app, "GET", "/api/v1/stats", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Valid read token → 200 on a read.
    let (st, _) = call(&app, "GET", "/api/v1/stats", Some(&secret), None).await;
    assert_eq!(st, StatusCode::OK);

    // Read token on a write route → 403 (missing Write scope). Localhost, so the network ceiling
    // is not the blocker — the identity scope is.
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/jobs/scan",
        Some(&secret),
        Some(json!({"sources": [], "mode": "full"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // A bogus token → 401.
    let (st, _) = call(&app, "GET", "/api/v1/stats", Some("dam_nope"), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

// ── flag write semantics ─────────────────────────────────────────────────────

#[tokio::test]
async fn flag_set_confirm_and_optimistic_concurrency() {
    // Stay in Off mode throughout so the owner keeps admin access across requests.
    let (app, _store, _lib) = harness(true).await;

    // Enabling network writes is exposure-increasing → without `confirm`, 400.
    let (st, _) = call(
        &app,
        "PUT",
        "/admin/api/flags/network_writes",
        None,
        Some(json!({"value": true})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // With `confirm`, it applies and bumps the version.
    let (st, info) = call(
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
    let (st, _) = call(
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
    assert_eq!(info.version, 1);

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

// ── storage & maintenance routes (tech-spec 10 §5) ───────────────────────────

#[tokio::test]
async fn maintenance_usage_wipe_gate_and_audit() {
    let (app, store, _lib) = harness(true).await;

    // Usage is reachable and well-formed on an empty library.
    let (st, usage) = call(&app, "GET", "/admin/api/maintenance/usage", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(usage["asset_count"], 0);
    assert!(usage["thumbnails"]["files"].is_number());

    // Clearing caches on a cold cache succeeds (nothing to free).
    let (st, cache) = call(
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
    let (st, _) = call(
        &app,
        "POST",
        "/admin/api/maintenance/wipe",
        None,
        Some(json!({ "confirm": false })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // With confirm it runs and reports (empty catalog → zero removed).
    let (st, wipe) = call(
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
    let (st, _) = call(
        &app,
        "POST",
        "/admin/api/maintenance/factory-reset",
        None,
        Some(json!({ "confirm": false })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // With confirm → tokens erased and the audit log holds just the reset entry.
    let (st, report) = call(
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

// ── MCP endpoint mount/unmount + write gating ────────────────────────────────

#[tokio::test]
async fn mcp_off_is_404_then_mounts_and_gates_writes() {
    let (app, store, _lib) = harness(true).await;
    use dam_api::admin::{FlagKey, FlagValue, McpMode, SetFlag};

    // Off ⇒ the route is absent (404, not 403) — cannot even be probed.
    let (st, _) = rpc(&app, None, "initialize", json!({})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // ReadOnly ⇒ mounted; initialize works; tools/list has read tools but no write tools.
    store
        .set_flag(
            FlagKey::McpServer,
            SetFlag {
                value: FlagValue::Mcp(McpMode::ReadOnly),
                expected_version: None,
                confirm: false,
            },
            "t",
        )
        .unwrap();
    let (st, body) = rpc(&app, None, "initialize", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["result"]["protocolVersion"].is_string());

    let (_, body) = rpc(&app, None, "tools/list", json!({})).await;
    let names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"search".to_string()));
    assert!(
        !names.contains(&"convert".to_string()),
        "read-only must hide write tools"
    );

    // ReadWrite ⇒ write tools appear (localhost owner holds Write; gate permits).
    store
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
    let (_, body) = rpc(&app, None, "tools/list", json!({})).await;
    let names: Vec<String> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"convert".to_string()),
        "read-write must expose write tools"
    );
    assert!(names.contains(&"add_source".to_string()));
}

// ── MCP adapter (the stdio-equivalent surface) ───────────────────────────────

#[tokio::test]
async fn mcp_adapter_stdio_is_locally_trusted() {
    let lib = Arc::new(EmbeddedLibrary::open(&unique_tmp()).await.unwrap());
    let library: Arc<dyn dam_api::service::LibraryService> = lib;
    let adapter = dam_server::McpAdapter::new(library, WriteGate::local_stdio());
    let ctx = dam_api::service::AuthContext::embedded();

    // A notification (no id) yields no reply.
    let none = adapter
        .handle_message(
            &ctx,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
    assert!(none.is_none());

    // stdio is locally trusted → write tools are present.
    let reply = adapter
        .handle_message(
            &ctx,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await
        .unwrap();
    let names: Vec<String> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"convert".to_string()));

    // A read tool executes end-to-end against the empty library.
    let reply = adapter
        .handle_message(
            &ctx,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "library_stats", "arguments": {}}}),
        )
        .await
        .unwrap();
    assert_eq!(reply["result"]["isError"], false);
    assert_eq!(reply["result"]["structuredContent"]["total"], 0);

    // resource templates advertise the asset/source/collection addressing.
    let reply = adapter
        .handle_message(
            &ctx,
            json!({"jsonrpc": "2.0", "id": 3, "method": "resources/templates/list"}),
        )
        .await
        .unwrap();
    assert!(
        reply["result"]["resourceTemplates"]
            .as_array()
            .unwrap()
            .len()
            >= 3
    );
}
