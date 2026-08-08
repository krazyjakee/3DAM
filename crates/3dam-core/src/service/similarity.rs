//! Similarity search, duplicate groups, and suggestion/duplicate review.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn find_similar_impl(
        &self,
        ctx: &AuthContext,
        req: SimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        // The seed asset itself must be reachable (a hidden id must not seed a ranking), and the
        // neighbour candidates are ceiling-filtered inside the store's summary fetch.
        if !self.asset_visible(ctx, &req.asset).await? {
            if !req.local_only {
                if let Some(page) =
                    federation::federated_seed_similar(self, &req, &ctx.visibility).await
                {
                    return Ok(page);
                }
            }
            return Err(LibError::NotFound(format!("asset {}", req.asset)));
        }
        let (asset, k) = (req.asset, req.k);
        let filters = req.filters.clone();
        let vis = ctx.visibility.clone();
        let (space, hits) = self
            .db(move |s| s.similar(&asset, k, &filters, &vis))
            .await?;
        let hits = hits
            .into_iter()
            // Tag each hit with the space it was actually ranked in (the explanation, §3.2). This
            // is the `space_id` the store ranked against, not a string rebuilt from the media type:
            // spaces are not uniformly named (documents rank in `text-hash-v1`) and a model-backed
            // embedding ranks in its own space entirely, so reconstructing the label would state a
            // space the ranking never used.
            .map(|(asset, score)| SimilarHit {
                space: space.clone(),
                asset,
                score,
            })
            .collect::<Vec<_>>();
        if req.local_only {
            return Ok(Page::new(hits, None));
        }
        // Cross-peer similarity (phase 6, issue #40): ship the query asset's own vector to every
        // matched-space peer and merge one globally-ranked list. No vector yet (or a source-pinned
        // query) degrades to the local page. A peer-owned query asset is instead forwarded whole —
        // the owning peer ranks it in its index (`local_only` keeps that a single hop).
        if self.fed_peers().await.is_empty()
            || req.filters.iter().any(|f| f.field == FacetField::Source)
        {
            return Ok(Page::new(hits, None));
        }
        let embedding = self.db(move |s| s.embedding_for(&asset)).await?;
        match embedding {
            Some((space, vector)) => {
                let media = self.get_asset(ctx, &req.asset).await?.summary.media;
                Ok(federation::federated_similar(
                    self,
                    &req,
                    &ctx.visibility,
                    media,
                    space,
                    vector,
                    hits,
                )
                .await)
            }
            None => {
                let local_has = !hits.is_empty()
                    || self
                        .db(move |s| s.get_asset(&asset).map(|_| ()))
                        .await
                        .is_ok();
                if local_has {
                    return Ok(Page::new(hits, None));
                }
                for peer in self.fed_peers().await.iter() {
                    let mut fwd = req.clone();
                    fwd.local_only = true;
                    if let Ok(Ok(mut page)) = tokio::time::timeout(
                        federation::QUERY_DEADLINE,
                        peer.client.find_similar(ctx, fwd),
                    )
                    .await
                    {
                        for hit in &mut page.items {
                            hit.asset.origin = Origin::Peer(peer.name.clone());
                        }
                        return Ok(page);
                    }
                }
                Ok(Page::new(hits, None))
            }
        }
    }

    pub(crate) async fn find_similar_by_vector_impl(
        &self,
        ctx: &AuthContext,
        req: dam_api::VectorSimilarRequest,
    ) -> Result<Page<SimilarHit>, LibError> {
        let vis = ctx.visibility.clone();
        // The serving side of cross-peer similarity (issue #40): rank the shipped vector against
        // this catalog's own index. Strictly local by construction — never re-fans-out.
        let dam_api::VectorSimilarRequest {
            media: _,
            space,
            vector,
            k,
            filters,
        } = req;
        let space_for_hits = space.clone();
        let hits = self
            .db(move |s| s.similar_by_vector(&space, &vector, k, &filters, &vis))
            .await?
            .into_iter()
            .map(|(asset, score)| SimilarHit {
                space: space_for_hits.clone(),
                asset,
                score,
            })
            .collect::<Vec<_>>();
        Ok(Page::new(hits, None))
    }

    pub(crate) async fn list_duplicates_impl(
        &self,
        ctx: &AuthContext,
        req: DupRequest,
    ) -> Result<Page<DupGroup>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.duplicates(&req, &vis)).await
    }

    pub(crate) async fn duplicate_membership_impl(
        &self,
        ctx: &AuthContext,
        req: DupMembershipRequest,
    ) -> Result<Vec<DupMembership>, LibError> {
        if req.assets.len() > DUP_MEMBERSHIP_ASSET_MAX {
            return Err(LibError::BadRequest(format!(
                "duplicate membership accepts at most {DUP_MEMBERSHIP_ASSET_MAX} assets"
            )));
        }
        let vis = ctx.visibility.clone();
        self.db(move |s| s.duplicate_membership(&req.assets, &vis))
            .await
    }

    pub(crate) async fn duplicate_group_impl(
        &self,
        ctx: &AuthContext,
        asset: &AssetId,
    ) -> Result<Option<DupGroup>, LibError> {
        let vis = ctx.visibility.clone();
        let asset = *asset;
        self.db(move |s| s.duplicate_group(&asset, &vis)).await
    }

    pub(crate) async fn duplicate_group_members_impl(
        &self,
        ctx: &AuthContext,
        req: DupGroupMembersRequest,
    ) -> Result<Page<DupMember>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.duplicate_group_members(&req, &vis))
            .await
    }

    pub(crate) async fn review_duplicate_impl(
        &self,
        ctx: &AuthContext,
        req: DupReviewRequest,
    ) -> Result<(), LibError> {
        ctx.require(Scope::Write)?;
        // Review state is library-wide: allowing a restricted editor to dismiss a group would hide
        // it from unrelated reviewers. Full visibility also makes blocklist-wide consequences
        // explicit and matches the existing remove+block authority boundary.
        Self::require_full_visibility(ctx, "reviewing duplicate groups")?;
        if req.removals.len() > DUP_GROUP_MEMBER_MAX {
            return Err(LibError::BadRequest(format!(
                "a duplicate decision can remove at most {DUP_GROUP_MEMBER_MAX} assets"
            )));
        }
        for removal in &req.removals {
            self.require_asset_writable(ctx, &removal.asset).await?;
        }
        let outcome = self.db(move |store| store.review_duplicate(&req)).await?;
        for (id, source_id) in outcome.removed_assets {
            reliability::publish_event(
                &self.events,
                LibraryEvent::AssetRemoved {
                    id,
                    source_id: Some(source_id),
                },
                "publish duplicate review removal",
            );
        }
        Ok(())
    }

    pub(crate) async fn review_suggestion_impl(
        &self,
        ctx: &AuthContext,
        req: SuggestionReview,
    ) -> Result<(), LibError> {
        self.require_asset_writable(ctx, &req.asset).await?;
        let id = req.asset;
        let tag = req.tag.clone();
        let action = req.action;
        self.db(move |s| s.review_suggestion(&id, &tag, action))
            .await?;
        let source_id = self.db(move |s| s.asset_source(&id)).await?;
        reliability::publish_event(
            &self.events,
            LibraryEvent::AssetChanged {
                id: req.asset,
                source_id,
                kind: ChangeKind::Retagged,
            },
            "publish suggestion review",
        );
        Ok(())
    }
}
