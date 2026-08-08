//! Bounded derivative-cache controller (issue #144).
//!
//! One background-built startup inventory owns byte accounting for local and federated
//! derivatives. Existing hits remain readable while that snapshot is built; publications wait by
//! declining to cache until accounting is authoritative. Every subsequent hit, publication,
//! eviction, metric read, and explicit removal updates the inventory, so the hot path never walks
//! the cache tree. Preview entries are evicted before thumbnails, then LRU within a tier; peer
//! entries have their own budget and seven-day freshness.

use dam_api::admin::CacheUsage;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

pub(crate) const PEER_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_LOCAL_MAX: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_PEER_MAX: u64 = 2 * 1024 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Thumbnail,
    Preview,
    Peer,
}

impl Tier {
    fn eviction_priority(self) -> u8 {
        match self {
            Tier::Preview | Tier::Peer => 0,
            Tier::Thumbnail => 1,
        }
    }

    fn is_peer(self) -> bool {
        self == Tier::Peer
    }
}

/// Operator cache caps. `None` derives the ADR-0009 default from current free disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheOptions {
    pub local_bytes: Option<u64>,
    pub peer_bytes: Option<u64>,
}

impl CacheOptions {
    pub fn from_mebibytes(local_mb: Option<u64>, peer_mb: Option<u64>) -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            local_bytes: local_mb.map(|value| value.saturating_mul(MIB)),
            peer_bytes: peer_mb.map(|value| value.saturating_mul(MIB)),
        }
    }

    fn resolve(self, root: &Path) -> ResolvedOptions {
        let free = free_disk_bytes(root).unwrap_or(DEFAULT_LOCAL_MAX.saturating_mul(10));
        self.resolve_for_free(free)
    }

    fn resolve_for_free(self, free: u64) -> ResolvedOptions {
        let ten_percent_free = free / 10;
        ResolvedOptions {
            local_bytes: self
                .local_bytes
                .unwrap_or(DEFAULT_LOCAL_MAX.min(ten_percent_free)),
            peer_bytes: self
                .peer_bytes
                .unwrap_or(DEFAULT_PEER_MAX.min(ten_percent_free)),
        }
    }
}

#[derive(Clone, Copy)]
struct ResolvedOptions {
    local_bytes: u64,
    peer_bytes: u64,
}

#[derive(Clone)]
struct Entry {
    bytes: u64,
    tier: Tier,
    eviction_priority: u8,
    used: u64,
}

#[derive(Default)]
struct Counters {
    hits: u64,
    misses: u64,
    evictions: u64,
    stale_deleted: u64,
}

#[derive(Default)]
struct State {
    initialized: bool,
    inventory_running: bool,
    /// Paths read while the background snapshot is in flight. The final O(1) swap replays only
    /// this normally tiny set, so a hit/miss concurrent with the walk cannot be resurrected or
    /// lose its fresher LRU position.
    inventory_touched: HashSet<PathBuf>,
    clock: u64,
    entries: HashMap<PathBuf, Entry>,
    usage_bytes: [u64; 3],
    usage_files: [u64; 3],
    counters: [Counters; 3],
}

impl State {
    fn counter(&mut self, tier: Tier) -> &mut Counters {
        &mut self.counters[tier_index(tier)]
    }

    fn insert_entry(&mut self, path: PathBuf, entry: Entry) {
        if let Some(old) = self.entries.insert(path, entry.clone()) {
            let index = tier_index(old.tier);
            self.usage_bytes[index] = self.usage_bytes[index].saturating_sub(old.bytes);
            self.usage_files[index] = self.usage_files[index].saturating_sub(1);
        }
        let index = tier_index(entry.tier);
        self.usage_bytes[index] = self.usage_bytes[index].saturating_add(entry.bytes);
        self.usage_files[index] = self.usage_files[index].saturating_add(1);
    }

    fn remove_entry(&mut self, path: &Path) -> Option<Entry> {
        let old = self.entries.remove(path)?;
        let index = tier_index(old.tier);
        self.usage_bytes[index] = self.usage_bytes[index].saturating_sub(old.bytes);
        self.usage_files[index] = self.usage_files[index].saturating_sub(1);
        Some(old)
    }

    fn budget_usage(&self, tier: Tier) -> u64 {
        if tier.is_peer() {
            self.usage_bytes[tier_index(Tier::Peer)]
        } else {
            self.usage_bytes[tier_index(Tier::Thumbnail)]
                .saturating_add(self.usage_bytes[tier_index(Tier::Preview)])
        }
    }
}

fn tier_index(tier: Tier) -> usize {
    match tier {
        Tier::Thumbnail => 0,
        Tier::Preview => 1,
        Tier::Peer => 2,
    }
}

fn path_priority(path: &Path, tier: Tier) -> u8 {
    if tier == Tier::Peer {
        if path
            .extension()
            .is_some_and(|extension| extension == "dmsh")
        {
            Tier::Preview.eviction_priority()
        } else {
            Tier::Thumbnail.eviction_priority()
        }
    } else {
        tier.eviction_priority()
    }
}

/// Shared keyed-flight table. The map entry disappears after the last waiter completes, keeping RAM
/// proportional to concurrent misses rather than total historical cache keys.
struct Flights {
    map: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    current: AtomicUsize,
    maximum: AtomicUsize,
}

pub(crate) struct Controller {
    root: PathBuf,
    options: ResolvedOptions,
    peer_ttl: Duration,
    state: Mutex<State>,
    inventory_ready: Condvar,
    flights: Flights,
    inventory_runs: AtomicU64,
    model_derivative_generations: AtomicU64,
}

impl Controller {
    pub(crate) fn new(data_dir: &Path, options: CacheOptions) -> Arc<Self> {
        let root = data_dir.join("cache");
        // A brand-new data directory has nothing to inventory, so publications are immediately
        // enabled. An existing cache is accounted in the background by `EmbeddedLibrary::new`.
        let state = State {
            initialized: !root.exists(),
            ..State::default()
        };
        Arc::new(Self {
            options: options.resolve(&root),
            root,
            peer_ttl: PEER_TTL,
            state: Mutex::new(state),
            inventory_ready: Condvar::new(),
            flights: Flights {
                map: Mutex::new(HashMap::new()),
                current: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            },
            inventory_runs: AtomicU64::new(0),
            model_derivative_generations: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn new_with_ttl(data_dir: &Path, options: CacheOptions, peer_ttl: Duration) -> Arc<Self> {
        let controller = Self::new(data_dir, options);
        Arc::new(Self {
            root: controller.root.clone(),
            options: controller.options,
            peer_ttl,
            state: Mutex::new(State {
                initialized: !controller.root.exists(),
                ..State::default()
            }),
            inventory_ready: Condvar::new(),
            flights: Flights {
                map: Mutex::new(HashMap::new()),
                current: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            },
            inventory_runs: AtomicU64::new(0),
            model_derivative_generations: AtomicU64::new(0),
        })
    }

    pub(crate) async fn singleflight<T, F, Fut>(&self, key: String, work: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let flight = {
            let mut flights = self.flights.map.lock().unwrap();
            flights
                .entry(key.clone())
                .or_insert_with(|| {
                    let current = self.flights.current.fetch_add(1, Ordering::Relaxed) + 1;
                    self.flights.maximum.fetch_max(current, Ordering::Relaxed);
                    Arc::new(tokio::sync::Mutex::new(()))
                })
                .clone()
        };
        let guard = flight.lock().await;
        let result = work().await;
        drop(guard);
        let mut flights = self.flights.map.lock().unwrap();
        if Arc::strong_count(&flight) == 2 {
            flights.remove(&key);
            self.flights.current.fetch_sub(1, Ordering::Relaxed);
        }
        result
    }

    pub(crate) fn initialize_now(&self) {
        {
            let mut state = self.state.lock().unwrap();
            loop {
                if state.initialized {
                    return;
                }
                if !state.inventory_running {
                    state.inventory_running = true;
                    self.inventory_runs.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                state = self.inventory_ready.wait(state).unwrap();
            }
        }

        // The million-file walk and HashMap construction happen without the live state lock. A
        // request needs that lock only long enough to record its one touched path.
        let mut inventory = self.build_inventory();
        self.evict_to_fit(&mut inventory, Tier::Thumbnail, 0);
        self.evict_to_fit(&mut inventory, Tier::Peer, 0);

        let mut state = self.state.lock().unwrap();
        for index in 0..state.counters.len() {
            inventory.counters[index].hits = inventory.counters[index]
                .hits
                .saturating_add(state.counters[index].hits);
            inventory.counters[index].misses = inventory.counters[index]
                .misses
                .saturating_add(state.counters[index].misses);
            inventory.counters[index].evictions = inventory.counters[index]
                .evictions
                .saturating_add(state.counters[index].evictions);
            inventory.counters[index].stale_deleted = inventory.counters[index]
                .stale_deleted
                .saturating_add(state.counters[index].stale_deleted);
        }
        // Snapshot entries for concurrently touched paths may be stale. Replace them from the live
        // overlay only when the file still exists; this is bounded by requests during startup, not
        // by the cache's total entry count.
        let touched: Vec<_> = state.inventory_touched.drain().collect();
        for path in touched {
            inventory.remove_entry(&path);
            let Some(live) = state.entries.get(&path) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            inventory.clock += 1;
            inventory.insert_entry(
                path,
                Entry {
                    bytes: metadata.len(),
                    tier: live.tier,
                    eviction_priority: live.eviction_priority,
                    used: inventory.clock,
                },
            );
        }
        inventory.initialized = true;
        inventory.inventory_running = false;
        *state = inventory;
        drop(state);
        self.inventory_ready.notify_all();
    }

    /// Opportunistically join the same keyed flight from synchronous background workers. If an
    /// interactive request owns it, skip rather than parking the bounded pool: with a one-thread
    /// pool the request's queued generation must remain able to run.
    pub(crate) fn singleflight_blocking<T, F>(&self, key: String, work: F) -> Option<T>
    where
        F: FnOnce() -> T,
    {
        let flight = {
            let mut flights = self.flights.map.lock().unwrap();
            flights
                .entry(key.clone())
                .or_insert_with(|| {
                    let current = self.flights.current.fetch_add(1, Ordering::Relaxed) + 1;
                    self.flights.maximum.fetch_max(current, Ordering::Relaxed);
                    Arc::new(tokio::sync::Mutex::new(()))
                })
                .clone()
        };
        let Ok(guard) = flight.try_lock() else {
            return None;
        };
        let result = work();
        drop(guard);
        let mut flights = self.flights.map.lock().unwrap();
        if Arc::strong_count(&flight) == 2 {
            flights.remove(&key);
            self.flights.current.fetch_sub(1, Ordering::Relaxed);
        }
        Some(result)
    }

    #[cfg(test)]
    pub(crate) fn max_in_flight(&self) -> usize {
        self.flights.maximum.load(Ordering::Relaxed)
    }

    #[cfg(feature = "render")]
    pub(crate) fn record_model_derivative_generation(&self) {
        self.model_derivative_generations
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn model_derivative_generations(&self) -> u64 {
        self.model_derivative_generations.load(Ordering::Relaxed)
    }

    pub(crate) fn read(&self, path: &Path, tier: Tier) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        if !state.initialized {
            state.inventory_touched.insert(path.to_path_buf());
        }
        if tier == Tier::Peer && is_stale(path, self.peer_ttl) {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    state.remove_entry(path);
                    let counter = state.counter(tier);
                    counter.evictions += 1;
                    counter.stale_deleted += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    state.remove_entry(path);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "stale cache cleanup failed");
                }
            }
            state.counter(tier).misses += 1;
            return None;
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                if bytes.len() as u64 > self.budget(tier) {
                    state.remove_entry(path);
                    if let Err(error) = std::fs::remove_file(path) {
                        tracing::warn!(path = %path.display(), %error, "oversized cache entry cleanup failed");
                    }
                    state.counter(tier).misses += 1;
                    return None;
                }
                state.clock += 1;
                let used = state.clock;
                state.insert_entry(
                    path.to_path_buf(),
                    Entry {
                        bytes: bytes.len() as u64,
                        tier,
                        eviction_priority: path_priority(path, tier),
                        used,
                    },
                );
                self.evict_to_fit(&mut state, tier, 0);
                state.counter(tier).hits += 1;
                Some(bytes)
            }
            Err(_) => {
                state.remove_entry(path);
                state.counter(tier).misses += 1;
                None
            }
        }
    }

    /// Atomically publish one derivative and enforce its budget before the rename makes it visible.
    pub(crate) fn publish(&self, path: &Path, bytes: &[u8], tier: Tier) {
        let mut state = self.state.lock().unwrap();
        if !state.initialized {
            state.inventory_touched.insert(path.to_path_buf());
            // Serving the generated bytes is still correct, but publishing before existing usage
            // is known could exceed the disk budget with no accounted victim to evict. Decline the
            // cache write until the background inventory completes.
            return;
        }
        // Derivative keys are immutable. Another publisher may have won between the caller's
        // cache probe and this lock; preserving that complete file is both cheaper and genuinely
        // atomic (a later temp-write failure can never destroy a valid target).
        if path.exists() {
            state.clock += 1;
            let used = state.clock;
            if let Some(entry) = state.entries.get_mut(path) {
                entry.used = used;
            } else if let Ok(metadata) = std::fs::metadata(path) {
                state.insert_entry(
                    path.to_path_buf(),
                    Entry {
                        bytes: metadata.len(),
                        tier,
                        eviction_priority: path_priority(path, tier),
                        used,
                    },
                );
                self.evict_to_fit(&mut state, tier, 0);
            }
            return;
        }
        let budget = self.budget(tier);
        let size = bytes.len() as u64;
        if size > budget {
            return;
        }
        if !self.evict_for_publish(&mut state, tier, size) {
            return;
        }
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|v| v.to_str()))
        else {
            return;
        };
        if let Err(error) = std::fs::create_dir_all(dir) {
            tracing::warn!(path = %dir.display(), %error, "cache directory creation failed");
            return;
        }
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
        if let Err(error) = std::fs::write(&tmp, bytes) {
            tracing::warn!(path = %tmp.display(), %error, "cache temp write failed");
            return;
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            tracing::warn!(from = %tmp.display(), to = %path.display(), %error, "cache publish failed");
            if let Err(cleanup) = std::fs::remove_file(&tmp) {
                tracing::warn!(path = %tmp.display(), error = %cleanup, "cache temp cleanup failed");
            }
            return;
        }
        state.clock += 1;
        let used = state.clock;
        state.insert_entry(
            path.to_path_buf(),
            Entry {
                bytes: size,
                tier,
                eviction_priority: path_priority(path, tier),
                used,
            },
        );
    }

    pub(crate) fn usage(&self, tier: Tier) -> CacheUsage {
        // Administration asks for authoritative byte totals and may pay the wait; interactive
        // derivative reads never come through this path.
        self.initialize_now();
        let state = self.state.lock().unwrap();
        let index = tier_index(tier);
        let bytes = state.usage_bytes[index];
        let files = state.usage_files[index];
        let counters = &state.counters[index];
        CacheUsage {
            bytes,
            files,
            budget_bytes: self.budget(tier),
            hits: counters.hits,
            misses: counters.misses,
            evictions: counters.evictions,
            stale_deleted: counters.stale_deleted,
        }
    }

    pub(crate) fn clear(&self, tiers: &[Tier]) -> (u64, u64) {
        self.initialize_now();
        let mut state = self.state.lock().unwrap();
        let paths: Vec<_> = state
            .entries
            .iter()
            .filter(|(_, entry)| tiers.contains(&entry.tier))
            .map(|(path, entry)| (path.clone(), entry.bytes))
            .collect();
        let mut bytes = 0u64;
        let mut files = 0u64;
        for (path, size) in paths {
            if std::fs::remove_file(&path).is_ok() {
                state.remove_entry(&path);
                bytes = bytes.saturating_add(size);
                files += 1;
            }
        }
        // Explicit maintenance may pay a directory walk. Remove files that appeared out of band
        // after the live inventory was built as well, so "clear" remains authoritative.
        for tier in tiers {
            let dir = match tier {
                Tier::Thumbnail => self.root.join("thumbnails"),
                Tier::Preview => self.root.join("previews"),
                Tier::Peer => self.root.join("peer"),
            };
            walk_files(&dir, &mut |path, metadata| {
                if std::fs::remove_file(path).is_ok() {
                    state.remove_entry(path);
                    bytes = bytes.saturating_add(metadata.len());
                    files += 1;
                }
            });
        }
        (bytes, files)
    }

    pub(crate) fn remove_prefix(&self, tier: Tier, prefix: &str) -> u64 {
        self.initialize_now();
        let mut state = self.state.lock().unwrap();
        let paths: Vec<_> = state
            .entries
            .iter()
            .filter(|(path, entry)| {
                entry.tier == tier
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix))
            })
            .map(|(path, _)| path.clone())
            .collect();
        let mut removed = 0;
        for path in paths {
            if std::fs::remove_file(&path).is_ok() {
                state.remove_entry(&path);
                removed += 1;
            }
        }
        removed
    }

    /// Explicit lifecycle cleanup for one cache subtree (for example a removed peer owner). This
    /// may walk that subtree; unlike a read/metric request it is an infrequent maintenance event.
    pub(crate) fn remove_tree(&self, dir: &Path) -> (u64, u64) {
        self.initialize_now();
        let mut state = self.state.lock().unwrap();
        let mut bytes = 0u64;
        let mut files = 0u64;
        walk_files(
            dir,
            &mut |path, metadata| match std::fs::remove_file(path) {
                Ok(()) => {
                    state.remove_entry(path);
                    bytes = bytes.saturating_add(metadata.len());
                    files += 1;
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "cache tree entry cleanup failed");
                }
            },
        );
        if let Err(error) = std::fs::remove_dir_all(dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %dir.display(), %error, "cache tree cleanup incomplete");
            }
        }
        (bytes, files)
    }

    fn budget(&self, tier: Tier) -> u64 {
        if tier.is_peer() {
            self.options.peer_bytes
        } else {
            self.options.local_bytes
        }
    }

    fn evict_to_fit(&self, state: &mut State, tier: Tier, incoming: u64) {
        loop {
            let used = state.budget_usage(tier);
            if used.saturating_add(incoming) <= self.budget(tier) {
                break;
            }
            let victim = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.tier.is_peer() == tier.is_peer())
                .min_by_key(|(_, entry)| (entry.eviction_priority, entry.used))
                .map(|(path, entry)| (path.clone(), entry.tier));
            let Some((path, victim_tier)) = victim else {
                break;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    state.counter(victim_tier).evictions += 1;
                    state.remove_entry(&path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    state.remove_entry(&path);
                }
                Err(_) => break,
            }
        }
    }

    fn evict_for_publish(&self, state: &mut State, tier: Tier, incoming: u64) -> bool {
        loop {
            let used = state.budget_usage(tier);
            if used.saturating_add(incoming) <= self.budget(tier) {
                return true;
            }
            // A preview publication may displace only another preview. Thumbnails are the durable
            // local tier; thumbnail publications may first reclaim previews and then their own LRU.
            let victim = state
                .entries
                .iter()
                .filter(|(_, entry)| match tier {
                    Tier::Preview => entry.tier == Tier::Preview,
                    Tier::Thumbnail => !entry.tier.is_peer(),
                    Tier::Peer => entry.tier == Tier::Peer,
                })
                .min_by_key(|(_, entry)| (entry.eviction_priority, entry.used))
                .map(|(path, entry)| (path.clone(), entry.tier));
            let Some((path, victim_tier)) = victim else {
                return false;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    state.counter(victim_tier).evictions += 1;
                    state.remove_entry(&path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    state.remove_entry(&path);
                }
                Err(_) => return false,
            }
        }
    }

    fn build_inventory(&self) -> State {
        let mut state = State::default();
        for (dir, tier) in [
            (self.root.join("thumbnails"), Tier::Thumbnail),
            (self.root.join("previews"), Tier::Preview),
            (self.root.join("peer"), Tier::Peer),
        ] {
            walk_files(&dir, &mut |path, metadata| {
                if tier == Tier::Peer && metadata_is_stale(metadata, self.peer_ttl) {
                    if std::fs::remove_file(path).is_ok() {
                        let counter = state.counter(tier);
                        counter.evictions += 1;
                        counter.stale_deleted += 1;
                    }
                    return;
                }
                state.clock += 1;
                let used = state.clock;
                state.insert_entry(
                    path.to_path_buf(),
                    Entry {
                        bytes: metadata.len(),
                        tier,
                        eviction_priority: path_priority(path, tier),
                        used,
                    },
                );
            });
        }
        state.initialized = true;
        state
    }
}

/// Perf-harness seam over the production controller. The inventory thread is joined only after the
/// hit duration has been captured, keeping later harness metrics and work-directory cleanup free of
/// background I/O.
pub(crate) fn measure_first_thumbnail_hit(
    data_dir: &Path,
    hit_path: &Path,
) -> std::io::Result<Duration> {
    let cache = Controller::new(
        data_dir,
        CacheOptions {
            local_bytes: Some(u64::MAX),
            peer_bytes: Some(u64::MAX),
        },
    );
    std::thread::scope(|scope| {
        let inventory_cache = cache.clone();
        let inventory = scope.spawn(move || inventory_cache.initialize_now());
        let started = std::time::Instant::now();
        let hit = cache.read(hit_path, Tier::Thumbnail);
        let elapsed = started.elapsed();
        inventory
            .join()
            .map_err(|_| std::io::Error::other("derivative cache inventory thread panicked"))?;
        hit.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("thumbnail cache hit missing at {}", hit_path.display()),
            )
        })?;
        Ok(elapsed)
    })
}

fn walk_files(dir: &Path, visit: &mut impl FnMut(&Path, &std::fs::Metadata)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            walk_files(&path, visit);
        } else if metadata.is_file() && !entry.file_name().to_string_lossy().starts_with('.') {
            visit(&path, &metadata);
        }
    }
}

fn metadata_is_stale(metadata: &std::fs::Metadata, ttl: Duration) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > ttl)
}

fn is_stale(path: &Path, ttl: Duration) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata_is_stale(&metadata, ttl))
        .unwrap_or(false)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // statvfs field widths vary across Unix targets.
fn free_disk_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let existing = path.ancestors().find(|candidate| candidate.exists())?;
    let path = CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` points to writable storage for this call.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: successful `statvfs` initialized the structure.
    let stat = unsafe { stat.assume_init() };
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(not(unix))]
fn free_disk_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn defaults_match_adr_0009_free_disk_caps() {
        let dir = tempfile::tempdir().unwrap();
        let free = 50 * 1024 * 1024 * 1024;
        let options = CacheOptions::default().resolve_for_free(free);
        assert_eq!(options.local_bytes, 5 * 1024 * 1024 * 1024);
        assert_eq!(options.peer_bytes, DEFAULT_PEER_MAX);
        let actual = CacheOptions::default().resolve(dir.path());
        assert!(actual.local_bytes <= DEFAULT_LOCAL_MAX);
        assert!(actual.peer_bytes <= DEFAULT_PEER_MAX);
        assert_eq!(PEER_TTL, Duration::from_secs(7 * 24 * 60 * 60));
    }

    #[test]
    fn weighted_lru_keeps_thumbnails_ahead_of_previews_and_bounds_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Controller::new(
            dir.path(),
            CacheOptions {
                local_bytes: Some(16),
                peer_bytes: Some(8),
            },
        );
        let thumbs = dir.path().join("cache/thumbnails");
        let previews = dir.path().join("cache/previews");
        let peer = dir.path().join("cache/peer/source");
        let thumb = thumbs.join("keep.png");
        let old_preview = previews.join("old.dmsh");
        let new_preview = previews.join("new.dmsh");

        cache.publish(&thumb, &[1; 8], Tier::Thumbnail);
        cache.publish(&old_preview, &[2; 5], Tier::Preview);
        cache.publish(&new_preview, &[3; 5], Tier::Preview);
        assert!(
            thumb.exists(),
            "preview pressure must not evict a thumbnail"
        );
        assert!(
            !old_preview.exists(),
            "oldest preview should be reclaimed first"
        );
        assert!(new_preview.exists());
        assert!(cache.usage(Tier::Thumbnail).bytes + cache.usage(Tier::Preview).bytes <= 16);

        let peer_preview = peer.join("preview.dmsh");
        let peer_thumb = peer.join("thumb.png");
        cache.publish(&peer_preview, &[4; 6], Tier::Peer);
        cache.publish(&peer_thumb, &[5; 6], Tier::Peer);
        assert!(
            !peer_preview.exists(),
            "peer previews yield before thumbnails"
        );
        assert!(peer_thumb.exists());
        assert!(cache.usage(Tier::Peer).bytes <= 8);
        assert_eq!(cache.usage(Tier::Peer).files, 1);
    }

    #[test]
    fn failed_or_racing_republication_preserves_complete_target() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Controller::new(
            dir.path(),
            CacheOptions {
                local_bytes: Some(8),
                peer_bytes: Some(8),
            },
        );
        let target = dir.path().join("cache/thumbnails/immutable.png");
        cache.publish(&target, b"valid", Tier::Thumbnail);
        // This replacement is over-budget; a delete-before-temp implementation would lose the
        // valid target. Immutable publication instead treats the race as a completed hit.
        cache.publish(&target, b"replacement is too large", Tier::Thumbnail);
        assert_eq!(std::fs::read(&target).unwrap(), b"valid");
        assert_eq!(cache.usage(Tier::Thumbnail).bytes, 5);
    }

    #[test]
    fn sustained_unique_key_churn_never_exceeds_disk_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Controller::new(
            dir.path(),
            CacheOptions {
                local_bytes: Some(1024),
                peer_bytes: Some(512),
            },
        );
        let thumbs = dir.path().join("cache/thumbnails");
        for index in 0..10_000 {
            cache.publish(
                &thumbs.join(format!("unique-{index}.png")),
                &[index as u8; 32],
                Tier::Thumbnail,
            );
            if index % 257 == 0 {
                let usage = cache.usage(Tier::Thumbnail);
                assert!(usage.bytes <= usage.budget_bytes);
                assert!(usage.files <= 32);
            }
        }
        let usage = cache.usage(Tier::Thumbnail);
        assert_eq!(usage.bytes, 1024);
        assert_eq!(usage.files, 32);
        assert!(usage.evictions >= 9_968);
    }

    #[test]
    fn stale_peer_inventory_runs_once_and_updates_metrics_without_rescanning() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("cache/peer/source/stale.png");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"old").unwrap();
        let cache = Controller::new_with_ttl(
            dir.path(),
            CacheOptions {
                local_bytes: Some(1024),
                peer_bytes: Some(1024),
            },
            Duration::ZERO,
        );

        let first = cache.usage(Tier::Peer);
        assert_eq!(first.files, 0);
        assert_eq!(first.stale_deleted, 1);
        assert!(!stale.exists());
        let late = dir.path().join("cache/peer/source/late.png");
        std::fs::write(&late, b"late").unwrap();
        assert!(cache.read(&late, Tier::Peer).is_none());
        assert!(
            !late.exists(),
            "out-of-band stale files are deleted on read"
        );
        assert_eq!(cache.usage(Tier::Peer).stale_deleted, 2);
        for _ in 0..20 {
            let _ = cache.usage(Tier::Peer);
            let _ = cache.read(&stale, Tier::Peer);
        }
        assert_eq!(cache.inventory_runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn first_existing_hit_does_not_wait_for_or_start_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let thumbnails = dir.path().join("cache/thumbnails");
        std::fs::create_dir_all(&thumbnails).unwrap();
        for index in 0..1_000 {
            std::fs::write(thumbnails.join(format!("entry-{index}.png")), [index as u8]).unwrap();
        }
        let hit = thumbnails.join("entry-999.png");
        let cache = Controller::new(
            dir.path(),
            CacheOptions {
                local_bytes: Some(2_000),
                peer_bytes: Some(1),
            },
        );

        assert_eq!(cache.read(&hit, Tier::Thumbnail), Some(vec![231]));
        assert_eq!(cache.inventory_runs.load(Ordering::Relaxed), 0);

        cache.initialize_now();
        let usage = cache.usage(Tier::Thumbnail);
        assert_eq!(usage.files, 1_000);
        assert_eq!(usage.bytes, 1_000);
        assert_eq!(usage.hits, 1);
        assert_eq!(cache.inventory_runs.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_key_waiters_generate_once_and_recheck_populated_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Controller::new(
            dir.path(),
            CacheOptions {
                local_bytes: Some(1024),
                peer_bytes: Some(1024),
            },
        );
        let path = dir.path().join("cache/thumbnails/key.png");
        let generated = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let cache = cache.clone();
            let path = path.clone();
            let generated = generated.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .singleflight("thumbnail:key:64".into(), || async {
                        if cache.read(&path, Tier::Thumbnail).is_none() {
                            generated.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(5));
                            cache.publish(&path, b"one result", Tier::Thumbnail);
                        }
                        cache.read(&path, Tier::Thumbnail).unwrap()
                    })
                    .await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), b"one result");
        }
        assert_eq!(generated.load(Ordering::Relaxed), 1);
        assert_eq!(cache.max_in_flight(), 1);
    }
}
