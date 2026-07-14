//! Host-resource awareness — the "good neighbour" layer (tech-spec 14 §3.4).
//!
//! 3DAM's background grind (analysis embeddings, thumbnail/preview pre-render) is a guest on the
//! host, not its owner: on a shared box it must never starve co-tenants (a media server, CI
//! runners) or swap the machine to death. Two mechanisms, both engine-level so every role gets
//! them:
//!
//! 1. **Sizing** ([`effective_cpus`], [`background_thread_count`]): the background pool is sized
//!    from the *effective* CPU budget — the cgroup v2 quota when containerised (a `--cpus 2`
//!    container must not spawn 14 workers because the host has 16 cores) — and hard-capped by
//!    default at [`DEFAULT_BG_THREAD_CAP`] so a big host still leaves most cores to its other
//!    tenants. Workers are also reniced (+10, idle I/O class) on Linux so anything else on the
//!    box preempts them.
//! 2. **Pacing** ([`Governor`]): between work items, background loops consult a cheap cached
//!    sample of host memory + load. Under pressure (available memory below the floor, or load
//!    beyond the CPU budget) the loop *pauses* — work resumes when the host recovers. Fail-soft:
//!    on platforms without `/proc` / cgroups every probe returns `None` and the governor never
//!    pauses (the pre-existing behaviour).
//!
//! All probes are best-effort text reads of `/proc` and `/sys/fs/cgroup`; a missing or malformed
//! file simply yields `None`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default hard cap for the background pool. Deliberately modest: the pipeline is a warm-cache
/// optimisation, not the product — leaving cores free *is* the feature on a shared host.
pub const DEFAULT_BG_THREAD_CAP: usize = 4;

/// Re-sample host pressure at most this often — the probes are cheap file reads, but per-item
/// (thousands of assets) would still be noise.
const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// How long a paused loop sleeps between pressure re-checks.
const PAUSE_TICK: Duration = Duration::from_millis(500);

// ── probes ───────────────────────────────────────────────────────────────────

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Parse a cgroup v2 `cpu.max` ("<quota> <period>" or "max <period>") into a whole-CPU count.
fn parse_cpu_max(s: &str) -> Option<usize> {
    let mut it = s.split_whitespace();
    let quota: f64 = it.next()?.parse().ok()?; // "max" fails the parse → None (unlimited)
    let period: f64 = it.next()?.parse().ok()?;
    if period <= 0.0 || quota <= 0.0 {
        return None;
    }
    Some((quota / period).ceil().max(1.0) as usize)
}

/// The container CPU quota, when one is imposed (cgroup v2, then the v1 fallback).
fn cgroup_cpu_quota() -> Option<usize> {
    if let Some(s) = read_trimmed("/sys/fs/cgroup/cpu.max") {
        return parse_cpu_max(&s);
    }
    // cgroup v1: separate quota/period files; quota -1 = unlimited.
    let quota: f64 = read_trimmed("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")?
        .parse()
        .ok()?;
    let period: f64 = read_trimmed("/sys/fs/cgroup/cpu/cpu.cfs_period_us")?
        .parse()
        .ok()?;
    if quota <= 0.0 || period <= 0.0 {
        return None;
    }
    Some((quota / period).ceil().max(1.0) as usize)
}

/// The CPUs actually available to *this* process: the scheduler's view, clamped by any container
/// quota. This is the budget every "how parallel may I be?" decision should start from.
pub fn effective_cpus() -> usize {
    let sched = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    match cgroup_cpu_quota() {
        Some(q) => sched.min(q).max(1),
        None => sched,
    }
}

/// The memory ceiling this process runs under: the cgroup limit when one is imposed, else the
/// host's total. `None` when neither is readable (non-Linux).
pub fn memory_limit() -> Option<u64> {
    let cg = read_trimmed("/sys/fs/cgroup/memory.max")
        .or_else(|| read_trimmed("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
        .and_then(|s| s.parse::<u64>().ok()); // "max" fails the parse → unlimited
    let host = meminfo_kb("MemTotal").map(|kb| kb * 1024);
    match (cg, host) {
        (Some(c), Some(h)) => Some(c.min(h)),
        (c, h) => c.or(h),
    }
}

/// Memory still available before the host (or our cgroup) is in trouble: the smaller of the
/// host's `MemAvailable` and our cgroup headroom. The host-wide number matters even when we are
/// nowhere near our own limit — swapping the *box* to death is exactly the failure mode.
pub fn available_memory() -> Option<u64> {
    let host = meminfo_kb("MemAvailable").map(|kb| kb * 1024);
    let cg_headroom = (|| {
        let limit: u64 = read_trimmed("/sys/fs/cgroup/memory.max")?.parse().ok()?;
        let current: u64 = read_trimmed("/sys/fs/cgroup/memory.current")?
            .parse()
            .ok()?;
        Some(limit.saturating_sub(current))
    })();
    match (host, cg_headroom) {
        (Some(h), Some(c)) => Some(h.min(c)),
        (h, c) => h.or(c),
    }
}

/// One field of `/proc/meminfo`, in kB.
fn meminfo_kb(field: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_kb(&text, field)
}

fn parse_meminfo_kb(text: &str, field: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with(field) && l.as_bytes().get(field.len()) == Some(&b':'))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

/// The 1-minute load average.
pub fn loadavg_1() -> Option<f64> {
    read_trimmed("/proc/loadavg")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Size the bounded background pool: `effective_cpus − 2` (the inspector-priority headroom,
/// tech-spec 14), hard-capped at [`DEFAULT_BG_THREAD_CAP`] unless the operator overrides — and an
/// override is still clamped to the effective CPU budget, never past it.
pub fn background_thread_count(configured: Option<usize>) -> usize {
    let cpus = effective_cpus();
    match configured {
        Some(n) => n.clamp(1, cpus),
        None => cpus.saturating_sub(2).clamp(1, DEFAULT_BG_THREAD_CAP),
    }
}

// ── the governor ─────────────────────────────────────────────────────────────

/// Pressure thresholds + a cached sample. One instance per engine; background loops call
/// [`Governor::pace`] between items.
pub struct Governor {
    /// Pause background work while host available memory is below this floor.
    min_free_bytes: u64,
    /// Pause while the 1-min loadavg exceeds this multiple of the effective CPU budget.
    max_load_per_cpu: f64,
    state: Mutex<GovState>,
}

struct GovState {
    sampled_at: Option<Instant>,
    pressured: bool,
    /// For logging state transitions once, not per item.
    was_pressured: bool,
}

impl Governor {
    /// `min_free_memory_mb = None` picks the default floor: 10% of the memory ceiling, clamped to
    /// [256 MiB, 2 GiB]. On a host where no ceiling is readable the floor is 512 MiB.
    pub fn new(min_free_memory_mb: Option<u64>) -> Governor {
        let min_free_bytes = match min_free_memory_mb {
            Some(mb) => mb * 1024 * 1024,
            None => match memory_limit() {
                Some(total) => (total / 10).clamp(256 * 1024 * 1024, 2 * 1024 * 1024 * 1024),
                None => 512 * 1024 * 1024,
            },
        };
        Governor {
            min_free_bytes,
            max_load_per_cpu: 1.5,
            state: Mutex::new(GovState {
                sampled_at: None,
                pressured: false,
                was_pressured: false,
            }),
        }
    }

    /// The raw decision, injectable for tests.
    fn decide(&self, available: Option<u64>, load1: Option<f64>, cpus: usize) -> bool {
        let mem_pressure = available.is_some_and(|a| a < self.min_free_bytes);
        let load_pressure = load1.is_some_and(|l| l > self.max_load_per_cpu * cpus as f64);
        mem_pressure || load_pressure
    }

    /// Is the host under pressure right now? Samples at most every [`SAMPLE_EVERY`]; logs each
    /// transition (info level) so an operator can see the engine yielding.
    pub fn pressured(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.sampled_at.is_some_and(|at| at.elapsed() < SAMPLE_EVERY) {
            return st.pressured;
        }
        let available = available_memory();
        let load = loadavg_1();
        let cpus = effective_cpus();
        st.pressured = self.decide(available, load, cpus);
        st.sampled_at = Some(Instant::now());
        if st.pressured != st.was_pressured {
            st.was_pressured = st.pressured;
            if st.pressured {
                tracing::info!(
                    available_mb = available.map(|a| a / (1024 * 1024)),
                    load1 = load,
                    cpus,
                    "host under pressure — background work paused"
                );
            } else {
                tracing::info!("host pressure cleared — background work resumed");
            }
        }
        st.pressured
    }

    /// Blocking pace point for background worker loops: returns immediately when the host is
    /// healthy, otherwise sleeps in [`PAUSE_TICK`]s until pressure clears or `cancel` is set.
    /// Call between work items — never inside one.
    pub fn pace(&self, cancel: &AtomicBool) {
        while self.pressured() && !cancel.load(Ordering::Relaxed) {
            std::thread::sleep(PAUSE_TICK);
        }
    }
}

/// Deprioritise the calling thread on Linux: nice +10 and the idle I/O scheduling class, so any
/// co-tenant workload (or an interactive 3DAM read) preempts background grind for both CPU and
/// disk. Best-effort; a failure changes nothing.
pub fn deprioritize_current_thread() {
    #[cfg(target_os = "linux")]
    unsafe {
        // nice +10 for this thread (Linux niceness is per-thread despite the POSIX process story).
        let _ = libc::nice(10);
        // ioprio_set(IOPRIO_WHO_PROCESS, 0 = calling thread, class IDLE (3) << 13).
        let _ = libc::syscall(libc::SYS_ioprio_set, 1, 0, 3i32 << 13);
    }
}

/// Engine construction options (resource knobs). Server reads `[resources]` from the serve
/// config; embedded roles fall back to env (`3DAM_BG_THREADS`, `3DAM_MIN_FREE_MEMORY_MB`) then
/// defaults, so a container or systemd unit can tune without a config file.
#[derive(Default, Clone)]
pub struct ResourceOptions {
    /// Background pool size override (clamped to the effective CPU budget).
    pub background_threads: Option<usize>,
    /// Pause background work when host available memory dips below this (MiB).
    pub min_free_memory_mb: Option<u64>,
}

impl ResourceOptions {
    pub fn from_env() -> ResourceOptions {
        fn parse<T: std::str::FromStr>(k: &str) -> Option<T> {
            std::env::var(k).ok().and_then(|v| v.parse().ok())
        }
        ResourceOptions {
            background_threads: parse("3DAM_BG_THREADS"),
            min_free_memory_mb: parse("3DAM_MIN_FREE_MEMORY_MB"),
        }
    }

    /// `self`, with any unset knob taken from the environment.
    pub fn or_env(self) -> ResourceOptions {
        let env = ResourceOptions::from_env();
        ResourceOptions {
            background_threads: self.background_threads.or(env.background_threads),
            min_free_memory_mb: self.min_free_memory_mb.or(env.min_free_memory_mb),
        }
    }
}

/// True when running under a container-ish cgroup limit (used only for log context).
pub fn is_resource_limited() -> bool {
    cgroup_cpu_quota().is_some()
        || Path::new("/sys/fs/cgroup/memory.max").exists()
            && read_trimmed("/sys/fs/cgroup/memory.max").is_some_and(|s| s != "max")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_parses_quota_and_unlimited() {
        assert_eq!(parse_cpu_max("200000 100000"), Some(2));
        assert_eq!(parse_cpu_max("150000 100000"), Some(2)); // ceil
        assert_eq!(parse_cpu_max("50000 100000"), Some(1));
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("garbage"), None);
    }

    #[test]
    fn meminfo_field_parses() {
        let text = "MemTotal:       15990784 kB\nMemFree:          271024 kB\nMemAvailable:    1234567 kB\n";
        assert_eq!(parse_meminfo_kb(text, "MemAvailable"), Some(1_234_567));
        assert_eq!(parse_meminfo_kb(text, "MemTotal"), Some(15_990_784));
        assert_eq!(parse_meminfo_kb(text, "Mem"), None); // prefix must match a whole field
    }

    #[test]
    fn governor_decides_on_memory_and_load() {
        let g = Governor::new(Some(1024)); // 1 GiB floor
        let gib = 1024 * 1024 * 1024;
        // Healthy: plenty free, load under budget.
        assert!(!g.decide(Some(4 * gib), Some(2.0), 4));
        // Memory floor breached.
        assert!(g.decide(Some(gib / 2), Some(0.5), 4));
        // Load runaway (>1.5×cpus).
        assert!(g.decide(Some(4 * gib), Some(7.0), 4));
        // No probes (non-Linux): never pauses.
        assert!(!g.decide(None, None, 4));
    }

    #[test]
    fn bg_thread_count_is_capped_and_clamped() {
        let cpus = effective_cpus();
        // Default: never above the cap, never zero.
        let n = background_thread_count(None);
        assert!((1..=DEFAULT_BG_THREAD_CAP).contains(&n));
        // Override: clamped to the CPU budget.
        assert_eq!(background_thread_count(Some(1)), 1);
        assert!(background_thread_count(Some(10_000)) <= cpus);
    }

    #[test]
    fn pace_returns_immediately_when_cancelled() {
        let g = Governor::new(Some(u64::MAX / (1024 * 1024))); // impossible floor → always pressured
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        g.pace(&cancel); // must not sleep
        assert!(start.elapsed() < Duration::from_millis(400));
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    #[test]
    fn pressured_is_true_under_impossible_floor_on_linux() {
        if available_memory().is_none() {
            return; // non-Linux: governor is inert by design
        }
        let g = Governor::new(Some(9_999_999)); // ~9.5 TiB floor
        assert!(
            g.pressured(),
            "available={:?} load={:?} cpus={}",
            available_memory(),
            loadavg_1(),
            effective_cpus()
        );
    }
}
