//! `dam-store` — the SQLite metadata store (tech-spec 02). Private to `3dam-core`; no front-end
//! links it (dependency rule 4, tech-spec 01 §2). Methods are synchronous and internally locked;
//! `3dam-core` calls them from a blocking context off the async runtime (tech-spec 14).

#[cfg(feature = "ann")]
mod ann;
mod schema;

use dam_api::dto::*;
use dam_api::id::{AssetId, CollectionId, ContentHash, JobId, SourceId};
use dam_api::page::{Cursor, Page};
use dam_api::service::Visibility;
use dam_api::LibError;
use dam_sources::SourceConnection;
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

const QUERY_MAX_LIMIT: u32 = 500;

/// How long a writer waits for another process's write lock before giving up (see [`Store::migrate`]).
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// Time and the opaque-internal error wrapper are shared workspace-wide (dam-api). Re-export
// `now_ms` because `dam_store::now_ms` is part of this crate's surface (used by 3dam-core).
use dam_api::internal;
pub use dam_api::now_ms;

/// A source's `path -> (size_bytes, source_modified_at)` change-token index for delta re-scan.
pub type PathIndex = std::collections::HashMap<String, (Option<i64>, Option<i64>)>;

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

/// One asset the analysis pass must (re-)process — enough to locate the file and decode it, plus the
/// content hash that keys the extractor cache (tech-spec 05 §7.1). Produced by [`Store::list_analysis_targets`].
pub struct AnalysisTarget {
    pub id: AssetId,
    /// Absolute source root the relative `path` joins onto.
    pub source_uri: String,
    pub path: String,
    pub media: MediaType,
    pub format: String,
    pub content_hash: Option<ContentHash>,
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
    conn: Mutex<Connection>,
    /// Query-expansion vocabulary for text search (semantic-search M3). Built-in defaults plus any
    /// user `synonyms.txt`; loaded once at open so a query never touches the filesystem.
    synonyms: search::SynonymMap,
    /// Bumped on every embedding write so the ANN cache (M6) knows its indexes are stale. Always
    /// present (a cheap atomic); only *read* by the `ann` feature.
    embed_gen: std::sync::atomic::AtomicU64,
    /// Per-space HNSW index cache (semantic-search M6): `space_id → (generation, index)`. Rebuilt
    /// lazily when `embed_gen` has moved on. Only compiled under the `ann` feature.
    #[cfg(feature = "ann")]
    ann_cache: Mutex<std::collections::HashMap<String, (u64, std::sync::Arc<ann::AnnIndex>)>>,
}

impl Store {
    /// Open (creating if needed) `library.db` in `data_dir`, applying pending migrations.
    pub fn open(data_dir: &Path) -> Result<Store, LibError> {
        std::fs::create_dir_all(data_dir).map_err(internal)?;
        let db_path = data_dir.join("library.db");
        let conn = Connection::open(&db_path).map_err(internal)?;
        Self::from_conn(conn, search::SynonymMap::load(data_dir))
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Store, LibError> {
        let conn = Connection::open_in_memory().map_err(internal)?;
        Self::from_conn(conn, search::SynonymMap::builtin())
    }

    fn from_conn(conn: Connection, synonyms: search::SynonymMap) -> Result<Store, LibError> {
        // One data dir can legitimately be open in two processes (the desktop shell and a CLI run).
        // WAL lets their readers overlap, but writers still serialise, and rusqlite's default is to
        // fail instantly with `SQLITE_BUSY` rather than wait. Waiting is what we want everywhere:
        // the contended windows here are short (a migration step, a scan batch), and a spurious
        // "database is locked" would surface as a failed job.
        conn.busy_timeout(BUSY_TIMEOUT).map_err(internal)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(internal)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(internal)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(internal)?;
        let store = Store {
            conn: Mutex::new(conn),
            synonyms,
            embed_gen: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "ann")]
            ann_cache: Mutex::new(std::collections::HashMap::new()),
        };
        store.migrate()?;
        Ok(store)
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
        let mut conn = self.conn.lock().unwrap();
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
mod helpers;
mod jobs;
mod maintenance;
mod query;
pub mod search;
mod sources;
