//! Collection CRUD, membership, listing, and export HTTP handlers.

use crate::auth::{Reader, Writer};
use crate::{parse_id, ApiError, AppState};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post, MethodRouter};
use axum::Json;
use dam_api::dto::{
    AssetSummary, Collection, CollectionMembers, ExportReport, ExportRequest, NewCollection,
    UpdateCollection,
};
use dam_api::id::CollectionId;
use dam_api::service::LibraryService;

/// Method routers for collection management and synchronous export.
pub(crate) struct Routes {
    pub(crate) collections: MethodRouter<AppState>,
    pub(crate) collection: MethodRouter<AppState>,
    pub(crate) members: MethodRouter<AppState>,
    pub(crate) assets: MethodRouter<AppState>,
    pub(crate) export: MethodRouter<AppState>,
}

pub(crate) fn routes() -> Routes {
    Routes {
        collections: get(list_collections).post(create_collection),
        collection: get(get_collection)
            .put(update_collection)
            .delete(delete_collection),
        members: post(modify_collection_members),
        assets: post(collection_assets),
        export: post(export),
    }
}

async fn list_collections(
    Reader(ctx): Reader,
    State(st): State<AppState>,
) -> Result<Json<Vec<Collection>>, ApiError> {
    Ok(Json(st.lib.list_collections(&ctx).await?))
}

async fn get_collection(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Json<Collection>, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    Ok(Json(st.lib.get_collection(&ctx, &id).await?))
}

async fn create_collection(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<NewCollection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = st.lib.create_collection(&ctx, req).await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

async fn update_collection(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<UpdateCollection>,
) -> Result<StatusCode, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    st.lib.update_collection(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_collection(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    st.lib.delete_collection(&ctx, &id).await?;
    // Orphan-GC shares pointing at the deleted collection (issue #42; see remove_source).
    st.store.remove_shares_for_resource(
        dam_api::accounts::ShareResource::Collection,
        &id.to_string(),
        "system",
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn modify_collection_members(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(req): Json<CollectionMembers>,
) -> Result<StatusCode, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    st.lib.modify_collection_members(&ctx, &id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn collection_assets(
    Reader(ctx): Reader,
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Json(page): Json<dam_api::page::PageParams>,
) -> Result<Json<dam_api::page::Page<AssetSummary>>, ApiError> {
    let id: CollectionId = parse_id(&id, "collection")?;
    Ok(Json(st.lib.collection_assets(&ctx, &id, page).await?))
}

async fn export(
    Writer(ctx): Writer,
    State(st): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> Result<Json<ExportReport>, ApiError> {
    Ok(Json(st.lib.export(&ctx, req).await?))
}
