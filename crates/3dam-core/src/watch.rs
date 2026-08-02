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

use crate::scan;
use dam_api::dto::SourceKind;
use dam_api::dto::{JobKind, ScanMode};
use dam_api::event::LibraryEvent;
use dam_api::id::SourceId;
use dam_store::Store;
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

/// Quiet window a burst of local FS events must settle for before a re-scan fires.
const DEBOUNCE: Duration = Duration::from_millis(600);
/// Poll cadence for remote sources with no push channel (SFTP/SMB).
const POLL_INTERVAL: Duration = Duration::from_secs(60);

enum WatchEntry {
    /// The live OS watcher. Never read — held purely so its `Drop` (which stops notifications)
    /// doesn't run until the source is unwatched or the engine closes.
    Local {
        _watcher: notify::RecommendedWatcher,
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
}

impl WatchManager {
    pub(crate) fn new(
        store: Arc<Store>,
        secrets: crate::credentials::SecretVault,
        events: broadcast::Sender<LibraryEvent>,
        rt: tokio::runtime::Handle,
        governor: Arc<crate::resources::Governor>,
        scratch: std::path::PathBuf,
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
        self.rt.spawn_blocking(move || {
            // A watch channel retains one revision, not one item per callback. A 100k-file copy can
            // therefore make this counter race ahead, but it can never allocate a 100k-entry queue.
            let (tx, mut rx) = watch::channel(0_u64);
            let initial_scan = tx.clone();
            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    // Only a genuine content mutation may trigger a re-scan. The inotify backend also
                    // reports opens/reads/atime bumps (`OPEN`, `CLOSE_NOWRITE`, `ATTRIB`), and the scan
                    // opens+reads every file — so firing on those would make the scan re-trigger the
                    // very scan that produced them: an endless rescan-from-zero loop. Filter it out.
                    if let Ok(ev) = res {
                        if is_content_change(&ev.kind) {
                            mark_dirty(&tx);
                        }
                    }
                }) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!(source = %id, error = %e, "could not create local watcher");
                        live.lock().unwrap().remove(&id);
                        return;
                    }
                };
            if let Err(e) = watcher.watch(Path::new(&root), RecursiveMode::Recursive) {
                tracing::warn!(source = %id, error = %e, "could not start local watcher");
                live.lock().unwrap().remove(&id);
                return;
            }

            // Reconcile once after registration. A file can be created after `add_source` returns
            // but before the off-thread OS watch is live; that event cannot be replayed by notify.
            // The initial delta is idempotent and closes that otherwise permanent missed-event gap.
            mark_dirty(&initial_scan);

            rt.spawn(async move {
                while wait_for_quiet(&mut rx, DEBOUNCE).await {
                    trigger_delta(
                        &store, &secrets, &events, &in_flight, &governor, &scratch, id,
                    );
                }
            });
            // Publish the live watcher, replacing the `Pending` reservation.
            live.lock()
                .unwrap()
                .insert(id, WatchEntry::Local { _watcher: watcher });
        });
    }

    fn spawn_poll(&self, id: SourceId) {
        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch.clone();
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                trigger_delta(
                    &store, &secrets, &events, &in_flight, &governor, &scratch, id,
                );
            }
        });
    }
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
fn trigger_delta(
    store: &Arc<Store>,
    secrets: &crate::credentials::SecretVault,
    events: &broadcast::Sender<LibraryEvent>,
    in_flight: &Arc<Mutex<HashSet<SourceId>>>,
    governor: &Arc<crate::resources::Governor>,
    scratch: &Arc<std::path::PathBuf>,
    id: SourceId,
) {
    {
        let mut f = in_flight.lock().unwrap();
        if !f.insert(id) {
            return; // a scan for this source is already running
        }
    }
    let clear = || {
        in_flight.lock().unwrap().remove(&id);
    };
    let info = match store.get_source(&id) {
        Ok(Some(i)) => i,
        _ => {
            clear();
            return;
        }
    };
    let params = r#"{"mode":"delta","watch":true}"#;
    // An auto-rescan touches exactly the one watched source (issue #42).
    let job = match store.create_job(JobKind::Scan, params, None, &[id]) {
        Ok(j) => j,
        Err(_) => {
            clear();
            return;
        }
    };
    let store = store.clone();
    let secrets = secrets.clone();
    let events = events.clone();
    let in_flight = in_flight.clone();
    let governor = governor.clone();
    let scratch = scratch.clone();
    tokio::task::spawn_blocking(move || {
        let cancel = Arc::new(AtomicBool::new(false));
        scan::run_scan(
            store,
            secrets,
            events,
            job,
            vec![info],
            ScanMode::Delta,
            cancel,
            &governor,
            &scratch,
        );
        in_flight.lock().unwrap().remove(&id);
    });
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
