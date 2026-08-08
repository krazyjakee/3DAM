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

/// Internal benchmark seam for issue #141's 100k/1M HNSW evidence. Kept out of normal builds and
/// out of the Store contract; `cargo bench -p dam-store --features ann-bench --bench ann_scale`
/// owns it.
#[cfg(all(feature = "ann", feature = "ann-bench"))]
#[doc(hidden)]
pub struct AnnBenchmarkResult {
    pub build: std::time::Duration,
    pub serialize: std::time::Duration,
    pub deserialize: std::time::Duration,
    pub search_p50: std::time::Duration,
    pub exact_rerank_p50: std::time::Duration,
    pub recall_at_k: f64,
    pub candidate_count: usize,
    pub sidecar_bytes: usize,
    pub rss_before_kib: usize,
    pub rss_index_kib: usize,
    pub graph_rss_delta_kib: usize,
    pub rss_lifecycle_peak_kib: usize,
}

#[cfg(all(feature = "ann", feature = "ann-bench"))]
#[doc(hidden)]
pub fn benchmark_ann_build(
    items: Vec<(AssetId, Vec<f32>)>,
    queries: &[(Vec<f32>, Vec<AssetId>)],
    k: usize,
) -> AnnBenchmarkResult {
    fn rss_kib() -> usize {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|status| {
                    status.lines().find_map(|line| {
                        line.strip_prefix("VmRSS:")?
                            .split_whitespace()
                            .next()?
                            .parse()
                            .ok()
                    })
                })
                .unwrap_or(0)
        }
        #[cfg(not(target_os = "linux"))]
        0
    }

    let rss_before_kib = rss_kib();
    let started = std::time::Instant::now();
    let index = ann::AnnIndex::build_for_benchmark(&items)
        .expect("benchmark ANN build failed")
        .expect("benchmark requires non-empty input");
    let build = started.elapsed();
    let rss_index_kib = rss_kib();

    // Persistence is part of lifecycle cost: bincode materialises the checksummed sidecar payload,
    // and atomic publication briefly keeps the live base and payload at once.
    let started = std::time::Instant::now();
    let encoded = bincode::serialize(&index).expect("serialize benchmark index");
    let serialize = started.elapsed();
    let rss_serialized_kib = rss_kib();
    let started = std::time::Instant::now();
    let loaded: ann::AnnIndex = bincode::deserialize(&encoded).expect("load benchmark sidecar");
    let deserialize = started.elapsed();
    let rss_loaded_kib = rss_kib();

    let mut latencies = Vec::with_capacity(queries.len());
    let mut rerank_latencies = Vec::with_capacity(queries.len());
    let mut recalled = 0usize;
    let mut expected = 0usize;
    let mut candidate_count = 0usize;
    let canonical: std::collections::HashMap<AssetId, &[f32]> = items
        .iter()
        .map(|(id, vector)| (*id, vector.as_slice()))
        .collect();
    for (query, truth) in queries {
        let started = std::time::Instant::now();
        let candidates = loaded.candidates(query, k).expect("ANN benchmark lookup");
        latencies.push(started.elapsed());
        let started = std::time::Instant::now();
        let mut exact: Vec<_> = candidates
            .iter()
            .filter_map(|id| {
                canonical
                    .get(id)
                    .map(|vector| (*id, similarity::cosine(query, vector)))
            })
            .collect();
        exact.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        exact.truncate(truth.len());
        std::hint::black_box(&exact);
        rerank_latencies.push(started.elapsed());
        candidate_count += candidates.len();
        recalled += candidates.iter().filter(|id| truth.contains(id)).count();
        expected += truth.len().min(k);
    }
    latencies.sort_unstable();
    let search_p50 = latencies
        .get(latencies.len() / 2)
        .copied()
        .unwrap_or_default();
    rerank_latencies.sort_unstable();
    let exact_rerank_p50 = rerank_latencies
        .get(rerank_latencies.len() / 2)
        .copied()
        .unwrap_or_default();
    AnnBenchmarkResult {
        build,
        serialize,
        deserialize,
        search_p50,
        exact_rerank_p50,
        recall_at_k: recalled as f64 / expected.max(1) as f64,
        candidate_count,
        sidecar_bytes: encoded.len(),
        rss_before_kib,
        rss_index_kib,
        graph_rss_delta_kib: rss_index_kib.saturating_sub(rss_before_kib),
        rss_lifecycle_peak_kib: rss_index_kib.max(rss_serialized_kib).max(rss_loaded_kib),
    }
}

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
    /// Persistent per-space HNSW lifecycle. File stores get a background compactor; in-memory test
    /// stores deliberately use the exact path because they have no durable sidecar directory.
    #[cfg(feature = "ann")]
    ann: Option<ann::AnnManager>,
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
            #[cfg(feature = "ann")]
            ann: None,
        };
        store.migrate()?;
        #[cfg(feature = "ann")]
        let store = {
            let mut store = store;
            if let Some(path) = store.db.path() {
                let data_dir = path.parent().unwrap_or_else(|| Path::new("."));
                store.ann = Some(ann::AnnManager::start(path, data_dir)?);
            }
            store
        };
        Ok(store)
    }

    /// Borrow a read-only connection for the duration of one query (see [`db::Db::read`]).
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

    #[inline]
    pub(crate) fn wake_ann(&self, changed: bool) {
        #[cfg(feature = "ann")]
        if changed {
            if let Some(ann) = &self.ann {
                ann.kick();
            }
        }
        #[cfg(not(feature = "ann"))]
        let _ = changed;
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
mod analysis_plan;
mod analysis_write;
mod assets;
mod batch;
mod blocklist;
mod collections;
mod duplicates;
mod export;
mod helpers;
mod jobs;
mod maintenance;
mod query;
pub mod search;
mod similarity;
mod sources;
mod tags;

pub use batch::{
    AnalysisBatchContext, AnalysisBatchOutcome, AnalysisItemOutcome, AnalysisWrite,
    AudioFeatureWrite, EmbeddingWrite, ScanBatchContext, ScanBatchOutcome, ScanItemOutcome,
    ScanWrite, TagSuggestion,
};
pub use export::{ExportAssetRow, ExportSelection, ExportStreamStats};
