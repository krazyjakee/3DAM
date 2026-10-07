//! Manual production scan sweep: cargo run -p dam-core --release --example scan_performance -- --help
//! Injected latency is a scheduling model; JSON distinguishes it from physical device evidence.

use dam_api::dto::*;
use dam_api::event::{EventTopic, LibraryEvent, SubscribeRequest};
use dam_api::id::SourceId;
use dam_api::service::{AuthContext, LibraryService};
use dam_api::LibError;
use dam_core::{
    EmbeddedLibrary, IoOptions, PipelinePolicy, ResourceOptions, StorageKind, StorageOverride,
};
use dam_sources::{ContentStat, Fetched, FileEntry, FileSource, SourceConnection};
use futures::{FutureExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Serialize)]
struct Config {
    paths: u64,
    tiny_bytes: u64,
    changed: u64,
    removed: u64,
    rejected_percent: u64,
    absent_timestamp_percent: u64,
    width: u64,
    depth: u64,
    large_bytes: u64,
    metadata_us: u64,
    cold_metadata_us: u64,
    listing_us: u64,
    seek_us: u64,
    round_trip_us: u64,
    transfer_mib: u64,
    io_mib: u64,
    data_mib: u64,
    cancel_after_ms: u64,
    timeout_secs: u64,
    foreground_budget_ms: f64,
    quick_budget_ms: Option<f64>,
    placement: String,
    backend: String,
    data_dir: PathBuf,
    source_root: PathBuf,
    output: PathBuf,
    background: bool,
    pause_before_change: bool,
    pressure_policy: String,
    foreground_writes: bool,
    topology: String,
    co_tenant_cache: String,
}

impl Config {
    fn parse() -> Result<Option<Self>> {
        let mut args = std::env::args().skip(1);
        let mut options = BTreeMap::new();
        while let Some(key) = args.next() {
            if key == "--help" {
                println!("Manual scan sweep. Required: --data-dir NEW_DIRECTORY. Options:\n\
                    --backend generated|local --source-root PATH --paths 100000|1000000\n\
                    --placement shared|separate --tiny-bytes 1024 --changed 10 --removed 10\n\
                    --rejected-percent 25 --absent-timestamp-percent 0 --width 100 --depth 3\n\
                    --large-bytes 0 --metadata-us 0 --cold-metadata-us 0 --listing-us 0\n\
                    --seek-us 0 --round-trip-us 0 --transfer-mib 32 --io-mib 8 --data-mib 128\n\
                    --cancel-after-ms 250 --timeout-secs 3600 --foreground-budget-ms 60\n\
                    --quick-budget-ms MILLISECONDS --background true|false --output PATH\n\
                    --pause-before-change true|false --pressure-policy disabled|production\n\
                    --foreground-writes true|false --co-tenant-cache cached|evict\n\
                    --topology declared|detected (local default: detected; --io-mib 0 uses hardware defaults)\n\
                    Generated mode streams virtual paths and bytes; local mode never edits source files.");
                return Ok(None);
            }
            if !key.starts_with("--") {
                return Err(format!("unexpected argument {key}").into());
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {key}"))?;
            options.insert(key[2..].to_string(), value);
        }
        let number = |name: &str, default: u64| -> Result<u64> {
            Ok(options
                .get(name)
                .map(|value| value.parse())
                .transpose()?
                .unwrap_or(default))
        };
        let data_dir = PathBuf::from(options.get("data-dir").ok_or("--data-dir is required")?);
        let backend = options
            .get("backend")
            .cloned()
            .unwrap_or_else(|| "generated".into());
        let placement = options
            .get("placement")
            .cloned()
            .unwrap_or_else(|| "shared".into());
        let pressure_policy = options.get("pressure-policy").cloned().unwrap_or_else(|| {
            if backend == "generated" {
                "disabled".into()
            } else {
                "production".into()
            }
        });
        let topology = options.get("topology").cloned().unwrap_or_else(|| {
            if backend == "generated" {
                "declared".into()
            } else {
                "detected".into()
            }
        });
        let co_tenant_cache = options
            .get("co-tenant-cache")
            .cloned()
            .unwrap_or_else(|| "cached".into());
        let source_root = options
            .get("source-root")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("generated-source"));
        if !["generated", "local"].contains(&backend.as_str())
            || !["shared", "separate"].contains(&placement.as_str())
            || !["disabled", "production"].contains(&pressure_policy.as_str())
            || !["declared", "detected"].contains(&topology.as_str())
            || !["cached", "evict"].contains(&co_tenant_cache.as_str())
        {
            return Err(
                "invalid backend, placement, topology, pressure policy or co-tenant cache mode"
                    .into(),
            );
        }
        if backend == "local" && !options.contains_key("source-root") {
            return Err("local mode requires --source-root".into());
        }
        let config = Self {
            paths: number("paths", 100_000)?,
            tiny_bytes: number("tiny-bytes", 1024)?,
            changed: number("changed", 10)?,
            removed: number("removed", 10)?,
            rejected_percent: number("rejected-percent", 25)?,
            absent_timestamp_percent: number("absent-timestamp-percent", 0)?,
            width: number("width", 100)?,
            depth: number("depth", 3)?,
            large_bytes: number("large-bytes", 0)?,
            metadata_us: number("metadata-us", 0)?,
            cold_metadata_us: number("cold-metadata-us", 0)?,
            listing_us: number("listing-us", 0)?,
            seek_us: number("seek-us", 0)?,
            round_trip_us: number("round-trip-us", 0)?,
            transfer_mib: number("transfer-mib", if backend == "local" { 0 } else { 32 })?,
            io_mib: number("io-mib", if backend == "local" { 0 } else { 8 })?,
            data_mib: number("data-mib", 128)?,
            cancel_after_ms: number("cancel-after-ms", 250)?,
            timeout_secs: number("timeout-secs", 3600)?,
            foreground_budget_ms: options
                .get("foreground-budget-ms")
                .map(|value| value.parse())
                .transpose()?
                .unwrap_or(60.0),
            quick_budget_ms: options
                .get("quick-budget-ms")
                .map(|value| value.parse())
                .transpose()?,
            output: options
                .get("output")
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.join("scan-evidence.json")),
            background: options
                .get("background")
                .is_some_and(|value| value == "true"),
            pause_before_change: options
                .get("pause-before-change")
                .is_some_and(|value| value == "true"),
            foreground_writes: options
                .get("foreground-writes")
                .is_some_and(|value| value == "true"),
            topology,
            co_tenant_cache,
            pressure_policy,
            backend,
            placement,
            data_dir,
            source_root,
        };
        if config.paths == 0
            || config.width == 0
            || config.depth > 32
            || config.rejected_percent >= 100
            || config.absent_timestamp_percent > 100
            || config.data_mib == 0
            || config.removed >= config.paths
            || config.tiny_bytes == 0
        {
            return Err("invalid numeric bounds".into());
        }
        if config.background && config.backend == "generated" {
            return Err("--background true requires local mode: production analysis reopens real registered source paths".into());
        }
        if config.backend == "generated" && config.topology != "declared" {
            return Err(
                "generated mode requires declared topology; virtual source backing is modeled"
                    .into(),
            );
        }
        if config.data_dir.join("library.db").exists() {
            return Err(
                "choose a fresh data directory; the sweep must not mutate an existing catalog"
                    .into(),
            );
        }
        Ok(Some(config))
    }

    fn data_transfer_mib(&self) -> u64 {
        if self.transfer_mib == 0 || self.placement == "shared" {
            self.transfer_mib
        } else {
            self.data_mib
        }
    }
}

#[derive(Default, Serialize, Clone, Copy)]
struct Counts {
    walk_calls: u64,
    listed_paths: u64,
    listing_requests: u64,
    metadata_requests: u64,
    eligible_paths: u64,
    fetch_opens: u64,
    source_payload_bytes: u64,
    scratch_written_bytes: u64,
    round_trips: u64,
}

impl Counts {
    fn delta(self, before: Self) -> Self {
        Self {
            walk_calls: self.walk_calls - before.walk_calls,
            listed_paths: self.listed_paths - before.listed_paths,
            listing_requests: self.listing_requests - before.listing_requests,
            metadata_requests: self.metadata_requests - before.metadata_requests,
            eligible_paths: self.eligible_paths - before.eligible_paths,
            fetch_opens: self.fetch_opens - before.fetch_opens,
            source_payload_bytes: self.source_payload_bytes - before.source_payload_bytes,
            scratch_written_bytes: self.scratch_written_bytes - before.scratch_written_bytes,
            round_trips: self.round_trips - before.round_trips,
        }
    }
}

#[derive(Default)]
struct DiskState {
    issued: u64,
    serving: u64,
    max_queue: u64,
    transferred_bytes: u64,
}

#[derive(Default)]
struct Disk {
    state: Mutex<DiskState>,
    ready: Condvar,
}

impl Disk {
    fn request(&self, latency_us: u64, bytes: u64, mib_per_sec: u64) {
        let mut state = self.state.lock().unwrap();
        let ticket = state.issued;
        state.issued += 1;
        state.max_queue = state.max_queue.max(state.issued - state.serving);
        while ticket != state.serving {
            state = self.ready.wait(state).unwrap();
        }
        drop(state);
        let transfer = if mib_per_sec == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(bytes as f64 / (mib_per_sec as f64 * 1024.0 * 1024.0))
        };
        let delay = Duration::from_micros(latency_us) + transfer;
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let mut state = self.state.lock().unwrap();
        state.transferred_bytes = state.transferred_bytes.saturating_add(bytes);
        state.serving += 1;
        self.ready.notify_all();
    }
}

struct Source {
    config: Config,
    native: Option<Arc<dyn FileSource>>,
    source_disk: Arc<Disk>,
    data_disk: Arc<Disk>,
    counts: Mutex<Counts>,
    active: AtomicU64,
    changed: AtomicBool,
    warm: AtomicBool,
}

struct Active<'a>(&'a AtomicU64);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Source {
    fn enter(&self) -> Active<'_> {
        self.active.fetch_add(1, Ordering::Relaxed);
        Active(&self.active)
    }

    fn counts(&self) -> Counts {
        *self.counts.lock().unwrap()
    }

    fn metadata_delay(&self) -> u64 {
        if self.warm.load(Ordering::Relaxed) {
            self.config.metadata_us
        } else {
            self.config.cold_metadata_us
        }
    }

    fn path(&self, index: u64) -> String {
        let bucket = index / self.config.width;
        let mut path = String::new();
        for level in 0..self.config.depth {
            path.push_str(&format!("level_{level}/bucket_{bucket}/"));
        }
        let extension = if index % 100 < self.config.rejected_percent {
            "dat"
        } else {
            "txt"
        };
        format!("{path}item_{index:09}.{extension}")
    }

    fn edited(&self, index: u64) -> bool {
        if index % 100 < self.config.rejected_percent {
            return false;
        }
        let ordinal = index / 100 * (100 - self.config.rejected_percent) + index % 100
            - self.config.rejected_percent;
        self.changed.load(Ordering::Relaxed) && ordinal < self.config.changed
    }

    fn size(&self, index: u64) -> u64 {
        if self.edited(index)
            && index == self.config.rejected_percent
            && self.config.large_bytes > 0
        {
            self.config.large_bytes
        } else {
            self.config.tiny_bytes + u64::from(self.edited(index))
        }
    }

    fn modified_ms(&self, index: u64) -> Option<i64> {
        // Independent of the rejected-name population, so absent timestamps still cover accepted
        // files when most names are intentionally rejected.
        (index.wrapping_mul(37).wrapping_add(13) % 100 >= self.config.absent_timestamp_percent)
            .then_some(1 + i64::from(self.edited(index)))
    }

    fn metadata_request(&self) {
        let mut counts = self.counts.lock().unwrap();
        counts.metadata_requests += 1;
        counts.round_trips += u64::from(self.config.round_trip_us > 0);
        drop(counts);
        self.source_disk.request(
            self.metadata_delay() + self.config.round_trip_us,
            0,
            self.config.transfer_mib,
        );
    }

    fn listing_request(&self) {
        let mut counts = self.counts.lock().unwrap();
        counts.listing_requests += 1;
        counts.round_trips += u64::from(self.config.round_trip_us > 0);
        drop(counts);
        self.source_disk.request(
            self.config.listing_us + self.config.round_trip_us,
            0,
            self.config.transfer_mib,
        );
    }
}

impl FileSource for Source {
    fn storage_path(&self) -> Option<&Path> {
        Some(&self.config.source_root)
    }

    fn fetch_uses_scratch(&self, rel: &str) -> bool {
        self.native
            .as_ref()
            .is_none_or(|native| native.fetch_uses_scratch(rel))
    }

    fn walk(
        &self,
        sink: &mut dyn FnMut(std::result::Result<FileEntry, LibError>) -> bool,
    ) -> std::result::Result<(), LibError> {
        self.walk_filtered(&mut |_| Ok(true), &mut || Ok(()), sink)
    }

    fn walk_filtered(
        &self,
        eligible: &mut dyn FnMut(&str) -> std::result::Result<bool, LibError>,
        pace: &mut dyn FnMut() -> std::result::Result<(), LibError>,
        sink: &mut dyn FnMut(std::result::Result<FileEntry, LibError>) -> bool,
    ) -> std::result::Result<(), LibError> {
        let _active = self.enter();
        self.counts.lock().unwrap().walk_calls += 1;
        if let Some(native) = &self.native {
            return native.walk_filtered(
                &mut |path| {
                    self.counts.lock().unwrap().listed_paths += 1;
                    let admitted = eligible(path)?;
                    if admitted {
                        self.counts.lock().unwrap().eligible_paths += 1;
                        self.metadata_request();
                    }
                    Ok(admitted)
                },
                &mut || {
                    pace()?;
                    self.listing_request();
                    Ok(())
                },
                sink,
            );
        }
        let paths = self.config.paths
            - if self.changed.load(Ordering::Relaxed) {
                self.config.removed
            } else {
                0
            };
        pace()?;
        self.listing_request(); // root directory
        if self.config.depth > 0 {
            pace()?;
            self.listing_request(); // common level_0 directory
        }
        for index in 0..paths {
            pace()?;
            if self.config.depth > 0 && index % self.config.width == 0 {
                for _ in 0..self.config.depth * 2 - 1 {
                    pace()?;
                    self.listing_request();
                }
            }
            self.counts.lock().unwrap().listed_paths += 1;
            let path = self.path(index);
            if !eligible(&path)? {
                continue;
            }
            self.counts.lock().unwrap().eligible_paths += 1;
            pace()?;
            self.metadata_request();
            let modified_ms = self.modified_ms(index);
            if !sink(Ok(FileEntry {
                rel_path: path,
                size: self.size(index),
                modified_ms,
            })) {
                break;
            }
        }
        Ok(())
    }

    fn fetch(&self, rel: &str) -> std::result::Result<Fetched, LibError> {
        self.fetch_paced(rel, &mut |_| Ok(()))
    }

    fn fetch_paced(
        &self,
        rel: &str,
        pace: &mut dyn FnMut(u64) -> std::result::Result<(), LibError>,
    ) -> std::result::Result<Fetched, LibError> {
        let _active = self.enter();
        self.counts.lock().unwrap().fetch_opens += 1;
        if let Some(native) = &self.native {
            let mut first = true;
            let copies = native.fetch_uses_scratch(rel);
            let fetched = native.fetch_paced(rel, &mut |bytes| {
                pace(bytes)?;
                self.source_disk.request(
                    if first { self.config.seek_us } else { 0 } + self.config.round_trip_us,
                    bytes,
                    self.config.transfer_mib,
                );
                if copies {
                    self.data_disk.request(
                        if first { self.config.seek_us } else { 0 },
                        bytes,
                        self.config.data_transfer_mib(),
                    );
                }
                self.counts.lock().unwrap().round_trips += u64::from(self.config.round_trip_us > 0);
                first = false;
                Ok(())
            })?;
            let bytes = std::fs::metadata(fetched.path())
                .map_err(source_error)?
                .len();
            let mut counts = self.counts.lock().unwrap();
            counts.source_payload_bytes += bytes;
            if copies {
                counts.scratch_written_bytes += bytes;
            }
            return Ok(fetched);
        }
        let index: u64 = rel
            .rsplit('/')
            .next()
            .unwrap_or("")
            .strip_prefix("item_")
            .and_then(|name| name.split('.').next())
            .ok_or_else(|| LibError::BadRequest("invalid generated path".into()))?
            .parse()
            .map_err(|_| LibError::BadRequest("invalid generated index".into()))?;
        let mut output = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile_in(self.config.data_dir.join("scratch"))
            .map_err(source_error)?;
        let mut bytes = vec![b'a' + (index % 26) as u8; CHUNK];
        let header = format!("{index:016x} revision {}\n", u64::from(self.edited(index)));
        bytes[..header.len()].copy_from_slice(header.as_bytes());
        let mut remaining = self.size(index);
        let mut hasher = blake3::Hasher::new();
        let mut first = true;
        while remaining > 0 {
            let length = remaining.min(CHUNK as u64);
            pace(length)?;
            let seek = if first { self.config.seek_us } else { 0 };
            self.source_disk.request(
                seek + self.config.round_trip_us,
                length,
                self.config.transfer_mib,
            );
            self.data_disk
                .request(seek, length, self.config.data_transfer_mib());
            output
                .write_all(&bytes[..length as usize])
                .map_err(source_error)?;
            hasher.update(&bytes[..length as usize]);
            let mut counts = self.counts.lock().unwrap();
            counts.source_payload_bytes += length;
            counts.scratch_written_bytes += length;
            counts.round_trips += u64::from(self.config.round_trip_us > 0);
            remaining -= length;
            first = false;
        }
        Ok(Fetched::HashedTemp {
            file: output,
            content_hash: hasher.finalize().to_hex().to_string(),
            source_stat: Some(ContentStat {
                len: self.size(index),
                modified_ms: self.modified_ms(index),
            }),
        })
    }
}

fn source_error(error: std::io::Error) -> LibError {
    LibError::SourceUnavailable(error.to_string())
}

#[derive(Serialize, Default)]
struct Host {
    rss_bytes: Option<u64>,
    peak_rss_bytes: Option<u64>,
    io: BTreeMap<String, u64>,
    load: Option<String>,
    io_pressure: Option<String>,
}

fn host() -> Host {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let memory = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|kb| kb * 1024)
        })
    };
    let io = std::fs::read_to_string("/proc/self/io")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.to_string(), value.trim().parse().ok()?))
        })
        .collect();
    Host {
        rss_bytes: memory("VmRSS:"),
        peak_rss_bytes: memory("VmHWM:"),
        io,
        load: std::fs::read_to_string("/proc/loadavg").ok(),
        io_pressure: std::fs::read_to_string("/proc/pressure/io").ok(),
    }
}

fn io_delta(after: &Host, before: &Host) -> BTreeMap<String, u64> {
    after
        .io
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.saturating_sub(*before.io.get(key).unwrap_or(&0)),
            )
        })
        .collect()
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn prepare_co_tenant(path: &Path, mode: &str) -> std::io::Result<()> {
    if mode == "evict" {
        #[cfg(target_os = "linux")]
        {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_all()?;
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "--co-tenant-cache evict requires Linux POSIX_FADV_DONTNEED",
            ));
        }
    }
    Ok(())
}

fn evict_co_tenant(file: &std::fs::File, mode: &str) -> std::io::Result<()> {
    if mode == "evict" {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // The synced fixture is one page. Advice requests eviction from the OS page cache;
            // the filesystem and device may still cache the physical block.
            let error = unsafe {
                libc::posix_fadvise(file.as_raw_fd(), 0, 4096, libc::POSIX_FADV_DONTNEED)
            };
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = file;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "--co-tenant-cache evict requires Linux POSIX_FADV_DONTNEED",
            ));
        }
    }
    Ok(())
}

fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize])
}

fn latency(values: &[f64]) -> Value {
    json!({"samples": values.len(), "p95_ms": percentile(values, 0.95), "p99_ms": percentile(values, 0.99)})
}

struct Background;
impl PipelinePolicy for Background {
    fn auto_thumbnail(&self) -> bool {
        true
    }
    fn auto_analyze(&self) -> bool {
        true
    }
}

struct StopGuard(Arc<AtomicBool>);
impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn checkpoint(path: &Path) -> Result<Value> {
    let connection = rusqlite::Connection::open(path)?;
    let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let start = Instant::now();
    let (busy, frames, checkpointed): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    Ok(
        json!({"elapsed_ms": start.elapsed().as_secs_f64() * 1000.0, "busy": busy,
        "wal_frames": frames, "checkpointed_frames": checkpointed, "page_size": page_size,
        "logical_checkpoint_bytes": (checkpointed >= 0).then(|| checkpointed as u64 * page_size as u64),
        "physical_checkpoint_bytes": null}),
    )
}

async fn phase(
    library: Arc<EmbeddedLibrary>,
    source_id: SourceId,
    source: Arc<Source>,
    name: &str,
    mode: ScanMode,
    cancel: bool,
) -> Result<Value> {
    let ctx = AuthContext::embedded();
    let mut events = library
        .subscribe(
            &ctx,
            SubscribeRequest {
                topics: vec![EventTopic::Jobs],
            },
        )
        .await?;
    let before = source.counts();
    let sql_before = library.scan_sql_metrics();
    let host_before = host();
    let wal_path = source.config.data_dir.join("library.db-wal");
    let wal_before = file_bytes(&wal_path);
    let mut wal_peak = wal_before;
    let start = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = StopGuard(stop.clone());
    eprintln!("starting {name}");
    let browse_samples = Arc::new(Mutex::new(Vec::new()));
    let browse_failures = Arc::new(AtomicU64::new(0));
    let writer_samples = Arc::new(Mutex::new(Vec::new()));
    let browse = {
        let library = library.clone();
        let stop = stop.clone();
        let samples = browse_samples.clone();
        let source = source.clone();
        let failures = browse_failures.clone();
        let writer_samples = writer_samples.clone();
        tokio::spawn(async move {
            let mut sample_index = 0u64;
            let mut favorite = false;
            while !stop.load(Ordering::Relaxed) {
                let at = Instant::now();
                let disk = source.data_disk.clone();
                let delay = source.config.seek_us;
                let transfer = source.config.data_transfer_mib();
                if tokio::task::spawn_blocking(move || disk.request(delay, 4096, transfer))
                    .await
                    .is_err()
                {
                    failures.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                let page = match library
                    .query(&AuthContext::embedded(), QueryRequest::default())
                    .await
                {
                    Ok(page) => page,
                    Err(_) => {
                        failures.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                };
                samples
                    .lock()
                    .unwrap()
                    .push(at.elapsed().as_secs_f64() * 1000.0);
                if source.config.foreground_writes && sample_index.is_multiple_of(10) {
                    if let Some(asset) = page.items.first() {
                        favorite = !favorite;
                        let at = Instant::now();
                        if library
                            .set_favorite(
                                &AuthContext::embedded(),
                                FavoriteRequest {
                                    asset: asset.id,
                                    favorite,
                                },
                            )
                            .await
                            .is_err()
                        {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                        writer_samples
                            .lock()
                            .unwrap()
                            .push(at.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                sample_index += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let tenant_samples = Arc::new(Mutex::new(Vec::new()));
    let tenant_path = source.config.data_dir.join("co-tenant.bin");
    let tenant = {
        let stop = stop.clone();
        let samples = tenant_samples.clone();
        let source = source.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut input = std::fs::File::open(tenant_path)?;
            let mut bytes = [0; 4096];
            while !stop.load(Ordering::Relaxed) {
                use std::io::{Seek, SeekFrom};
                evict_co_tenant(&input, &source.config.co_tenant_cache)?;
                let at = Instant::now();
                source.data_disk.request(
                    source.config.seek_us,
                    4096,
                    source.config.data_transfer_mib(),
                );
                input.seek(SeekFrom::Start(0))?;
                input.read_exact(&mut bytes)?;
                samples
                    .lock()
                    .unwrap()
                    .push(at.elapsed().as_secs_f64() * 1000.0);
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })
    };
    let job = library
        .submit_scan_with_source(
            &ctx,
            ScanRequest {
                sources: vec![source_id],
                mode,
            },
            source.clone(),
        )
        .await?;
    let mut first_progress_ms = None;
    let mut progress_events = 0u64;
    let mut stream_lagged = false;
    let mut cancel_requested = None;
    let mut last_status;
    let mut peak_rss = host_before.rss_bytes;
    let mut timed_out = false;
    let mut yield_reasons = std::collections::BTreeSet::new();
    let mut last_policy_sample = Instant::now();
    let scan_elapsed;
    loop {
        if start.elapsed() > Duration::from_secs(source.config.timeout_secs) {
            timed_out = true;
            cancel_requested.get_or_insert_with(Instant::now);
            library.cancel_job(&ctx, &job).await?;
        }
        if cancel
            && cancel_requested.is_none()
            && start.elapsed() >= Duration::from_millis(source.config.cancel_after_ms)
        {
            cancel_requested = Some(Instant::now());
            library.cancel_job(&ctx, &job).await?;
        }
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(20), events.next()).await
        {
            match event {
                LibraryEvent::JobProgress(status) if status.id == job => {
                    if status.progress.done > 0 {
                        first_progress_ms.get_or_insert(start.elapsed().as_secs_f64() * 1000.0);
                    }
                    progress_events += 1;
                }
                LibraryEvent::StreamLagged => stream_lagged = true,
                _ => {}
            }
        }
        wal_peak = wal_peak.max(file_bytes(&wal_path));
        if last_policy_sample.elapsed() >= Duration::from_millis(250) {
            for budget in library.storage_usage().await?.io_budgets {
                yield_reasons.insert(budget.yield_reason);
            }
            last_policy_sample = Instant::now();
        }
        if let Some(rss) = host().rss_bytes {
            peak_rss = Some(peak_rss.unwrap_or(0).max(rss));
        }
        last_status = library.get_job(&ctx, &job).await?;
        if matches!(
            last_status.state,
            JobState::Done | JobState::Failed | JobState::Cancelled
        ) {
            scan_elapsed = start.elapsed();
            // A terminal persisted row can become visible before its queued progress events
            // are consumed. Drain the available queue without waiting for later background work.
            while let Some(Some(event)) = events.next().now_or_never() {
                match event {
                    LibraryEvent::JobProgress(status) if status.id == job => {
                        if status.progress.done > 0 {
                            first_progress_ms.get_or_insert(start.elapsed().as_secs_f64() * 1000.0);
                        }
                        progress_events += 1;
                    }
                    LibraryEvent::StreamLagged => stream_lagged = true,
                    _ => {}
                }
            }
            break;
        }
    }
    let mut cancellation_quiescence_ms = None;
    if let Some(requested) = cancel_requested {
        while source.active.load(Ordering::Relaxed) != 0 {
            if requested.elapsed() > Duration::from_secs(30) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if source.active.load(Ordering::Relaxed) == 0 {
            cancellation_quiescence_ms = Some(requested.elapsed().as_secs_f64() * 1000.0);
        }
    }
    stop.store(true, Ordering::Relaxed);
    browse.await?;
    tenant.await??;
    let host_after = host();
    let after = source.counts().delta(before);
    let usage = library.storage_usage().await?;
    let browse_latency = latency(&browse_samples.lock().unwrap());
    let tenant_latency = latency(&tenant_samples.lock().unwrap());
    let writer_latency = latency(&writer_samples.lock().unwrap());
    let sql_after = library.scan_sql_metrics();
    let quick = serde_json::to_value(mode)? == json!("quick");
    let mut failures = Vec::new();
    if timed_out {
        failures.push("phase exceeded timeout and was cancelled".into());
    }
    if !cancel && last_status.state != JobState::Done {
        failures.push(format!(
            "scan ended {:?}: {:?}",
            last_status.state, last_status.error
        ));
    }
    if cancel
        && (cancel_requested.is_none()
            || last_status.state != JobState::Cancelled
            || cancellation_quiescence_ms.is_none())
    {
        failures.push(if cancel_requested.is_none() && last_status.state == JobState::Done {
            "cancellation inconclusive: scan completed before the configured deadline; use a larger fixture or earlier cancellation".into()
        } else {
            "cancellation acceptance failed: require a requested cancellation, Cancelled job and source callback quiescence within 30 seconds".into()
        });
    }
    if quick
        && (after.fetch_opens != 0
            || after.source_payload_bytes != 0
            || after.scratch_written_bytes != 0)
    {
        failures.push("quick scan opened/materialized payload bytes".into());
    }
    for (participant, summary) in [("browse", &browse_latency), ("co_tenant", &tenant_latency)] {
        if summary["p95_ms"]
            .as_f64()
            .is_some_and(|value| value > source.config.foreground_budget_ms)
        {
            failures.push(format!(
                "{participant} p95 exceeded {} ms",
                source.config.foreground_budget_ms
            ));
        }
    }
    if writer_latency["p95_ms"]
        .as_f64()
        .is_some_and(|value| value > source.config.foreground_budget_ms)
    {
        failures.push("foreground writer p95 exceeded the configured latency budget".into());
    }
    if quick
        && source
            .config
            .quick_budget_ms
            .is_some_and(|budget| scan_elapsed.as_secs_f64() * 1000.0 > budget)
    {
        failures.push("quick duration exceeded the explicitly configured budget".into());
    }
    if browse_failures.load(Ordering::Relaxed) > 0 {
        failures.push("foreground browse failed".into());
    }
    let checkpoint_before = host();
    let checkpoint = checkpoint(&source.config.data_dir.join("library.db"))?;
    let checkpoint_after = host();
    eprintln!(
        "finished {name}: {:.1} ms",
        scan_elapsed.as_secs_f64() * 1000.0
    );
    Ok(json!({"phase": name, "mode": mode, "job": last_status,
        "cache_state": if source.warm.load(Ordering::Relaxed) { "warm_model" } else { "cold_metadata_model" },
        "elapsed_ms": scan_elapsed.as_secs_f64() * 1000.0, "first_observed_committed_progress_ms": first_progress_ms,
        "progress_events": progress_events, "stream_lagged": stream_lagged, "source": after,
        "scratch_read_bytes": null, "sql_read_statements": null,
        "sql_writer_statements": sql_after.writer_statements.saturating_sub(sql_before.writer_statements),
        "asset_rows_updated": sql_after.asset_rows_updated.saturating_sub(sql_before.asset_rows_updated),
        "sql_trigger_statements": sql_after.trigger_statements.saturating_sub(sql_before.trigger_statements),
        "sql_commit_statements": sql_after.writer_commits.saturating_sub(sql_before.writer_commits),
        "sql_writer_elapsed_ms": sql_after.writer_elapsed_ns.saturating_sub(sql_before.writer_elapsed_ns) as f64 / 1_000_000.0,
        "wal_before_bytes": wal_before, "wal_peak_bytes": wal_peak, "wal_end_bytes": file_bytes(&wal_path),
        "wal_growth_bytes": wal_peak.saturating_sub(wal_before), "checkpoint": checkpoint,
        "checkpoint_process_io": io_delta(&checkpoint_after, &checkpoint_before),
        "process_io": io_delta(&host_after, &host_before), "host_before": host_before, "host_after": host_after,
        "peak_rss_bytes": peak_rss, "browse": browse_latency, "co_tenant": tenant_latency,
        "co_tenant_cache_mode": source.config.co_tenant_cache,
        "co_tenant_cache_limit": "cached reuses one OS page; evict requests Linux page-cache eviction before each read, without bypassing filesystem/device caches or proving physical I/O",
        "foreground_writer": writer_latency,
        "cancellation_requested": cancel_requested.is_some(), "cancellation_source_quiescence_ms": cancellation_quiescence_ms,
        "cancellation_measured": cancel_requested.is_some() && last_status.state == JobState::Cancelled,
        "yield_reasons_observed": yield_reasons, "storage_usage": usage, "assertion_failures": failures}))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let Some(config) = Config::parse()? else {
        return Ok(());
    };
    std::fs::create_dir_all(&config.data_dir)?;
    if config.backend == "generated" {
        std::fs::create_dir_all(&config.source_root)?;
    }
    std::fs::write(config.data_dir.join("co-tenant.bin"), [0; 4096])?;
    prepare_co_tenant(
        &config.data_dir.join("co-tenant.bin"),
        &config.co_tenant_cache,
    )?;
    let source_resource = "sweep-source";
    let data_resource = if config.placement == "shared" {
        source_resource
    } else {
        "sweep-data"
    };
    let options = ResourceOptions {
        io: IoOptions {
            max_mib_per_sec: Some(config.io_mib),
            concurrency: Some(1),
            storage: vec![
                StorageOverride {
                    path: config.source_root.clone(),
                    resource: source_resource.into(),
                    kind: StorageKind::Rotational,
                    max_mib_per_sec: Some(config.io_mib),
                    concurrency: Some(1),
                },
                StorageOverride {
                    path: config.data_dir.clone(),
                    resource: data_resource.into(),
                    kind: if config.placement == "shared" {
                        StorageKind::Rotational
                    } else {
                        StorageKind::SolidState
                    },
                    max_mib_per_sec: Some(if config.placement == "shared" {
                        config.io_mib
                    } else {
                        config.data_mib
                    }),
                    concurrency: Some(1),
                },
            ],
        },
        background_threads: Some(2),
        min_free_memory_mb: if config.pressure_policy == "disabled" {
            Some(0)
        } else {
            None
        },
        max_io_stall_pct: if config.pressure_policy == "disabled" {
            Some(100.0)
        } else {
            None
        },
        max_load_per_cpu: if config.pressure_policy == "disabled" {
            Some(f64::INFINITY)
        } else {
            None
        },
        defer_ingest: true,
    };
    let mut options = options;
    if config.topology == "detected" {
        options.io.storage.clear();
    }
    let library = Arc::new(EmbeddedLibrary::open_with(&config.data_dir, options).await?);
    library.wait_for_cache_inventory().await?;
    library.enable_scan_sql_metrics();
    let source_id = library
        .add_source(
            &AuthContext::embedded(),
            AddSource {
                kind: SourceKind::LocalFs,
                uri: config.source_root.to_string_lossy().into_owned(),
                name: Some("manual scan sweep".into()),
                options: SourceOptions::default(),
            },
        )
        .await?;
    let native = if config.backend == "local" {
        Some(Arc::from(dam_sources::open_source(
            &SourceConnection::LocalFs {
                root: config.source_root.to_string_lossy().into_owned(),
            },
            &library.scratch_dir(),
        )?))
    } else {
        None
    };
    let source_disk = Arc::new(Disk::default());
    let data_disk = if config.placement == "shared" {
        source_disk.clone()
    } else {
        Arc::new(Disk::default())
    };
    let source = Arc::new(Source {
        config: config.clone(),
        native,
        source_disk,
        data_disk,
        counts: Mutex::new(Counts::default()),
        active: AtomicU64::new(0),
        changed: AtomicBool::new(false),
        warm: AtomicBool::new(false),
    });
    let quick: ScanMode = serde_json::from_value(json!("quick"))?;
    let mut reports = Vec::new();
    for (name, mode, warm) in [
        ("first_discovery_quick", quick, false),
        ("unchanged_quick_cold", quick, false),
        ("unchanged_quick_warm", quick, true),
        ("first_full_verification", ScanMode::Full, true),
    ] {
        source.warm.store(warm, Ordering::Relaxed);
        reports.push(
            phase(
                library.clone(),
                source_id,
                source.clone(),
                name,
                mode,
                false,
            )
            .await?,
        );
    }
    if config.pause_before_change {
        eprintln!("Full baseline is complete. Apply changes/removals to the fixture source, then press Enter.");
        let mut acknowledgement = String::new();
        std::io::stdin().read_line(&mut acknowledgement)?;
    }
    if config.background {
        library.start_background_pipeline(Arc::new(Background));
    }
    if config.backend == "generated" {
        source.changed.store(true, Ordering::Relaxed);
    }
    for (name, mode) in [
        ("few_change_quick", quick),
        ("verify_changes_full", ScanMode::Full),
        ("unchanged_delta", ScanMode::Delta),
        ("unchanged_full", ScanMode::Full),
    ] {
        reports.push(
            phase(
                library.clone(),
                source_id,
                source.clone(),
                name,
                mode,
                false,
            )
            .await?,
        );
    }
    reports.push(
        phase(
            library.clone(),
            source_id,
            source.clone(),
            "cancel_full",
            ScanMode::Full,
            true,
        )
        .await?,
    );
    let revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    let working_tree_dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty());
    let evidence = json!({"schema_version": 1, "revision": revision, "working_tree_dirty": working_tree_dirty,
        "package_version": env!("CARGO_PKG_VERSION"), "config": config,
        "scope": "production LibraryService jobs, scan coordinator, source callbacks, store batches and resource scheduler",
        "limitations": ["Injected delays model request costs; wall-clock timing still depends on this host.",
            "Cold metadata is modeled; operating-system page caches are not dropped.",
            "Local counters describe logical source admissions and materialized bytes, not physical disk reads.",
            "Scratch reads and read-pool SQL counts require external tracing and are null; writer SQL/commits are measured on the catalog writer.",
            "WAL length/growth is not cumulative WAL writes; checkpoint bytes are logical pages.",
            "Process I/O includes scan, SQLite, browsing, cache inventory and co-tenant work.",
            "Automatic enrichment is deferred so discovery measurements do not include an unrelated verifier; Full phases measure verification explicitly.",
            "Pressure gates follow the recorded policy; generated profiles disable host load/memory/PSI gates while retaining I/O budgets.",
            "Local few-change phases do not mutate the real source; arrange changes externally."],
        "profiles": reports});
    std::fs::write(&source.config.output, serde_json::to_vec_pretty(&evidence)?)?;
    println!("{}", source.config.output.display());
    if evidence["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|profile| !profile["assertion_failures"].as_array().unwrap().is_empty())
    {
        return Err("scan sweep assertions failed; inspect the JSON evidence".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    static PROFILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn fixture_config(root: &Path) -> Config {
        Config {
            paths: 100,
            tiny_bytes: 32,
            changed: 3,
            removed: 5,
            rejected_percent: 25,
            absent_timestamp_percent: 0,
            width: 10,
            depth: 3,
            large_bytes: 0,
            metadata_us: 0,
            cold_metadata_us: 0,
            listing_us: 0,
            seek_us: 0,
            round_trip_us: 0,
            transfer_mib: 0,
            io_mib: 1024,
            data_mib: 1024,
            cancel_after_ms: 5,
            timeout_secs: 60,
            foreground_budget_ms: 1_000_000.0,
            quick_budget_ms: None,
            placement: "shared".into(),
            backend: "generated".into(),
            data_dir: root.join("data"),
            source_root: root.join("source"),
            output: root.join("evidence.json"),
            background: false,
            pause_before_change: false,
            pressure_policy: "disabled".into(),
            foreground_writes: false,
            topology: "declared".into(),
            co_tenant_cache: "cached".into(),
        }
    }

    async fn fixture(config: Config) -> (Arc<EmbeddedLibrary>, SourceId, Arc<Source>) {
        std::fs::create_dir_all(&config.source_root).unwrap();
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::write(config.data_dir.join("co-tenant.bin"), [0; 4096]).unwrap();
        let library = Arc::new(
            EmbeddedLibrary::open_with(
                &config.data_dir,
                ResourceOptions {
                    defer_ingest: true,
                    ..ResourceOptions::ungoverned()
                },
            )
            .await
            .unwrap(),
        );
        library.wait_for_cache_inventory().await.unwrap();
        library.enable_scan_sql_metrics();
        let source_id = library
            .add_source(
                &AuthContext::embedded(),
                AddSource {
                    kind: SourceKind::LocalFs,
                    uri: config.source_root.to_string_lossy().into_owned(),
                    name: Some("instrumented fixture".into()),
                    options: SourceOptions::default(),
                },
            )
            .await
            .unwrap();
        let disk = Arc::new(Disk::default());
        let source = Arc::new(Source {
            config,
            native: None,
            source_disk: disk.clone(),
            data_disk: disk,
            counts: Mutex::new(Counts::default()),
            active: AtomicU64::new(0),
            changed: AtomicBool::new(false),
            warm: AtomicBool::new(true),
        });
        (library, source_id, source)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_quick_and_delta_guards_use_real_source_and_store_paths() {
        let _profile = PROFILE_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let (library, source_id, source) = fixture(fixture_config(root.path())).await;
        let quick: ScanMode = serde_json::from_value(json!("quick")).unwrap();
        for name in ["discovery", "unchanged"] {
            let result = phase(
                library.clone(),
                source_id,
                source.clone(),
                name,
                quick,
                false,
            )
            .await
            .unwrap();
            assert_eq!(result["source"]["fetch_opens"], 0);
            assert_eq!(result["source"]["source_payload_bytes"], 0);
            assert_eq!(result["source"]["metadata_requests"], 75);
            assert_eq!(result["source"]["listed_paths"], 100);
            assert_eq!(
                result["storage_usage"]["asset_count"], 0,
                "discovery does not admit unverified paths"
            );
            assert!(
                result["assertion_failures"].as_array().unwrap().is_empty(),
                "{result}"
            );
        }
        let full = phase(
            library.clone(),
            source_id,
            source.clone(),
            "verification",
            ScanMode::Full,
            false,
        )
        .await
        .unwrap();
        assert_eq!(full["storage_usage"]["asset_count"], 75);
        assert_eq!(full["source"]["fetch_opens"], 75);
        assert_eq!(full["source"]["source_payload_bytes"], 75 * 32);
        assert_eq!(full["source"]["scratch_written_bytes"], 75 * 32);
        assert!(full["first_observed_committed_progress_ms"]
            .as_f64()
            .is_some());
        assert!(full["sql_commit_statements"].as_u64().is_some());
        assert_eq!(full["co_tenant_cache_mode"], "cached");
        let delta = phase(
            library.clone(),
            source_id,
            source.clone(),
            "delta",
            ScanMode::Delta,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            delta["source"]["fetch_opens"], 0,
            "unchanged Delta opened source payloads: {delta}"
        );
        assert_eq!(
            delta["asset_rows_updated"], 0,
            "unchanged Delta must not rewrite catalogue rows"
        );
        source.changed.store(true, Ordering::Relaxed);
        let changed = phase(
            library.clone(),
            source_id,
            source.clone(),
            "changed",
            quick,
            false,
        )
        .await
        .unwrap();
        assert_eq!(changed["source"]["fetch_opens"], 0);
        assert_eq!(changed["source"]["metadata_requests"], 70);
        let verified = phase(
            library,
            source_id,
            source,
            "changed verification",
            ScanMode::Full,
            false,
        )
        .await
        .unwrap();
        assert_eq!(verified["source"]["source_payload_bytes"], 70 * 32 + 3);
        assert!(
            verified["assertion_failures"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{verified}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_latency_cancellation_waits_for_source_quiescence() {
        let _profile = PROFILE_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let mut config = fixture_config(root.path());
        config.metadata_us = 40_000;
        let (library, source_id, source) = fixture(config).await;
        let result = phase(
            library,
            source_id,
            source.clone(),
            "cancel",
            ScanMode::Full,
            true,
        )
        .await
        .unwrap();
        assert_eq!(result["job"]["state"], "cancelled");
        assert_eq!(result["cancellation_measured"], true);
        assert!(result["cancellation_source_quiescence_ms"]
            .as_f64()
            .is_some());
        assert_eq!(source.active.load(Ordering::Relaxed), 0);
        assert!(
            result["assertion_failures"].as_array().unwrap().is_empty(),
            "{result}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_fixture_does_not_silently_pass_cancellation_acceptance() {
        let _profile = PROFILE_LOCK.lock().await;
        let root = tempfile::tempdir().unwrap();
        let mut config = fixture_config(root.path());
        config.cancel_after_ms = 60_000;
        let (library, source_id, source) = fixture(config).await;
        let result = phase(library, source_id, source, "cancel", ScanMode::Full, true)
            .await
            .unwrap();
        assert_eq!(result["cancellation_measured"], false);
        assert!(
            result["assertion_failures"]
                .as_array()
                .unwrap()
                .iter()
                .any(|failure| {
                    failure
                        .as_str()
                        .is_some_and(|text| text.starts_with("cancellation inconclusive:"))
                }),
            "{result}"
        );
    }

    #[test]
    fn foreground_data_transfer_matches_placement_and_disabled_injection() {
        let mut config = fixture_config(Path::new("unused"));
        config.transfer_mib = 32;
        config.data_mib = 128;
        assert_eq!(config.data_transfer_mib(), 32);
        config.placement = "separate".into();
        assert_eq!(config.data_transfer_mib(), 128);
        config.transfer_mib = 0;
        assert_eq!(config.data_transfer_mib(), 0);
    }

    #[test]
    fn co_tenant_eviction_is_explicit_and_preserves_fixture_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("co-tenant.bin");
        std::fs::write(&path, [7; 4096]).unwrap();
        #[cfg(target_os = "linux")]
        {
            prepare_co_tenant(&path, "evict").unwrap();
            let mut file = std::fs::File::open(&path).unwrap();
            evict_co_tenant(&file, "evict").unwrap();
            let mut bytes = [0; 4096];
            file.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, [7; 4096]);
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(
                prepare_co_tenant(&path, "evict").unwrap_err().kind(),
                std::io::ErrorKind::Unsupported
            );
            let file = std::fs::File::open(&path).unwrap();
            assert_eq!(
                evict_co_tenant(&file, "evict").unwrap_err().kind(),
                std::io::ErrorKind::Unsupported
            );
        }
    }
}
