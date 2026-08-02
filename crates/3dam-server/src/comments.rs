//! Per-asset discussion threads (issue #82) — the `/api/v1` surface over `LibraryService`'s
//! comment methods, behind **one** router-level `UserAccounts` gate.
//!
//! Two things happen here that cannot happen in the engine, and they are why this module exists at
//! all rather than the routes living beside the other asset routes in `lib.rs`:
//!
//! 1. **Author resolution.** Comments live in `library.db`; accounts live in `server.db`. The engine
//!    stores and returns an account *id* and has no way to turn it into a name. This layer is the
//!    only one that can see both, so it fills `author.display` on the way out — and leaves it
//!    `None` when the account is gone, which is how a deleted user's messages survive as
//!    "unresolved author" instead of disappearing or crashing the render.
//! 2. **Audit.** Every post/edit/delete lands in `audit_log`, consistent with how issue #42 treats
//!    account-adjacent mutations.
//!
//! The gate is router-level for the reason [`crate::gate_accounts`] documents: discussion is
//! meaningless without identity, so with `UserAccounts` off the whole surface 404s rather than
//! answering with an unattributable thread.

use crate::auth::Commenter;
use crate::{ApiError, AppState};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use dam_api::dto::{Comment, EditComment, NewComment};
use dam_api::id::{AssetId, CommentId};
use dam_api::service::LibraryService;
use dam_api::LibError;

/// The whole discussion surface, gated once at the router.
pub fn routes(st: AppState) -> Router<AppState> {
    let r = Router::new()
        .route(
            "/api/v1/assets/{id}/comments",
            get(list_comments).post(post_comment),
        )
        .route(
            "/api/v1/comments/{id}",
            put(edit_comment).delete(delete_comment),
        );
    crate::gate_accounts(r, st)
}

/// Fill in `author.display` from `server.db`.
///
/// An id that no longer resolves stays `None` rather than becoming an error or a placeholder
/// string: the client decides how to render an absent author, and inventing "deleted user" here
/// would make it indistinguishable from someone who really is called that.
fn resolve_authors(st: &AppState, comments: &mut [Comment]) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    for c in comments.iter_mut() {
        let display = seen
            .entry(c.author.id.clone())
            .or_insert_with(|| {
                st.store
                    .get_account(&c.author.id)
                    .ok()
                    .map(|a| a.display_name.unwrap_or(a.username))
            })
            .clone();
        c.author.display = display;
    }
}

fn resolve_one(st: &AppState, mut comment: Comment) -> Comment {
    resolve_authors(st, std::slice::from_mut(&mut comment));
    comment
}

use crate::actor_of;

fn parse_comment_id(raw: &str) -> Result<CommentId, ApiError> {
    raw.parse()
        .map_err(|_| ApiError(LibError::BadRequest(format!("invalid comment id {raw:?}"))))
}

fn parse_asset_id(raw: &str) -> Result<AssetId, ApiError> {
    raw.parse()
        .map_err(|_| ApiError(LibError::BadRequest(format!("invalid asset id {raw:?}"))))
}

async fn list_comments(
    Commenter(ctx): Commenter,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Vec<Comment>>, ApiError> {
    let id = parse_asset_id(&id)?;
    let mut comments = st.lib.list_comments(&ctx, &id).await?;
    resolve_authors(&st, &mut comments);
    Ok(Json(comments))
}

/// Posting rides `Commenter` (read-scoped + CSRF), not `Writer` — see that extractor for why a
/// `viewer` must be able to post. The engine still requires a signed-in account, so anonymous and
/// bearer-token callers cannot.
async fn post_comment(
    Commenter(ctx): Commenter,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<NewComment>,
) -> Result<(StatusCode, Json<Comment>), ApiError> {
    let id = parse_asset_id(&id)?;
    let comment = st.lib.post_comment(&ctx, &id, req).await?;
    // library.db and server.db cannot share a SQLite transaction. The mutation is already durable;
    // if its required audit cannot be written, fail the request instead of claiming unaudited
    // success. Clients may reconcile by listing the thread before retrying.
    st.store.audit(
        &actor_of(&ctx),
        "comment.post",
        Some(&comment.id.to_string()),
        Some(serde_json::json!({ "asset": id.to_string() })),
    )?;
    Ok((StatusCode::CREATED, Json(resolve_one(&st, comment))))
}

async fn edit_comment(
    Commenter(ctx): Commenter,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<EditComment>,
) -> Result<Json<Comment>, ApiError> {
    let id = parse_comment_id(&id)?;
    let comment = st.lib.edit_comment(&ctx, &id, req).await?;
    st.store.audit(
        &actor_of(&ctx),
        "comment.edit",
        Some(&id.to_string()),
        Some(serde_json::json!({ "asset": comment.asset.to_string() })),
    )?;
    Ok(Json(resolve_one(&st, comment)))
}

async fn delete_comment(
    Commenter(ctx): Commenter,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let id = parse_comment_id(&id)?;
    st.lib.delete_comment(&ctx, &id).await?;
    st.store.audit(
        &actor_of(&ctx),
        "comment.delete",
        Some(&id.to_string()),
        None,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
