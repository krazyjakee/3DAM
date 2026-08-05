//! The MCP surface (ADR 0003, tech-spec 11): the HTTP endpoint that is a 404 until the flag mounts
//! it and then gates writes, the stdio-equivalent adapter's local trust, and the write tools
//! following the caller's token scopes rather than the network ceiling.
//!
//! The router is exercised with `ServiceExt::oneshot` (no socket bind); the server store is shared
//! (`Arc`) with the test so a flag flip is visible to the next request.

mod support;

use axum::http::StatusCode;
use dam_core::EmbeddedLibrary;
use dam_server::WriteGate;
use serde_json::{json, Value};
use std::sync::Arc;
use support::{harness, mint, rpc, set_auth, unique_tmp};

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
    let lib = Arc::new(
        EmbeddedLibrary::open_with(
            &unique_tmp("phase5"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
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

#[tokio::test]
async fn mcp_write_tools_follow_token_scope_not_ceiling() {
    let (app, store, _lib) = harness(false).await;
    use dam_api::admin::{FlagKey, FlagValue, McpMode, SetFlag};
    use dam_api::service::Scope;

    store
        .set_flag(
            FlagKey::McpServer,
            SetFlag {
                value: FlagValue::Mcp(McpMode::ReadWrite),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();

    let write_tools = |body: &Value| -> bool {
        body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "convert")
    };

    // Implicit trust (auth off, non-loopback, network_writes off): write tools hidden.
    let (_, body) = rpc(&app, None, "tools/list", json!({})).await;
    assert!(
        !write_tools(&body),
        "implicit trust beyond localhost sees read tools only"
    );

    // A verified write token sees the write tools — ceiling does not apply to identities.
    let secret = mint(
        &store,
        "agent",
        dam_api::service::Scopes::none()
            .with(Scope::Read)
            .with(Scope::Write)
            .with(Scope::McpUse),
    );
    set_auth(&store, dam_api::admin::AuthMode::Token);
    let (_, body) = rpc(&app, Some(&secret), "tools/list", json!({})).await;
    assert!(write_tools(&body), "verified write token sees write tools");

    // ReadOnly stays a surface gate: it hides write tools from everyone, tokens included.
    store
        .set_flag(
            FlagKey::McpServer,
            SetFlag {
                value: FlagValue::Mcp(McpMode::ReadOnly),
                expected_version: None,
                confirm: false,
            },
            "test",
        )
        .unwrap();
    let (_, body) = rpc(&app, Some(&secret), "tools/list", json!({})).await;
    assert!(
        !write_tools(&body),
        "ReadOnly hides write tools even from write tokens"
    );
}
