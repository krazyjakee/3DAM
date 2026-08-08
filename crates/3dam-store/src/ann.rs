//! Persistent `usearch` HNSW indexes (ADR 0016, issue #141).
//!
//! SQLite's `embedding` rows are canonical. Each space has an immutable sidecar base at an indexed
//! generation plus the transactionally maintained `ann_delta` overlay from schema V27. HNSW itself
//! has no mutation API, so inserts/updates/deletes are applied incrementally through that overlay;
//! a background compactor rebuilds only the affected space once the overlay is large. Queries never
//! build an index and always exact-rerank the bounded candidate set against current SQLite vectors.

use crate::db::{configure, Role};
use crate::similarity::bytes_to_f32;
use dam_api::id::AssetId;
use dam_api::{internal, LibError};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub(crate) const FORMAT_VERSION: u32 = 2;
pub(crate) const COMPACT_DELTA_COUNT: i64 = 256;
pub(crate) const OVERLAY_CANDIDATE_MAX: usize = 512;
const BACKEND: &str = "usearch-2.25.3-f16-ef2048";
const CONNECTIVITY: usize = 16;
const EXPANSION_ADD: usize = 256;
const EXPANSION_SEARCH: usize = 2_048;
const MAGIC: &[u8; 8] = b"3DAMANN\0";
static TEMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
type SnapshotRow = (i64, Option<Vec<u8>>, Option<Vec<u8>>);

/// One immutable HNSW base for one embedding space.
pub(crate) struct AnnIndex {
    index: usearch::Index,
    /// USearch keys are `u64`; the canonical IDs are UUIDs, so keys are stable positions in this
    /// immutable generation and this table is persisted beside the native graph.
    keys: Vec<AssetId>,
}

impl AnnIndex {
    pub(crate) fn build(items: Vec<(AssetId, Vec<f32>)>) -> Result<Option<Self>, LibError> {
        Self::build_slice(&items)
    }

    #[cfg(feature = "ann-bench")]
    pub(crate) fn build_for_benchmark(
        items: &[(AssetId, Vec<f32>)],
    ) -> Result<Option<Self>, LibError> {
        Self::build_slice(items)
    }

    fn build_slice(items: &[(AssetId, Vec<f32>)]) -> Result<Option<Self>, LibError> {
        if items.is_empty() {
            return Ok(None);
        }
        let dimensions = items[0].1.len();
        if dimensions == 0 || items.iter().any(|(_, vector)| vector.len() != dimensions) {
            return Err(LibError::Internal(
                "cannot build ANN index with empty or mixed dimensions".into(),
            ));
        }
        let options = usearch::IndexOptions {
            dimensions,
            metric: usearch::MetricKind::Cos,
            // The graph generates a bounded, over-fetched candidate set; exact f32 vectors from
            // SQLite determine final ranking. f16 halves the resident vector tape while the scale
            // harness guards product candidate recall at the same 8x over-fetch used by queries.
            quantization: usearch::ScalarKind::F16,
            connectivity: CONNECTIVITY,
            expansion_add: EXPANSION_ADD,
            expansion_search: EXPANSION_SEARCH,
            multi: false,
        };
        let index = usearch::Index::new(&options)
            .map_err(|error| LibError::Internal(format!("failed to create ANN index: {error}")))?;
        index.change_expansion_search(EXPANSION_SEARCH);
        let threads = rayon::current_num_threads().max(1);
        index
            .reserve_capacity_and_threads(items.len(), threads)
            .map_err(|error| LibError::Internal(format!("failed to reserve ANN index: {error}")))?;
        let inserted: Result<(), String> =
            items
                .par_iter()
                .enumerate()
                .try_for_each(|(position, (_, vector))| {
                    index
                        .add(position as u64, vector)
                        .map_err(|error| error.to_string())
                });
        inserted.map_err(|error| {
            LibError::Internal(format!("failed to populate ANN index: {error}"))
        })?;
        Ok(Some(Self {
            index,
            keys: items.iter().map(|(id, _)| *id).collect(),
        }))
    }

    /// Approximate ids only. Callers must fetch canonical vectors and exact-rerank them.
    pub(crate) fn candidates(&self, query: &[f32], k: usize) -> Result<Vec<AssetId>, LibError> {
        if query.len() != self.index.dimensions() {
            return Err(LibError::Internal(format!(
                "ANN query dimension {} does not match index dimension {}",
                query.len(),
                self.index.dimensions()
            )));
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let matches = self
            .index
            .search(query, k.min(self.keys.len()))
            .map_err(|error| LibError::Internal(format!("ANN lookup failed: {error}")))?;
        matches
            .keys
            .into_iter()
            .map(|key| {
                self.keys.get(key as usize).copied().ok_or_else(|| {
                    LibError::Internal(format!("ANN graph returned unknown key {key}"))
                })
            })
            .collect()
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn dimensions(&self) -> usize {
        self.index.dimensions()
    }
}

/// The native graph has its own stable byte format. Implementing serde at this seam lets the
/// existing checksummed/versioned sidecar envelope and lifecycle benchmark remain backend-neutral.
#[derive(Serialize, Deserialize)]
struct AnnIndexWire {
    keys: Vec<AssetId>,
    graph: Vec<u8>,
}

impl Serialize for AnnIndex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut graph = vec![0; self.index.serialized_length()];
        self.index
            .save_to_buffer(&mut graph)
            .map_err(serde::ser::Error::custom)?;
        AnnIndexWire {
            keys: self.keys.clone(),
            graph,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AnnIndex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = AnnIndexWire::deserialize(deserializer)?;
        let index = usearch::Index::restore_from_buffer(&wire.graph).map_err(D::Error::custom)?;
        if index.size() != wire.keys.len() {
            return Err(D::Error::custom("ANN key table does not match graph size"));
        }
        index.change_expansion_search(EXPANSION_SEARCH);
        Ok(Self {
            index,
            keys: wire.keys,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    format_version: u32,
    backend: String,
    space_id: String,
    generation: i64,
    dimension: usize,
    item_count: usize,
    index: Option<AnnIndex>,
}

#[derive(Clone)]
pub(crate) struct CachedIndex {
    pub(crate) generation: i64,
    pub(crate) index: Option<Arc<AnnIndex>>,
}

struct Inner {
    db_path: PathBuf,
    dir: PathBuf,
    cache: Mutex<HashMap<String, CachedIndex>>,
    wake: mpsc::SyncSender<()>,
}

/// Owns the lifecycle worker and the currently published bases. The worker has only a `Weak`
/// reference, so dropping the Store closes the wake channel and ends it without a join cycle.
pub(crate) struct AnnManager {
    inner: Arc<Inner>,
}

impl AnnManager {
    pub(crate) fn start(db_path: &Path, data_dir: &Path) -> Result<Self, LibError> {
        let dir = data_dir.join("vectors");
        fs::create_dir_all(&dir).map_err(internal)?;
        cleanup_stale_sidecar_work(&dir);
        // A persisted `ready` marker is not trusted across a process boundary until this build has
        // checked the sidecar checksum, format, backend, space id, and generation. This small state
        // transition also makes corruption recovery externally observable without doing any index
        // I/O or CPU work under the interactive Store's writer guard.
        {
            let conn = Connection::open(db_path).map_err(internal)?;
            configure(&conn, Role::Writer)?;
            conn.execute(
                "UPDATE ann_space_state SET lifecycle='pending'
                  WHERE indexed_generation>0",
                [],
            )
            .map_err(internal)?;
        }
        let (wake, receive) = mpsc::sync_channel(1);
        let inner = Arc::new(Inner {
            db_path: db_path.to_path_buf(),
            dir,
            cache: Mutex::new(HashMap::new()),
            wake,
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("3dam-ann-index".into())
            .spawn(move || {
                while let Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) =
                    receive.recv_timeout(Duration::from_millis(500))
                {
                    let Some(inner) = weak.upgrade() else { break };
                    if let Err(error) = reconcile(&inner) {
                        tracing::warn!(%error, "ANN lifecycle reconciliation failed; exact fallback remains available");
                    }
                }
            })
            .map_err(internal)?;
        let manager = Self { inner };
        manager.kick();
        Ok(manager)
    }

    pub(crate) fn kick(&self) {
        let _ = self.inner.wake.try_send(());
    }

    pub(crate) fn cached(&self, space_id: &str) -> Option<CachedIndex> {
        self.inner.cache.lock().unwrap().get(space_id).cloned()
    }

    /// Drop a process-local base that failed a native lookup. The next request uses exact fallback
    /// while the lifecycle worker reloads and revalidates the persisted generation.
    pub(crate) fn evict(&self, space_id: &str) {
        self.inner.cache.lock().unwrap().remove(space_id);
        self.kick();
    }

    #[cfg(test)]
    pub(crate) fn sidecar_dir(&self) -> &Path {
        &self.inner.dir
    }
}

fn reconcile(inner: &Inner) -> Result<(), LibError> {
    let mut conn = Connection::open(&inner.db_path).map_err(internal)?;
    configure(&conn, Role::Writer)?;
    let states: Vec<(String, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT s.space_id, s.indexed_generation, COUNT(d.asset_id)
                   FROM ann_space_state s LEFT JOIN ann_delta d ON d.space_id = s.space_id
                  GROUP BY s.space_id ORDER BY s.space_id",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(internal)?
            .collect::<rusqlite::Result<_>>()
            .map_err(internal)?;
        rows
    };

    for (space, indexed_generation, delta_count) in states {
        let cached_generation = inner
            .cache
            .lock()
            .unwrap()
            .get(&space)
            .map(|entry| entry.generation);
        let mut base_missing = indexed_generation <= 0;
        if indexed_generation > 0 && cached_generation != Some(indexed_generation) {
            match load_sidecar(&inner.dir, &space, indexed_generation) {
                Ok(sidecar) => {
                    inner.cache.lock().unwrap().insert(
                        space.clone(),
                        CachedIndex {
                            generation: indexed_generation,
                            index: sidecar.index.map(Arc::new),
                        },
                    );
                    conn.execute(
                        "UPDATE ann_space_state SET lifecycle='ready', last_error=NULL
                          WHERE space_id=?1 AND indexed_generation=?2",
                        params![space, indexed_generation],
                    )
                    .map_err(internal)?;
                }
                Err(error) => {
                    tracing::warn!(space_id = %space, %error, "ANN sidecar is corrupt or missing; scheduling recovery");
                    let claimed = claim_corrupt_generation(
                        &conn,
                        &space,
                        indexed_generation,
                        &error.to_string(),
                    )?;
                    if claimed == 1 {
                        inner.cache.lock().unwrap().remove(&space);
                        base_missing = true;
                    } else {
                        // Another process published a newer pointer after this reconcile snapshot.
                        // Leave it intact; the next pass loads and validates that generation.
                        base_missing = false;
                    }
                }
            }
        }

        if base_missing || delta_count >= COMPACT_DELTA_COUNT {
            rebuild_space(inner, &mut conn, &space)?;
        }
    }
    Ok(())
}

fn claim_corrupt_generation(
    conn: &Connection,
    space: &str,
    expected_generation: i64,
    error: &str,
) -> Result<usize, LibError> {
    conn.execute(
        "UPDATE ann_space_state SET lifecycle='recovering', last_error=?2,
                indexed_generation=0
          WHERE space_id=?1 AND indexed_generation=?3",
        params![space, error, expected_generation],
    )
    .map_err(internal)
}

/// Snapshot vectors quickly, release SQLite, then do the expensive HNSW build and fsync outside
/// every interactive Store guard. Publication advances to that snapshot generation in an immediate
/// transaction and preserves every newer journal entry as overlay work.
fn rebuild_space(inner: &Inner, conn: &mut Connection, space: &str) -> Result<(), LibError> {
    conn.execute(
        "UPDATE ann_space_state SET lifecycle='building', last_error=NULL WHERE space_id=?1",
        params![space],
    )
    .map_err(internal)?;

    // The generation label and its vectors must come from the same SQLite statement snapshot.
    // Reading the generation before this SELECT would force publication to require a completely
    // write-free HNSW build, which can starve forever on an actively analysed 1M-item space.
    let (generation, items): (i64, Vec<(AssetId, Vec<f32>)>) = {
        let mut stmt = conn
            .prepare(
                "SELECT state.generation, embedding.asset_id, embedding.vec
                   FROM ann_space_state state
                   LEFT JOIN embedding ON embedding.space_id = state.space_id
                  WHERE state.space_id=?1 ORDER BY embedding.asset_id",
            )
            .map_err(internal)?;
        let rows: Vec<SnapshotRow> = stmt
            .query_map(params![space], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })
            .map_err(internal)?
            .collect::<rusqlite::Result<_>>()
            .map_err(internal)?;
        let mut generation = None;
        let mut items = Vec::new();
        for (snapshot_generation, id, bytes) in rows {
            generation = Some(snapshot_generation);
            if let (Some(id), Some(bytes)) = (id, bytes) {
                items.push((
                    AssetId::from_bytes(id.as_slice().try_into().map_err(|_| {
                        LibError::Internal("invalid asset id in embedding row".into())
                    })?),
                    bytes_to_f32(&bytes),
                ));
            }
        }
        let generation = generation
            .ok_or_else(|| LibError::Internal(format!("missing ANN state for {space:?}")))?;
        (generation, items)
    };
    let dimension = items.first().map_or(0, |(_, vector)| vector.len());
    let item_count = items.len();
    let sidecar = Sidecar {
        format_version: FORMAT_VERSION,
        backend: BACKEND.into(),
        space_id: space.to_string(),
        generation,
        dimension,
        item_count,
        index: AnnIndex::build(items)?,
    };
    persist_sidecar(&inner.dir, &sidecar)?;

    if !publish_generation(conn, space, generation)? {
        let _ = fs::remove_file(sidecar_path(&inner.dir, space, generation));
        return Ok(());
    }

    inner.cache.lock().unwrap().insert(
        space.to_string(),
        CachedIndex {
            generation,
            index: sidecar.index.map(Arc::new),
        },
    );
    remove_old_sidecars(&inner.dir, space, generation);
    Ok(())
}

/// Publish a snapshot base even when newer writes landed during its build. Those writes remain in
/// `ann_delta` and are applied as the bounded overlay; only a base already newer than this snapshot
/// makes the work obsolete. This is the liveness property that keeps compaction progressing under
/// continuous analysis writes.
fn publish_generation(
    conn: &mut Connection,
    space: &str,
    generation: i64,
) -> Result<bool, LibError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    let state: Option<(i64, i64)> = tx
        .query_row(
            "SELECT generation,indexed_generation FROM ann_space_state WHERE space_id=?1",
            params![space],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((current_generation, indexed_generation)) = state else {
        tx.commit().map_err(internal)?;
        return Ok(false);
    };
    if indexed_generation > generation || current_generation < generation {
        tx.commit().map_err(internal)?;
        return Ok(false);
    }
    tx.execute(
        "UPDATE ann_space_state SET indexed_generation=?2, format_version=?3,
                lifecycle='ready', last_error=NULL WHERE space_id=?1",
        params![space, generation, FORMAT_VERSION],
    )
    .map_err(internal)?;
    tx.execute(
        "DELETE FROM ann_delta WHERE space_id=?1 AND generation<=?2",
        params![space, generation],
    )
    .map_err(internal)?;
    tx.commit().map_err(internal)?;
    Ok(true)
}

fn sidecar_stem(space: &str) -> String {
    blake3::hash(space.as_bytes()).to_hex().to_string()
}

fn sidecar_path(dir: &Path, space: &str, generation: i64) -> PathBuf {
    dir.join(format!("{}-{generation}.hnsw", sidecar_stem(space)))
}

fn persist_sidecar(dir: &Path, sidecar: &Sidecar) -> Result<(), LibError> {
    let payload = bincode::serialize(sidecar).map_err(internal)?;
    let checksum = blake3::hash(&payload);
    let path = sidecar_path(dir, &sidecar.space_id, sidecar.generation);
    let temp = dir.join(format!(
        ".{}-{}-{}-{}.tmp",
        sidecar_stem(&sidecar.space_id),
        sidecar.generation,
        std::process::id(),
        TEMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(internal)?;
    let write_result = (|| -> Result<(), LibError> {
        file.write_all(MAGIC).map_err(internal)?;
        file.write_all(checksum.as_bytes()).map_err(internal)?;
        file.write_all(&payload).map_err(internal)?;
        file.sync_all().map_err(internal)?;
        // Another process may have completed the same snapshot while this one serialized. A valid
        // generation file wins unchanged; replacing it through a quarantine window would make a
        // concurrently loaded pointer disappear for no benefit.
        if path.exists() && load_sidecar(dir, &sidecar.space_id, sidecar.generation).is_ok() {
            fs::remove_file(&temp).map_err(internal)?;
            return Ok(());
        }
        // Recovery can replace a corrupt file at the same generation. Windows does not rename over
        // an existing target, so quarantine it first; normal publication has a new destination.
        let quarantine = path.with_extension("corrupt");
        if path.exists() {
            let _ = fs::remove_file(&quarantine);
            fs::rename(&path, &quarantine).map_err(internal)?;
        }
        fs::rename(&temp, &path).map_err(internal)?;
        let _ = fs::remove_file(quarantine);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result?;
    if let Ok(directory) = OpenOptions::new().read(true).open(dir) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn cleanup_stale_sidecar_work(dir: &Path) {
    const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_work_file = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                (name.starts_with('.') && name.ends_with(".tmp")) || name.ends_with(".corrupt")
            });
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if is_work_file && stale {
            let _ = fs::remove_file(path);
        }
    }
}

fn load_sidecar(dir: &Path, space: &str, generation: i64) -> Result<Sidecar, LibError> {
    let bytes = fs::read(sidecar_path(dir, space, generation)).map_err(internal)?;
    if bytes.len() < MAGIC.len() + 32 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(LibError::Internal("invalid ANN sidecar header".into()));
    }
    let expected = &bytes[MAGIC.len()..MAGIC.len() + 32];
    let payload = &bytes[MAGIC.len() + 32..];
    if blake3::hash(payload).as_bytes() != expected {
        return Err(LibError::Internal("ANN sidecar checksum mismatch".into()));
    }
    let sidecar: Sidecar = bincode::deserialize(payload).map_err(internal)?;
    if sidecar.format_version != FORMAT_VERSION
        || sidecar.backend != BACKEND
        || sidecar.space_id != space
        || sidecar.generation != generation
    {
        return Err(LibError::Internal("ANN sidecar metadata mismatch".into()));
    }
    match &sidecar.index {
        Some(index)
            if index.len() == sidecar.item_count
                && index.dimensions() == sidecar.dimension
                && sidecar.item_count > 0 => {}
        None if sidecar.item_count == 0 && sidecar.dimension == 0 => {}
        _ => {
            return Err(LibError::Internal(
                "ANN sidecar graph does not match declared shape".into(),
            ));
        }
    }
    Ok(sidecar)
}

fn remove_old_sidecars(dir: &Path, space: &str, keep_generation: i64) {
    let prefix = format!("{}-", sidecar_stem(space));
    let keep = sidecar_path(dir, space, keep_generation);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let older_generation = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|name| name.strip_suffix(".hnsw"))
            .and_then(|generation| generation.parse::<i64>().ok())
            .is_some_and(|generation| generation < keep_generation);
        if path != keep && older_generation {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{now_ms, Store};
    use dam_api::dto::MediaType;
    use dam_api::id::SourceId;

    #[test]
    fn persisted_index_round_trips_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<_> = (0..4).map(|_| AssetId::new()).collect();
        let sidecar = Sidecar {
            format_version: FORMAT_VERSION,
            backend: BACKEND.into(),
            space_id: "image@test".into(),
            generation: 7,
            dimension: 3,
            item_count: 4,
            index: AnnIndex::build(vec![
                (ids[0], vec![1.0, 0.0, 0.0]),
                (ids[1], vec![0.0, 1.0, 0.0]),
                (ids[2], vec![0.0, 0.0, 1.0]),
                (ids[3], vec![0.9, 0.1, 0.0]),
            ])
            .unwrap(),
        };
        persist_sidecar(dir.path(), &sidecar).unwrap();
        let loaded = load_sidecar(dir.path(), "image@test", 7).unwrap();
        assert_eq!(
            loaded
                .index
                .unwrap()
                .candidates(&[1.0, 0.0, 0.0], 1)
                .unwrap(),
            vec![ids[0]]
        );

        let path = sidecar_path(dir.path(), "image@test", 7);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(path, bytes).unwrap();
        assert!(load_sidecar(dir.path(), "image@test", 7).is_err());
    }

    #[test]
    fn snapshot_publication_preserves_newer_deltas() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ann_space_state(
                 space_id TEXT PRIMARY KEY, generation INTEGER NOT NULL,
                 indexed_generation INTEGER NOT NULL, format_version INTEGER NOT NULL,
                 lifecycle TEXT NOT NULL, last_error TEXT
             );
             CREATE TABLE ann_delta(
                 space_id TEXT NOT NULL, asset_id BLOB NOT NULL,
                 generation INTEGER NOT NULL, operation TEXT NOT NULL,
                 PRIMARY KEY(space_id,asset_id)
             );
             INSERT INTO ann_space_state VALUES('image@test',8,0,1,'building',NULL);
             INSERT INTO ann_delta VALUES('image@test',x'01',5,'upsert');
             INSERT INTO ann_delta VALUES('image@test',x'02',8,'upsert');",
        )
        .unwrap();

        assert!(publish_generation(&mut conn, "image@test", 5).unwrap());
        let indexed: i64 = conn
            .query_row(
                "SELECT indexed_generation FROM ann_space_state WHERE space_id='image@test'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 5);
        let deltas: Vec<i64> = conn
            .prepare("SELECT generation FROM ann_delta ORDER BY generation")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            deltas,
            vec![8],
            "writes newer than the snapshot stay overlaid"
        );
        assert!(
            !publish_generation(&mut conn, "image@test", 4).unwrap(),
            "an older concurrent builder must not downgrade the published base"
        );
        conn.execute(
            "UPDATE ann_space_state SET generation=10,indexed_generation=9 WHERE space_id='image@test'",
            [],
        )
        .unwrap();
        assert_eq!(
            claim_corrupt_generation(&conn, "image@test", 5, "stale corrupt read").unwrap(),
            0,
            "a stale corrupt loader must not claim a newer published pointer"
        );
        assert_eq!(
            conn.query_row(
                "SELECT indexed_generation FROM ann_space_state WHERE space_id='image@test'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            9
        );
    }

    fn wait_ready(store: &Store, space: &str) -> i64 {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let state: Option<(i64, i64, String)> = {
                let conn = store.read().unwrap();
                conn.query_row(
                    "SELECT generation,indexed_generation,lifecycle FROM ann_space_state
                      WHERE space_id=?1",
                    params![space],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .unwrap()
            };
            if let Some((generation, indexed, lifecycle)) = state {
                if indexed > 0 && lifecycle == "ready" {
                    return generation;
                }
            }
            assert!(std::time::Instant::now() < deadline, "ANN build timed out");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn insert_asset(store: &Store, source: SourceId, id: AssetId, filename: &str) {
        let conn = store.write();
        conn.execute(
            "INSERT INTO asset(id,source_id,path,filename,scanned_at,media_type,format,
                               created_at,updated_at)
             VALUES(?1,?2,?3,?3,0,'image','png',0,0)",
            params![id.as_bytes().to_vec(), source.as_bytes().to_vec(), filename],
        )
        .unwrap();
    }

    #[test]
    fn lifecycle_persists_base_overlays_changes_and_recovers_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let source = SourceId::new();
        let first = AssetId::new();
        let second = AssetId::new();
        let path;
        {
            let store = Store::open(dir.path()).unwrap();
            {
                let conn = store.write();
                conn.execute(
                    "INSERT INTO source(id,name,kind,connection,created_at,updated_at)
                     VALUES(?1,'test','local_fs','{}',?2,?2)",
                    params![source.as_bytes().to_vec(), now_ms()],
                )
                .unwrap();
            }
            insert_asset(&store, source, first, "first.png");
            store
                .set_embedding(
                    &first,
                    "image@test",
                    MediaType::Image,
                    &[1.0, 0.0],
                    "test@1",
                )
                .unwrap();
            let base_generation = wait_ready(&store, "image@test");
            path = sidecar_path(
                store.ann.as_ref().unwrap().sidecar_dir(),
                "image@test",
                base_generation,
            );
            assert!(path.exists());

            // One insert and one update stay in the durable overlay rather than globally rebuilding.
            insert_asset(&store, source, second, "second.png");
            store
                .set_embedding(
                    &second,
                    "image@test",
                    MediaType::Image,
                    &[0.8, 0.6],
                    "test@1",
                )
                .unwrap();
            store
                .set_embedding(
                    &first,
                    "image@test",
                    MediaType::Image,
                    &[0.0, 1.0],
                    "test@1",
                )
                .unwrap();
            let nearest = store
                .nearest_in_space("image@test", &[1.0, 0.0], 2)
                .unwrap();
            assert_eq!(
                nearest[0].0, second,
                "overlay is included and exact-reranked"
            );
            let indexed: i64 = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT indexed_generation FROM ann_space_state WHERE space_id='image@test'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                indexed, base_generation,
                "small deltas do not rebuild the base"
            );

            store.clear_embedding(&second, "image@test").unwrap();
            let nearest = store
                .nearest_in_space("image@test", &[1.0, 0.0], 2)
                .unwrap();
            assert_eq!(
                nearest.iter().map(|hit| hit.0).collect::<Vec<_>>(),
                vec![first]
            );
        }

        // The indexed generation points at a checksummed sidecar. Damage is detected on restart,
        // exact fallback remains usable, and the worker atomically replaces it.
        let mut bytes = fs::read(&path).unwrap();
        bytes[MAGIC.len() + 4] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        let store = Store::open(dir.path()).unwrap();
        let recovered_generation = wait_ready(&store, "image@test");
        assert!(sidecar_path(
            store.ann.as_ref().unwrap().sidecar_dir(),
            "image@test",
            recovered_generation
        )
        .exists());
    }

    #[test]
    fn ann_recall_matches_exact_on_clustered_vectors() {
        let mut items = Vec::new();
        for index in 0..2_000usize {
            let mut vector = vec![0.0; 32];
            vector[index % 32] = 1.0;
            vector[(index * 7 + 3) % 32] += (index % 17) as f32 / 200.0;
            let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            vector.iter_mut().for_each(|value| *value /= norm);
            items.push((AssetId::new(), vector));
        }
        let index = AnnIndex::build(items.clone()).unwrap().unwrap();
        let mut recalled = 0usize;
        let mut expected_total = 0usize;
        for (_, query) in items.iter().step_by(97).take(20) {
            let mut exact: Vec<_> = items
                .iter()
                .map(|(id, vector)| (*id, crate::similarity::cosine(query, vector)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let expected: std::collections::HashSet<_> =
                exact.iter().take(10).map(|hit| hit.0).collect();
            let actual = index.candidates(query, 10).unwrap();
            recalled += actual.iter().filter(|id| expected.contains(id)).count();
            expected_total += expected.len();
        }
        assert!(
            recalled as f32 / expected_total as f32 >= 0.9,
            "recall@10 fell below the ADR acceptance floor"
        );
    }

    #[test]
    fn nonempty_mixed_dimension_build_is_an_error() {
        let result = AnnIndex::build(vec![
            (AssetId::new(), vec![1.0, 0.0]),
            (AssetId::new(), vec![1.0, 0.0, 0.0]),
        ]);
        assert!(
            matches!(result, Err(LibError::Internal(message)) if message.contains("mixed dimensions")),
            "a failed non-empty build must not become a ready empty base"
        );
    }
}
