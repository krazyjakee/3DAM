//! Collection CRUD, membership mutation, and collection-scoped browsing.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn list_collections_impl(
        &self,
        ctx: &AuthContext,
    ) -> Result<Vec<Collection>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.list_collections_vis(&vis)).await
    }

    pub(crate) async fn get_collection_impl(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<Collection, LibError> {
        let id = *id;
        let vis = ctx.visibility.clone();
        let mut collection = self
            .db(move |s| {
                if !s.collection_visible(&id, &vis)? {
                    return Err(LibError::NotFound(format!("collection {id}")));
                }
                s.get_collection(&id, &vis)
            })
            .await?;
        if collection.kind == CollectionKind::Smart {
            // Count through the same path used to open the folder, including federation. Limit the
            // payload to one row and explicitly request the exact answering-stream total.
            let mut query = collection
                .query
                .clone()
                .ok_or_else(incompatible_smart_query)?;
            query.page = PageParams {
                after: None,
                limit: 1,
            };
            query.include_total = Some(true);
            collection.count = self.query(ctx, query).await?.total;
        }
        Ok(collection)
    }

    pub(crate) async fn create_collection_impl(
        &self,
        ctx: &AuthContext,
        req: NewCollection,
    ) -> Result<CollectionId, LibError> {
        // v1: collections are library-level objects with no owner concept — a restricted identity
        // cannot create one (post-v1 ownership may relax this; ADR 0009 freeze).
        Self::require_full_visibility(ctx, "creating a collection")?;
        if req.kind == CollectionKind::Smart && req.query.is_none() {
            return Err(LibError::BadRequest(
                "a smart folder requires a query".into(),
            ));
        }
        let query_json = serialize_opt_query(&req.query)?;
        let name = req.name.clone();
        let kind = req.kind;
        self.db(move |s| s.create_collection(&name, kind, query_json.as_deref()))
            .await
    }

    pub(crate) async fn update_collection_impl(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: UpdateCollection,
    ) -> Result<(), LibError> {
        self.require_collection_writable(ctx, id).await?;
        // Replacing a smart folder's saved query changes what it *matches*, not what the caller
        // can reach — results stay ceiling-filtered — so a write share safely covers it.
        let id = *id;
        let query_json = serialize_opt_query(&req.query)?;
        let name = req.name.clone();
        self.db(move |s| s.update_collection(&id, name.as_deref(), query_json.as_deref()))
            .await
    }

    pub(crate) async fn delete_collection_impl(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        self.require_collection_writable(ctx, id).await?;
        let id = *id;
        self.db(move |s| s.delete_collection(&id)).await
    }

    pub(crate) async fn modify_collection_members_impl(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        req: CollectionMembers,
    ) -> Result<(), LibError> {
        self.require_collection_writable(ctx, id).await?;
        // Every asset being *added* must itself be reachable — otherwise membership in a shared
        // collection would grant visibility of a hidden asset (issue #42 rule 5, inverted).
        if !ctx.visibility.is_full() {
            for a in &req.add {
                self.require_asset_visible(ctx, a).await?;
            }
        }
        let id = *id;
        let add = req.add.clone();
        let remove = req.remove.clone();
        self.db(move |s| s.modify_collection_members(&id, &add, &remove))
            .await
    }

    pub(crate) async fn collection_assets_impl(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
        page: PageParams,
    ) -> Result<Page<AssetSummary>, LibError> {
        let id = *id;
        let vis = ctx.visibility.clone();
        let collection = self
            .db(move |s| {
                if !s.collection_visible(&id, &vis)? {
                    return Err(LibError::NotFound(format!("collection {id}")));
                }
                s.get_collection(&id, &vis)
            })
            .await?;
        match collection.kind {
            CollectionKind::Manual => {
                let vis = ctx.visibility.clone();
                self.db(move |s| {
                    let items = s.collection_summaries(&id, page.clamped(500), &vis)?;
                    Ok(Page::new(items, None))
                })
                .await
            }
            CollectionKind::Smart => {
                // Resolve through the ordinary query entry point, not the local store directly:
                // saved source filters and unscoped searches retain federation semantics. A query
                // that an upgraded build can no longer decode fails closed instead of widening to
                // the entire library; the web UI surfaces this as a replace-query warning.
                let mut query = collection.query.ok_or_else(incompatible_smart_query)?;
                query.page = page;
                self.query(ctx, query).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incompatible_smart_query_fails_closed_instead_of_listing_every_asset() {
        let temp = tempfile::tempdir().unwrap();
        let library = EmbeddedLibrary::open(temp.path()).await.unwrap();
        let id = library
            .db(|store| {
                store.create_collection(
                    "legacy search",
                    CollectionKind::Smart,
                    Some(
                        r#"{"filters":[{"field":"removed_facet","op":"eq","value":{"str":"x"}}]}"#,
                    ),
                )
            })
            .await
            .unwrap();
        let error = library
            .collection_assets(
                &AuthContext::embedded(),
                &id,
                PageParams {
                    after: None,
                    limit: 24,
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("cannot read"),
            "legacy query should produce an actionable error: {error}"
        );
    }
}
