//! `dam-store` — the SQLite metadata store (tech-spec 02). Private to `3dam-core`; no front-end
//! links it (dependency rule 4, tech-spec 01 §2). Methods are synchronous and internally locked;
//! `3dam-core` calls them from a blocking context off the async runtime (tech-spec 14).
//!
//! Connection ownership — one writer, a pooled set of readers, and the maintenance gate that
//! drains both — lives in [`db`]; every method here reaches SQLite through [`Store::read`],
//! [`Store::write`], or [`Store::exclusive`] rather than a bare connection.

#[cfg(feature = "ann")]
mod ann;
mod db;
mod schema;

use dam_api::dto::*;
use dam_api::id::{AssetId, CollectionId, CommentId, ContentHash, JobId, SourceId};
use dam_api::page::{Cursor, Page};
use dam_api::service::Visibility;
use dam_api::LibError;
use dam_sources::SourceConnection;
use db::Db;
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use uuid::Uuid;

const QUERY_MAX_LIMIT: u32 = 500;

/// How long a writer waits for another process's write lock before giving up (see [`Store::migrate`]).
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// Time and the opaque-internal error wrapper are shared workspace-wide (dam-api). Re-export
// `now_ms` because `dam_store::now_ms` is part of this crate's surface (used by 3dam-core).
use dam_api::internal;
pub use dam_api::now_ms;

/// A row to insert/reconcile during a scan.
pub struct NewAsset {
    pub source_id: SourceId,
    pub path: String,
    pub filename: String,
    pub content_hash: Option<ContentHash>,
    pub size_bytes: Option<i64>,
    pub source_modified_at: Option<i64>,
    pub scanned_at: i64,
    pub media_type: MediaType,
    pub format: String,
}

/// Cheap source metadata used to decide whether a streamed delta-scan entry needs reopening.
pub type SourceChangeToken = (Option<i64>, Option<i64>);

/// Committed tag-edit details used by the engine to emit one post-commit event per changed asset.
pub struct ManualTagEditOutcome {
    pub result: TagEditResult,
    pub changed_assets: Vec<(AssetId, Option<SourceId>)>,
}

/// Catalog rows removed by one transactional duplicate-review decision, with source attribution
/// captured before the delete so the engine can emit visibility-safe live events afterwards.
pub struct DuplicateReviewOutcome {
    pub removed_assets: Vec<(AssetId, SourceId)>,
}

/// One asset the analysis pass must (re-)process — enough to locate the file and decode it, plus the
/// content hash that keys the extractor cache (tech-spec 05 §7.1). Produced by [`Store::list_analysis_targets`].
pub struct AnalysisTarget {
    pub id: AssetId,
    /// The source this asset belongs to — attribution for the `AssetChanged` event the pass emits per
    /// analysed asset, and for the analyse job's own `sources` set (issue #42).
    pub source_id: SourceId,
    /// How to rebuild this asset's source backend. Protected connections carry only an opaque ref
    /// until dam-core hydrates them from the host secret store; the pass fetches bytes through
    /// `FileSource::fetch` rather than joining a root onto `path`, so an SFTP/SMB asset
    /// materialises to a temp file exactly as it does during a scan (issue #48). Held per target
    /// (not per source) because targets arrive as one flat list; the runner de-duplicates by
    /// `source_id` so a backend is opened once per source, not once per asset.
    pub connection: SourceConnection,
    /// Path relative to the source root, normalised to `/`.
    pub path: String,
    pub media: MediaType,
    pub format: String,
    pub content_hash: Option<ContentHash>,
}

/// Stable keyset checkpoint for a bounded analysis/derivative planner batch. Ordering by source and
/// asset id keeps a source's rows adjacent and makes the cursor independent of mutable filenames or
/// timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalysisPlanCursor {
    pub due_version: i64,
    pub source_id: SourceId,
    pub asset_id: AssetId,
}

/// A distinct source referenced by a planner batch. The consumer resolves its connection only on
/// the first sighting and shares one opened backend across every later page.
pub struct AnalysisPlanSource {
    pub source_id: SourceId,
}

/// One bounded-plan row. Source connection state lives in [`AnalysisPlanBatch::sources`] rather
/// than being cloned and reparsed into every asset.
#[derive(Clone, Debug)]
pub struct AnalysisPlanTarget {
    pub id: AssetId,
    pub source_id: SourceId,
    pub path: String,
    pub media: MediaType,
    pub format: String,
    pub content_hash: Option<ContentHash>,
}

/// A bounded, resumable planner page plus the distinct source state needed by its targets.
pub struct AnalysisPlanBatch {
    pub targets: Vec<AnalysisPlanTarget>,
    pub sources: Vec<AnalysisPlanSource>,
    pub next: Option<AnalysisPlanCursor>,
}

/// Cheap job metadata computed without materializing the eligible asset rows.
pub struct AnalysisPlanSummary {
    pub total: u64,
    pub sources: Vec<SourceId>,
    pub end: Option<AnalysisPlanCursor>,
}

/// Derived image signals the analysis pass persists (tech-spec 05 §5, §6). Passed as primitives so the
/// store never depends on `dam-media`.
pub struct ImageAnalysis {
    pub phash: u64,
    pub tileability: f32,
    pub repeat_period: Option<i64>,
    pub tile_class: String,
    pub dominant_colors: Vec<String>,
    pub class: String,
}

pub struct Store {
    /// Connection ownership: one writer, a lazy read pool, and the maintenance gate (see [`db`]).
    db: Db,
    /// Query-expansion vocabulary for text search (semantic-search M3). Built-in defaults plus any
    /// user `synonyms.txt`; loaded once at open so a query never touches the filesystem.
    synonyms: search::SynonymMap,
    /// Bumped on every embedding write so the ANN cache (M6) knows its indexes are stale. Always
    /// present (a cheap atomic); only *read* by the `ann` feature.
    embed_gen: std::sync::atomic::AtomicU64,
    /// Per-space HNSW index cache (semantic-search M6): `space_id → (generation, index)`. Rebuilt
    /// lazily when `embed_gen` has moved on. Only compiled under the `ann` feature.
    #[cfg(feature = "ann")]
    ann_cache:
        std::sync::Mutex<std::collections::HashMap<String, (u64, std::sync::Arc<ann::AnnIndex>)>>,
}

impl Store {
    /// Open (creating if needed) `library.db` in `data_dir`, applying pending migrations.
    pub fn open(data_dir: &Path) -> Result<Store, LibError> {
        std::fs::create_dir_all(data_dir).map_err(internal)?;
        let db_path = data_dir.join("library.db");
        Self::open_at(&db_path, search::SynonymMap::load(data_dir))
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Store, LibError> {
        let conn = Connection::open_in_memory().map_err(internal)?;
        Self::from_conn(conn, search::SynonymMap::builtin())
    }

    /// The file-backed constructor: a writer connection plus a (lazily filled) read pool.
    fn open_at(path: &Path, synonyms: search::SynonymMap) -> Result<Store, LibError> {
        Self::build(Db::open_file(path)?, synonyms)
    }

    /// Adopt an already-open connection as a single-connection catalog. Only `:memory:` databases
    /// arrive this way: a second `:memory:` handle would be a *different* database, so this store
    /// cannot be pooled and its reads share the writer's connection (see [`db`]).
    fn from_conn(conn: Connection, synonyms: search::SynonymMap) -> Result<Store, LibError> {
        Self::build(Db::in_memory(conn)?, synonyms)
    }

    fn build(db: Db, synonyms: search::SynonymMap) -> Result<Store, LibError> {
        let store = Store {
            db,
            synonyms,
            embed_gen: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "ann")]
            ann_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Borrow a read-only connection for the duration of one query (see [`db::Db::read`]).
    #[allow(dead_code)] // Call sites move over in step 2 of issue #137.
    pub(crate) fn read(&self) -> Result<db::ReadGuard<'_>, LibError> {
        self.db.read()
    }

    /// Borrow the writer connection. Readers keep running alongside it.
    pub(crate) fn write(&self) -> db::WriteGuard<'_> {
        self.db.write()
    }

    /// Borrow the whole catalog with readers drained — `VACUUM`, `wal_checkpoint(TRUNCATE)`,
    /// migrations.
    pub(crate) fn exclusive(&self) -> db::ExclusiveGuard<'_> {
        self.db.exclusive()
    }

    /// Apply pending schema steps, forward-only (`PRAGMA user_version`).
    ///
    /// Two processes may open the same data dir at the same moment (the desktop shell boots its
    /// in-process server while a CLI run starts), and both would otherwise read the same old
    /// version outside any transaction and replay the same steps — the loser failing on "table
    /// already exists" with a half-applied schema behind it. So each step takes an **immediate**
    /// transaction (rusqlite's default `BEGIN` is deferred, and only upgrades to a write lock at
    /// the first write — far too late) and re-reads `user_version` *inside* it. Whoever gets the
    /// write lock applies the step; the other waits out `busy_timeout`, sees the new version, and
    /// skips. Idempotent either way.
    fn migrate(&self) -> Result<(), LibError> {
        // Exclusive: a schema step rewrites tables under everyone's feet, so no reader may hold a
        // snapshot across it (and on first open there are no pooled readers to drain anyway).
        let mut conn = self.exclusive();
        let current: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(internal)?;
        let target = schema::MIGRATIONS.len() as i64;
        if current > target {
            return Err(LibError::Internal(format!(
                "library is schema v{current} but this build only understands v{target}; upgrade 3dam"
            )));
        }
        for (i, step) in schema::MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as i64;
            if version <= current {
                continue;
            }
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let applied: i64 = tx
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .map_err(internal)?;
            if applied >= version {
                tracing::debug!(version, "migration already applied by another process");
                continue;
            }
            tx.execute_batch(step).map_err(internal)?;
            tx.pragma_update(None, "user_version", version)
                .map_err(internal)?;
            tx.commit().map_err(internal)?;
            tracing::info!(version, "applied migration");
        }
        Ok(())
    }
}

// The `impl Store` surface is split across these modules by concern; each adds methods to the same
// `Store` type. Shared free helpers (SQL filter building, blob↔id conversions) live in `helpers`.
mod analysis;
mod assets;
mod blocklist;
mod collections;
mod export;
mod helpers;
mod jobs;
mod maintenance;
mod query;
pub mod search;
mod sources;

pub use export::{ExportAssetRow, ExportSelection, ExportStreamStats};
