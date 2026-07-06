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
use dam_api::dto::{JobKind, ScanMode};
use dam_api::event::LibraryEvent;
use dam_api::id::SourceId;
use dam_api::dto::SourceKind;
use dam_store::Store;
use notify::{RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// Quiet window a burst of local FS events must settle for before a re-scan fires.
const DEBOUNCE: Duration = Duration::from_millis(600);
/// Poll cadence for remote sources with no push channel (SFTP/SMB).
const POLL_INTERVAL: Duration = Duration::from_secs(60);

enum WatchEntry {
    /// The live OS watcher. Never read — held purely so its `Drop` (which stops notifications)
    /// doesn't run until the source is unwatched or the engine closes.
    Local(#[allow(dead_code)] notify::RecommendedWatcher),
    /// A detached poll task marks its source watched here (nothing to keep alive).
    Poll,
    /// Slot reserved while a local watcher is being registered off-thread. Reserving synchronously
    /// stops a second `ensure()` from double-spawning before the background setup lands.
    Pending,
}

pub(crate) struct WatchManager {
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    rt: tokio::runtime::Handle,
    live: Arc<Mutex<HashMap<SourceId, WatchEntry>>>,
    in_flight: Arc<Mutex<HashSet<SourceId>>>,
}

impl WatchManager {
    pub(crate) fn new(
        store: Arc<Store>,
        events: broadcast::Sender<LibraryEvent>,
        rt: tokio::runtime::Handle,
    ) -> WatchManager {
        WatchManager {
            store,
            events,
            rt,
            live: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
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
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        let live = self.live.clone();
        let rt = self.rt.clone();
        self.rt.spawn_blocking(move || {
            let (tx, mut rx) = mpsc::unbounded_channel::<()>();
            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if res.is_ok() {
                        let _ = tx.send(());
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

            rt.spawn(async move {
                while rx.recv().await.is_some() {
                    // Coalesce the burst: keep resetting the quiet timer until it elapses.
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(DEBOUNCE) => break,
                            more = rx.recv() => if more.is_none() { return },
                        }
                    }
                    trigger_delta(&store, &events, &in_flight, id);
                }
            });
            // Publish the live watcher, replacing the `Pending` reservation.
            live.lock().unwrap().insert(id, WatchEntry::Local(watcher));
        });
    }

    fn spawn_poll(&self, id: SourceId) {
        let store = self.store.clone();
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                trigger_delta(&store, &events, &in_flight, id);
            }
        });
    }
}

/// Submit a background delta scan for one source, unless one is already running for it.
fn trigger_delta(
    store: &Arc<Store>,
    events: &broadcast::Sender<LibraryEvent>,
    in_flight: &Arc<Mutex<HashSet<SourceId>>>,
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
    let job = match store.create_job(JobKind::Scan, params, None) {
        Ok(j) => j,
        Err(_) => {
            clear();
            return;
        }
    };
    let store = store.clone();
    let events = events.clone();
    let in_flight = in_flight.clone();
    tokio::task::spawn_blocking(move || {
        let cancel = Arc::new(AtomicBool::new(false));
        scan::run_scan(store, events, job, vec![info], ScanMode::Delta, cancel);
        in_flight.lock().unwrap().remove(&id);
    });
}
