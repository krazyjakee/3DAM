//! Tags, favorites, and license metadata mutations.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn edit_tags_impl(
        &self,
        ctx: &AuthContext,
        mut req: TagEditRequest,
    ) -> Result<TagEditResult, LibError> {
        const TAG_NAME_MAX: usize = 64;
        const TAG_DELTA_MAX: usize = 50;

        ctx.require(Scope::Write)?;
        require_single_selector(
            &req.assets,
            req.collection.as_ref(),
            req.query.as_ref(),
            "tag edit",
        )?;
        if req.assets.len() > TAG_EDIT_EXPLICIT_MAX {
            return Err(LibError::BadRequest(format!(
                "tag edit accepts at most {TAG_EDIT_EXPLICIT_MAX} explicit assets"
            )));
        }
        let normalize = |tags: Vec<String>| -> Result<Vec<String>, LibError> {
            let mut normalized = std::collections::BTreeSet::new();
            for tag in tags {
                let tag = tag.trim().to_lowercase();
                if tag.is_empty() || tag.chars().count() > TAG_NAME_MAX {
                    return Err(LibError::BadRequest(format!(
                        "tag names must contain 1–{TAG_NAME_MAX} characters"
                    )));
                }
                normalized.insert(tag);
            }
            if normalized.len() > TAG_DELTA_MAX {
                return Err(LibError::BadRequest(format!(
                    "tag edit accepts at most {TAG_DELTA_MAX} additions or removals"
                )));
            }
            Ok(normalized.into_iter().collect())
        };
        req.add = normalize(std::mem::take(&mut req.add))?;
        req.remove = normalize(std::mem::take(&mut req.remove))?;
        if req.add.is_empty() && req.remove.is_empty() {
            return Err(LibError::BadRequest(
                "tag edit requires at least one addition or removal".into(),
            ));
        }
        if let Some(tag) = req.add.iter().find(|tag| req.remove.contains(tag)) {
            return Err(LibError::BadRequest(format!(
                "tag '{tag}' cannot be added and removed in one edit"
            )));
        }

        let (readable, writable) = self
            .resolve_bulk_targets(ctx, req.assets.clone(), req.collection, req.query.clone())
            .await?;
        let warnings = Self::bulk_target_warnings(&req.assets, &readable, &writable);

        let add = req.add.clone();
        let remove = req.remove.clone();
        let dry_run = req.dry_run;
        let mut outcome = self
            .db(move |store| store.edit_manual_tags(&writable, &add, &remove, dry_run))
            .await?;
        outcome.result.warnings = warnings;
        if !dry_run {
            for (id, source_id) in outcome.changed_assets {
                reliability::publish_event(
                    &self.events,
                    LibraryEvent::AssetChanged {
                        id,
                        source_id,
                        kind: ChangeKind::Retagged,
                    },
                    "publish tag edit",
                );
            }
        }
        Ok(outcome.result)
    }

    pub(crate) async fn list_tags_impl(
        &self,
        ctx: &AuthContext,
        mut req: TagListRequest,
    ) -> Result<Vec<TagInfo>, LibError> {
        ctx.require(Scope::Read)?;
        req.prefix = req
            .prefix
            .map(|prefix| prefix.trim().to_lowercase())
            .filter(|prefix| !prefix.is_empty());
        if req.prefix.as_ref().is_some_and(|prefix| prefix.len() > 64) {
            return Err(LibError::BadRequest(
                "tag prefix must be at most 64 bytes".into(),
            ));
        }
        let vis = ctx.visibility.clone();
        self.db(move |store| store.list_tags(req.prefix.as_deref(), req.limit, &vis))
            .await
    }

    pub(crate) async fn set_favorite_impl(
        &self,
        ctx: &AuthContext,
        req: FavoriteRequest,
    ) -> Result<(), LibError> {
        self.require_asset_writable(ctx, &req.asset).await?;
        let id = req.asset;
        let on = req.favorite;
        self.db(move |s| s.set_favorite(&id, on)).await?;
        let source_id = self.db(move |s| s.asset_source(&id)).await?;
        reliability::publish_event(
            &self.events,
            LibraryEvent::AssetChanged {
                id: req.asset,
                source_id,
                kind: ChangeKind::Metadata,
            },
            "publish favourite change",
        );
        Ok(())
    }

    /// Apply a rights patch across a selection (issue #106).
    ///
    /// Deliberately the same shape as [`edit_tags`](Self::edit_tags) — same three selectors, same
    /// `(readable, writable)` split, same bounded warnings — because correcting an extractor's
    /// licence guess is a bulk act, and a bulk act that silently skips half its targets is worse
    /// than one that refuses. Both share [`Self::resolve_bulk_targets`] so the authorization half
    /// cannot drift.
    ///
    /// Two things are *not* computed here. `license_status` is derived by the store at write time
    /// from the patched id and rights (tech-spec 02 §5, ADR 0009 §1), so a caller can never assert
    /// "permissive" without naming a licence. And a dry run reports the authorized selection size
    /// as its `changed` rather than a true per-row diff — that diff only exists inside the write —
    /// so it reads as "at most this many rows will change", and its `status` mix is left empty
    /// rather than guessed.
    pub(crate) async fn set_license_impl(
        &self,
        ctx: &AuthContext,
        req: SetLicenseRequest,
    ) -> Result<LicenseEditResult, LibError> {
        ctx.require(Scope::Write)?;
        require_single_selector(
            &req.assets,
            req.collection.as_ref(),
            req.query.as_ref(),
            "license edit",
        )?;
        if req.assets.len() > LICENSE_EDIT_EXPLICIT_MAX {
            return Err(LibError::BadRequest(format!(
                "license edit accepts at most {LICENSE_EDIT_EXPLICIT_MAX} explicit assets"
            )));
        }

        let (readable, writable) = self
            .resolve_bulk_targets(ctx, req.assets.clone(), req.collection, req.query.clone())
            .await?;
        let warnings = Self::bulk_target_warnings(&req.assets, &readable, &writable);
        let mut result = LicenseEditResult {
            matched: writable.len() as u64,
            warnings,
            ..LicenseEditResult::default()
        };

        // An all-absent patch is a legitimate no-op — the caller still learns what its selector
        // resolved to and which targets it could not have written. Nothing reaches the store.
        if req.license.is_empty() {
            return Ok(result);
        }
        if req.dry_run {
            result.changed = writable.len() as u64;
            return Ok(result);
        }

        let patch = req.license.clone();
        let changed = self
            .db(move |store| {
                let changed = store.set_license(&writable, &patch)?;
                // Source attribution for the events below. Resolved on the same blocking thread,
                // after the write guard is gone, so the event fan-out costs no extra round trip
                // per asset from the async side.
                changed
                    .into_iter()
                    .map(|(id, status)| store.asset_source(&id).map(|source| (id, status, source)))
                    .collect::<Result<Vec<_>, LibError>>()
            })
            .await?;

        result.changed = changed.len() as u64;
        // Fixed-order tally (permissive → attribution → restricted → unknown) rather than a map:
        // the mix is four buckets, and a stable order lets a client render it without sorting.
        const MIX: [LicenseStatus; 4] = [
            LicenseStatus::Permissive,
            LicenseStatus::Attribution,
            LicenseStatus::Restricted,
            LicenseStatus::Unknown,
        ];
        let mut mix = [0u64; MIX.len()];
        for (id, status, source_id) in changed {
            if let Some(slot) = MIX.iter().position(|candidate| *candidate == status) {
                mix[slot] += 1;
            }
            reliability::publish_event(
                &self.events,
                LibraryEvent::AssetChanged {
                    id,
                    source_id,
                    kind: ChangeKind::LicenseSet,
                },
                "publish license edit",
            );
        }
        result.status = MIX
            .into_iter()
            .zip(mix)
            .filter(|(_, count)| *count > 0)
            .map(|(status, count)| LicenseStatusCount { status, count })
            .collect();
        Ok(result)
    }
}
