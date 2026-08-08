use crate::EmbeddedLibrary;
use dam_api::dto::{
    Asset, AssetSummary, CollectionKind, ExportRequest, MediaType, QueryRequest, SearchMode,
};
use dam_api::id::{AssetId, CollectionId, SourceId};
use dam_api::page::{Page, PageParams};
use dam_api::service::{AuthContext, LibraryService, Visibility};
use dam_api::{ItemWarning, LibError};
use std::collections::HashSet;

/// Per-target warnings a bulk metadata edit will itemise before collapsing the rest into one
/// summary row. Bulk results stay summary-shaped: a 1000-id request must not answer with 1000 rows.
const BULK_WARNING_MAX: usize = 50;

/// Reject a bulk-edit request that names no selector or more than one. `noun` names the edit in the
/// message ("tag edit", "license edit").
pub(super) fn require_single_selector(
    assets: &[AssetId],
    collection: Option<&CollectionId>,
    query: Option<&QueryRequest>,
    noun: &str,
) -> Result<(), LibError> {
    let selectors = usize::from(!assets.is_empty())
        + usize::from(collection.is_some())
        + usize::from(query.is_some());
    if selectors == 1 {
        Ok(())
    } else {
        Err(LibError::BadRequest(format!(
            "{noun} requires exactly one assets, collection, or query selector"
        )))
    }
}

/// The account id behind a request, or `Forbidden` (issue #82).
///
/// Posting requires a **person**, not merely a credential. A bearer token has an identity string
/// but is typically a shared machine credential — attributing a conversation to one would be a
/// fiction — and an anonymous caller has nothing to attribute at all. The embedded engine likewise
/// has no account, which is consistent: discussion is a multi-user feature, and the single-user
/// local library has notes.
pub(super) fn require_account(ctx: &AuthContext) -> Result<String, LibError> {
    ctx.account
        .as_ref()
        .map(|a| a.account_id.clone())
        .ok_or_else(|| {
            LibError::Forbidden("posting to a discussion requires a signed-in account".into())
        })
}

// The ceiling is resolved once at auth time (server) and enforced here, in the engine, as a query
// predicate — so search, similarity, dedup, stats, folders, collections, previews, and export all
// filter through the same few store choke points and no transport handler can forget. An
// unreachable resource answers `NotFound`/absent, never `Forbidden` — existence is part of what
// the ceiling hides.
impl EmbeddedLibrary {
    /// Restricted contexts may not run library-wide operations (source management, scans,
    /// analysis, blocklist edits, conversion): the reachable set is per-resource, but these
    /// operations span the whole catalog.
    pub(super) fn require_full_visibility(ctx: &AuthContext, what: &str) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            Ok(())
        } else {
            Err(LibError::Forbidden(format!(
                "{what} requires unrestricted visibility"
            )))
        }
    }

    /// Guard a single-asset read: an asset outside the ceiling answers `NotFound`,
    /// indistinguishable from a nonexistent id.
    pub(super) async fn require_asset_visible(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<(), LibError> {
        if self.asset_visible(ctx, id).await? {
            Ok(())
        } else {
            Err(LibError::NotFound(format!("asset {id}")))
        }
    }

    /// Local-catalog half of an asset guard. Peer-owned assets are not stored in this database, so
    /// follow-up reads use the explicit federated source hint after this returns false.
    pub(super) async fn asset_visible(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<bool, LibError> {
        if ctx.visibility.is_full() {
            let id = *id;
            return self
                .db(move |store| match store.get_asset(&id) {
                    Ok(_) => Ok(true),
                    Err(LibError::NotFound(_)) => Ok(false),
                    Err(error) => Err(error),
                })
                .await;
        }
        let vis = ctx.visibility.clone();
        let id = *id;
        self.db(move |s| s.asset_visible(&id, &vis)).await
    }

    /// Whether a failed local lookup may be routed to a peer. Restricted callers must carry the
    /// source attribution returned by search and that source must still be in the current ceiling;
    /// unrestricted owner contexts retain bounded hintless bookmark recovery.
    pub(super) fn may_proxy_peer(ctx: &AuthContext, source: Option<SourceId>) -> bool {
        ctx.visibility.is_full()
            || source.is_some_and(|source| ctx.visibility.allows_source(&source))
    }

    /// Guard a single-asset write: the asset must be *write*-reachable (a `write` share on its
    /// source or a containing shared collection). Unreachable-for-read stays `NotFound`; readable
    /// but not writable is `Forbidden` — the share level, not existence, is what's denied here.
    pub(super) async fn require_asset_writable(
        &self,
        ctx: &AuthContext,
        id: &AssetId,
    ) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            return Ok(());
        }
        self.require_asset_visible(ctx, id).await?;
        let wv = ctx.visibility.write_view();
        let id = *id;
        let ok = self.db(move |s| s.asset_visible(&id, &wv)).await?;
        if ok {
            Ok(())
        } else {
            Err(LibError::Forbidden(
                "no write access to this asset (a write share is required)".into(),
            ))
        }
    }

    /// Guard a collection write: `NotFound` when unreachable, `Forbidden` without a write share.
    pub(super) async fn require_collection_writable(
        &self,
        ctx: &AuthContext,
        id: &CollectionId,
    ) -> Result<(), LibError> {
        if ctx.visibility.is_full() {
            return Ok(());
        }
        let vis = ctx.visibility.clone();
        let cid = *id;
        let visible = self.db(move |s| s.collection_visible(&cid, &vis)).await?;
        if !visible {
            return Err(LibError::NotFound(format!("collection {id}")));
        }
        if ctx.visibility.allows_collection_write(id) {
            Ok(())
        } else {
            Err(LibError::Forbidden(
                "no write access to this collection (a write share is required)".into(),
            ))
        }
    }

    /// Resolve a bulk-edit selection into `(readable, writable)` id lists, de-duplicated and in
    /// selection order.
    ///
    /// Shared by every bulk metadata mutation (`edit_tags`, `set_license`) so they cannot drift
    /// apart on the part that decides *who may be written*. Exactly one selector must be present;
    /// explicit ids win, then a collection (manual members or a smart folder's live query), then a
    /// raw query — matching export selection.
    ///
    /// The two sets are computed with two passes of the same reachability predicate: `readable`
    /// under the caller's ceiling, `writable` under its [`Visibility::write_view`]. Keeping them
    /// separate is what lets a caller be told "you can see this but not edit it"
    /// (`target_read_only`) rather than "no such asset" (`target_unavailable`).
    ///
    /// Ids the local catalog does not hold simply fall out of both sets: `visible_asset_ids` only
    /// returns rows that exist. Peer-owned assets are therefore excluded structurally — federation
    /// proxies reads rather than mirroring rows, so a peer id is never in the local `asset` table
    /// and no local write path can reach it (PRODUCT_SPEC §5: federated assets are read-only,
    /// origin-attributed).
    pub(super) async fn resolve_bulk_targets(
        &self,
        ctx: &AuthContext,
        assets: Vec<AssetId>,
        collection: Option<CollectionId>,
        query: Option<QueryRequest>,
    ) -> Result<(Vec<AssetId>, Vec<AssetId>), LibError> {
        let read_vis = ctx.visibility.clone();
        let write_vis = ctx.visibility.write_view();
        self.db(move |store| {
            let candidates = if !assets.is_empty() {
                assets
            } else if let Some(collection) = collection {
                if !store.collection_visible(&collection, &read_vis)? {
                    return Err(LibError::NotFound(format!("collection {collection}")));
                }
                let record = store.get_collection(&collection, &read_vis)?;
                match record.kind {
                    CollectionKind::Manual => store.collection_member_ids(&collection)?,
                    CollectionKind::Smart => {
                        store.query_asset_ids(&record.query.unwrap_or_default(), &read_vis)?
                    }
                }
            } else {
                store.query_asset_ids(&query.expect("selector validated"), &read_vis)?
            };
            let filter = |visibility: &Visibility| -> Result<Vec<AssetId>, LibError> {
                let mut visible = HashSet::new();
                for chunk in candidates.chunks(500) {
                    visible.extend(store.visible_asset_ids(chunk, visibility)?);
                }
                let mut seen = HashSet::new();
                Ok(candidates
                    .iter()
                    .copied()
                    .filter(|id| visible.contains(id) && seen.insert(*id))
                    .collect())
            };
            let readable = filter(&read_vis)?;
            let writable_set: HashSet<_> = filter(&write_vis)?.into_iter().collect();
            let writable: Vec<AssetId> = readable
                .iter()
                .copied()
                .filter(|id| writable_set.contains(id))
                .collect();
            Ok((readable, writable))
        })
        .await
    }

    /// Bounded per-target warnings for a bulk edit, shared by every bulk metadata mutation.
    ///
    /// An explicit selection names ids the caller believes in, so each excluded one earns its own
    /// warning — capped at [`BULK_WARNING_MAX`], with a summary row standing in for the remainder
    /// so a 1000-id request can never return a 1000-row response. A server-resolved selection
    /// (collection/query) names nothing, so exclusions collapse into a single count: the caller
    /// never asserted those ids and cannot act on them individually.
    pub(super) fn bulk_target_warnings(
        explicit: &[AssetId],
        readable: &[AssetId],
        writable: &[AssetId],
    ) -> Vec<ItemWarning> {
        let mut warnings = Vec::new();
        if explicit.is_empty() {
            if readable.len() > writable.len() {
                warnings.push(ItemWarning {
                    subject: "selection".into(),
                    code: "targets_excluded".into(),
                    message: format!(
                        "{} readable local targets were excluded because they require a write share",
                        readable.len() - writable.len()
                    ),
                });
            }
            return warnings;
        }
        let readable_set: HashSet<_> = readable.iter().copied().collect();
        let writable_set: HashSet<_> = writable.iter().copied().collect();
        for id in explicit {
            let warning = if !readable_set.contains(id) {
                Some((
                    "target_unavailable",
                    "Asset is unavailable or outside your read scope",
                ))
            } else if !writable_set.contains(id) {
                Some((
                    "target_read_only",
                    "Asset is readable but requires a write share",
                ))
            } else {
                None
            };
            if let Some((code, message)) = warning {
                if warnings.len() < BULK_WARNING_MAX {
                    warnings.push(ItemWarning {
                        subject: id.to_string(),
                        code: code.into(),
                        message: message.into(),
                    });
                }
            }
        }
        let excluded = explicit.len().saturating_sub(writable.len());
        if excluded > warnings.len() {
            warnings.push(ItemWarning {
                subject: "selection".into(),
                code: "warnings_truncated".into(),
                message: format!(
                    "{excluded} targets were excluded; individual warning details are capped at {BULK_WARNING_MAX}"
                ),
            });
        }
        warnings
    }

    /// The local-index query path under a visibility ceiling (issue #42) — the ceiling composes into
    /// the store's WHERE clause, so lexical, hybrid, and semantic paths all filter identically.
    pub(crate) async fn local_query_vis(
        &self,
        req: QueryRequest,
        vis: Visibility,
    ) -> Result<Page<AssetSummary>, LibError> {
        let model = self.semantic.clone();
        self.db(move |s| {
            // Model-backed text→asset search (semantic-search M4): when a semantic model is loaded
            // and this is a Hybrid/Semantic text query, encode the query string into the model's
            // shared space so assets that match the *meaning* (not the filename) rank in. Encoding
            // is CPU-bound and runs here on the blocking DB thread. No model ⇒ `None` ⇒ model-free.
            let text_vec = match (&model, req.mode, req.text.as_deref()) {
                (Some(m), SearchMode::Hybrid | SearchMode::Semantic, Some(t)) if !t.is_empty() => m
                    .encode_text(MediaType::Image, t)
                    .map(|v| (m.space_id(MediaType::Image), v)),
                _ => None,
            };
            s.query_assets_semantic(&req, text_vec, &vis)
        })
        .await
    }

    /// Materialize the authorized union only when a restricted export includes at least one
    /// federated source. Peer rows are never copied into the local store, so the local SQL exporter
    /// cannot see them; collecting details through the ordinary guarded read seam keeps source
    /// selection, peer credentials, deadlines, and revocation identical to interactive reads.
    pub(super) async fn federated_export_assets(
        &self,
        ctx: &AuthContext,
        req: &ExportRequest,
    ) -> Result<Option<Vec<Asset>>, LibError> {
        if ctx.visibility.is_full() {
            return Ok(None);
        }
        let peer_sources = self
            .fed_peers()
            .await
            .iter()
            .filter(|peer| ctx.visibility.allows_source(&peer.source_id))
            .map(|peer| peer.source_id)
            .collect::<Vec<_>>();
        if peer_sources.is_empty() {
            return Ok(None);
        }

        if !req.assets.is_empty() {
            let mut assets = Vec::new();
            for id in &req.assets {
                if let Ok(asset) = self.get_asset(ctx, id).await {
                    assets.push(asset);
                    continue;
                }
                for source in &peer_sources {
                    if let Ok(asset) = self.get_asset_from(ctx, id, Some(*source)).await {
                        assets.push(asset);
                        break;
                    }
                }
            }
            return Ok(Some(assets));
        }

        let mut query = if let Some(collection) = req.collection {
            let record = self.get_collection(ctx, &collection).await?;
            if record.kind == CollectionKind::Manual {
                return Ok(None);
            }
            record.query.ok_or_else(crate::incompatible_smart_query)?
        } else {
            req.query.clone().unwrap_or_default()
        };
        query.include_facets = false;
        query.include_total = Some(false);
        query.page = PageParams {
            after: None,
            limit: 500,
        };

        let mut assets = Vec::new();
        loop {
            let page = self.query(ctx, query.clone()).await?;
            if !page.partial.complete {
                return Err(LibError::SourceUnavailable(
                    "federated export stopped because an authorized peer did not answer".into(),
                ));
            }
            for summary in page.items {
                assets.push(
                    self.get_asset_from(ctx, &summary.id, summary.source_id)
                        .await?,
                );
            }
            let Some(cursor) = page.cursor else {
                break;
            };
            query.page.after = Some(cursor);
        }
        Ok(Some(assets))
    }
}
