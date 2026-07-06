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
}

pub(crate) struct WatchManager {
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    rt: tokio::runtime::Handle,
    live: Mutex<HashMap<SourceId, WatchEntry>>,
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
            live: Mutex::new(HashMap::new()),
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
        let mut live = self.live.lock().unwrap();
        if live.contains_key(&id) {
            return;
        }
        let info = match self.store.get_source(&id) {
            Ok(Some(i)) if i.watch => i,
            _ => return,
        };
        match info.kind {
            SourceKind::LocalFs => match self.spawn_local(id, &info.uri) {
                Some(w) => {
                    live.insert(id, WatchEntry::Local(w));
                }
                None => tracing::warn!(source = %id, "could not start local watcher"),
            },
            SourceKind::Sftp | SourceKind::Smb => {
                self.spawn_poll(id);
                live.insert(id, WatchEntry::Poll);
            }
            SourceKind::Federated => {}
        }
    }

    fn spawn_local(&self, id: SourceId, root: &str) -> Option<notify::RecommendedWatcher> {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if res.is_ok() {
                    let _ = tx.send(());
                }
            })
            .ok()?;
        watcher.watch(Path::new(root), RecursiveMode::Recursive).ok()?;

        let store = self.store.clone();
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        self.rt.spawn(async move {
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
        Some(watcher)
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
