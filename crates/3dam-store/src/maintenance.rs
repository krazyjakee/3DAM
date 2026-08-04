//! Storage maintenance — catalog wipe, analysis reset, and vacuum — part of the `Store` impl, split
//! out of `lib.rs`. These are the operator-plane "reset my library" primitives behind the admin
//! `/admin/api/maintenance/*` surface (tech-spec 10 §5).
//!
//! Non-destructive by construction: every method here mutates rows *in place* — the open WAL
//! `Connection` is shared (behind `Arc<Store>`) with the engine, watchers, and in-flight
//! `spawn_blocking` calls, so deleting `library.db` out from under it is never an option. Files
//! inside registered sources are untouched; only catalog rows are ever removed.
use super::*;
use dam_api::admin::{ClearAnalysisReport, WipeReport};

impl Store {
    // ── catalog wipe (Settings §Storage → Reset catalog) ─────────────────────

    /// Reset the catalog to empty — assets, sources, collections, tags, embeddings, suggestions,
    /// jobs, and the blocklist — without touching any files in registered sources. Schema and
    /// `PRAGMA user_version` are preserved (rows deleted, tables kept), so the DB stays openable by
    /// this build. Deletes in one transaction, then `VACUUM`s (outside the tx) to shrink the file.
    pub fn wipe_catalog(&self) -> Result<WipeReport, LibError> {
        // Exclusive: this ends in a `VACUUM`, which rewrites the database file and cannot run while
        // any pooled reader holds a snapshot.
        let mut conn = self.exclusive();
        // Snapshot the headline counts before the wipe, for the report + audit detail.
        let count = |c: &Connection, t: &str| -> Result<u64, LibError> {
            c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n as u64)
            .map_err(internal)
        };
        let report = WipeReport {
            assets_removed: count(&conn, "asset")?,
            sources_removed: count(&conn, "source")?,
            collections_removed: count(&conn, "collection")?,
            tags_removed: count(&conn, "tag")?,
        };

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // Preserve opaque refs in the retry queue before `source` is emptied. Credential deletion
        // happens in the host backend after this transaction; failures remain recoverable on the
        // next open/reset instead of silently orphaning secrets.
        tx.execute(
            "INSERT OR IGNORE INTO host_secret_cleanup(auth_ref)
             SELECT auth_ref FROM source WHERE auth_ref IS NOT NULL",
            [],
        )
        .map_err(internal)?;
        // Child → parent order (explicit even though the FKs cascade — clearer, and order-safe
        // inside one transaction). `host_migration` and `host_secret_cleanup` are operational
        // metadata: the former survives resets; the latter is drained after this commit.
        for table in [
            "embedding",
            "asset_tag",
            "collection_member",
            "asset_note",
            "audio_attr",
            "image_attr",
            "model_attr",
            "video_attr",
            "document_attr",
            "asset",
            "tag",
            "collection",
            "source",
            "job",
            "blocklist",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), [])
                .map_err(internal)?;
        }
        tx.commit().map_err(internal)?;
        // Reclaim the freed pages so the on-disk `library.db` actually shrinks. `VACUUM` cannot run
        // inside a transaction, so it follows the commit.
        conn.execute("VACUUM", []).map_err(internal)?;
        Ok(report)
    }

    // ── analysis reset (Settings §Storage → Clear analysis suggestions) ──────

    /// Drop the analysis layer without wiping the catalog: remove every auto-suggested/rejected tag
    /// (keeping user-confirmed tags), delete all embeddings, null the derived per-media attributes
    /// the analysis pass writes, and mark every asset due for re-analysis (`analysis_version = 0`).
    /// The next `analyze` run re-derives everything.
    pub fn clear_analysis(&self) -> Result<ClearAnalysisReport, LibError> {
        let mut conn = self.write();
        let count = |c: &Connection, sql: &str| -> Result<u64, LibError> {
            c.query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(|n| n as u64)
                .map_err(internal)
        };
        let suggestions_removed = count(
            &conn,
            "SELECT COUNT(*) FROM asset_tag WHERE state IN ('suggested','rejected')",
        )?;
        let embeddings_removed = count(&conn, "SELECT COUNT(*) FROM embedding")?;

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        // Suggestions + a remembered "rejected" reset so re-analysis starts clean; user-confirmed
        // tags survive (they are `state = 'confirmed'`).
        tx.execute(
            "DELETE FROM asset_tag WHERE state IN ('suggested','rejected')",
            [],
        )
        .map_err(internal)?;
        tx.execute("DELETE FROM embedding", []).map_err(internal)?;
        // Null exactly the columns the analysis pass writes (set_image_analysis / set_media_class);
        // the cheap-tier scan metadata (dimensions, codec, geometry counts) is left intact.
        tx.execute(
            "UPDATE image_attr SET phash = NULL, tileability = NULL, repeat_period = NULL,
                                   tile_class = NULL, dominant_colors = NULL, class = NULL",
            [],
        )
        .map_err(internal)?;
        tx.execute("UPDATE audio_attr SET class = NULL", [])
            .map_err(internal)?;
        tx.execute("UPDATE model_attr SET class = NULL", [])
            .map_err(internal)?;
        tx.execute("UPDATE video_attr SET class = NULL", [])
            .map_err(internal)?;
        tx.execute("UPDATE document_attr SET class = NULL", [])
            .map_err(internal)?;
        // Document body text lives *inside* the FTS index (schema V10), not in a base-table column,
        // so it has to be cleared here too — otherwise "clear analysis" leaves every document still
        // findable by prose it can no longer explain. The name/token/tag columns are scan-time data
        // and stay.
        tx.execute("UPDATE asset_fts SET text = ''", [])
            .map_err(internal)?;
        // Mark everything due for re-analysis (the Plan gate reads `analysis_version`, §7.2).
        tx.execute(
            "UPDATE asset SET analysis_version = 0, analysed_at = NULL",
            [],
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)?;

        Ok(ClearAnalysisReport {
            suggestions_removed,
            embeddings_removed,
        })
    }

    /// Rebuild every maintained library/facet count from its authoritative catalog rows.
    ///
    /// Normal writes cannot drift because the V26 triggers update both sides transactionally. This
    /// explicit repair seam exists for integrity tooling and recovery from an externally modified
    /// database. The delete + backfill is one immediate transaction, so readers observe either the
    /// old complete snapshot or the repaired complete snapshot, never an empty intermediate state.
    pub fn repair_aggregates(&self) -> Result<(), LibError> {
        let mut conn = self.write();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(internal)?;
        tx.execute("DELETE FROM source_tag_stat", [])
            .map_err(internal)?;
        tx.execute("DELETE FROM tag_stat", []).map_err(internal)?;
        tx.execute("DELETE FROM source_media_stat", [])
            .map_err(internal)?;
        tx.execute("DELETE FROM source_stat", [])
            .map_err(internal)?;
        tx.execute("DELETE FROM media_stat", []).map_err(internal)?;
        tx.execute("DELETE FROM library_stat", [])
            .map_err(internal)?;
        tx.execute_batch(
            "INSERT INTO library_stat(singleton, asset_count, unanalyzed_count, source_count)
             SELECT 1, (SELECT COUNT(*) FROM asset),
                       (SELECT COUNT(*) FROM asset WHERE analysed_at IS NULL),
                       (SELECT COUNT(*) FROM source);
             INSERT INTO media_stat(media_type, asset_count)
             SELECT media_type, COUNT(*) FROM asset GROUP BY media_type;
             INSERT INTO source_stat(source_id, asset_count, unanalyzed_count)
             SELECT s.id, COUNT(a.id), COUNT(a.id) FILTER (WHERE a.analysed_at IS NULL)
               FROM source s LEFT JOIN asset a ON a.source_id = s.id GROUP BY s.id;
             INSERT INTO source_media_stat(source_id, media_type, asset_count)
             SELECT source_id, media_type, COUNT(*) FROM asset GROUP BY source_id, media_type;
             INSERT INTO tag_stat(tag_id, asset_count, manual_count)
             SELECT t.id, COUNT(at.asset_id) FILTER (WHERE at.state = 'confirmed'),
                    COUNT(at.asset_id) FILTER (
                        WHERE at.state = 'confirmed' AND at.source = 'user')
               FROM tag t LEFT JOIN asset_tag at ON at.tag_id = t.id GROUP BY t.id;
             INSERT INTO source_tag_stat(source_id, tag_id, asset_count, manual_count)
             SELECT a.source_id, at.tag_id, COUNT(*),
                    COUNT(*) FILTER (WHERE at.source = 'user')
               FROM asset_tag at JOIN asset a ON a.id = at.asset_id
              WHERE at.state = 'confirmed' GROUP BY a.source_id, at.tag_id;",
        )
        .map_err(internal)?;
        tx.commit().map_err(internal)
    }

    // ── vacuum (Settings §Storage → Compact database) ────────────────────────

    /// Compact `library.db` in place (`VACUUM`), reclaiming pages freed by deletes. Sizing the file
    /// is the caller's job (the engine holds `data_dir`); this just runs the reclaim.
    pub fn vacuum(&self) -> Result<(), LibError> {
        // Exclusive: `VACUUM` rebuilds the file, so in-flight readers are drained first.
        let conn = self.exclusive();
        conn.execute("VACUUM", []).map_err(internal)?;
        Ok(())
    }
}
