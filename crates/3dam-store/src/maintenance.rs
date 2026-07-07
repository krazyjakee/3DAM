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
        let mut conn = self.conn.lock().unwrap();
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

        let tx = conn.transaction().map_err(internal)?;
        // Child → parent order (explicit even though the FKs cascade — clearer, and order-safe
        // inside one transaction). Every table in `library.db` is catalog content.
        for table in [
            "embedding",
            "asset_tag",
            "collection_member",
            "audio_attr",
            "image_attr",
            "model_attr",
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
        let mut conn = self.conn.lock().unwrap();
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

        let tx = conn.transaction().map_err(internal)?;
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

    // ── vacuum (Settings §Storage → Compact database) ────────────────────────

    /// Compact `library.db` in place (`VACUUM`), reclaiming pages freed by deletes. Sizing the file
    /// is the caller's job (the engine holds `data_dir`); this just runs the reclaim.
    pub fn vacuum(&self) -> Result<(), LibError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("VACUUM", []).map_err(internal)?;
        Ok(())
    }
}
