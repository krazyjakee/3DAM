//! Collections under the ceiling (issue #42; issue #127): membership ids on an asset record, the
//! manual-collection count as a cardinality oracle, the canonical spelling of a share's resource id
//! (so the UI and the orphan GC agree), and a smart-collection share that grants the *view* without
//! ever widening its live query.
//!
//! Driven through the real axum router in-process (`ServiceExt::oneshot`, no socket) over the
//! shared [`leak_world`] fixture.

mod support;

use axum::http::StatusCode;
use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use serde_json::json;
use support::{
    asset_id_by_name, call, create_account, leak_world, login, query_names, share, unique_tmp,
};

// ── collection membership on the asset record honours the ceiling ────────────

#[tokio::test]
async fn asset_record_hides_unreachable_collection_ids() {
    let (app, _s, _l, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;

    // Two collections both holding the hidden asset; only the first is shared with vera.
    let mut ids = Vec::new();
    for name in ["Shared picks", "Private picks"] {
        let (st, body) = call(
            &app,
            "POST",
            "/api/v1/collections",
            Some(&admin),
            Some(json!({"name": name, "kind": "manual"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let cid = body["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "POST",
            &format!("/api/v1/collections/{cid}/members"),
            Some(&admin),
            Some(json!({"add": [secret_id]})),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        ids.push(cid);
    }
    let (shared_cid, private_cid) = (ids[0].clone(), ids[1].clone());
    share(
        &app,
        &admin,
        "collection",
        &shared_cid,
        ("account_id", &vera_id),
        "read",
    )
    .await;

    // Vera reaches the asset through the shared collection…
    let (st, body) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{secret_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let listed: Vec<&str> = body["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    // …and learns about that collection only. The other one 404s by id, so naming it on the record
    // would be the record contradicting the ceiling.
    assert_eq!(listed, vec![shared_cid.as_str()], "leaked: {body}");
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{private_cid}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The admin, unrestricted, still sees both.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{secret_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body["collections"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn manual_collection_count_is_ceiling_filtered() {
    let (app, _s, _l, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    let visible_id = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/collections",
        Some(&admin),
        Some(json!({"name": "Mixed", "kind": "manual"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cid = body["id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{cid}/members"),
        Some(&admin),
        Some(json!({"add": [secret_id, visible_id]})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    share(
        &app,
        &admin,
        "collection",
        &cid,
        ("account_id", &vera_id),
        "read",
    )
    .await;

    // The collection share grants its members, so vera reaches both — count 2, matching the grid.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{cid}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(body["count"], 2);

    // Now revoke the collection share and grant the source instead: vera reaches only brick_red,
    // and the count must follow the grid rather than announce the hidden member.
    let shares = call(&app, "GET", "/admin/api/shares", Some(&admin), None)
        .await
        .1;
    let sid = shares
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["resource"] == "collection")
        .unwrap()["share_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{sid}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/api/v1/collections", Some(&vera), None).await;
    let seen = list.as_array().unwrap();
    assert_eq!(seen.len(), 1, "reachable as a view over the shared source");
    assert_eq!(
        seen[0]["count"], 1,
        "count must not be a cardinality oracle"
    );
}

// ── share ids are canonical, so the UI and the orphan GC agree ───────────────

#[tokio::test]
async fn share_resource_id_is_canonicalised() {
    let (app, store, _l, admin, _vera, shared, _secret, vera_id) = leak_world().await;
    // The same source id in its unhyphenated ("simple") spelling — `Uuid::parse_str` accepts it.
    let simple = shared.replace('-', "");
    assert_ne!(simple, shared);
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "source", "resource_id": simple,
            "account_id": vera_id, "access": "read",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // Stored and echoed canonically — string-equality consumers (the web share list, the orphan GC)
    // would otherwise never match it, leaving a live grant that cannot be seen or revoked.
    assert_eq!(body["resource_id"], shared);
    assert!(store
        .list_shares()
        .unwrap()
        .iter()
        .all(|s| s.resource_id == shared));

    // And the GC, which binds the canonical form, actually reaches it.
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
        store.list_shares().unwrap().is_empty(),
        "a non-canonically-spelled share must still be GC-able"
    );
}

// ── smart-folder shares (issue #127) ─────────────────────────────────────────

#[tokio::test]
async fn smart_collection_share_grants_the_view_but_never_widens_its_live_query() {
    let (app, store, lib, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let smart = lib
        .create_collection(
            &AuthContext::embedded(),
            NewCollection {
                name: "All images".into(),
                kind: CollectionKind::Smart,
                query: Some(QueryRequest::default()),
            },
        )
        .await
        .unwrap();
    let smart_only_id = create_account(&app, &admin, "smart-only", "viewer").await;
    let (st, body) = call(
        &app,
        "POST",
        "/admin/api/shares",
        Some(&admin),
        Some(json!({
            "resource": "collection", "resource_id": smart.to_string(),
            "account_id": vera_id, "access": "read",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "smart-folder share failed: {body}");
    let vera_share = body["share_id"].as_str().unwrap().to_string();
    let smart_only_share = share(
        &app,
        &admin,
        "collection",
        &smart.to_string(),
        ("account_id", &smart_only_id),
        "read",
    )
    .await;

    // Vera separately holds one source grant. The smart folder's default "all assets" query is
    // intersected with that grant, so it returns the two shared-source rows and not the two hidden
    // rows. The live count is evaluated through the same ceiling-filtered query.
    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{smart}/assets"),
        Some(&vera),
        Some(json!({"limit": 50})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|asset| asset["name"].as_str())
        .collect();
    assert_eq!(names, ["brick_blue.png", "brick_red.png"]);
    let (st, body) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{smart}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], 2);

    // The second viewer has the smart-folder grant *alone*. The folder record is reachable, but
    // its saved query grants no assets on search/detail/stats/jobs/export surfaces.
    let smart_only = login(&app, "smart-only", "password123").await;
    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{smart}/assets"),
        Some(&smart_only),
        Some(json!({"limit": 50})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());
    assert!(query_names(&app, &smart_only).await.is_empty());
    let (_, stats) = call(&app, "GET", "/api/v1/stats", Some(&smart_only), None).await;
    assert_eq!(stats["total"], 0);
    let secret = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    for uri in [
        format!("/api/v1/assets/{secret}"),
        format!("/api/v1/assets/{secret}/content"),
        format!("/api/v1/assets/{secret}/thumbnail"),
    ] {
        let (status, _) = call(&app, "GET", &uri, Some(&smart_only), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "smart query widened {uri}");
    }
    let (_, jobs) = call(
        &app,
        "POST",
        "/api/v1/jobs/list",
        Some(&smart_only),
        Some(json!({})),
    )
    .await;
    assert!(jobs["items"].as_array().unwrap().is_empty());

    let account = store.get_account(&smart_only_id).unwrap();
    let visibility = store
        .resolve_visibility(&dam_api::AccountIdentity {
            account_id: account.account_id,
            username: account.username.clone(),
            role: account.role,
        })
        .unwrap();
    let smart_ctx =
        AuthContext::connected(Some(account.username), account.role.scopes(), visibility);
    let output = unique_tmp("accounts").join("smart-only.json");
    let report = lib
        .export(
            &smart_ctx,
            ExportRequest {
                assets: Vec::new(),
                collection: Some(smart),
                query: None,
                format: ExportFormat::Json,
                output: output.to_string_lossy().into_owned(),
                attribution_only: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(report.assets, 0, "a smart share widened export selection");

    // Both direct and group/account revocations already share this store path. Assert this specific
    // smart grant bumps the live-subscription generation and disappears on the very next request.
    let generation = store.visibility_generation();
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{smart_only_share}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(store.visibility_generation() > generation);
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{smart}"),
        Some(&smart_only),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Keep the first share live through all assertions, then prove its direct revocation too.
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/shares/{vera_share}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}
