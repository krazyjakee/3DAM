//! Best-effort storage topology and bounded background I/O (issue #187).
//! No benchmarks, privileged probes, or writes to source trees. Device permits are acquired in
//! sorted order with rollback, so overlapping source/scratch pairs cannot deadlock.

use super::{Cancellation, Governor};
use dam_api::admin::IoBudget;
use dam_api::LibError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
mod scenario;

pub(crate) const CHUNK: usize = 256 * 1024;
const TICK: Duration = Duration::from_millis(25);
const SAMPLE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default)]
pub struct IoOptions {
    /// Per-device background read + write ceiling, MiB/s. Zero uses the hardware default.
    pub max_mib_per_sec: Option<u64>,
    /// Per-device concurrency ceiling. An explicit cap overrides the hardware default.
    pub concurrency: Option<usize>,
    /// Explicit backing-resource identities for topology hidden by containers/network mounts.
    pub storage: Vec<StorageOverride>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct StorageOverride {
    pub path: PathBuf,
    pub resource: String,
    #[serde(default)]
    pub kind: StorageKind,
    pub max_mib_per_sec: Option<u64>,
    pub concurrency: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Rotational,
    SolidState,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Rotational,
    SolidState,
    Unknown,
}

impl Kind {
    fn from_override(kind: StorageKind) -> Self {
        match kind {
            StorageKind::Rotational => Self::Rotational,
            StorageKind::SolidState => Self::SolidState,
            StorageKind::Unknown => Self::Unknown,
        }
    }
    fn defaults(self) -> (u64, usize) {
        match self {
            Self::Rotational => (8 * 1024 * 1024, 1),
            Self::SolidState => (128 * 1024 * 1024, 2),
            Self::Unknown => (4 * 1024 * 1024, 1),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Rotational => "rotational",
            Self::SolidState => "solid_state",
            Self::Unknown => "unknown_or_network",
        }
    }
}

struct Device {
    id: String,
    kind: Kind,
    sys: Option<PathBuf>,
    limit: u64,
    concurrency: usize,
    state: Mutex<DeviceState>,
}

struct DeviceState {
    probe_checked: Option<Instant>,
    active: usize,
    deferred: usize,
    next: Instant,
    rate: u64,
    bytes: u64,
    sample: Option<(Instant, DiskStats)>,
    throughput: Option<f64>,
    queue: Option<f64>,
    latency: Option<f64>,
    reason: &'static str,
}

#[derive(Clone, Copy)]
struct DiskStats {
    ops: u64,
    sectors: u64,
    latency_ms: u64,
    queue_ms: u64,
}

fn disk_stats(text: &str) -> Option<DiskStats> {
    let v: Vec<u64> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    Some(DiskStats {
        ops: v.first()?.saturating_add(*v.get(4)?),
        sectors: v.get(2)?.saturating_add(*v.get(6)?),
        latency_ms: v.get(3)?.saturating_add(*v.get(7)?),
        queue_ms: *v.get(10)?,
    })
}

impl Device {
    fn new(id: String, kind: Kind, sys: Option<PathBuf>, options: IoOptions) -> Self {
        let (default_rate, default_concurrency) = kind.defaults();
        let limit = options
            .max_mib_per_sec
            .filter(|v| *v > 0)
            .map(|v| v.saturating_mul(1024 * 1024))
            .unwrap_or(default_rate);
        Self {
            id,
            kind,
            sys,
            limit,
            concurrency: options.concurrency.unwrap_or(default_concurrency).max(1),
            state: Mutex::new(DeviceState {
                probe_checked: None,
                active: 0,
                deferred: 0,
                next: Instant::now(),
                rate: limit,
                bytes: 0,
                sample: None,
                throughput: None,
                queue: None,
                latency: None,
                reason: "ready",
            }),
        }
    }

    fn observe(&self, state: &mut DeviceState, now: Instant) {
        if state
            .probe_checked
            .is_some_and(|at| now.duration_since(at) < SAMPLE)
        {
            return;
        }
        state.probe_checked = Some(now);
        let Some(stats) = self
            .sys
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p.join("stat")).ok())
            .as_deref()
            .and_then(disk_stats)
        else {
            state.sample = None;
            state.throughput = None;
            state.queue = None;
            state.latency = None;
            state.rate = self.limit;
            return;
        };
        if let Some((at, previous)) = state.sample {
            let seconds = now.duration_since(at).as_secs_f64().max(0.001);
            let ops = stats.ops.saturating_sub(previous.ops);
            state.throughput =
                Some(stats.sectors.saturating_sub(previous.sectors) as f64 * 512.0 / seconds);
            state.queue =
                Some(stats.queue_ms.saturating_sub(previous.queue_ms) as f64 / (seconds * 1000.0));
            state.latency = (ops > 0)
                .then(|| stats.latency_ms.saturating_sub(previous.latency_ms) as f64 / ops as f64);
            let (queue_limit, latency_limit) = if self.kind == Kind::SolidState {
                (2.0, 5.0)
            } else {
                (0.5, 20.0)
            };
            if state.queue.is_some_and(|q| q > queue_limit)
                || state.latency.is_some_and(|ms| ms > latency_limit)
            {
                state.rate = (state.rate / 2).max(64 * 1024).min(self.limit);
            } else {
                state.rate = state.rate.saturating_add(self.limit / 16).min(self.limit);
            }
        }
        state.sample = Some((now, stats));
    }
}

pub(crate) struct Scheduler {
    options: IoOptions,
    devices: Mutex<BTreeMap<String, Arc<Device>>>,
    paths: Mutex<BTreeMap<PathBuf, Vec<Arc<Device>>>>,
}

impl Scheduler {
    pub(crate) fn new(options: IoOptions) -> Self {
        Self {
            options,
            devices: Mutex::new(BTreeMap::new()),
            paths: Mutex::new(BTreeMap::new()),
        }
    }

    fn resolve(&self, path: Option<&Path>) -> Vec<Arc<Device>> {
        if let Some(path) = path {
            if let Some(devices) = self.paths.lock().unwrap().get(path).cloned() {
                return devices;
            }
        }
        let override_ = path.and_then(|path| {
            self.options
                .storage
                .iter()
                .filter(|entry| path.starts_with(&entry.path))
                .max_by_key(|entry| entry.path.components().count())
        });
        let detected = if let Some(entry) = override_ {
            let mut kind = Kind::from_override(entry.kind);
            for other in self
                .options
                .storage
                .iter()
                .filter(|other| other.resource == entry.resource)
            {
                let other = Kind::from_override(other.kind);
                if other == Kind::Unknown || kind == Kind::Unknown {
                    kind = Kind::Unknown;
                } else if other == Kind::Rotational {
                    kind = Kind::Rotational;
                }
            }
            vec![(format!("override:{}", entry.resource), kind, None)]
        } else {
            path.map(topology).unwrap_or_default()
        };
        let detected = if detected.is_empty() {
            vec![("unknown".to_string(), Kind::Unknown, None)]
        } else {
            detected
        };
        let mut registry = self.devices.lock().unwrap();
        let devices: Vec<_> = detected.into_iter().map(|(id, kind, sys)| {
            registry.entry(id.clone()).or_insert_with(|| {
                let options = self.override_options(override_);
                let device = Arc::new(Device::new(id, kind, sys, options));
                tracing::info!(device = %device.id, kind = kind.name(), bytes_per_sec = device.limit, concurrency = device.concurrency,
                    fallback = kind == Kind::Unknown, "background storage budget resolved");
                device
            }).clone()
        }).collect();
        drop(registry);
        if let Some(path) = path {
            let mut paths = self.paths.lock().unwrap();
            // Assets and temporary files can have millions of distinct paths. Topology remains
            // best effort, but its cache must not grow in proportion to the catalog.
            if paths.len() >= 4096 {
                paths.clear();
            }
            paths.insert(path.to_path_buf(), devices.clone());
        }
        devices
    }

    fn override_options(&self, override_: Option<&StorageOverride>) -> IoOptions {
        let mut options = IoOptions {
            max_mib_per_sec: self.options.max_mib_per_sec,
            concurrency: self.options.concurrency,
            storage: Vec::new(),
        };
        // Shared identities take their strictest cap, independent of which root is accessed first.
        if let Some(entry) = override_ {
            let mut rate = u64::MAX;
            let mut concurrency = usize::MAX;
            for other in self
                .options
                .storage
                .iter()
                .filter(|other| other.resource == entry.resource)
            {
                let (default_rate, default_concurrency) =
                    Kind::from_override(other.kind).defaults();
                let root_rate = other
                    .max_mib_per_sec
                    .or(self.options.max_mib_per_sec)
                    .filter(|n| *n > 0)
                    .unwrap_or(default_rate / (1024 * 1024));
                let root_concurrency = other
                    .concurrency
                    .or(self.options.concurrency)
                    .unwrap_or(default_concurrency)
                    .max(1);
                rate = rate.min(root_rate);
                concurrency = concurrency.min(root_concurrency);
            }
            options.max_mib_per_sec = Some(rate);
            options.concurrency = Some(concurrency);
        }
        options
    }

    pub(crate) fn acquire<'a>(
        &self,
        governor: &'a Governor,
        paths: &[Option<&Path>],
        cancel: &'a dyn Cancellation,
    ) -> Result<Work<'a>, LibError> {
        let mut unique = BTreeMap::new();
        for path in paths {
            for device in self.resolve(*path) {
                unique.insert(device.id.clone(), device);
            }
        }
        let devices: Vec<_> = unique.into_values().collect();
        for device in &devices {
            device.state.lock().unwrap().deferred += 1;
        }
        loop {
            if cancel.cancelled() {
                for device in &devices {
                    device.state.lock().unwrap().deferred -= 1;
                }
                return Err(cancelled());
            }
            if let Some(reason) = governor.pressure_reason() {
                for device in &devices {
                    device.state.lock().unwrap().reason = reason;
                }
                std::thread::sleep(TICK);
                continue;
            }
            let mut acquired = 0;
            for device in &devices {
                let mut state = device.state.lock().unwrap();
                if state.active >= device.concurrency {
                    state.reason = "concurrency";
                    break;
                }
                state.active += 1;
                acquired += 1;
            }
            if acquired == devices.len() {
                for device in &devices {
                    device.state.lock().unwrap().deferred -= 1;
                }
                return Ok(Work {
                    devices,
                    governor,
                    cancel,
                });
            }
            for device in &devices[..acquired] {
                device.state.lock().unwrap().active -= 1;
            }
            std::thread::sleep(TICK);
        }
    }

    pub(crate) fn diagnostics(&self) -> Vec<IoBudget> {
        self.devices
            .lock()
            .unwrap()
            .values()
            .map(|device| {
                let state = device.state.lock().unwrap();
                IoBudget {
                    device: device.id.clone(),
                    kind: device.kind.name().to_string(),
                    limit_bytes_per_sec: device.limit,
                    current_bytes_per_sec: state.rate,
                    concurrency: device.concurrency,
                    active: state.active,
                    deferred: state.deferred,
                    accounted_bytes: state.bytes,
                    observed_bytes_per_sec: state.throughput,
                    queue_depth: state.queue,
                    request_latency_ms: state.latency,
                    yield_reason: state.reason.to_string(),
                    probes_available: state.sample.is_some(),
                }
            })
            .collect()
    }
}

pub(crate) struct Work<'a> {
    devices: Vec<Arc<Device>>,
    governor: &'a Governor,
    cancel: &'a dyn Cancellation,
}

impl Work<'_> {
    /// No future reservation is queued: cancellation never leaves bandwidth debt behind.
    /// Charging reads and writes separately means a same-disk source→scratch copy pays twice.
    pub(crate) fn pace(&self, bytes: u64) -> Result<(), LibError> {
        let mut remaining = bytes;
        while remaining > 0 {
            let chunk = remaining.min(CHUNK as u64);
            loop {
                if self.cancel.cancelled() {
                    return Err(cancelled());
                }
                let pressure = self.governor.pressure_reason();
                let now = Instant::now();
                let mut states: Vec<_> = self
                    .devices
                    .iter()
                    .map(|d| d.state.lock().unwrap())
                    .collect();
                let mut wait = false;
                for (device, state) in self.devices.iter().zip(&mut states) {
                    device.observe(state, now);
                    state.reason = if let Some(reason) = pressure {
                        reason
                    } else if now < state.next {
                        "bandwidth"
                    } else if state.rate < device.limit {
                        "device_latency_or_queue"
                    } else {
                        "ready"
                    };
                    wait |= pressure.is_some() || now < state.next;
                }
                if !wait {
                    for state in &mut states {
                        state.next =
                            now + Duration::from_secs_f64(chunk as f64 / state.rate as f64);
                        state.bytes = state.bytes.saturating_add(chunk);
                    }
                    break;
                }
                drop(states);
                std::thread::sleep(TICK);
            }
            remaining -= chunk;
        }
        Ok(())
    }
}

impl Drop for Work<'_> {
    fn drop(&mut self) {
        for device in &self.devices {
            let mut state = device.state.lock().unwrap();
            state.active -= 1;
            if state.active == 0 && state.deferred == 0 {
                state.reason = "ready";
            }
        }
    }
}

fn cancelled() -> LibError {
    LibError::Internal("background I/O cancelled".to_string())
}

fn topology(path: &Path) -> Vec<(String, Kind, Option<PathBuf>)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(metadata) = path.ancestors().find_map(|p| std::fs::metadata(p).ok()) else {
            return Vec::new();
        };
        let dev = metadata.dev();
        let sys = PathBuf::from(format!(
            "/sys/dev/block/{}:{}",
            libc::major(dev),
            libc::minor(dev)
        ));
        let mut leaves = BTreeMap::new();
        physical_devices(&sys, 0, &mut leaves);
        leaves.into_values().collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn physical_devices(
    sys: &Path,
    depth: usize,
    leaves: &mut BTreeMap<String, (String, Kind, Option<PathBuf>)>,
) {
    if depth > 16 {
        return;
    }
    let Ok(mut sys) = sys.canonicalize() else {
        return;
    };
    if sys.join("partition").exists() {
        let Some(parent) = sys.parent() else {
            return;
        };
        sys = parent.to_path_buf();
    }
    let slaves: Vec<_> = std::fs::read_dir(sys.join("slaves"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    if !slaves.is_empty() {
        for slave in slaves {
            physical_devices(&slave, depth + 1, leaves);
        }
        return;
    }
    let Ok(id) = std::fs::read_to_string(sys.join("dev")) else {
        return;
    };
    let id = id.trim().to_string();
    let kind = match std::fs::read_to_string(sys.join("queue/rotational"))
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("1") => Kind::Rotational,
        Some("0") => Kind::SolidState,
        _ => Kind::Unknown,
    };
    leaves.insert(id.clone(), (id, kind, Some(sys)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_include_reads_and_writes() {
        let stats = disk_stats("10 0 100 20 30 0 200 40 0 0 60").unwrap();
        assert_eq!(
            (stats.ops, stats.sectors, stats.latency_ms, stats.queue_ms),
            (40, 300, 60, 60)
        );
        assert!(disk_stats("broken").is_none());
    }

    #[test]
    fn unknown_defaults_and_operator_caps_are_conservative() {
        let unknown = Device::new("x".into(), Kind::Unknown, None, IoOptions::default());
        let fast = Device::new("y".into(), Kind::SolidState, None, IoOptions::default());
        assert_eq!((unknown.limit, unknown.concurrency), (4 * 1024 * 1024, 1));
        assert!(fast.limit > unknown.limit);
        let capped = Device::new(
            "z".into(),
            Kind::SolidState,
            None,
            IoOptions {
                max_mib_per_sec: Some(1),
                concurrency: Some(1),
                ..Default::default()
            },
        );
        assert_eq!((capped.limit, capped.concurrency), (1024 * 1024, 1));
    }

    #[test]
    fn cancellation_releases_permits_and_does_not_queue_bandwidth_debt() {
        let governor = Governor::new(Some(0), Some(100.0));
        let cancel = AtomicBool::new(false);
        let work = governor
            .io
            .acquire(&governor, &[None, None], &cancel)
            .unwrap();
        assert_eq!(work.devices.len(), 1);
        cancel.store(true, Ordering::Relaxed);
        assert!(work.pace(CHUNK as u64).is_err());
        drop(work);
        assert_eq!(governor.io.diagnostics()[0].active, 0);
        assert_eq!(governor.io.diagnostics()[0].accounted_bytes, 0);
    }

    #[test]
    fn queue_feedback_reduces_budget_then_recovers_below_operator_cap() {
        let dir = tempfile::tempdir().unwrap();
        let device = Device::new(
            "test".into(),
            Kind::Rotational,
            Some(dir.path().to_path_buf()),
            IoOptions::default(),
        );
        let start = Instant::now();
        let mut state = device.state.lock().unwrap();
        std::fs::write(dir.path().join("stat"), "0 0 0 0 0 0 0 0 0 0 0").unwrap();
        device.observe(&mut state, start);
        std::fs::write(dir.path().join("stat"), "10 0 1000 500 0 0 0 0 0 0 1000").unwrap();
        device.observe(&mut state, start + SAMPLE);
        assert_eq!(state.rate, device.limit / 2);
        assert_eq!(state.queue, Some(4.0));
        assert_eq!(state.latency, Some(50.0));
        for step in 2..=20 {
            device.observe(&mut state, start + SAMPLE * step);
        }
        assert_eq!(state.rate, device.limit);
        std::fs::remove_file(dir.path().join("stat")).unwrap();
        device.observe(&mut state, start + SAMPLE * 21);
        assert!(state.sample.is_none());
        assert!(state.queue.is_none());
        assert!(state.latency.is_none());
        assert_eq!(state.rate, device.limit);
    }

    #[test]
    fn independent_devices_progress_while_one_device_is_occupied() {
        let mut governor = Governor::new(Some(0), Some(100.0));
        governor.max_load_per_cpu = f64::INFINITY;
        let a = Arc::new(Device::new(
            "a".into(),
            Kind::Rotational,
            None,
            IoOptions::default(),
        ));
        let b = Arc::new(Device::new(
            "b".into(),
            Kind::SolidState,
            None,
            IoOptions::default(),
        ));
        governor
            .io
            .paths
            .lock()
            .unwrap()
            .insert(PathBuf::from("a"), vec![a.clone()]);
        governor
            .io
            .paths
            .lock()
            .unwrap()
            .insert(PathBuf::from("b"), vec![b]);
        let cancel = AtomicBool::new(false);
        let first = governor
            .io
            .acquire(&governor, &[Some(Path::new("a"))], &cancel)
            .unwrap();
        let second = governor
            .io
            .acquire(&governor, &[Some(Path::new("b"))], &cancel)
            .unwrap();
        assert_eq!(first.devices[0].state.lock().unwrap().active, 1);
        assert_eq!(second.devices[0].state.lock().unwrap().active, 1);
        let cancelled = AtomicBool::new(true);
        assert!(governor
            .io
            .acquire(
                &governor,
                &[Some(Path::new("a")), Some(Path::new("b"))],
                &cancelled
            )
            .is_err());
        assert_eq!(a.state.lock().unwrap().active, 1);
    }

    #[test]
    fn overrides_group_hidden_shared_storage_and_separate_independent_roots() {
        let override_ = |path: &str, resource: &str| StorageOverride {
            path: path.into(),
            resource: resource.into(),
            kind: StorageKind::Rotational,
            max_mib_per_sec: Some(2),
            concurrency: Some(1),
        };
        let scheduler = Scheduler::new(IoOptions {
            storage: vec![
                override_("/assets", "shared"),
                override_("/data", "shared"),
                override_("/other", "independent"),
            ],
            ..Default::default()
        });
        let assets = scheduler.resolve(Some(Path::new("/assets/large.obj")));
        let data = scheduler.resolve(Some(Path::new("/data/cache/mesh.dmsh")));
        let other = scheduler.resolve(Some(Path::new("/other/large.obj")));
        assert!(Arc::ptr_eq(&assets[0], &data[0]));
        assert!(!Arc::ptr_eq(&assets[0], &other[0]));
        assert_eq!(assets[0].limit, 2 * 1024 * 1024);
    }

    #[test]
    fn pressure_yields_between_chunks_resumes_and_cancels_without_redoing_work() {
        let mut governor = Governor::new(Some(0), Some(100.0));
        governor.max_load_per_cpu = f64::INFINITY;
        let cancel = AtomicBool::new(false);
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let governor = &governor;
            let cancel = &cancel;
            scope.spawn(move || {
                let work = governor.io.acquire(governor, &[None], cancel).unwrap();
                work.pace(CHUNK as u64).unwrap();
                progress_tx.send(1).unwrap();
                go_rx.recv().unwrap();
                work.pace(CHUNK as u64).unwrap();
                progress_tx.send(2).unwrap();
                go_rx.recv().unwrap();
                assert!(work.pace(CHUNK as u64).is_err());
            });
            assert_eq!(progress_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
            {
                let mut state = governor.state.lock().unwrap();
                state.sampled_at = Some(Instant::now());
                state.pressured = true;
                state.reason = Some("io_stall");
            }
            go_tx.send(()).unwrap();
            assert!(progress_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err());
            {
                let mut state = governor.state.lock().unwrap();
                state.pressured = false;
                state.reason = None;
            }
            assert_eq!(progress_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 2);
            cancel.store(true, Ordering::Relaxed);
            go_tx.send(()).unwrap();
        });
        let diagnostics = governor.io.diagnostics();
        assert_eq!(diagnostics[0].accounted_bytes, (CHUNK * 2) as u64);
        assert_eq!(diagnostics[0].active, 0);
        assert_eq!(diagnostics[0].deferred, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partitions_and_mapper_slaves_share_physical_disk_budget() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk");
        let partition = disk.join("partition");
        let mapper = dir.path().join("mapper");
        std::fs::create_dir_all(disk.join("queue")).unwrap();
        std::fs::create_dir_all(&partition).unwrap();
        std::fs::create_dir_all(mapper.join("slaves")).unwrap();
        std::fs::write(disk.join("dev"), "8:0").unwrap();
        std::fs::write(disk.join("queue/rotational"), "1").unwrap();
        std::fs::write(partition.join("partition"), "1").unwrap();
        symlink(&partition, mapper.join("slaves/sda1")).unwrap();
        let mut leaves = BTreeMap::new();
        physical_devices(&mapper, 0, &mut leaves);
        physical_devices(&partition, 0, &mut leaves);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves["8:0"].1, Kind::Rotational);
    }
}
