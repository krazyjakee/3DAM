//! The admin API (tech-spec 10 §5) — the single source of truth both the web Settings surface and
//! the CLI drive (parity by *both being clients of these routes*, ADR 0004 decision 2). Mounted
//! under `/admin/api`, guarded by `Scope::Admin` via the [`AdminAuth`] extractor; every mutating
//! call goes through the store's audited `set` path (§2.3, §4.5).

use crate::auth::AdminAuth;
use crate::{ApiError, AppState};
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use dam_api::admin::*;
use dam_api::service::AuthContext;
use dam_api::LibError;

/// The audit actor string for a request (tech-spec 10 §4.5): the token label, or `owner` for the
/// unauthenticated localhost owner under `Authentication = Off`.
fn actor_of(ctx: &AuthContext) -> String {
    ctx.identity.clone().unwrap_or_else(|| "owner".to_string())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/api/status", get(status))
        .route("/admin/api/flags", get(list_flags))
        .route("/admin/api/flags/{key}", get(get_flag).put(set_flag))
        .route("/admin/api/tokens", get(list_tokens).post(create_token))
        .route("/admin/api/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/admin/api/audit", get(list_audit))
}

fn parse_key(key: &str) -> Result<FlagKey, ApiError> {
    FlagKey::parse(key).ok_or_else(|| ApiError(LibError::NotFound(format!("no such flag: {key}"))))
}

async fn status(AdminAuth(_ctx): AdminAuth, State(st): State<AppState>) -> Json<AdminStatus> {
    Json(st.store.status(&st.bind, st.localhost_only, st.tls))
}

async fn list_flags(AdminAuth(_ctx): AdminAuth, State(st): State<AppState>) -> Json<Vec<FlagInfo>> {
    Json(st.store.all_flags())
}

async fn get_flag(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<FlagInfo>, ApiError> {
    Ok(Json(st.store.flag_info(parse_key(&key)?)))
}

async fn set_flag(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(key): Path<String>,
    Json(req): Json<SetFlag>,
) -> Result<Json<FlagInfo>, ApiError> {
    let key = parse_key(&key)?;
    Ok(Json(st.store.set_flag(key, req, &actor_of(&ctx))?))
}

async fn list_tokens(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<Vec<TokenInfo>>, ApiError> {
    Ok(Json(st.store.list_tokens()?))
}

async fn create_token(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<NewToken>,
) -> Result<Json<NewTokenReply>, ApiError> {
    Ok(Json(st.store.create_token(req, &actor_of(&ctx))?))
}

async fn revoke_token(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    st.store.revoke_token(&id, &actor_of(&ctx))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct AuditQuery {
    limit: Option<u32>,
}

async fn list_audit(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEntry>>, ApiError> {
    Ok(Json(st.store.list_audit(q.limit.unwrap_or(100).min(1000))?))
}
