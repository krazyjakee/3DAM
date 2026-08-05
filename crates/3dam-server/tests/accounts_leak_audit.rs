//! The leak audit (issue #42): one test per read path, proving an unshared asset is *absent* —
//! not forbidden — for a restricted identity. Query/get/content/thumbnail, stats/sources/folders,
//! similar/duplicates, bulk tag edit, collections, jobs, WS events, and the MCP tools all compose
//! the same reachability predicate, so each surface gets its own proof that it did.
//!
//! Every test shares the [`leak_world`] fixture, driven through the real axum router in-process
//! (`ServiceExt::oneshot`, no socket) over a store shared with the test, so a share edit is visible
//! to the next request — the same live path the admin UI uses.

mod support;

use axum::http::StatusCode;
use dam_api::dto::*;
use dam_api::event::LibraryEvent;
use dam_api::service::{AuthContext, LibraryService};
use serde_json::{json, Value};
use support::{
    asset_id_by_name, call, claim, create_account, enable_accounts, harness, leak_world, login,
    next_event, query_names, seed_two_sources, share, wait_job, Session,
};

#[tokio::test]
async fn leak_audit_query_get_content_thumbnail() {
    let (app, _s, _l, admin, vera, _shared, _secret, _vid) = leak_world().await;

    // Query: only the shared source's assets. The admin sees all four.
    assert_eq!(
        query_names(&app, &vera).await,
        ["brick_blue.png", "brick_red.png"]
    );
    assert_eq!(query_names(&app, &admin).await.len(), 4);

    // Direct-by-id reads on a hidden asset: absent (404), for detail, bytes, and thumbnail alike.
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    for uri in [
        format!("/api/v1/assets/{secret_id}"),
        format!("/api/v1/assets/{secret_id}/content"),
        format!("/api/v1/assets/{secret_id}/thumbnail"),
        format!("/api/v1/assets/{secret_id}/preview-mesh"),
    ] {
        let (st, _) = call(&app, "GET", &uri, Some(&vera), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri} must be absent");
    }
    // A shared asset's bytes flow normally.
    let ok_id = asset_id_by_name(&app, &admin, "brick_red.png").await;
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{ok_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn leak_audit_stats_sources_folders() {
    let (app, _s, _l, admin, vera, shared, secret, _vid) = leak_world().await;

    // Aggregates are no oracle: totals and breakdowns count only reachable assets, and the
    // unshared source's name never appears.
    let (st, body) = call(&app, "GET", "/api/v1/stats", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["total"], 2);
    assert_eq!(body["sources"], 1);
    assert!(body["by_source"].get("secret-src").is_none());
    let (_, admin_stats) = call(&app, "GET", "/api/v1/stats", Some(&admin), None).await;
    assert_eq!(admin_stats["total"], 4);

    // Source-scoped stats + the source record + folder tree of the hidden source: absent.
    for uri in [
        format!("/api/v1/stats?source={secret}"),
        format!("/api/v1/sources/{secret}"),
    ] {
        let (st, _) = call(&app, "GET", &uri, Some(&vera), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri} must be absent");
    }
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/folders",
        Some(&vera),
        Some(json!({"source": secret, "prefix": ""})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The sources list contains exactly the shared one.
    let (st, body) = call(&app, "GET", "/api/v1/sources", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![shared.as_str()]);
}

#[tokio::test]
async fn leak_audit_similar_and_duplicates() {
    let (app, _s, _l, admin, vera, _shared, _secret, _vid) = leak_world().await;
    let red_id = asset_id_by_name(&app, &admin, "brick_red.png").await;

    // Similar: the byte-identical twin lives in the unshared source. The admin sees it as the top
    // neighbour; the viewer must not see it at any rank.
    let sim = |body: Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["asset"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/similar",
        Some(&admin),
        Some(json!({"asset": red_id, "k": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        sim(body).contains(&"brick_red_copy.png".to_string()),
        "admin similar should surface the twin"
    );
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/similar",
        Some(&vera),
        Some(json!({"asset": red_id, "k": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !sim(body).contains(&"brick_red_copy.png".to_string()),
        "viewer similar leaked an unshared neighbour"
    );

    // A hidden seed asset must not rank at all.
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    let (st, _) = call(
        &app,
        "POST",
        "/api/v1/similar",
        Some(&vera),
        Some(json!({"asset": secret_id, "k": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Exact duplicates: the red pair spans shared+secret. Admin sees the group; for the viewer it
    // collapses to one visible member — and a group of one is not a duplicate, so it vanishes.
    let dup_req = json!({"kind": "exact", "limit": 50});
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/duplicates",
        Some(&admin),
        Some(dup_req.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !body["items"].as_array().unwrap().is_empty(),
        "admin should see the cross-source duplicate group"
    );
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/duplicates",
        Some(&vera),
        Some(dup_req),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body["items"].as_array().unwrap().is_empty(),
        "viewer duplicates leaked a group spanning an unshared source: {body}"
    );
}

#[tokio::test]
async fn bulk_tag_edit_warns_for_read_only_and_hidden_targets_without_writing() {
    let (app, _store, lib) = harness(true).await;
    let (shared, secret) = seed_two_sources(&lib).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let editor_id = create_account(&app, &admin, "tagger", "editor").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &editor_id),
        "read",
    )
    .await;
    let editor = login(&app, "tagger", "password123").await;
    let shared_asset = asset_id_by_name(&app, &admin, "brick_red.png").await;
    let hidden_asset = asset_id_by_name(&app, &admin, "secret_wall.png").await;
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/tags/edit",
        Some(&editor),
        Some(json!({
            "assets": [shared_asset.clone(), hidden_asset],
            "add": ["curated"],
            "dry_run": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["matched"], 0);
    assert_eq!(body["changed"], 0);
    let codes: Vec<&str> = body["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"target_read_only"));
    assert!(codes.contains(&"target_unavailable"));

    let (_, detail) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{shared_asset}"),
        Some(&admin),
        None,
    )
    .await;
    assert!(!detail["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tag| tag["name"] == "curated"));
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/tags/edit",
        Some(&admin),
        Some(json!({"assets": [shared_asset], "add": ["Curated"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, vocabulary) = call(
        &app,
        "POST",
        "/api/v1/tags/list",
        Some(&admin),
        Some(json!({"prefix": "cur", "limit": 5000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vocabulary.as_array().unwrap().len(), 1);
    assert_eq!(vocabulary[0]["name"], "curated");
    assert_eq!(vocabulary[0]["manual"], true);
    let _ = secret;
}

#[tokio::test]
async fn leak_audit_collections_and_shared_collection_grant() {
    let (app, _s, _l, admin, vera, _shared, _secret, vera_id) = leak_world().await;
    let secret_id = asset_id_by_name(&app, &admin, "secret_wall.png").await;

    // Admin builds a collection holding only the hidden asset.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/collections",
        Some(&admin),
        Some(json!({"name": "Hidden picks", "kind": "manual"})),
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

    // Unshared: absent from the list, and its record/assets 404 by id.
    let (st, body) = call(&app, "GET", "/api/v1/collections", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body.as_array().unwrap().is_empty(),
        "collection leaked: {body}"
    );
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/collections/{cid}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Sharing the collection grants exactly its members (rule 5): the collection appears, its
    // member asset resolves, but the rest of the secret source stays hidden.
    share(
        &app,
        &admin,
        "collection",
        &cid,
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let (st, body) = call(&app, "GET", "/api/v1/collections", Some(&vera), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/collections/{cid}/assets"),
        Some(&vera),
        // `PageParams::limit` has no serde default — the wire form always names it.
        Some(json!({"limit": 50})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{secret_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a shared collection's member must resolve"
    );
    // The explicit source hint is a federated-routing requirement only. A local asset reached
    // through manual collection membership must retain every ordinary by-id preview surface.
    for uri in [
        format!("/api/v1/assets/{secret_id}/content"),
        format!("/api/v1/assets/{secret_id}/thumbnail"),
    ] {
        let (st, _) = call(&app, "GET", &uri, Some(&vera), None).await;
        assert_eq!(
            st,
            StatusCode::OK,
            "manual collection preview regressed: {uri}"
        );
    }
    // Collection ⊅ source: the twin in the same source is still absent.
    let twin_id = asset_id_by_name(&app, &admin, "brick_red_copy.png").await;
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{twin_id}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // And the query surface now includes the granted member.
    assert_eq!(
        query_names(&app, &vera).await,
        ["brick_blue.png", "brick_red.png", "secret_wall.png"]
    );
}

#[tokio::test]
async fn leak_audit_jobs_are_absent_for_restricted_identities() {
    let (app, _s, lib, admin, vera, shared, _secret, _vid) = leak_world().await;
    // The seeding scan/analyze jobs span *both* sources, so every one of them names paths vera
    // cannot reach. A job is observable only when its whole source set is inside the ceiling
    // (`Visibility::allows_job`) — "all", not "any", precisely so `progress.current` can never carry
    // a path out of an unshared source.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/list",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let admin_jobs = body["items"].as_array().unwrap().len();
    assert!(admin_jobs >= 2);
    // …the restricted viewer gets an empty page, and a direct get is absent.
    let (st, body) = call(
        &app,
        "POST",
        "/api/v1/jobs/list",
        Some(&vera),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());
    let ctx = AuthContext::embedded();
    let job = lib
        .list_jobs(&ctx, JobListRequest::default())
        .await
        .unwrap()
        .items[0]
        .id;
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{job}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // …but a job confined to the source she *does* hold is hers to watch. This is the other half of
    // the rule: absence is driven by attribution, not by a blanket "restricted identities get no
    // jobs" — otherwise her status bar could never show her own scan running (issue #42).
    let mine = lib
        .submit_scan(
            &ctx,
            ScanRequest {
                sources: vec![shared.parse().unwrap()],
                mode: ScanMode::Full,
            },
        )
        .await
        .unwrap();
    wait_job(&lib, &ctx, &mine).await;
    let (st, body) = call(
        &app,
        "GET",
        &format!("/api/v1/jobs/{mine}"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a scan of her own source is visible: {body}"
    );
    assert_eq!(body["sources"], json!([shared]));

    // Visible is not cancellable: watching a job is a read, stopping one is a library-wide act.
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/jobs/{mine}/cancel"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// The restricted-subscriber contract (issue #42): a share-based identity must get a *live* stream
/// for the sources it holds, not a socket that never speaks. Every `LibraryEvent` now carries the
/// attribution the ceiling is evaluated against, so this asserts both directions at once — the
/// shared source's events arrive, the unshared source's events never do.
///
/// Each pair below emits the **secret** event first. If the filter were merely slow rather than
/// exclusive, the secret event would arrive first and fail the assertion; ordering is what turns
/// "we saw the shared one" into "we saw *only* the shared one".
#[tokio::test]
async fn restricted_subscriber_receives_events_only_for_shared_sources() {
    let (app, store, lib, admin, _vera, shared, secret, vera_id) = leak_world().await;
    let shared_sid: dam_api::SourceId = shared.parse().unwrap();
    let secret_sid: dam_api::SourceId = secret.parse().unwrap();

    // Vera's ceiling, resolved from her real share rows the same way the auth layer resolves it for
    // a session — not a hand-built scope, so the share→visibility mapping is under test too.
    let acct = store.get_account(&vera_id).unwrap();
    let vis = store
        .resolve_visibility(&dam_api::AccountIdentity {
            account_id: acct.account_id.clone(),
            username: acct.username.clone(),
            role: acct.role,
        })
        .unwrap();
    assert!(
        !vis.is_full(),
        "vera must be restricted or this test proves nothing"
    );
    let vera_ctx = AuthContext::connected(Some(acct.username.clone()), acct.role.scopes(), vis);
    let ctx = AuthContext::embedded();

    let mut stream = lib
        .subscribe(&vera_ctx, dam_api::SubscribeRequest::default())
        .await
        .unwrap();

    // ── AssetChanged: a favourite toggle on each source ──────────────────────
    let secret_asset: dam_api::AssetId = asset_id_by_name(&app, &admin, "secret_wall.png")
        .await
        .parse()
        .unwrap();
    let shared_asset: dam_api::AssetId = asset_id_by_name(&app, &admin, "brick_red.png")
        .await
        .parse()
        .unwrap();
    for asset in [secret_asset, shared_asset] {
        lib.set_favorite(
            &ctx,
            FavoriteRequest {
                asset,
                favorite: true,
            },
        )
        .await
        .unwrap();
    }
    match next_event(&mut stream).await {
        LibraryEvent::AssetChanged { id, source_id, .. } => {
            assert_eq!(
                id, shared_asset,
                "the secret source's change must be dropped"
            );
            assert_eq!(source_id, Some(shared_sid));
        }
        other => panic!("expected AssetChanged for the shared asset, got {other:?}"),
    }

    // ── AssetRemoved: attribution captured before the row disappears ─────────
    let secret_dup: dam_api::AssetId = asset_id_by_name(&app, &admin, "brick_red_copy.png")
        .await
        .parse()
        .unwrap();
    let shared_other: dam_api::AssetId = asset_id_by_name(&app, &admin, "brick_blue.png")
        .await
        .parse()
        .unwrap();
    for asset in [secret_dup, shared_other] {
        lib.remove_asset(&ctx, &asset, RemoveAsset { block: false })
            .await
            .unwrap();
    }
    match next_event(&mut stream).await {
        LibraryEvent::AssetRemoved { id, source_id } => {
            assert_eq!(
                id, shared_other,
                "the secret source's removal must be dropped"
            );
            assert_eq!(
                source_id,
                Some(shared_sid),
                "a removal must still name its source after the row is gone"
            );
        }
        other => panic!("expected AssetRemoved for the shared asset, got {other:?}"),
    }

    // ── JobProgress: a scan of each source in turn ───────────────────────────
    for sid in [secret_sid, shared_sid] {
        let job = lib
            .submit_scan(
                &ctx,
                ScanRequest {
                    sources: vec![sid],
                    mode: ScanMode::Full,
                },
            )
            .await
            .unwrap();
        wait_job(&lib, &ctx, &job).await;
    }
    // Scans also re-add the two assets removed above, so drain to the first job event.
    let progress = loop {
        match next_event(&mut stream).await {
            LibraryEvent::JobProgress(js) => break js,
            LibraryEvent::AssetAdded(a) => {
                assert_eq!(
                    a.source_id,
                    Some(shared_sid),
                    "a re-scan must not announce the secret source's assets"
                );
            }
            other => panic!("unexpected event while waiting for job progress: {other:?}"),
        }
    };
    assert_eq!(
        progress.sources,
        vec![shared_sid],
        "only the scan confined to her own source may surface"
    );
}

#[tokio::test]
async fn leak_audit_mcp_tools_inherit_the_ceiling() {
    // The MCP surface is the same engine over a different transport: it gets the visibility filter
    // for free *only* if enforcement really lives in the query path (issue #42 leak audit).
    let (app, _s, _l, admin, vera, _shared, _secret, _vid) = leak_world().await;
    let (st, _) = call(
        &app,
        "PUT",
        "/admin/api/flags/mcp_server",
        Some(&admin),
        Some(json!({"value": "read_only", "expected_version": null, "confirm": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let rpc = |session: Session, tool: &str, args: Value| {
        let app = app.clone();
        let tool = tool.to_string();
        async move {
            let (st, body) = call(
                &app,
                "POST",
                "/mcp",
                Some(&session),
                Some(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": tool, "arguments": args},
                })),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "mcp call failed: {body}");
            // Tool payloads ride as a JSON text block in `content[0].text`.
            let text = body["result"]["content"][0]["text"].as_str().unwrap_or("");
            serde_json::from_str::<Value>(text).unwrap_or(Value::Null)
        }
    };

    // `search` shows only the shared source's assets…
    let out = rpc(vera.clone(), "search", json!({"limit": 50})).await;
    let names: Vec<&str> = out["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(names.len(), 2, "mcp search leaked: {out}");
    assert!(
        !names.contains(&"secret_wall.png"),
        "mcp search leaked: {out}"
    );
    // …and `library_stats` is no aggregate oracle either.
    let stats = rpc(vera, "library_stats", json!({})).await;
    assert_eq!(stats["total"], 2, "mcp stats leaked a total: {stats}");
    // The admin, over the same transport, still sees everything.
    let all = rpc(admin, "search", json!({"limit": 50})).await;
    assert_eq!(all["items"].as_array().unwrap().len(), 4);
}
