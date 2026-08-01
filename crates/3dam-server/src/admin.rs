//! The admin API (tech-spec 10 §5) — the single source of truth both the web Settings surface and
//! the CLI drive (parity by *both being clients of these routes*, ADR 0004 decision 2). Mounted
//! under `/admin/api`, guarded by `Scope::Admin` via the [`AdminAuth`] extractor; every mutating
//! call goes through the store's audited `set` path (§2.3, §4.5).

use crate::auth::AdminAuth;
use crate::{ApiError, AppState};
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use dam_api::accounts::*;
use dam_api::admin::*;
use dam_api::dto::{CollectionKind, SourceKind};
use dam_api::service::LibraryService;
use dam_api::LibError;

pub(crate) use crate::actor_of;

pub fn routes(st: AppState) -> Router<AppState> {
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
        // OIDC provider config (issue #41). Not under the accounts block: an operator configures
        // the provider *before* switching the flag on, and the accounts gate would hide it.
        .route("/admin/api/oidc", get(get_oidc).put(set_oidc))
        // Identity links. Load-bearing rather than a convenience: `OidcProvisioning::Linked` is the
        // default, so without a way to create a link a default-configured provider is one no one
        // can ever sign in through.
        .route(
            "/admin/api/oidc/identities",
            get(list_oidc_identities).post(link_oidc_identity),
        )
        .route(
            "/admin/api/oidc/identities/{subject}",
            axum::routing::delete(unlink_oidc_identity),
        )
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
        .merge(accounts_routes(st))
}

/// The accounts / groups / shares block of the admin plane — every route gated identically on the
/// `UserAccounts` flag, so the guard is one `route_layer` here rather than a `require_accounts(&st)?`
/// line pasted into each of the twelve handlers (ADR 0004: off ⇒ the surface is absent).
fn accounts_routes(st: AppState) -> Router<AppState> {
    let r = Router::new()
        .route(
            "/admin/api/accounts",
            get(list_accounts).post(create_account),
        )
        .route(
            "/admin/api/accounts/{id}",
            axum::routing::put(update_account).delete(delete_account),
        )
        .route(
            "/admin/api/accounts/{id}/sessions",
            axum::routing::delete(revoke_account_sessions),
        )
        .route("/admin/api/groups", get(list_groups).post(create_group))
        .route(
            "/admin/api/groups/{id}",
            axum::routing::delete(delete_group),
        )
        .route(
            "/admin/api/groups/{id}/members",
            axum::routing::put(set_group_members),
        )
        .route("/admin/api/shares", get(list_shares).post(create_share))
        .route(
            "/admin/api/shares/{id}",
            axum::routing::delete(delete_share),
        );
    crate::gate_accounts(r, st)
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

// ── accounts / groups / shares (phase 6, issue #42; tech-spec 10 §5) ─────────
// All behind `AdminAuth` + the `UserAccounts` flag guard. Sharing writes are admin-only in v1 —
// an "editor shares their own source" needs an ownership concept the frozen scope doesn't have
// (noted in the tech-spec 10 §4 amendment); revisit post-v1.

async fn list_accounts(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<Vec<AccountInfo>>, ApiError> {
    Ok(Json(st.store.list_accounts()?))
}

async fn create_account(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<NewAccount>,
) -> Result<Json<AccountInfo>, ApiError> {
    // argon2id hashing belongs on the blocking pool, not a tokio worker (CLAUDE.md golden rule 5).
    let (store, actor) = (st.store.clone(), actor_of(&ctx));
    Ok(Json(
        tokio::task::spawn_blocking(move || store.create_account(&req, &actor))
            .await
            .map_err(|e| ApiError(LibError::Internal(e.to_string())))??,
    ))
}

async fn update_account(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAccount>,
) -> Result<Json<AccountInfo>, ApiError> {
    // A password change hashes with argon2id — off the async runtime (CLAUDE.md golden rule 5).
    let (store, actor) = (st.store.clone(), actor_of(&ctx));
    Ok(Json(
        tokio::task::spawn_blocking(move || store.update_account(&id, &req, &actor))
            .await
            .map_err(|e| ApiError(LibError::Internal(e.to_string())))??,
    ))
}

async fn delete_account(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    st.store.delete_account(&id, &actor_of(&ctx))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn revoke_account_sessions(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let n = st.store.revoke_account_sessions(&id, &actor_of(&ctx))?;
    Ok(Json(serde_json::json!({ "revoked": n })))
}

async fn list_groups(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<Vec<GroupInfo>>, ApiError> {
    Ok(Json(st.store.list_groups()?))
}

async fn create_group(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<NewGroup>,
) -> Result<Json<GroupInfo>, ApiError> {
    Ok(Json(st.store.create_group(&req, &actor_of(&ctx))?))
}

async fn delete_group(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    st.store.delete_group(&id, &actor_of(&ctx))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn set_group_members(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<GroupMembers>,
) -> Result<Json<GroupInfo>, ApiError> {
    Ok(Json(st.store.set_group_members(
        &id,
        &req.account_ids,
        &actor_of(&ctx),
    )?))
}

async fn list_shares(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<Vec<ShareInfo>>, ApiError> {
    Ok(Json(st.store.list_shares()?))
}

async fn create_share(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<NewShare>,
) -> Result<Json<ShareInfo>, ApiError> {
    // Liveness check across the database boundary: the shared resource must exist in the catalog
    // *now* (the store only validates uuid shape; ids are never recycled, so this can't be raced
    // into granting a future resource).
    //
    // It must also be a resource a share can actually *grant*. Two shapes pass the liveness check
    // yet grant nothing, and a share that is accepted but inert is worse than a refusal — the
    // operator believes access was given:
    //
    //  - a **smart collection**: `push_visibility` expands a granted collection only through
    //    `collection_member`, and a smart folder's membership is a live query, so it has no rows
    //    there — ever. The grant would be permanently empty.
    //  - a **federated peer source**: the engine skips the peer path for any restricted context
    //    (`is_full()` guards on query / get_asset / read_content / read_thumbnail), so the grantee
    //    would see the source in the sidebar with a non-zero count from the proxied stats — over a
    //    permanently empty grid.
    //
    // Both are v1 gaps in *reach*, not sharing bugs; when the engine can evaluate a peer or a
    // smart query under a ceiling, these rejections come out.
    let ectx = dam_api::service::AuthContext::embedded();
    match req.resource {
        ShareResource::Source => {
            let id = req
                .resource_id
                .parse::<dam_api::id::SourceId>()
                .map_err(|_| ApiError(LibError::BadRequest("invalid source id".into())))?;
            let source = st.lib.get_source(&ectx, &id).await?;
            if source.kind == SourceKind::Federated {
                return Err(ApiError(LibError::BadRequest(
                    "a federated peer source cannot be shared: reads against a peer are not                      evaluated under a visibility ceiling in v1, so the grant would show an empty                      library".into(),
                )));
            }
        }
        ShareResource::Collection => {
            let id = req
                .resource_id
                .parse::<dam_api::id::CollectionId>()
                .map_err(|_| ApiError(LibError::BadRequest("invalid collection id".into())))?;
            let coll = st.lib.get_collection(&ectx, &id).await?;
            if coll.kind == CollectionKind::Smart {
                return Err(ApiError(LibError::BadRequest(
                    "a smart folder cannot be shared: its membership is a live query with no                      stored members, so the grant would reach nothing. Share the source(s) it                      draws from instead".into(),
                )));
            }
        }
    }
    Ok(Json(st.store.create_share(&req, &actor_of(&ctx))?))
}

async fn delete_share(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    st.store.delete_share(&id, &actor_of(&ctx))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
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

// ── OIDC provider configuration (issue #41) ─────────────────────────────────

/// `GET /admin/api/oidc` — the configured provider, or `null` when none is set.
///
/// Returns [`OidcConfigInfo`], which has no field for the client secret: the "never returned by a
/// GET" rule (tech-spec 10 §5) is enforced by the type, not by remembering to strip it.
async fn get_oidc(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<Option<OidcConfigInfo>>, ApiError> {
    Ok(Json(st.store.oidc_config_info()?))
}

/// `PUT /admin/api/oidc` — set the provider config. The secret is write-only, and omitting it keeps
/// whatever is stored (see `ServerStore::set_oidc_config`).
async fn set_oidc(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<SetOidcConfig>,
) -> Result<Json<Option<OidcConfigInfo>>, ApiError> {
    st.store.set_oidc_config(&req, &actor_of(&ctx))?;
    Ok(Json(st.store.oidc_config_info()?))
}

/// `GET /admin/api/oidc/identities` — every provider subject linked to a local account.
async fn list_oidc_identities(
    AdminAuth(_ctx): AdminAuth,
    State(st): State<AppState>,
) -> Result<Json<Vec<OidcIdentity>>, ApiError> {
    Ok(Json(st.store.list_oidc_identities()?))
}

/// `POST /admin/api/oidc/identities` — link a provider subject to an existing account.
///
/// The issuer comes from the configured provider rather than the request, so a link can only ever
/// name an issuer this instance actually accepts tokens from.
async fn link_oidc_identity(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Json(req): Json<LinkOidcIdentity>,
) -> Result<Json<Vec<OidcIdentity>>, ApiError> {
    let cfg = st.store.oidc_config_info()?.ok_or_else(|| {
        ApiError(LibError::BadRequest(
            "configure the OIDC provider before linking identities to it".into(),
        ))
    })?;
    // Resolve the account first so a bad id is a clean 404 rather than a foreign-key error.
    let account = st.store.get_account(&req.account_id)?;
    st.store.link_oidc_identity(
        &cfg.config.issuer,
        req.subject.trim(),
        &account.account_id,
        &actor_of(&ctx),
    )?;
    Ok(Json(st.store.list_oidc_identities()?))
}

/// `DELETE /admin/api/oidc/identities/{subject}` — revoke the provider's ability to sign in as the
/// linked account. The account itself is untouched.
async fn unlink_oidc_identity(
    AdminAuth(ctx): AdminAuth,
    State(st): State<AppState>,
    Path(subject): Path<String>,
) -> Result<Json<Vec<OidcIdentity>>, ApiError> {
    let cfg = st.store.oidc_config_info()?.ok_or_else(|| {
        ApiError(LibError::BadRequest(
            "no OIDC provider is configured".into(),
        ))
    })?;
    st.store
        .unlink_oidc_identity(&cfg.config.issuer, &subject, &actor_of(&ctx))?;
    Ok(Json(st.store.list_oidc_identities()?))
}
