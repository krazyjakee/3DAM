//! Per-asset discussion (issue #82): attribution across accounts, the ceiling applied to the
//! comment surface itself, edit marks and delete tombstones that keep replies intact, who may edit
//! versus who may delete, what survives a deleted account, and the surface's absence while
//! `user_accounts` is off.
//!
//! Driven through the real axum router in-process (`ServiceExt::oneshot`, no socket).

mod support;

use axum::http::StatusCode;
use dam_api::dto::*;
use dam_api::service::{AuthContext, LibraryService};
use serde_json::json;
use support::{
    asset_id_by_name, call, claim, create_account, enable_accounts, harness, login,
    seed_two_sources, share,
};

// ── per-asset discussion (issue #82) ────────────────────────────────────────

/// The headline acceptance: two accounts talking on one asset, each message correctly attributed —
/// and a **viewer** among them. Requiring `Scope::Write` to post would have locked out exactly the
/// reviewing art director this feature exists for, so this asserts the looser rule holds.
#[tokio::test]
async fn two_accounts_hold_a_conversation_correctly_attributed() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    // A non-admin account reaches nothing until something is shared with it (issue #42's fail-safe
    // ceiling), so the read share is what puts vera in the room at all.
    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;

    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        Some(json!({"body": "Is this the final bake?"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "owner could not post: {body}");

    // A viewer holds Read but not Write — and must still be able to join the discussion.
    let (st, body) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        Some(json!({"body": "No, see the -v3."})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "a viewer must be able to post: {body}"
    );

    let (st, thread) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let msgs = thread.as_array().unwrap();
    assert_eq!(msgs.len(), 2, "both messages present: {thread}");
    // Oldest first, and each resolved to its author's name across the database boundary.
    assert_eq!(msgs[0]["body"], "Is this the final bake?");
    assert_eq!(msgs[0]["author"]["display"], "owner");
    assert_eq!(msgs[1]["body"], "No, see the -v3.");
    assert_eq!(msgs[1]["author"]["display"], "vera");
}

/// The leak audit entry for discussion: an asset you cannot see has no thread you can read, and no
/// thread you can post to. A message body can quote a hidden path, so this is the same trap the
/// rest of issue #42's read paths guard.
#[tokio::test]
async fn discussion_on_an_unreachable_asset_is_absent() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let hidden = asset_id_by_name(&app, &admin, "secret_wall.png").await;

    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;

    // Something *is* there — the owner can see it — so this is absence, not emptiness.
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{hidden}/comments"),
        Some(&admin),
        Some(json!({"body": "internal only"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{hidden}/comments"),
        Some(&vera),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "an unreachable asset's thread must be absent, not forbidden"
    );

    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{hidden}/comments"),
        Some(&vera),
        Some(json!({"body": "nope"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Editing marks the message; deleting leaves a tombstone that keeps a reply's parent intact.
#[tokio::test]
async fn editing_marks_and_deleting_tombstones_without_breaking_replies() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let (_, first) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        Some(json!({"body": "original"})),
    )
    .await;
    let first_id = first["id"].as_str().unwrap().to_string();

    let (st, reply) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        Some(json!({"body": "a reply", "reply_to": first_id})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "reply failed: {reply}");

    let (st, edited) = call(
        &app,
        "PUT",
        &format!("/api/v1/comments/{first_id}"),
        Some(&admin),
        Some(json!({"body": "corrected"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(edited["body"], "corrected");
    assert!(edited["edited_at"].is_i64(), "an edit is marked: {edited}");

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/comments/{first_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (_, thread) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        None,
    )
    .await;
    let msgs = thread.as_array().unwrap();
    assert_eq!(msgs.len(), 2, "the tombstone stays in the thread: {thread}");
    assert!(
        msgs[0]["deleted_at"].is_i64(),
        "deleted message is a tombstone"
    );
    assert_eq!(msgs[0]["body"], "", "a tombstone carries no text");
    assert_eq!(
        msgs[1]["reply_to"], first_id,
        "the reply still points at its (now deleted) parent"
    );
}

/// Moderation boundaries: nobody edits someone else's words — not even an admin — but an admin may
/// remove a message. The asymmetry is the point: an edited message still carries its author's name.
#[tokio::test]
async fn only_the_author_edits_but_an_admin_may_delete() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let vera_id = create_account(&app, &admin, "vera", "viewer").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;

    let (_, posted) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        Some(json!({"body": "vera's words"})),
    )
    .await;
    let id = posted["id"].as_str().unwrap().to_string();

    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/v1/comments/{id}"),
        Some(&admin),
        Some(json!({"body": "put words in her mouth"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "an admin must not rewrite someone's message"
    );

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/comments/{id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NO_CONTENT,
        "an admin may moderate by removing"
    );
}

/// A deleted account's messages survive, rendered as an unresolved author — cascade-deleting them
/// would silently rewrite the project's history.
#[tokio::test]
async fn a_deleted_account_leaves_its_messages_intact() {
    let (app, _store, lib) = harness(true).await;
    enable_accounts(&app).await;
    let admin = claim(&app, "owner").await;
    let (shared, _secret) = seed_two_sources(&lib).await;
    let asset = asset_id_by_name(&app, &admin, "brick_red.png").await;

    let vera_id = create_account(&app, &admin, "vera", "editor").await;
    share(
        &app,
        &admin,
        "source",
        &shared.to_string(),
        ("account_id", &vera_id),
        "read",
    )
    .await;
    let vera = login(&app, "vera", "password123").await;
    call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&vera),
        Some(json!({"body": "archiving this one"})),
    )
    .await;

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/admin/api/accounts/{vera_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert!(st.is_success(), "account delete failed: {st}");

    let (st, thread) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let msgs = thread.as_array().unwrap();
    assert_eq!(msgs.len(), 1, "the message survived its author: {thread}");
    assert_eq!(msgs[0]["body"], "archiving this one");
    assert!(
        msgs[0]["author"]["display"].is_null(),
        "an unresolvable author renders as absent, not as a fabricated name: {thread}"
    );
    assert_eq!(msgs[0]["author"]["id"], vera_id, "the id is kept forever");
}

/// Flag off ⇒ the surface is absent (ADR 0004), not merely empty or forbidden.
#[tokio::test]
async fn the_discussion_surface_404s_while_user_accounts_is_off() {
    let (app, _store, lib) = harness(true).await;
    seed_two_sources(&lib).await;
    // No `enable_accounts` — the flag is off, which is the default posture.
    let assets = lib
        .query(&AuthContext::embedded(), QueryRequest::default())
        .await
        .unwrap();
    let asset = assets.items[0].id;

    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/v1/assets/{asset}/comments"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/v1/assets/{asset}/comments"),
        None,
        Some(json!({"body": "hello?"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
