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
//!    sample of host memory + load + disk stall. Under pressure (available memory below the
//!    floor, load beyond the CPU budget, or I/O stall beyond the ceiling) the loop *pauses* —
//!    work resumes when the host recovers. Fail-soft: on platforms without `/proc` / cgroups
//!    every probe returns `None` and the governor never pauses (the pre-existing behaviour).
//!
//! The I/O signal exists because the other two can't see a disk blockade: bulk reads (scan
//! hashing, decode for analysis/thumbnails) on a slow HDD queue behind each other until *every*
//! task touching that disk — SQLite, health probes, co-tenants — blocks in `D` state, yet a
//! handful of stalled workers never push loadavg past `1.5 × cpus` and memory stays healthy.
//! PSI (`/proc/pressure/io`, kernel ≥ 4.20) measures the stall directly.
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

/// Default I/O full-stall ceiling (%). At 25% the `avg10` decay makes a saturated HDD duty-cycle
/// the grind to roughly a few seconds of reads per ~15 s pause — the disk keeps breathing for
/// everyone else. The `avg10` window itself is the hysteresis: no flapping, no log spam.
pub const DEFAULT_MAX_IO_STALL_PCT: f64 = 25.0;

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

/// Parse the `full` line of a PSI file into its `avg10` percentage.
///
/// `full avg10=…` is the share of the last 10 s in which **every** non-idle task was stalled on
/// I/O at once — the direct signature of a saturated disk blockading the box, and deliberately
/// not `some` (one stalled task is normal life on any busy disk).
fn parse_psi_full_avg10(text: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with("full"))?
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
}

/// Current I/O stall (%), the worse of two views: the host's (`/proc/pressure/io` — the whole box
/// is I/O-blocked, the incident mode) and our cgroup's (`io.pressure` — *our* bulk reads have a
/// slow disk saturated, even if the rest of the host still runs). `None` where PSI is absent
/// (non-Linux, kernel < 4.20, or booted `psi=0`) — fail-soft, like every other probe here.
pub fn io_stall_pct() -> Option<f64> {
    let host = std::fs::read_to_string("/proc/pressure/io")
        .ok()
        .as_deref()
        .and_then(parse_psi_full_avg10);
    let cg = std::fs::read_to_string("/sys/fs/cgroup/io.pressure")
        .ok()
        .as_deref()
        .and_then(parse_psi_full_avg10);
    match (host, cg) {
        (Some(h), Some(c)) => Some(h.max(c)),
        (h, c) => h.or(c),
    }
}

/// One reading of everything the governor decides on. Bundling the probes behind a value (and
/// [`sample_host`] behind a callable) is what lets the governor's own logic — caching, transition
/// logging, the pause loop — be tested against a fixed host instead of whatever `/proc` happens to
/// say on a busy CI runner.
#[derive(Clone, Copy)]
struct Sample {
    available: Option<u64>,
    load1: Option<f64>,
    io_stall: Option<f64>,
    cpus: usize,
}

/// Read the live host probes. The only place the governor touches the filesystem.
fn sample_host() -> Sample {
    Sample {
        available: available_memory(),
        load1: loadavg_1(),
        io_stall: io_stall_pct(),
        cpus: effective_cpus(),
    }
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
    /// Pause while the I/O full-stall share ([`io_stall_pct`]) exceeds this percentage.
    max_io_stall_pct: f64,
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
    /// `max_io_stall_pct = None` picks [`DEFAULT_MAX_IO_STALL_PCT`]; ≥ 100 disables the I/O gate.
    pub fn new(min_free_memory_mb: Option<u64>, max_io_stall_pct: Option<f64>) -> Governor {
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
            max_io_stall_pct: max_io_stall_pct.unwrap_or(DEFAULT_MAX_IO_STALL_PCT),
            state: Mutex::new(GovState {
                sampled_at: None,
                pressured: false,
                was_pressured: false,
            }),
        }
    }

    /// The raw decision, injectable for tests.
    fn decide(
        &self,
        available: Option<u64>,
        load1: Option<f64>,
        io_stall: Option<f64>,
        cpus: usize,
    ) -> bool {
        let mem_pressure = available.is_some_and(|a| a < self.min_free_bytes);
        let load_pressure = load1.is_some_and(|l| l > self.max_load_per_cpu * cpus as f64);
        let io_pressure = io_stall.is_some_and(|s| s > self.max_io_stall_pct);
        mem_pressure || load_pressure || io_pressure
    }

    /// Is the host under pressure right now? Samples at most every [`SAMPLE_EVERY`]; logs each
    /// transition (info level) so an operator can see the engine yielding.
    pub fn pressured(&self) -> bool {
        self.pressured_at(Instant::now(), sample_host)
    }

    /// [`Governor::pressured`] against an injected clock and host reading — the seam that lets the
    /// caching and transition logic be tested against a fixed host and a fixed `now`, instead of
    /// whatever `/proc` and the wall clock happen to say on a loaded CI runner.
    ///
    /// `sample` stays a closure rather than a value so the [`SAMPLE_EVERY`] cache still
    /// short-circuits *before* any probing — paying for the file reads per item (thousands of
    /// assets) is exactly what the cache exists to avoid.
    fn pressured_at(&self, now: Instant, sample: impl Fn() -> Sample) -> bool {
        let mut st = self.state.lock().unwrap();
        if st
            .sampled_at
            .is_some_and(|at| now.duration_since(at) < SAMPLE_EVERY)
        {
            return st.pressured;
        }
        let Sample {
            available,
            load1: load,
            io_stall,
            cpus,
        } = sample();
        st.pressured = self.decide(available, load, io_stall, cpus);
        st.sampled_at = Some(now);
        if st.pressured != st.was_pressured {
            st.was_pressured = st.pressured;
            if st.pressured {
                tracing::info!(
                    available_mb = available.map(|a| a / (1024 * 1024)),
                    load1 = load,
                    io_stall_pct = io_stall,
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

    /// [`Governor::pace`] against an injectable host reading and sleeper, so a test can assert
    /// *how many times the loop slept* rather than how long it took — the wall clock on a loaded
    /// box says nothing about whether the cancel flag was honoured.
    #[cfg(test)]
    fn pace_with(
        &self,
        cancel: &AtomicBool,
        sample: impl Fn() -> Sample,
        mut sleep: impl FnMut(Duration),
    ) {
        while self.pressured_at(Instant::now(), &sample) && !cancel.load(Ordering::Relaxed) {
            sleep(PAUSE_TICK);
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
/// config; embedded roles fall back to env (`3DAM_BG_THREADS`, `3DAM_MIN_FREE_MEMORY_MB`,
/// `3DAM_MAX_IO_STALL_PCT`) then defaults, so a container or systemd unit can tune without a
/// config file.
#[derive(Default, Clone)]
pub struct ResourceOptions {
    /// Background pool size override (clamped to the effective CPU budget).
    pub background_threads: Option<usize>,
    /// Pause background work when host available memory dips below this (MiB).
    pub min_free_memory_mb: Option<u64>,
    /// Pause bulk reads/grind when I/O full-stall (PSI `avg10`) exceeds this (%). ≥ 100 disables.
    pub max_io_stall_pct: Option<f64>,
}

impl ResourceOptions {
    pub fn from_env() -> ResourceOptions {
        fn parse<T: std::str::FromStr>(k: &str) -> Option<T> {
            std::env::var(k).ok().and_then(|v| v.parse().ok())
        }
        ResourceOptions {
            background_threads: parse("3DAM_BG_THREADS"),
            min_free_memory_mb: parse("3DAM_MIN_FREE_MEMORY_MB"),
            max_io_stall_pct: parse("3DAM_MAX_IO_STALL_PCT"),
        }
    }

    /// Options that render the pressure governor inert: no memory floor, no I/O-stall gate.
    /// For hermetic tests (and one-shot tooling) on busy dev/CI boxes — there, a scan parking
    /// because the *build itself* is hammering the disk is flake, not good-neighbourliness.
    /// Every knob is `Some`, so `or_env()` leaves these as-is.
    pub fn ungoverned() -> ResourceOptions {
        ResourceOptions {
            background_threads: None,
            min_free_memory_mb: Some(0),
            max_io_stall_pct: Some(f64::INFINITY),
        }
    }

    /// `self`, with any unset knob taken from the environment.
    pub fn or_env(self) -> ResourceOptions {
        let env = ResourceOptions::from_env();
        ResourceOptions {
            background_threads: self.background_threads.or(env.background_threads),
            min_free_memory_mb: self.min_free_memory_mb.or(env.min_free_memory_mb),
            max_io_stall_pct: self.max_io_stall_pct.or(env.max_io_stall_pct),
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
    fn psi_full_avg10_parses() {
        let text = "some avg10=12.34 avg60=5.00 avg300=1.00 total=123456\n\
                    full avg10=8.15 avg60=3.00 avg300=0.50 total=65432\n";
        assert_eq!(parse_psi_full_avg10(text), Some(8.15));
        // `some` alone (old kernels expose only it for some resources) is deliberately ignored.
        assert_eq!(
            parse_psi_full_avg10("some avg10=99.0 avg60=0.0 avg300=0.0 total=1\n"),
            None
        );
        assert_eq!(parse_psi_full_avg10(""), None);
        assert_eq!(parse_psi_full_avg10("full garbage\n"), None);
    }

    #[test]
    fn governor_decides_on_memory_load_and_io() {
        let g = Governor::new(Some(1024), None); // 1 GiB floor, default 25% I/O ceiling
        let gib = 1024 * 1024 * 1024;
        // Healthy: plenty free, load under budget, disk quiet.
        assert!(!g.decide(Some(4 * gib), Some(2.0), Some(0.5), 4));
        // Memory floor breached.
        assert!(g.decide(Some(gib / 2), Some(0.5), Some(0.0), 4));
        // Load runaway (>1.5×cpus).
        assert!(g.decide(Some(4 * gib), Some(7.0), Some(0.0), 4));
        // Disk blockade: everything else healthy, but the box is I/O-stalled. This is exactly
        // the incident load/memory could not see — bulk HDD reads queueing every task in D state.
        assert!(g.decide(Some(4 * gib), Some(1.0), Some(60.0), 4));
        assert!(!g.decide(Some(4 * gib), Some(1.0), Some(24.9), 4));
        // No probes (non-Linux, or psi=0): never pauses.
        assert!(!g.decide(None, None, None, 4));
        // Operator override: a 90% ceiling tolerates the 60% stall; ≥100 disables the gate.
        let lax = Governor::new(Some(1024), Some(90.0));
        assert!(!lax.decide(Some(4 * gib), Some(1.0), Some(60.0), 4));
        let off = Governor::new(Some(1024), Some(100.0));
        assert!(!off.decide(Some(4 * gib), Some(1.0), Some(100.0), 4));
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

    /// A host that trips the memory floor of every governor built below (1 GiB / impossible), with
    /// load and disk deliberately healthy so only the memory gate is under test.
    fn starved_host() -> Sample {
        Sample {
            available: Some(0),
            load1: Some(0.1),
            io_stall: Some(0.0),
            cpus: 4,
        }
    }

    #[test]
    fn pace_does_not_sleep_when_cancelled() {
        let g = Governor::new(Some(1024), None);
        // Vacuity guard: without this the loop would exit on `pressured()` alone and the test
        // would pass no matter what `pace` did with the cancel flag.
        assert!(
            Governor::new(Some(1024), None).pressured_at(Instant::now(), starved_host),
            "the injected host must be pressured for the cancel check to mean anything"
        );

        let cancel = AtomicBool::new(true);
        let mut naps = 0usize;
        g.pace_with(&cancel, starved_host, |_| naps += 1);
        assert_eq!(naps, 0, "pace must not sleep once cancel is set");
    }

    #[test]
    fn pace_sleeps_while_pressured_and_stops_when_cancel_flips() {
        let g = Governor::new(Some(1024), None);
        let cancel = AtomicBool::new(false);
        let mut naps = 0usize;
        g.pace_with(&cancel, starved_host, |tick| {
            assert_eq!(tick, PAUSE_TICK);
            naps += 1;
            if naps == 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(naps, 3, "pace parks while pressured, then honours cancel");
    }

    /// The [`SAMPLE_EVERY`] cache must short-circuit *before* probing — the probes are file reads
    /// and background loops call this per item. Driven off an injected `now` so it asserts on the
    /// window itself, not on how fast the test machine gets through two statements.
    #[test]
    fn pressure_is_probed_at_most_once_per_window() {
        let g = Governor::new(Some(1024), None);
        let calls = std::cell::Cell::new(0usize);
        let sample = || {
            calls.set(calls.get() + 1);
            starved_host()
        };
        let t0 = Instant::now();
        assert!(g.pressured_at(t0, sample));
        assert_eq!(calls.get(), 1);
        // Still inside the window: cached verdict, no probing.
        assert!(g.pressured_at(t0 + SAMPLE_EVERY / 2, sample));
        assert_eq!(calls.get(), 1, "the cache short-circuits probing");
        // Past it: re-probes.
        assert!(g.pressured_at(t0 + SAMPLE_EVERY, sample));
        assert_eq!(calls.get(), 2, "the window expires");
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;

    /// The live probes are wired into the decision: on a host where `/proc` answers, an impossible
    /// memory floor pauses background work.
    ///
    /// Takes **one** reading of the host and decides from that snapshot. The previous shape
    /// probed once to decide whether to skip, then let `pressured()` probe again to assert — two
    /// independent reads of live state, so a transient failure of the second (fd exhaustion under
    /// a heavy parallel build, a cgroup file churning) flipped the verdict with nothing wrong in
    /// the code. The pure floor→pressured arithmetic is covered hermetically by
    /// `tests::governor_decides_on_memory_load_and_io`; this test only adds "the real probes feed
    /// it", which one snapshot proves just as well.
    #[test]
    fn live_sample_drives_the_decision_on_linux() {
        let host = sample_host();
        if host.available.is_none() {
            return; // non-Linux, or no /proc: governor is inert by design
        }
        let g = Governor::new(Some(9_999_999), None); // ~9.5 TiB floor
        assert!(
            g.pressured_at(Instant::now(), || host),
            "available={:?} load={:?} io_stall={:?} cpus={}",
            host.available,
            host.load1,
            host.io_stall,
            host.cpus
        );
    }
}
