//! Bounded batch persistence for scan and analysis (issue #138) — part of the `Store` impl.
//!
//! A scan used to persist each asset through four or five independent `&self` store calls, every
//! one of them re-taking the writer and autocommitting: a hundred-thousand-file library meant
//! roughly half a million transactions, each with its own WAL frame flush. The catalog write side
//! is the same shape as [`Store::edit_manual_tags`] — a slice in, one transaction, per-item work,
//! and an outcome struct carrying what the engine needs in order to emit events *after* the commit
//! — so these methods follow it.
//!
//! Three rules make batching safe rather than merely fast:
//!
//! **One guard, one transaction.** A batch takes a single [`Store::write`] guard and everything
//! inside it runs on that `&Connection`. Calling another `&self` store method from in here would
//! take a second guard and trip the nested-guard check in [`crate::db`] (a deadlock on an
//! in-memory store), which is why every step has an `_in` sibling that borrows the caller's
//! connection.
//!
//! **A per-item savepoint, not catch-and-continue.** One corrupt asset must not sink the batch,
//! but it must not half-land either. The catalog has trigger-maintained read models on both sides
//! of an asset row — the V26 `library_stat`/`media_stat`/`source_stat`/`source_media_stat`/
//! `tag_stat`/`source_tag_stat` aggregates and the V20 `folder` hierarchy — so an item that
//! inserted its `asset` row and then failed on its attributes would leave every one of those
//! counters incremented for a row that no longer exists. `ROLLBACK TO SAVEPOINT` unwinds trigger
//! effects together with the base rows that caused them; it is the only thing that keeps the
//! aggregates honest under partial failure.
//!
//! **Tag interning is hoisted above the savepoints.** Every distinct tag name in a batch is
//! interned once, before the first item savepoint opens. That is a correctness requirement, not an
//! optimisation: a `tag_id` minted inside a savepoint that later rolls back would be a dangling
//! foreign key for every other item that reused it. The reverse leftover — a `tag` row interned for
//! an item that then rolled back — is harmless, because `tag_stat`'s own backfill recipe yields
//! `(0, 0)` for a tag no asset carries.

use super::*;
use std::collections::HashMap;

// ── scan ─────────────────────────────────────────────────────────────────────

/// Per-flush state shared by every item in one scan batch.
///
/// `generation` is the source's current scan generation ([`Store::begin_source_scan`]). It lives on
/// the context rather than the item because a scan walks one source at a time, and a batch is
/// flushed inside that walk.
pub struct ScanBatchContext {
    pub job: JobId,
    pub state: JobState,
    /// Progress counters as of the end of this batch — written inside the same transaction, so
    /// "N assets persisted" and "N assets reported" cannot disagree after a crash.
    pub done: u64,
    pub total: Option<u64>,
    pub current: Option<String>,
    pub generation: i64,
}

/// One asset's whole scan-time persistence: the catalog row plus its cheap-tier attributes.
pub struct ScanWrite {
    pub asset: NewAsset,
    pub attrs: MediaAttributes,
}

/// What one scan item did, for the events the caller emits after the commit.
pub enum ScanItemOutcome {
    /// The row was written.
    Written {
        id: AssetId,
        /// A brand-new catalog row (as opposed to a delta reconcile of an existing one).
        inserted: bool,
        /// `false` when a newer scan generation superseded this one while the batch was being
        /// filled: this walk must not finalise missing rows.
        generation_current: bool,
    },
    /// The content hash is on the blocklist (issue #21), so nothing was written. Checked inside the
    /// transaction, which both saves a read checkout per asset and closes the window where a hash
    /// blocked mid-scan still gets catalogued.
    Blocked,
}

/// Per-item results, positionally aligned with the input slice, plus the job as it stands after the
/// commit — the caller reads `job.state` to notice a cancellation that landed mid-batch.
pub struct ScanBatchOutcome {
    pub items: Vec<Result<ScanItemOutcome, String>>,
    pub job: JobStatus,
}

// ── analysis ─────────────────────────────────────────────────────────────────

/// Per-flush state shared by every item in one analysis batch.
pub struct AnalysisBatchContext {
    pub job: JobId,
    pub state: JobState,
    pub done: u64,
    pub total: Option<u64>,
    pub current: Option<String>,
}

/// The continuous acoustic features of one audio asset (issue #61).
pub struct AudioFeatureWrite {
    pub loudness_lufs: f32,
    pub brightness: f32,
    pub harmonicity: f32,
}

/// One vector for one [EmbeddingSpace](crate). `vector` must already be L2-normalised.
pub struct EmbeddingWrite {
    pub space_id: String,
    pub media: MediaType,
    pub vector: Vec<f32>,
    pub extractor: String,
}

/// One automatic tag suggestion (§1.4).
pub struct TagSuggestion {
    pub name: String,
    pub confidence: f32,
    pub extractor: String,
    pub explanation: String,
}

/// Everything the analysis pass derives for one asset, in one all-or-nothing unit.
///
/// Build it with [`AnalysisWrite::new`] and fill in only the parts a given media type produces;
/// every field is independently optional, and an empty `AnalysisWrite` writes nothing at all.
pub struct AnalysisWrite {
    pub id: AssetId,
    /// Cheap-tier attributes refined by the deep pass (a model's exact counts, a document's page
    /// and word counts).
    pub attrs: Option<MediaAttributes>,
    pub image: Option<ImageAnalysis>,
    /// The auto-category guess, on the attr row of the given media type.
    pub class: Option<(MediaType, String)>,
    pub audio_features: Option<AudioFeatureWrite>,
    pub audio_peaks: Option<Vec<f32>>,
    /// Extracted body text for the `text` FTS column (schema V11).
    pub document_text: Option<String>,
    pub embeddings: Vec<EmbeddingWrite>,
    /// Spaces whose vector must go away because this re-analysis produced none (a document
    /// revision with no readable text).
    pub cleared_spaces: Vec<String>,
    pub tags: Vec<TagSuggestion>,
    /// The pipeline version to stamp. Written **last**, so a failure anywhere above leaves the
    /// asset legitimately due for re-analysis instead of marked done with half a derivation.
    pub analysed_version: Option<i64>,
}

impl AnalysisWrite {
    pub fn new(id: AssetId) -> AnalysisWrite {
        AnalysisWrite {
            id,
            attrs: None,
            image: None,
            class: None,
            audio_features: None,
            audio_peaks: None,
            document_text: None,
            embeddings: Vec::new(),
            cleared_spaces: Vec::new(),
            tags: Vec::new(),
            analysed_version: None,
        }
    }
}

/// What one analysis item did.
pub struct AnalysisItemOutcome {
    pub id: AssetId,
    /// The version marker was applied, i.e. this asset is now genuinely analysed.
    pub analysed: bool,
}

pub struct AnalysisBatchOutcome {
    pub items: Vec<Result<AnalysisItemOutcome, String>>,
    pub job: JobStatus,
}

impl Store {
    /// Delta-probe a chunk of enumerated paths in one transaction, returning each path's prior
    /// change token positionally.
    ///
    /// This is [`Store::observe_source_path`] in bulk: the streaming walk asks it before deciding
    /// which entries are worth opening, and that question used to cost one transaction and one
    /// writer hand-off per directory entry.
    pub fn probe_scan_chunk(
        &self,
        source: &SourceId,
        generation: i64,
        paths: &[String],
    ) -> Result<Vec<Option<SourceChangeToken>>, LibError> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            out.push(Self::observe_source_path_in(&tx, source, path, generation)?);
        }
        tx.commit().map_err(internal)?;
        Ok(out)
    }

    /// Persist a slice of scanned assets — catalog row, cheap-tier attributes, generation stamp —
    /// plus the job's progress, in exactly one write transaction with one savepoint per asset.
    ///
    /// Per-item failures are reported in place and never abort the batch; the returned `Err` is
    /// reserved for a failure of the transaction itself (or a `ctx.job` that does not exist, which
    /// only the post-commit read-back can detect). Nothing here emits events: the caller does that
    /// from the outcome, once the rows are durable.
    pub fn apply_scan_batch(
        &self,
        ctx: &ScanBatchContext,
        items: &[ScanWrite],
    ) -> Result<ScanBatchOutcome, LibError> {
        let mut conn = self.write();
        let mut tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        let mut results = Vec::with_capacity(items.len());
        let mut dropped_embeddings = false;
        for item in items {
            let mut sp = tx.savepoint().map_err(internal)?;
            match Self::scan_item_in(&sp, ctx, item) {
                Ok((outcome, dropped)) => {
                    sp.commit().map_err(internal)?;
                    dropped_embeddings |= dropped;
                    results.push(Ok(outcome));
                }
                Err(err) => {
                    // Rolling back to the savepoint takes the item's trigger effects with it; the
                    // savepoint stays open afterwards, so it still has to be released.
                    sp.rollback().map_err(internal)?;
                    sp.commit().map_err(internal)?;
                    results.push(Err(err.to_string()));
                }
            }
        }
        Self::update_job_progress_in(
            &tx,
            &ctx.job,
            ctx.state,
            ctx.done,
            ctx.total,
            ctx.current.as_deref(),
        )?;
        tx.commit().map_err(internal)?;
        if dropped_embeddings {
            self.embed_gen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Read back on the writer connection we already hold: a second guard would trip the
        // nested-guard check, and this is the caller's cancellation signal, so it must see the
        // state this very transaction just committed.
        let job = Self::job_summary_in(&conn, &ctx.job)?;
        Ok(ScanBatchOutcome {
            items: results,
            job,
        })
    }

    /// One scanned asset, inside its own savepoint.
    fn scan_item_in(
        conn: &Connection,
        ctx: &ScanBatchContext,
        item: &ScanWrite,
    ) -> Result<(ScanItemOutcome, bool), LibError> {
        if let Some(hash) = &item.asset.content_hash {
            if Self::is_blocked_in(conn, hash)? {
                return Ok((ScanItemOutcome::Blocked, false));
            }
        }
        let up = Self::upsert_asset_in(conn, &item.asset)?;
        // Only a fresh row needs stamping here: an existing one was stamped by the delta probe
        // that decided it needed reopening. `None` means a newer generation took over.
        let generation_current = if up.inserted {
            Self::observe_source_path_in(
                conn,
                &item.asset.source_id,
                &item.asset.path,
                ctx.generation,
            )?
            .is_some()
        } else {
            true
        };
        Self::set_media_attrs_in(conn, &up.id, &item.attrs)?;
        Ok((
            ScanItemOutcome::Written {
                id: up.id,
                inserted: up.inserted,
                generation_current,
            },
            up.dropped_embeddings,
        ))
    }

    /// Persist a slice of analysed assets and the job's progress in one write transaction, one
    /// savepoint per asset — the analysis-side twin of [`Store::apply_scan_batch`], with the same
    /// failure contract.
    pub fn apply_analysis_batch(
        &self,
        ctx: &AnalysisBatchContext,
        items: &[AnalysisWrite],
    ) -> Result<AnalysisBatchOutcome, LibError> {
        let mut conn = self.write();
        let mut tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // Above every savepoint (see the module header): an id minted inside one that later rolls
        // back would be a dangling FK for the items that reused it. Keyed case-insensitively
        // because `intern_tag` itself is.
        let mut tags: HashMap<String, Vec<u8>> = HashMap::new();
        for item in items {
            for tag in &item.tags {
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    tags.entry(tag.name.to_lowercase())
                {
                    slot.insert(Self::intern_tag(&tx, &tag.name)?);
                }
            }
        }
        let mut results = Vec::with_capacity(items.len());
        let mut touched_embeddings = false;
        for item in items {
            let mut sp = tx.savepoint().map_err(internal)?;
            match Self::analysis_item_in(&sp, item, &tags) {
                Ok((outcome, touched)) => {
                    sp.commit().map_err(internal)?;
                    touched_embeddings |= touched;
                    results.push(Ok(outcome));
                }
                Err(err) => {
                    sp.rollback().map_err(internal)?;
                    sp.commit().map_err(internal)?;
                    results.push(Err(err.to_string()));
                }
            }
        }
        Self::update_job_progress_in(
            &tx,
            &ctx.job,
            ctx.state,
            ctx.done,
            ctx.total,
            ctx.current.as_deref(),
        )?;
        tx.commit().map_err(internal)?;
        if touched_embeddings {
            self.embed_gen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let job = Self::job_summary_in(&conn, &ctx.job)?;
        Ok(AnalysisBatchOutcome {
            items: results,
            job,
        })
    }

    /// One analysed asset, inside its own savepoint. The order is deliberate: derived rows, then
    /// vectors, then tags (one FTS reindex for the lot, not one per tag), then the version stamp.
    fn analysis_item_in(
        conn: &Connection,
        item: &AnalysisWrite,
        tags: &HashMap<String, Vec<u8>>,
    ) -> Result<(AnalysisItemOutcome, bool), LibError> {
        if let Some(attrs) = &item.attrs {
            Self::set_media_attrs_in(conn, &item.id, attrs)?;
        }
        if let Some(image) = &item.image {
            Self::set_image_analysis_in(conn, &item.id, image)?;
        }
        if let Some((media, class)) = &item.class {
            Self::set_media_class_in(conn, &item.id, *media, class)?;
        }
        if let Some(f) = &item.audio_features {
            Self::set_audio_features_in(
                conn,
                &item.id,
                f.loudness_lufs,
                f.brightness,
                f.harmonicity,
            )?;
        }
        if let Some(peaks) = &item.audio_peaks {
            Self::set_audio_peaks_in(conn, &item.id, peaks)?;
        }
        if let Some(text) = &item.document_text {
            Self::set_document_text_in(conn, &item.id, text)?;
        }
        let mut touched = false;
        for e in &item.embeddings {
            Self::set_embedding_in(
                conn,
                &item.id,
                &e.space_id,
                e.media,
                &e.vector,
                &e.extractor,
            )?;
            touched = true;
        }
        for space in &item.cleared_spaces {
            touched |= Self::clear_embedding_in(conn, &item.id, space)?;
        }
        for tag in &item.tags {
            let tag_id = tags
                .get(&tag.name.to_lowercase())
                .ok_or_else(|| internal(format!("tag {:?} was not interned", tag.name)))?;
            Self::suggest_tag_in(
                conn,
                &item.id,
                tag_id,
                tag.confidence,
                &tag.extractor,
                &tag.explanation,
            )?;
        }
        if !item.tags.is_empty() {
            // Once per asset. `asset_fts.tags` is rewritten wholesale from the confirmed set, so
            // doing this per suggestion rewrote the same column N times for N tags.
            Self::reindex_asset_tags(conn, &item.id);
        }
        if let Some(version) = item.analysed_version {
            Self::mark_analysed_in(conn, &item.id, version)?;
        }
        Ok((
            AnalysisItemOutcome {
                id: item.id,
                analysed: item.analysed_version.is_some(),
            },
            touched,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_sources::SourceConnection;
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const SCANNED_AT: i64 = 1_700_000_000_000;

    fn store_with_source() -> (Store, SourceId) {
        let store = Store::open_in_memory().unwrap();
        let src = store
            .add_source(
                &SourceConnection::LocalFs {
                    root: "/tmp".into(),
                },
                "t",
                false,
            )
            .unwrap();
        (store, src)
    }

    fn job_for(store: &Store, src: SourceId) -> JobId {
        store
            .create_job(JobKind::Scan, "{}", Some(100), &[src])
            .unwrap()
    }

    fn scan_ctx(job: JobId, done: u64) -> ScanBatchContext {
        ScanBatchContext {
            job,
            state: JobState::Running,
            done,
            total: Some(100),
            current: None,
            generation: 0,
        }
    }

    fn analysis_ctx(job: JobId, done: u64) -> AnalysisBatchContext {
        AnalysisBatchContext {
            job,
            state: JobState::Running,
            done,
            total: Some(100),
            current: None,
        }
    }

    /// An image asset under `pack/<n>/`, whose `image_attr.width` doubles as the failure switch:
    /// the temp trigger installed by [`arm_attr_failure`] aborts on width 666, *after* the asset
    /// row and its aggregate/folder trigger effects have already landed.
    fn image_write(src: SourceId, n: usize, width: i64) -> ScanWrite {
        ScanWrite {
            asset: NewAsset {
                source_id: src,
                path: format!("pack/{n}/tex{n}.png"),
                filename: format!("tex{n}.png"),
                content_hash: Some(ContentHash([n as u8; 32])),
                size_bytes: Some(64),
                source_modified_at: Some(1),
                scanned_at: SCANNED_AT,
                media_type: MediaType::Image,
                format: "png".into(),
            },
            attrs: MediaAttributes::Image(ImageAttributes {
                width: Some(width),
                height: Some(16),
                ..Default::default()
            }),
        }
    }

    /// Fail one item *mid-way*: the abort has to land after the asset insert, because a failure in
    /// the insert itself would be undone by SQLite's own statement journal and would prove nothing
    /// about savepoints.
    fn arm_attr_failure(store: &Store) {
        store
            .write()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_attr AFTER INSERT ON image_attr
                 WHEN new.width = 666 BEGIN SELECT RAISE(ABORT, 'injected attr failure'); END;",
            )
            .unwrap();
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store
            .read()
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    /// Every column of every row of `sql`, rendered to text so heterogeneous tables can be diffed.
    fn rows(store: &Store, sql: &str) -> Vec<String> {
        let conn = store.read().unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        stmt.query_map([], |r| {
            let mut out = String::new();
            for i in 0..cols {
                use rusqlite::types::ValueRef;
                out.push_str(&match r.get_ref(i)? {
                    ValueRef::Null => "NULL".to_string(),
                    ValueRef::Integer(v) => v.to_string(),
                    ValueRef::Real(v) => v.to_string(),
                    ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                    ValueRef::Blob(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
                });
                out.push('|');
            }
            Ok(out)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    /// The V20 hierarchy, recomputed from `asset.path` with the migration's own recursive recipe.
    /// Empty directories are pruned by the delete trigger, so only source roots survive with zero
    /// descendants.
    fn assert_folder_integrity(store: &Store) {
        let expected = rows(
            store,
            "WITH RECURSIVE hierarchy(source_id, path, rest) AS (
                 SELECT source_id, '', path FROM asset
                 UNION ALL
                 SELECT source_id, path || substr(rest, 1, instr(rest, '/')),
                        substr(rest, instr(rest, '/') + 1)
                   FROM hierarchy WHERE instr(rest, '/') > 0
             )
             SELECT source_id, path, sum(instr(rest, '/') = 0), count(*)
               FROM hierarchy GROUP BY source_id, path
             UNION ALL
             SELECT id, '', 0, 0 FROM source s
              WHERE NOT EXISTS (SELECT 1 FROM asset WHERE source_id = s.id)
             ORDER BY 1, 2",
        );
        let actual = rows(
            store,
            "SELECT source_id, path, direct_asset_count, descendant_asset_count
               FROM folder ORDER BY 1, 2",
        );
        assert_eq!(actual, expected, "folder hierarchy drifted");
    }

    fn assert_aggregates(store: &Store) {
        crate::schema::assert_aggregate_integrity(&store.read().unwrap());
        assert_folder_integrity(store);
    }

    /// A five-item batch whose middle item aborts. The other four are committed, and the failure
    /// is reported in place — the engine's per-asset fail-soft rule (DG §6) survives batching.
    #[test]
    fn a_failing_item_does_not_sink_its_batch() {
        let (store, src) = store_with_source();
        let job = job_for(&store, src);
        arm_attr_failure(&store);
        let items: Vec<ScanWrite> = (0..5)
            .map(|n| image_write(src, n, if n == 2 { 666 } else { 32 }))
            .collect();

        let out = store.apply_scan_batch(&scan_ctx(job, 5), &items).unwrap();

        assert_eq!(out.items.len(), 5);
        for (i, item) in out.items.iter().enumerate() {
            if i == 2 {
                assert!(item.is_err(), "the poisoned item should have failed");
            } else {
                assert!(
                    matches!(item, Ok(ScanItemOutcome::Written { inserted: true, .. })),
                    "item {i} should have been written"
                );
            }
        }
        assert_eq!(count(&store, "SELECT COUNT(*) FROM asset"), 4);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM asset WHERE path = 'pack/2/tex2.png'"
            ),
            0,
            "the failed item left a catalog row behind"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM image_attr"), 4);
        // The progress update rides in the same transaction and is unaffected by the item failure.
        assert_eq!(out.job.progress.done, 5);
    }

    /// The reason per-item failure has to be a savepoint and not a caught error: the failing item
    /// had already inserted its `asset` row, and every V26 aggregate plus the V20 folder counters
    /// were incremented by triggers on the way. Nothing but a rollback unwinds those with it.
    #[test]
    fn a_rolled_back_item_leaves_no_aggregate_drift() {
        let (store, src) = store_with_source();
        let job = job_for(&store, src);
        arm_attr_failure(&store);
        let items: Vec<ScanWrite> = (0..5)
            .map(|n| image_write(src, n, if n == 2 { 666 } else { 32 }))
            .collect();
        store.apply_scan_batch(&scan_ctx(job, 5), &items).unwrap();

        assert_aggregates(&store);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM folder WHERE path = 'pack/2/'"),
            0,
            "the failed item's folder survived it"
        );

        // And again with the analysis batch, which reaches the tag dimensions of the same tables.
        let ids = asset_ids(&store);
        let job = job_for(&store, src);
        let mut good = AnalysisWrite::new(ids[0]);
        good.tags.push(suggestion("rock"));
        good.analysed_version = Some(3);
        let mut doomed = AnalysisWrite::new(ids[1]);
        doomed.tags.push(suggestion("rock"));
        doomed.analysed_version = Some(FAIL_VERSION);
        arm_mark_failure(&store);
        store
            .apply_analysis_batch(&analysis_ctx(job, 2), &[good, doomed])
            .unwrap();
        assert_aggregates(&store);
    }

    /// The whole point of the change: a batch is one commit, however many assets it carries and
    /// whether or not one of them rolled back. Counted with SQLite's own commit hook, so the
    /// assertion is about actual durable transactions rather than about our call graph.
    #[test]
    fn a_batch_is_exactly_one_write_transaction() {
        let (store, src) = store_with_source();
        let job = job_for(&store, src);
        arm_attr_failure(&store);
        let commits = Arc::new(AtomicUsize::new(0));
        {
            let counter = Arc::clone(&commits);
            store
                .write()
                .commit_hook(Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }))
                .unwrap();
        }

        let clean: Vec<ScanWrite> = (0..128).map(|n| image_write(src, n, 32)).collect();
        commits.store(0, Ordering::Relaxed);
        store.apply_scan_batch(&scan_ctx(job, 128), &clean).unwrap();
        assert_eq!(
            commits.load(Ordering::Relaxed),
            1,
            "128 scanned assets should cost exactly one commit"
        );

        let mixed: Vec<ScanWrite> = (128..256)
            .map(|n| image_write(src, n, if n == 200 { 666 } else { 32 }))
            .collect();
        commits.store(0, Ordering::Relaxed);
        store.apply_scan_batch(&scan_ctx(job, 256), &mixed).unwrap();
        assert_eq!(
            commits.load(Ordering::Relaxed),
            1,
            "an item rollback must not split the batch's commit"
        );

        let ids = asset_ids(&store);
        let analysis: Vec<AnalysisWrite> = ids
            .iter()
            .map(|id| {
                let mut w = AnalysisWrite::new(*id);
                w.class = Some((MediaType::Image, "texture".into()));
                w.tags.push(suggestion("texture"));
                w.analysed_version = Some(3);
                w
            })
            .collect();
        let job = job_for(&store, src);
        commits.store(0, Ordering::Relaxed);
        store
            .apply_analysis_batch(&analysis_ctx(job, analysis.len() as u64), &analysis)
            .unwrap();
        assert_eq!(
            commits.load(Ordering::Relaxed),
            1,
            "an analysis batch should cost exactly one commit too"
        );

        arm_mark_failure(&store);
        let mut poisoned = AnalysisWrite::new(ids[0]);
        poisoned.analysed_version = Some(FAIL_VERSION);
        let job = job_for(&store, src);
        commits.store(0, Ordering::Relaxed);
        store
            .apply_analysis_batch(&analysis_ctx(job, 1), &[poisoned])
            .unwrap();
        assert_eq!(
            commits.load(Ordering::Relaxed),
            1,
            "an analysis item rollback must not split the commit either"
        );
    }

    /// The anti-drift guard for the `_in` refactor: the batch path and the one-call-at-a-time path
    /// must leave byte-identical catalogs. Ids and wall-clock stamps are excluded (they are
    /// generated per row), everything else — including the index-only FTS columns, the folder
    /// hierarchy, and all six aggregate tables — is compared row for row.
    #[test]
    fn batched_writes_match_unbatched_writes_row_for_row() {
        // A fixed source id makes the two catalogs comparable without normalising blobs.
        let src = SourceId::new();
        let fixed = |store: Store| -> Store {
            store
                .add_source_with_auth(
                    src,
                    &SourceConnection::LocalFs {
                        root: "/tmp".into(),
                    },
                    "t",
                    false,
                    None,
                )
                .unwrap();
            store
        };
        let single = fixed(Store::open_in_memory().unwrap());
        let batched = fixed(Store::open_in_memory().unwrap());
        let items: Vec<ScanWrite> = (0..24)
            .map(|n| image_write(src, n, 32 + n as i64))
            .collect();

        for item in &items {
            let (id, inserted) = single.upsert_asset(&item.asset).unwrap();
            assert!(inserted);
            single
                .observe_source_path(&src, &item.asset.path, 0)
                .unwrap();
            single.set_media_attrs(&id, &item.attrs).unwrap();
        }
        let job = job_for(&batched, src);
        batched
            .apply_scan_batch(&scan_ctx(job, items.len() as u64), &items)
            .unwrap();

        for sql in [
            "SELECT path, filename, hex(content_hash), size_bytes, source_modified_at, scanned_at,
                    media_type, format, analysis_version, analysed_at, derivative_version, flags,
                    seen_generation, hex(source_id) FROM asset ORDER BY path",
            "SELECT a.path, i.width, i.height, i.color_depth, i.has_alpha, i.color_space, i.phash,
                    i.class, i.texture_format, i.mip_levels
               FROM image_attr i JOIN asset a ON a.id = i.asset_id ORDER BY a.path",
            "SELECT a.path, u.duration_ms, u.class FROM audio_attr u
               JOIN asset a ON a.id = u.asset_id ORDER BY a.path",
            "SELECT a.path, m.vertex_count, m.class FROM model_attr m
               JOIN asset a ON a.id = m.asset_id ORDER BY a.path",
            "SELECT a.path, v.duration_ms, v.class FROM video_attr v
               JOIN asset a ON a.id = v.asset_id ORDER BY a.path",
            "SELECT a.path, d.page_count, d.class FROM document_attr d
               JOIN asset a ON a.id = d.asset_id ORDER BY a.path",
            "SELECT a.path, f.tokens, f.folder, f.tags, f.text, f.note
               FROM asset_fts f JOIN asset a ON a.rowid = f.rowid ORDER BY a.path",
            "SELECT path, parent_path, name, direct_asset_count, descendant_asset_count
               FROM folder ORDER BY path",
            "SELECT asset_count, unanalyzed_count, source_count FROM library_stat",
            "SELECT media_type, asset_count FROM media_stat ORDER BY media_type",
            "SELECT hex(source_id), asset_count, unanalyzed_count FROM source_stat
              ORDER BY source_id",
            "SELECT hex(source_id), media_type, asset_count FROM source_media_stat
              ORDER BY source_id, media_type",
            "SELECT t.name, s.asset_count, s.manual_count FROM tag_stat s
               JOIN tag t ON t.id = s.tag_id ORDER BY t.name",
            "SELECT t.name, s.asset_count, s.manual_count FROM source_tag_stat s
               JOIN tag t ON t.id = s.tag_id ORDER BY t.name",
        ] {
            assert_eq!(
                rows(&batched, sql),
                rows(&single, sql),
                "batched and unbatched catalogs differ for: {sql}"
            );
        }
        assert_aggregates(&batched);
    }

    /// Folding the three same-row UPDATEs of `upsert_asset` into one changed how often
    /// `aggregate_asset_au`/`folder_asset_au` fire, and those triggers reason on old→new
    /// transitions rather than on absolute state. Both gates the split statements used to open —
    /// analysis and derivative — must still open, on both the content-changed and media-changed
    /// paths, with the aggregates still exactly right afterwards.
    #[test]
    fn a_merged_upsert_still_reopens_both_gates() {
        let (store, src) = store_with_source();
        let seed = |hash: u8, media: MediaType, format: &str| NewAsset {
            source_id: src,
            path: "pack/clip.mp4".into(),
            filename: "clip.mp4".into(),
            content_hash: Some(ContentHash([hash; 32])),
            size_bytes: Some(10),
            source_modified_at: Some(1),
            scanned_at: SCANNED_AT,
            media_type: media,
            format: format.into(),
        };
        let gates = |store: &Store| -> (i64, Option<i64>, i64) {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT analysis_version, analysed_at, derivative_version FROM asset",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };

        let (id, _) = store
            .upsert_asset(&seed(1, MediaType::Video, "mp4"))
            .unwrap();
        store
            .set_media_attrs(
                &id,
                &MediaAttributes::Video(VideoAttributes {
                    duration_ms: Some(1234),
                    ..Default::default()
                }),
            )
            .unwrap();
        store
            .set_embedding(&id, "video-stats-v1", MediaType::Video, &[1.0, 0.0], "t@1")
            .unwrap();
        store.set_document_text(&id, "stale body text").unwrap();
        store.mark_analysed(&id, 3).unwrap();
        store
            .mark_derivative_ready(&id, 2, Some(ContentHash([1; 32])))
            .unwrap();
        assert_eq!(gates(&store).0, 3, "fixture should start analysed");
        assert_aggregates(&store);

        // Same bytes, same media, but a new content hash: analysis and derivative gates reopen,
        // the attr row survives.
        store
            .upsert_asset(&seed(2, MediaType::Video, "mp4"))
            .unwrap();
        assert_eq!(
            gates(&store),
            (0, None, 0),
            "content change reopened nothing"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM video_attr"), 1);
        assert_aggregates(&store);

        store.mark_analysed(&id, 3).unwrap();
        store
            .mark_derivative_ready(&id, 2, Some(ContentHash([2; 32])))
            .unwrap();
        assert_aggregates(&store);

        // Reclassified audio-only: the video attr row, the vector, and the indexed body text all
        // describe a media type this asset no longer is.
        store
            .upsert_asset(&seed(2, MediaType::Audio, "m4a"))
            .unwrap();
        assert_eq!(gates(&store), (0, None, 0), "media change reopened nothing");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM video_attr"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM embedding"), 0);
        assert_eq!(
            rows(&store, "SELECT text FROM asset_fts"),
            vec!["|".to_string()],
            "the old media's body text stayed indexed"
        );
        assert_eq!(
            rows(&store, "SELECT media_type, asset_count FROM media_stat"),
            vec!["audio|1|".to_string()],
            "the merged UPDATE mis-counted the media transition"
        );
        assert_aggregates(&store);
    }

    /// Interning above the savepoints is what makes a rolled-back item safe for its neighbours.
    ///
    /// The doomed item deliberately goes **first** and names both tags: if interning happened
    /// inside its savepoint, the rollback would take `shared` with it and the survivor behind it
    /// would either reuse a dangling id or silently mint a second one. Hoisted, both names stay
    /// interned and `orphan` is left carried by nobody — the acceptable leftover, because a tag no
    /// asset holds reads as `(0, 0)` through `tag_stat`'s own backfill recipe.
    #[test]
    fn interned_tags_survive_a_rolled_back_item_consistently() {
        let (store, src) = store_with_source();
        let job = job_for(&store, src);
        let items: Vec<ScanWrite> = (0..2).map(|n| image_write(src, n, 32)).collect();
        store.apply_scan_batch(&scan_ctx(job, 2), &items).unwrap();
        let ids = asset_ids(&store);
        arm_mark_failure(&store);

        let mut doomed = AnalysisWrite::new(ids[0]);
        doomed.tags.push(suggestion("shared"));
        doomed.tags.push(suggestion("orphan"));
        doomed.analysed_version = Some(FAIL_VERSION);
        let mut survivor = AnalysisWrite::new(ids[1]);
        survivor.tags.push(suggestion("shared"));
        survivor.analysed_version = Some(3);
        let job = job_for(&store, src);
        let out = store
            .apply_analysis_batch(&analysis_ctx(job, 2), &[doomed, survivor])
            .unwrap();
        assert!(out.items[0].is_err());
        assert!(out.items[1].is_ok());

        assert_eq!(
            rows(
                &store,
                "SELECT t.name FROM asset_tag at JOIN tag t ON t.id = at.tag_id ORDER BY t.name"
            ),
            vec!["shared|".to_string()],
            "the rolled-back item kept or lost the wrong tag rows"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM asset_tag at LEFT JOIN tag t ON t.id = at.tag_id
                  WHERE t.id IS NULL"
            ),
            0,
            "a tag id was minted inside a savepoint that rolled back"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM tag"),
            2,
            "both names should stay interned; the unused one is inert"
        );
        assert_aggregates(&store);
    }

    /// An analysis item is all-or-nothing: an embedding that fails must take the derived
    /// attributes, the tags and the version marker with it, or the next planner pass would skip an
    /// asset whose derivation is half missing.
    #[test]
    fn an_analysis_item_is_all_or_nothing() {
        let (store, src) = store_with_source();
        let job = job_for(&store, src);
        let items: Vec<ScanWrite> = (0..2).map(|n| image_write(src, n, 32)).collect();
        store.apply_scan_batch(&scan_ctx(job, 2), &items).unwrap();
        let ids = asset_ids(&store);
        // Abort on the vector's dimension, i.e. after this item's attrs and class have landed.
        store
            .write()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_embed AFTER INSERT ON embedding
                 WHEN new.dim = 3 BEGIN SELECT RAISE(ABORT, 'injected embedding failure'); END;",
            )
            .unwrap();

        let mut doomed = AnalysisWrite::new(ids[0]);
        doomed.image = Some(ImageAnalysis {
            phash: 7,
            tileability: 0.5,
            repeat_period: Some(4),
            tile_class: "tiled".into(),
            dominant_colors: vec!["#ffffff".into()],
            class: "texture".into(),
        });
        doomed.class = Some((MediaType::Image, "texture".into()));
        doomed.embeddings.push(EmbeddingWrite {
            space_id: "image-stats-v1".into(),
            media: MediaType::Image,
            vector: vec![1.0, 0.0, 0.0],
            extractor: "image-stats@1".into(),
        });
        doomed.tags.push(suggestion("texture"));
        doomed.analysed_version = Some(3);

        let mut fine = AnalysisWrite::new(ids[1]);
        fine.class = Some((MediaType::Image, "photo".into()));
        fine.embeddings.push(EmbeddingWrite {
            space_id: "image-stats-v1".into(),
            media: MediaType::Image,
            vector: vec![0.0, 1.0],
            extractor: "image-stats@1".into(),
        });
        fine.analysed_version = Some(3);

        let job = job_for(&store, src);
        let out = store
            .apply_analysis_batch(&analysis_ctx(job, 2), &[doomed, fine])
            .unwrap();
        assert!(
            out.items[0].is_err(),
            "the poisoned item should have failed"
        );
        assert!(out.items[1].is_ok());

        let key = ids[0].as_bytes().to_vec();
        let (version, analysed): (i64, Option<i64>) = store
            .read()
            .unwrap()
            .query_row(
                "SELECT analysis_version, analysed_at FROM asset WHERE id = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (version, analysed),
            (0, None),
            "a failed item claimed to be analysed"
        );
        assert_eq!(
            rows(
                &store,
                "SELECT phash, class FROM image_attr i JOIN asset a ON a.id = i.asset_id
                  ORDER BY a.path"
            )[0],
            "NULL|NULL|",
            "the failed item's derived columns survived"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM asset_tag"),
            0,
            "the failed item's tags survived"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM embedding"), 1);
        assert_aggregates(&store);
    }

    /// `asset_fts.tags` is rewritten wholesale from the confirmed set, so the reindex belongs once
    /// per asset, not once per suggestion. Both halves are asserted: the trace counter proves the
    /// statement ran exactly once for each asset that carried tags, and the resulting column proves
    /// the rewrite is still the *confirmed*-only projection (pending automation must not silently
    /// power full-text results before review).
    #[test]
    fn suggestions_reindex_fts_tags_once_per_asset() {
        let (store, src) = store_with_source();
        let job = job_for(&store, src);
        let items: Vec<ScanWrite> = (0..2).map(|n| image_write(src, n, 32)).collect();
        store.apply_scan_batch(&scan_ctx(job, 2), &items).unwrap();
        let ids = asset_ids(&store);
        // One confirmed tag up front, so "confirmed only" is a real filter and not a vacuous "".
        store
            .edit_manual_tags(&ids, &["approved".to_string()], &[], false)
            .unwrap();

        let batch: Vec<AnalysisWrite> = ids
            .iter()
            .map(|id| {
                let mut w = AnalysisWrite::new(*id);
                for name in ["alpha", "beta", "gamma", "delta"] {
                    w.tags.push(suggestion(name));
                }
                w.analysed_version = Some(3);
                w
            })
            .collect();
        let job = job_for(&store, src);
        REINDEXES.store(0, Ordering::Relaxed);
        store
            .write()
            .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_reindexes));
        store
            .apply_analysis_batch(&analysis_ctx(job, 2), &batch)
            .unwrap();
        store.write().trace_v2(TraceEventCodes::empty(), None);

        assert_eq!(
            REINDEXES.load(Ordering::Relaxed),
            2,
            "the FTS tag column should be rewritten once per asset, not once per suggestion"
        );
        assert_eq!(
            rows(&store, "SELECT tags FROM asset_fts ORDER BY rowid"),
            vec!["approved|".to_string(), "approved|".to_string()],
            "suggested tags leaked into the confirmed-only FTS column"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM asset_tag WHERE state = 'suggested'"
            ),
            8,
            "the suggestions themselves should still be there for review"
        );
        assert_aggregates(&store);
    }

    // ── shared fixtures ──────────────────────────────────────────────────────

    /// The version number [`arm_mark_failure`]'s trigger aborts on — the last statement of an
    /// analysis item, so everything else in that item has already been written when it fires.
    const FAIL_VERSION: i64 = 999;

    fn arm_mark_failure(store: &Store) {
        store
            .write()
            .execute_batch(&format!(
                "CREATE TEMP TRIGGER IF NOT EXISTS fail_mark AFTER UPDATE OF analysis_version
                 ON asset WHEN new.analysis_version = {FAIL_VERSION}
                 BEGIN SELECT RAISE(ABORT, 'injected mark failure'); END;"
            ))
            .unwrap();
    }

    fn suggestion(name: &str) -> TagSuggestion {
        TagSuggestion {
            name: name.to_string(),
            confidence: 0.6,
            extractor: "test@1".into(),
            explanation: "because".into(),
        }
    }

    fn asset_ids(store: &Store) -> Vec<AssetId> {
        let conn = store.read().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM asset ORDER BY path").unwrap();
        let ids = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        ids.iter()
            .map(|b| crate::helpers::blob_to_asset_id(b))
            .collect()
    }

    /// SQLite's own statement trace, counting executions of the tag reindex. `trace_v2` takes a
    /// bare function pointer, so the counter has to be a static; only one test installs it.
    static REINDEXES: AtomicUsize = AtomicUsize::new(0);

    fn count_reindexes(event: TraceEvent<'_>) {
        if let TraceEvent::Stmt(_, sql) = event {
            if sql.contains("asset_fts SET tags") {
                REINDEXES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
