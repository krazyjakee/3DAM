//! Source, folder, asset-removal, and blocklist HTTP handlers.

use crate::auth::{Reader, Writer};
use crate::{parse_id, ApiError, AppState};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, MethodRouter};
use axum::Json;
use dam_api::dto::{
    AddSource, BlockEntry, FolderEntry, FolderListing, RemoveAsset, RemoveSource, SourceInfo,
};
use dam_api::id::{AssetId, ContentHash, SourceId};
use dam_api::service::LibraryService;
use dam_api::LibError;

/// Method routers for source registration, browsing, removal, and blocklisting.
pub(crate) struct Routes {
    pub(crate) sources: MethodRouter<AppState>,
    pub(crate) folders: MethodRouter<AppState>,
    pub(crate) source: MethodRouter<AppState>,
    pub(crate) blocklist: MethodRouter<AppState>,
    pub(crate) unblock: MethodRouter<AppState>,
}

pub(crate) fn routes() -> Routes {
    Routes {
        sources: get(list_sources).post(add_source),
        folders: post(list_folders),
        source: get(get_source).delete(remove_source),
        blocklist: get(list_blocklist),
        unblock: delete(unblock),
    }
}

impl Routes {
    /// Add source-owned asset removal to the asset detail router before its one path registration.
    pub(crate) fn with_remove_asset(
        &self,
        router: MethodRouter<AppState>,
    ) -> MethodRouter<AppState> {
        router.delete(remove_asset)
    }
}

async fn list_sources(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<SourceInfo>>, ApiError> {
    Ok(Json(st.lib.list_sources(&ctx).await?))
}

async fn list_folders(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    Json(req): Json<FolderListing>,
) -> Result<Json<Vec<FolderEntry>>, ApiError> {
    Ok(Json(st.lib.list_folders(&ctx, req).await?))
}

async fn get_source(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<SourceInfo>, ApiError> {
    let id: SourceId = parse_id(&id, "source")?;
    Ok(Json(st.lib.get_source(&ctx, &id).await?))
}

async fn add_source(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<AddSource>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = st.lib.add_source(&ctx, req).await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

async fn remove_source(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RemoveSource>,
) -> Result<StatusCode, ApiError> {
    let id: SourceId = parse_id(&id, "source")?;
    st.lib.remove_source(&ctx, &id, req).await?;
    // Orphan-GC the cross-database soft references (issue #42): shares of a deleted source are
    // dead grants. This is a required durable cleanup; the resource mutation lives in the other
    // database, so a cleanup failure fails the request and is safe to retry.
    st.store.remove_shares_for_resource(
        dam_api::accounts::ShareResource::Source,
        &id.to_string(),
        "system",
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_asset(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RemoveAsset>,
) -> Result<StatusCode, ApiError> {
    let id: AssetId = parse_id(&id, "asset")?;
    st.lib.remove_asset(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_blocklist(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<BlockEntry>>, ApiError> {
    Ok(Json(st.lib.list_blocklist(&ctx).await?))
}

async fn unblock(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(hash): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let hash = ContentHash::from_hex(&hash)
        .ok_or_else(|| ApiError(LibError::BadRequest("invalid content hash".into())))?;
    st.lib.unblock(&ctx, &hash).await?;
    Ok(StatusCode::NO_CONTENT)
}
