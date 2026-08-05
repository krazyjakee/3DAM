//! Shares as a grant mechanism: groups, read-vs-write resolution, the upload picker following a
//! write grant, the export ceiling, the admin guard rails around the last admin and orphaned share
//! rows, and shares handed to a *federated peer* (issue #42; issue #127).
//!
//! Driven through the real axum router in-process (`ServiceExt::oneshot`, no socket) over a store
//! shared with the test, so a share edit is visible to the next request. The federated case is the
//! exception: its handshake is real HTTP, so it binds a second server on an ephemeral port.

mod support;

use axum::body::Body;
use axum::http::StatusCode;
use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use dam_core::EmbeddedLibrary;
use dam_server::{router, ServerStore};
use serde_json::json;
use std::sync::Arc;
use support::{
    asset_id_by_name, call, create_account, leak_world, login, query_names, share, unique_tmp,
    upload_body_call, upload_call, write_png,
};

// ── groups + write shares (resolution rules) ─────────────────────────────────

#[tokio::test]
async fn group_shares_grant_members_and_write_needs_both_gates() {
    let (app, _s, _l, admin, _vera, shared, _secret, _vid) = leak_world().await;
    // A group with a viewer and an editor; the *group* gets a write share on shared-src.
    let viewer2 = create_account(&app, &admin, "viewer2", "viewer").await;
    let editor2 = create_account(&app, &admin, "editor2", "editor").await;
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/groups",
        Some(&admin),
        Some(json!({"name": "Team"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let gid = body["group_id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/admin/api/groups/{gid}/members"),
        Some(&admin),
        Some(json!({"account_ids": [viewer2, editor2]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let share_id = share(&app, &admin, "source", &shared, ("group_id", &gid), "write").await;

    let v2 = login(&app, "viewer2", "password123").await;
    let e2 = login(&app, "editor2", "password123").await;

    // Both members reach the source through the group grant.
    assert_eq!(
        query_names(&app, &v2).await,
        ["brick_blue.png", "brick_red.png"]
    );
    assert_eq!(
        query_names(&app, &e2).await,
        ["brick_blue.png", "brick_red.png"]
    );

    // Write needs *both* gates: the editor (Write scope + write share) succeeds; the viewer with
    // the very same write share still lacks the scope → 403 (issue #42 resolution rule 4).
    let red = asset_id_by_name(&app, &admin, "brick_red.png").await;
    let fav = json!({"asset": red, "favorite": true});
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&e2),
        Some(fav.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "editor + write share must pass");
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&v2),
        Some(fav.clone()),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "viewer + write share must still fail"
    );

    // Revoking the share removes access on the next request — for every member.
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{share_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(query_names(&app, &v2).await.is_empty());
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&e2),
        Some(fav),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "revoked share: the asset is absent again"
    );
}

#[tokio::test]
async fn read_share_does_not_grant_write() {
    let (app, _s, _l, admin, _vera, shared, _secret, _vid) = leak_world().await;
    let erin = create_account(&app, &admin, "erin", "editor").await;
    share(
        &app,
        &admin,
        "source",
        &shared,
        ("account_id", &erin),
        "read",
    )
    .await;
    let e = login(&app, "erin", "password123").await;
    let red = asset_id_by_name(&app, &admin, "brick_red.png").await;
    // Editor scope + read-only share: visible, but writes are Forbidden (share level, not absence).
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{red}"),
        Some(&e),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/assets/favorite",
        Some(&e),
        Some(json!({"asset": red, "favorite": true})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn upload_source_picker_and_route_follow_write_grants_immediately() {
    let (app, store, _lib, admin, vera, shared, secret, _vid) = leak_world().await;
    store
        .set_flag(
            dam_api::admin::FlagKey::Upload,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();

    let staged = unique_tmp("accounts").with_extension("png");
    write_png(&staged, [90, 120, 180]);
    let bytes = std::fs::read(&staged).unwrap();

    // Vera can read the source, but her viewer role lacks Write. The source remains visible while
    // the picker gives the capability reason, and the route refuses the same caller.
    let (st, sources) = call(&app, "GET", "/api/v1/sources", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sources.as_array().unwrap().len(), 1);
    assert_eq!(sources[0]["id"], shared);
    assert_eq!(sources[0]["writable"], false);
    assert_eq!(
        sources[0]["writable_reason"],
        "read-only — write scope is required"
    );
    let (st, _) = upload_call(&app, &vera, &shared, "viewer.png", bytes.clone()).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    let erin_id = create_account(&app, &admin, "upload-editor", "editor").await;
    let read_share = share(
        &app,
        &admin,
        "source",
        &shared,
        ("account_id", &erin_id),
        "read",
    )
    .await;
    let erin = login(&app, "upload-editor", "password123").await;

    // Write scope + a read share is still read-only, both in the picker and at the write boundary.
    let (st, sources) = call(&app, "GET", "/api/v1/sources", Some(&erin), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sources.as_array().unwrap().len(), 1);
    assert_eq!(sources[0]["writable"], false);
    assert_eq!(
        sources[0]["writable_reason"],
        "read-only — a write share is required"
    );
    let (st, source) = call(
        &app,
        "GET",
        &format!("/api/v1/sources/{shared}"),
        Some(&erin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(source["writable"], false);
    let body_polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poll_marker = body_polled.clone();
    let guarded_body = Body::from_stream(futures::stream::once(async move {
        poll_marker.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"must not be read"))
    }));
    let (st, _) = upload_body_call(&app, &erin, &shared, "read-share.png", guarded_body).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(
        !body_polled.load(std::sync::atomic::Ordering::SeqCst),
        "a known-forbidden upload body must not be polled"
    );

    // An unshared destination is absent, including at the direct upload route.
    let (st, _) = upload_call(&app, &erin, &secret, "hidden.png", bytes.clone()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Replacing the read grant with a write grant takes effect on the very next request: no login
    // refresh or server restart is needed.
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{read_share}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let write_share = share(
        &app,
        &admin,
        "source",
        &shared,
        ("account_id", &erin_id),
        "write",
    )
    .await;
    let (st, sources) = call(&app, "GET", "/api/v1/sources", Some(&erin), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sources[0]["writable"], true);
    assert!(sources[0].get("writable_reason").is_none());
    let (st, source) = call(
        &app,
        "GET",
        &format!("/api/v1/sources/{shared}"),
        Some(&erin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(source["writable"], true);
    let (st, uploaded) = upload_call(&app, &erin, &shared, "granted.png", bytes.clone()).await;
    assert_eq!(st, StatusCode::OK, "{uploaded}");
    assert!(uploaded["asset"].is_string(), "explicit ingest: {uploaded}");

    // Revocation also applies on the next list/get/upload request. Since the write share carried
    // the read reach too, the source disappears rather than leaking its current backend state.
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{write_share}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, sources) = call(&app, "GET", "/api/v1/sources", Some(&erin), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(sources.as_array().unwrap().is_empty());
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/sources/{shared}"),
        Some(&erin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = upload_call(&app, &erin, &shared, "revoked.png", bytes).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let audit = store.list_audit(100).unwrap();
    assert!(
        audit
            .iter()
            .any(|entry| entry.action == "source.upload" && entry.actor.contains("upload-editor")),
        "the granted source write is attributed: {audit:?}"
    );
    assert!(
        audit.iter().any(|entry| {
            entry.action == "source.upload.refused"
                && entry.actor.contains("upload-editor")
                && entry
                    .detail
                    .as_ref()
                    .and_then(|detail| detail["bytes"].as_u64())
                    == Some(0)
        }),
        "pre-body per-source grant refusals are attributed with zero bytes: {audit:?}"
    );
}

#[tokio::test]
async fn leak_audit_export_respects_the_ceiling() {
    // Engine-level: export (bulk egress) composes the same predicate as browse. A restricted
    // context exporting "the whole library" gets only its reachable slice; explicit hidden ids
    // are silently filtered out.
    let (_app, _s, lib, _admin, _vera, shared, _secret, _vid) = leak_world().await;
    let ectx = AuthContext::embedded();
    let all = lib.query(&ectx, QueryRequest::default()).await.unwrap();
    let secret_id = all
        .items
        .iter()
        .find(|a| a.name == "secret_wall.png")
        .unwrap()
        .id;

    let mut scope = dam_api::VisibilityScope::default();
    scope.sources.insert(shared.parse().unwrap());
    let restricted = AuthContext::connected(
        Some("vera".into()),
        dam_api::Role::Editor.scopes(),
        dam_api::Visibility::Restricted(scope),
    );

    let out = unique_tmp("accounts");
    std::fs::create_dir_all(&out).unwrap();
    let whole = lib
        .export(
            &restricted,
            ExportRequest {
                assets: vec![],
                collection: None,
                query: None,
                format: ExportFormat::Json,
                output: out.join("manifest.json").to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        whole.assets, 2,
        "whole-library export must be ceiling-sized"
    );

    let by_id = lib
        .export(
            &restricted,
            ExportRequest {
                assets: vec![secret_id],
                collection: None,
                query: None,
                format: ExportFormat::Json,
                output: out.join("by-id.json").to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(by_id.assets, 0, "a hidden id must not export");
}

// ── admin guard rails ────────────────────────────────────────────────────────

#[tokio::test]
async fn last_admin_cannot_be_demoted_and_share_gc_runs_on_source_removal() {
    let (app, store, _l, admin, _vera, shared, _secret, vera_id) = leak_world().await;

    // The sole admin account resists demotion/deletion (409).
    let (st, body) = call(&app, "GET", "/admin/api/accounts", Some(&admin), None).await;
    assert_eq!(st, StatusCode::OK);
    let admin_id = body
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["username"] == "owner")
        .unwrap()["account_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/admin/api/accounts/{admin_id}"),
        Some(&admin),
        Some(json!({"role": "viewer"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/accounts/{admin_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // Removing the shared source garbage-collects its share rows (cross-DB soft refs).
    assert!(store
        .list_shares()
        .unwrap()
        .iter()
        .any(|s| s.resource_id == shared));
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/sources/{shared}"),
        Some(&admin),
        Some(json!({"keep_metadata": false})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(
        !store
            .list_shares()
            .unwrap()
            .iter()
            .any(|s| s.resource_id == shared),
        "orphaned share rows must be GC'd"
    );
    let _ = vera_id; // world fixture
}

// ── shares handed to a federated peer (issue #127) ───────────────────────────

#[tokio::test]
async fn federated_peer_accepts_read_share_rejects_write_and_revokes_next_request() {
    // Needs a *real* peer: the federated `add_source` handshakes over HTTP, so the oneshot seam
    // can't stand in. Bind a second server on an ephemeral port with the federation flag on.
    let peer_lib = Arc::new(
        EmbeddedLibrary::open_with(
            &unique_tmp("accounts"),
            dam_core::ResourceOptions::ungoverned(),
        )
        .await
        .unwrap(),
    );
    let peer_store = Arc::new(ServerStore::open_in_memory().unwrap());
    peer_store
        .set_flag(
            dam_api::admin::FlagKey::Federation,
            dam_api::admin::SetFlag {
                value: dam_api::admin::FlagValue::Bool(true),
                expected_version: None,
                confirm: true,
            },
            "test",
        )
        .unwrap();
    let peer_app = router(peer_lib, peer_store, "127.0.0.1:0", true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = listener.local_addr().unwrap();
    let peer_task = tokio::spawn(async move { axum::serve(listener, peer_app).await.unwrap() });

    let (app, store, lib, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let peer_sid = lib
        .add_source(
            &AuthContext::embedded(),
            AddSource {
                kind: SourceKind::Federated,
                uri: format!("http://{peer_addr}"),
                name: Some("peer".into()),
                options: SourceOptions::default(),
            },
        )
        .await
        .unwrap();

    let generation = store.visibility_generation();
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "source", "resource_id": peer_sid.to_string(),
            "account_id": vera_id, "access": "read",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "federated read share failed: {body}");
    assert!(store.visibility_generation() > generation);
    let share_id = body["share_id"].as_str().unwrap().to_string();
    let (st, sources) = call(&app, "GET", "/api/v1/sources", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(sources
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["id"] == peer_sid.to_string()));

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{share_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, sources) = call(&app, "GET", "/api/v1/sources", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!sources
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["id"] == peer_sid.to_string()));

    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "source", "resource_id": peer_sid.to_string(),
            "account_id": vera_id, "access": "write",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("read-only"));
    peer_task.abort();
}
