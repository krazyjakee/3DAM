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

/// The audit actor string for a request (tech-spec 10 §4.5). A store-verified token records as
/// `token:<label>`; an unauthenticated localhost owner (under `Authentication = Off`) records as
/// `local-owner`. The two were both bare `owner` before — indistinguishable in the trail, since the
/// bootstrap token is *labelled* "owner" — so the prefix makes attribution unambiguous: was this the
/// no-credential local owner, or the holder of a token named "owner"?
fn actor_of(ctx: &AuthContext) -> String {
    match &ctx.identity {
        Some(label) => format!("token:{label}"),
        None => "local-owner".to_string(),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/api/status", get(status))
        .route("/admin/api/flags", get(list_flags))
        .route("/admin/api/flags/{key}", get(get_flag).put(set_flag))
        .route("/admin/api/tokens", get(list_tokens).post(create_token))
        .route(
            "/admin/api/tokens/{id}",
            axum::routing::delete(revoke_token),
        )
        .route("/admin/api/audit", get(list_audit))
        .route("/admin/api/maintenance/usage", get(maintenance_usage))
        .route(
            "/admin/api/maintenance/clear-cache",
            axum::routing::post(clear_cache),
        )
        .route(
            "/admin/api/maintenance/clear-analysis",
            axum::routing::post(clear_analysis),
        )
        .route("/admin/api/maintenance/vacuum", axum::routing::post(vacuum))
        .route("/admin/api/maintenance/wipe", axum::routing::post(wipe))
        .route(
            "/admin/api/maintenance/factory-reset",
            axum::routing::post(factory_reset),
        )
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
) -> Result<Json<SetFlagReply>, ApiError> {
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

// ── storage & maintenance (tech-spec 10 §5) ──────────────────────────────────
// Read `usage` is unaudited; every mutating op writes a `maintenance.*` audit row via the same
// `st.store.audit` path the flag/token routes use. The two wipes are gated on `confirm=true` — the
// machine form of warn-and-confirm, mirroring the exposure-confirm on `set_flag`.

async fn maintenance_usage(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<StorageUsage>, ApiError> {
    Ok(Json(st.lib.storage_usage().await?))
}

async fn clear_cache(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<ClearCacheRequest>,
) -> Result<Json<ClearCacheReport>, ApiError> {
    let report = st.lib.clear_caches(req.target).await?;
    st.store.audit(
        &actor_of(&ctx),
        "maintenance.clear_cache",
        None,
        Some(serde_json::json!({
            "target": req.target,
            "bytes_freed": report.bytes_freed,
            "files_deleted": report.files_deleted,
        })),
    )?;
    Ok(Json(report))
}

async fn clear_analysis(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<ClearAnalysisReport>, ApiError> {
    let report = st.lib.clear_analysis().await?;
    st.store.audit(
        &actor_of(&ctx),
        "maintenance.clear_analysis",
        None,
        Some(serde_json::json!({
            "suggestions_removed": report.suggestions_removed,
            "embeddings_removed": report.embeddings_removed,
        })),
    )?;
    Ok(Json(report))
}

async fn vacuum(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<VacuumReport>, ApiError> {
    let report = st.lib.vacuum().await?;
    st.store.audit(
        &actor_of(&ctx),
        "maintenance.vacuum",
        None,
        Some(serde_json::json!({ "reclaimed_bytes": report.reclaimed_bytes })),
    )?;
    Ok(Json(report))
}

async fn wipe(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<ConfirmRequest>,
) -> Result<Json<WipeReport>, ApiError> {
    if !req.confirm {
        return Err(ApiError(LibError::BadRequest(
            "resetting the catalog is destructive — resend with confirm=true".into(),
        )));
    }
    let report = st.lib.wipe_catalog().await?;
    st.store.audit(
        &actor_of(&ctx),
        "maintenance.wipe",
        None,
        Some(serde_json::json!({
            "assets_removed": report.assets_removed,
            "sources_removed": report.sources_removed,
            "collections_removed": report.collections_removed,
            "tags_removed": report.tags_removed,
        })),
    )?;
    Ok(Json(report))
}

async fn factory_reset(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<ConfirmRequest>,
) -> Result<Json<FactoryResetReport>, ApiError> {
    if !req.confirm {
        return Err(ApiError(LibError::BadRequest(
            "a factory reset erases the catalog, caches, tokens, flags, and audit — resend with confirm=true".into(),
        )));
    }
    // Engine plane first: catalog + regenerable caches.
    let catalog = st.lib.wipe_catalog().await?;
    let cache = st.lib.clear_caches(CacheTarget::All).await?;
    // Server plane: erase tokens/flags/audit and drop flags to the safe floor. This clears the
    // audit log, so we record the reset *after* it — the fresh log then holds exactly this entry.
    let actor = actor_of(&ctx);
    let tokens_removed = st.store.factory_reset()?;
    st.store.audit(
        &actor,
        "maintenance.factory_reset",
        None,
        Some(serde_json::json!({
            "assets_removed": catalog.assets_removed,
            "tokens_removed": tokens_removed,
            "cache_bytes_freed": cache.bytes_freed,
        })),
    )?;
    Ok(Json(FactoryResetReport {
        catalog,
        cache,
        tokens_removed,
    }))
}
