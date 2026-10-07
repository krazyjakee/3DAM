//! Watch / auto-rescan (tech-spec 07 §3.1–§3.3, phase 4 Reach).
//!
//! Watch-enabled sources are kept current without a manual `scan`. **Local FS** uses OS change
//! notification (`notify` → inotify/FSEvents/ReadDirectoryChangesW), debounced so a burst of writes
//! coalesces into one delta re-scan. **SFTP/SMB** have no reliable push channel, so they are
//! **polled** on an interval (§3.2–§3.3). Either way the trigger is a delta scan — cheap, since it
//! only opens bytes for files whose size/mtime changed.
//!
//! A watch scan reuses [`crate::scan::run_scan`] directly (store + event bus), so the manager needs
//! no back-reference to the full engine. Overlapping scans of one source are suppressed.

use crate::scan::ScanOutcome;
use dam_api::dto::JobKind;
use dam_api::dto::SourceKind;
use dam_api::event::LibraryEvent;
use dam_api::id::SourceId;
use dam_store::Store;
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

/// Quiet window a burst of local FS events must settle for before a re-scan fires.
const DEBOUNCE: Duration = Duration::from_millis(600);
const MAX_DIRTY_PATHS: usize = 512;

#[derive(Default)]
struct DirtyPaths {
    full: bool,
    trusted: bool,
    paths: BTreeSet<String>,
}
impl DirtyPaths {
    fn fallback(&mut self) {
        self.full = true;
        self.trusted = false;
        self.paths.clear();
    }
    fn add(&mut self, root: &Path, path: &Path) {
        if self.full {
            return;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            self.fallback();
            return;
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            self.fallback();
            return;
        }
        self.paths
            .insert(relative.to_string_lossy().replace('\\', "/"));
        if self.paths.len() > MAX_DIRTY_PATHS {
            self.fallback();
        }
    }
    fn take(&mut self) -> Vec<String> {
        if self.full {
            self.full = false;
            self.paths.clear();
            Vec::new()
        } else {
            std::mem::take(&mut self.paths).into_iter().collect()
        }
    }
}

#[derive(Clone, Copy)]
struct PollPolicy {
    base: Duration,
    max: Duration,
}
impl PollPolicy {
    fn from_env() -> Self {
        let seconds = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        };
        let base = seconds("DAM_SOURCE_POLL_SECONDS", 60).clamp(1, 86400);
        let max = seconds("DAM_SOURCE_POLL_MAX_SECONDS", 900).clamp(base, 86400);
        Self {
            base: Duration::from_secs(base),
            max: Duration::from_secs(max),
        }
    }
    fn next(self, previous: Duration, outcome: ScanOutcome) -> Duration {
        if outcome.healthy && outcome.changed {
            self.base
        } else {
            previous.saturating_mul(2).min(self.max)
        }
    }
    // Stable per-source jitter, below 10%; never exceed the configured freshness bound.
    fn delay(self, interval: Duration, id: SourceId) -> Duration {
        let seed = id
            .as_bytes()
            .iter()
            .fold(0u64, |n, b| n.wrapping_mul(31).wrapping_add(*b as u64));
        interval
            .saturating_add(Duration::from_millis(
                interval.as_millis() as u64 * (seed % 100) / 1000,
            ))
            .min(self.max)
    }
}

enum WatchEntry {
    /// The live OS watcher. Never read — held purely so its `Drop` (which stops notifications)
    /// doesn't run until the source is unwatched or the engine closes.
    Local {
        _watcher: notify::RecommendedWatcher,
        dirty: Arc<Mutex<DirtyPaths>>,
    },
    /// A detached poll task marks its source watched here (nothing to keep alive).
    Poll,
    /// Slot reserved while a local watcher is being registered off-thread. Reserving synchronously
    /// stops a second `ensure()` from double-spawning before the background setup lands.
    Pending,
}

pub(crate) struct WatchManager {
    store: Arc<Store>,
    secrets: crate::credentials::SecretVault,
    events: broadcast::Sender<LibraryEvent>,
    rt: tokio::runtime::Handle,
    live: Arc<Mutex<HashMap<SourceId, WatchEntry>>>,
    in_flight: Arc<Mutex<HashSet<SourceId>>>,
    /// Host-pressure governor the triggered delta scans pace against (tech-spec 14 §3.4) — a
    /// watch-driven re-scan is the same bulk reader as a submitted one.
    governor: Arc<crate::resources::Governor>,
    /// Scratch dir the triggered scans hand to remote sources (issue #87).
    scratch: Arc<std::path::PathBuf>,
    coordinator: Arc<crate::scan_admission::Coordinator>,
}

impl WatchManager {
    pub(crate) fn new(
        store: Arc<Store>,
        secrets: crate::credentials::SecretVault,
        events: broadcast::Sender<LibraryEvent>,
        rt: tokio::runtime::Handle,
        governor: Arc<crate::resources::Governor>,
        scratch: std::path::PathBuf,
        coordinator: Arc<crate::scan_admission::Coordinator>,
    ) -> WatchManager {
        WatchManager {
            store,
            secrets,
            events,
            rt,
            live: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            governor,
            scratch: Arc::new(scratch),
            coordinator,
        }
    }

    /// A watcher becomes a trusted discovery journal only after a successful reconciliation.
    /// Pending/failed registration, dirty revisions and active scans all require enumeration.
    pub(crate) fn trusted_clean(&self, id: SourceId) -> bool {
        if self.in_flight.lock().unwrap().contains(&id) {
            return false;
        }
        let live = self.live.lock().unwrap();
        match live.get(&id) {
            Some(WatchEntry::Local { dirty, .. }) => {
                let dirty = dirty.lock().unwrap();
                dirty.trusted && !dirty.full && dirty.paths.is_empty()
            }
            _ => false,
        }
    }

    /// Start watchers for every source already flagged `watch` (called at engine open).
    pub(crate) fn start_all(&self) {
        if let Ok(sources) = self.store.list_sources() {
            for s in sources.into_iter().filter(|s| s.watch) {
                self.ensure(s.id);
            }
        }
    }

    /// Idempotently begin watching one source. No-op if already watched, not flagged `watch`, or a
    /// federated peer. Best-effort: a watcher that can't be set up is logged, never fatal.
    pub(crate) fn ensure(&self, id: SourceId) {
        // Reserve the slot under the lock so overlapping `ensure()` calls can't both spawn.
        {
            let mut live = self.live.lock().unwrap();
            if live.contains_key(&id) {
                return;
            }
            live.insert(id, WatchEntry::Pending);
        }
        let info = match self.store.get_source(&id) {
            Ok(Some(i)) if i.watch => i,
            _ => {
                self.live.lock().unwrap().remove(&id);
                return;
            }
        };
        match info.kind {
            SourceKind::LocalFs => self.spawn_local(id, info.uri),
            SourceKind::Sftp | SourceKind::Smb => {
                self.spawn_poll(id);
                self.live.lock().unwrap().insert(id, WatchEntry::Poll);
            }
            SourceKind::Federated => {
                self.live.lock().unwrap().remove(&id);
            }
        }
    }

    /// Register a recursive OS watch for a local source **off the async runtime**. Adding a recursive
    /// inotify/FSEvents watch walks the whole subtree synchronously — seconds-to-forever on a huge or
    /// network-backed (CIFS/NFS) root — so doing it inline would stall `EmbeddedLibrary::open()` and
    /// every role with it. The slot is left `Pending` until the watcher lands (or is dropped on error).
    fn spawn_local(&self, id: SourceId, root: String) {
        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        let live = self.live.clone();
        let rt = self.rt.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch.clone();
        let coordinator = self.coordinator.clone();
        self.rt.spawn_blocking(move || {
            // A watch channel retains one revision, not one item per callback. A 100k-file copy can
            // therefore make this counter race ahead, but it can never allocate a 100k-entry queue.
            let (tx, mut rx) = watch::channel(0_u64);
            let initial_scan = tx.clone();
            let dirty = Arc::new(Mutex::new(DirtyPaths::default()));
            let callback_dirty = dirty.clone();
            let callback_root = std::path::PathBuf::from(&root);
            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    // Only a genuine content mutation may trigger a re-scan. The inotify backend also
                    // reports opens/reads/atime bumps (`OPEN`, `CLOSE_NOWRITE`, `ATTRIB`), and the scan
                    // opens+reads every file — so firing on those would make the scan re-trigger the
                    // very scan that produced them: an endless rescan-from-zero loop. Filter it out.
                    match res {
                        Ok(ev)
                            if ev.need_rescan()
                                || matches!(ev.kind, EventKind::Any | EventKind::Other) =>
                        {
                            callback_dirty.lock().unwrap().fallback();
                            mark_dirty(&tx);
                        }
                        Ok(ev) if is_content_change(&ev.kind) => {
                            let mut pending = callback_dirty.lock().unwrap();
                            if ev.paths.is_empty() {
                                pending.fallback();
                            }
                            for path in &ev.paths {
                                pending.add(&callback_root, path);
                            }
                            drop(pending);
                            mark_dirty(&tx);
                        }
                        Err(_) => {
                            callback_dirty.lock().unwrap().fallback();
                            mark_dirty(&tx);
                        }
                        _ => {}
                    }
                }) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!(source = %id, error = %e, "could not create local watcher");
                        spawn_poll_task(
                            &rt,
                            id,
                            store,
                            secrets,
                            events,
                            in_flight,
                            governor,
                            scratch,
                            coordinator,
                        );
                        live.lock().unwrap().insert(id, WatchEntry::Poll);
                        return;
                    }
                };
            if let Err(e) = watcher.watch(Path::new(&root), RecursiveMode::Recursive) {
                tracing::warn!(source = %id, error = %e, "could not start local watcher");
                spawn_poll_task(
                    &rt,
                    id,
                    store,
                    secrets,
                    events,
                    in_flight,
                    governor,
                    scratch,
                    coordinator,
                );
                live.lock().unwrap().insert(id, WatchEntry::Poll);
                return;
            }

            // Reconcile once after registration. A file can be created after `add_source` returns
            // but before the off-thread OS watch is live; that event cannot be replayed by notify.
            // The initial delta is idempotent and closes that otherwise permanent missed-event gap.
            dirty.lock().unwrap().fallback();
            mark_dirty(&initial_scan);

            let task_live = live.clone();
            let task_dirty = dirty.clone();
            rt.spawn(async move {
                let fallback = PollPolicy::from_env().max;
                loop {
                    if !matches!(store.get_source(&id),Ok(Some(info)) if info.watch) {
                        task_live.lock().unwrap().remove(&id);
                        break;
                    }
                    let scopes = tokio::select! {
                        changed=wait_for_quiet(&mut rx,DEBOUNCE) => {
                            if !changed { break; }
                            task_dirty.lock().unwrap().take()
                        },
                        _=tokio::time::sleep(fallback) => Vec::new(),
                    };
                    if let Some(task) = trigger_delta(
                        &store,
                        &secrets,
                        &events,
                        &in_flight,
                        &governor,
                        &scratch,
                        &coordinator,
                        id,
                        scopes,
                    ) {
                        let outcome = task.await.unwrap_or_default();
                        let mut pending = task_dirty.lock().unwrap();
                        if outcome.healthy && !pending.full {
                            pending.trusted = true;
                        } else if !outcome.healthy {
                            pending.fallback();
                        }
                    }
                }
            });
            // Publish the live watcher, replacing the `Pending` reservation.
            live.lock().unwrap().insert(
                id,
                WatchEntry::Local {
                    _watcher: watcher,
                    dirty,
                },
            );
        });
    }

    fn spawn_poll(&self, id: SourceId) {
        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch.clone();
        let coordinator = self.coordinator.clone();
        spawn_poll_task(
            &self.rt,
            id,
            store,
            secrets,
            events,
            in_flight,
            governor,
            scratch,
            coordinator,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_poll_task(
    rt: &tokio::runtime::Handle,
    id: SourceId,
    store: Arc<Store>,
    secrets: crate::credentials::SecretVault,
    events: broadcast::Sender<LibraryEvent>,
    in_flight: Arc<Mutex<HashSet<SourceId>>>,
    governor: Arc<crate::resources::Governor>,
    scratch: Arc<std::path::PathBuf>,
    coordinator: Arc<crate::scan_admission::Coordinator>,
) {
    rt.spawn(async move {
        let policy = PollPolicy::from_env();
        let mut interval = policy.base;
        loop {
            tokio::time::sleep(policy.delay(interval, id)).await;
            if !matches!(store.get_source(&id),Ok(Some(info)) if info.watch) {
                break;
            }
            if let Some(task) = trigger_delta(
                &store,
                &secrets,
                &events,
                &in_flight,
                &governor,
                &scratch,
                &coordinator,
                id,
                Vec::new(),
            ) {
                interval = policy.next(interval, task.await.unwrap_or_default());
            }
        }
    });
}

/// Mark a watched source dirty without queueing one allocation per filesystem event.
fn mark_dirty(tx: &watch::Sender<u64>) {
    tx.send_modify(|revision| *revision = revision.wrapping_add(1));
}

/// Wait until at least one dirty revision arrives and no newer revision appears for `quiet`.
async fn wait_for_quiet(rx: &mut watch::Receiver<u64>, quiet: Duration) -> bool {
    if rx.changed().await.is_err() {
        return false;
    }
    loop {
        tokio::select! {
            changed = rx.changed() => if changed.is_err() { return false },
            _ = tokio::time::sleep(quiet) => return true,
        }
    }
}

/// Does this filesystem event represent an actual content change worth a delta re-scan?
///
/// Deny-list, not allow-list, so it stays correct across backends (inotify/FSEvents/ReadDirectoryW):
/// anything that could be a real create/write/rename/remove passes; only the read-side noise is
/// dropped. That noise is exactly what would otherwise loop — the scan `open()`s and reads every file
/// (bumping atime), which the inotify backend surfaces as `Access(_)` and `Modify(Metadata)` events.
/// Treating those as changes made each scan trigger the next one endlessly (the "counts to N, done,
/// starts from zero again, forever" symptom). Real writes always arrive as `Modify(Data)`/`Create`/
/// rename regardless, so ignoring reads and metadata-only bumps loses no genuine change.
fn is_content_change(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_)             // opens, reads, close-nowrite — includes the scan's own reads
            | EventKind::Modify(ModifyKind::Metadata(_)) // atime/permission bumps (a read touches atime)
            | EventKind::Any
            | EventKind::Other
    )
}

/// Submit a background delta scan for one source, unless one is already running for it.
#[allow(clippy::too_many_arguments)]
fn trigger_delta(
    store: &Arc<Store>,
    secrets: &crate::credentials::SecretVault,
    events: &broadcast::Sender<LibraryEvent>,
    in_flight: &Arc<Mutex<HashSet<SourceId>>>,
    governor: &Arc<crate::resources::Governor>,
    scratch: &Arc<std::path::PathBuf>,
    coordinator: &Arc<crate::scan_admission::Coordinator>,
    id: SourceId,
    scopes: Vec<String>,
) -> Option<tokio::task::JoinHandle<ScanOutcome>> {
    {
        let mut f = in_flight.lock().unwrap();
        if !f.insert(id) {
            return None; // a scan for this source is already running
        }
    }
    let clear = || {
        in_flight.lock().unwrap().remove(&id);
    };
    let info = match store.get_source(&id) {
        Ok(Some(i)) if i.watch => i,
        _ => {
            clear();
            return None;
        }
    };
    let params = r#"{"mode":"quick","watch":true}"#;
    // An auto-rescan touches exactly the one watched source (issue #42).
    let job = match store.create_job(JobKind::Scan, params, None, &[id]) {
        Ok(j) => j,
        Err(_) => {
            clear();
            return None;
        }
    };
    let store = store.clone();
    let secrets = secrets.clone();
    let events = events.clone();
    let in_flight = in_flight.clone();
    let governor = governor.clone();
    let scratch = scratch.clone();
    let coordinator = coordinator.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    coordinator
        .cancels
        .lock()
        .unwrap()
        .insert(job, cancel.clone());
    Some(tokio::task::spawn_blocking(move || {
        let outcome = match crate::quick_scan::run_quick_scan(
            store.clone(),
            secrets,
            events.clone(),
            job,
            vec![info],
            cancel,
            &governor,
            &scratch,
            &coordinator,
            false,
            &scopes,
            None,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                crate::reliability::background_job_failed(&job, "run watched scan", &error);
                crate::reliability::required_background_write(
                    store.set_job_state(
                        &job,
                        dam_api::dto::JobState::Failed,
                        Some(&error.to_string()),
                    ),
                    "persist watched scan failure",
                    &job,
                );
                crate::emit_progress(&store, &events, &job);
                ScanOutcome::default()
            }
        };
        coordinator.cancels.lock().unwrap().remove(&job);
        in_flight.lock().unwrap().remove(&id);
        outcome
    }))
}

#[cfg(test)]
mod tests {
    use super::{is_content_change, mark_dirty, wait_for_quiet};
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify::EventKind;
    use std::time::Duration;
    use tokio::sync::watch;

    #[test]
    fn dirty_paths_are_bounded_and_rename_keeps_both_sides() {
        let mut paths = super::DirtyPaths::default();
        let root = std::path::Path::new("/root");
        paths.add(root, &root.join("old/subtree"));
        paths.add(root, &root.join("new/subtree"));
        assert_eq!(paths.take(), vec!["new/subtree", "old/subtree"]);
        for n in 0..100_000 {
            paths.add(root, &root.join(format!("{n}.png")));
        }
        assert!(paths.full);
        assert!(paths.paths.is_empty());
        assert!(paths.take().is_empty());
        paths.add(root, &root.join("during-scan.png"));
        assert_eq!(paths.take(), vec!["during-scan.png"]);
        paths.add(root, std::path::Path::new("/outside"));
        assert!(paths.full);
    }

    #[test]
    fn unknown_watcher_state_never_claims_an_unchanged_source() {
        let mut pending = super::DirtyPaths::default();
        assert!(!pending.trusted);
        pending.trusted = true;
        pending.add(
            std::path::Path::new("/root"),
            std::path::Path::new("/root/new.png"),
        );
        assert!(!pending.paths.is_empty());
        pending.fallback();
        assert!(!pending.trusted);
        assert!(pending.take().is_empty());
        assert!(
            !pending.trusted,
            "taking a dirty snapshot is not successful reconciliation"
        );
    }

    #[test]
    fn remote_polling_backs_off_and_obeys_freshness_bound() {
        let policy = super::PollPolicy {
            base: Duration::from_secs(60),
            max: Duration::from_secs(900),
        };
        let mut interval = policy.base;
        for _ in 0..20 {
            interval = policy.next(
                interval,
                super::ScanOutcome {
                    changed: false,
                    healthy: true,
                },
            );
        }
        assert_eq!(interval, policy.max);
        let changed = super::ScanOutcome {
            changed: true,
            healthy: true,
        };
        assert_eq!(policy.next(interval, changed), policy.base);
        assert!(policy.delay(interval, dam_api::id::SourceId::new()) <= policy.max);
        assert_eq!(
            policy.next(policy.base, super::ScanOutcome::default()),
            Duration::from_secs(120)
        );
    }

    #[tokio::test]
    async fn hundred_thousand_dirty_events_coalesce_in_one_slot() {
        let (tx, mut rx) = watch::channel(0_u64);
        for _ in 0..100_000 {
            mark_dirty(&tx);
        }

        assert!(wait_for_quiet(&mut rx, Duration::from_millis(1)).await);
        assert_eq!(
            *rx.borrow(),
            100_000,
            "the latest revision must survive the burst"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(5),
                wait_for_quiet(&mut rx, Duration::from_millis(1))
            )
            .await
            .is_err(),
            "one retained watch value must not replay 100k queued notifications"
        );
    }

    #[test]
    fn reads_never_trigger_a_rescan() {
        // These are exactly what the scan's own file opens/reads produce; if any triggered a
        // re-scan the watcher would loop the scan forever (regression guard).
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Any),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)), // atime bump from a read
            EventKind::Any,
            EventKind::Other,
        ] {
            assert!(
                !is_content_change(&kind),
                "read-side event triggered: {kind:?}"
            );
        }
    }

    #[test]
    fn real_changes_trigger_a_rescan() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
        ] {
            assert!(is_content_change(&kind), "real change ignored: {kind:?}");
        }
    }
}
