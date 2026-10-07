//! Watch / auto-rescan (tech-spec 07 §3.1–§3.3, phase 4 Reach).
//!
//! Watch-enabled sources are kept current without a manual `scan`. Verified local Linux roots
//! use debounced OS notifications as a discovery journal. Network, unknown and nested-mount roots
//! use polling and full reconciliation. Other platforms retain OS notifications as hints requiring
//! full reconciliation. SFTP/SMB are polled on an interval (§3.2–§3.3).
//!
//! A watch scan reuses [`crate::quick_scan::run_quick_scan`] directly (store + event bus), so the manager needs
//! no back-reference to the full engine. Overlapping scans of one source are coordinated.

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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

/// Quiet window a burst of local FS events must settle for before a re-scan fires.
const DEBOUNCE: Duration = Duration::from_millis(600);
const MAX_DIRTY_PATHS: usize = 512;

/// A retained directory handle prevents reuse of the watched inode after a rename/removal.
/// Filesystem and mount probes run only on blocking workers, never inside notification callbacks.
struct LocalBinding {
    broken: AtomicBool,
    #[cfg(target_os = "linux")]
    root: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    identity: RootIdentity,
    #[cfg(target_os = "linux")]
    mount: MountIdentity,
    #[cfg(target_os = "linux")]
    _root_file: std::fs::File,
}

impl LocalBinding {
    fn current(&self) -> bool {
        if self.broken.load(Ordering::Acquire) {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            local_binding(&self.root).is_some_and(|current| {
                current.identity == self.identity && current.mount == self.mount
            })
        }
        #[cfg(not(target_os = "linux"))]
        false
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
struct MountIdentity {
    id: u64,
    parent: u64,
    device: String,
    root: std::path::PathBuf,
    point: std::path::PathBuf,
    filesystem: String,
}

#[cfg(target_os = "linux")]
fn trusted_filesystem(magic: u64) -> bool {
    // Linux uapi/linux/magic.h: ext2/3/4, XFS, Btrfs and tmpfs. Network, FUSE, overlay and
    // unrecognised implementations may change without a complete local notification stream.
    matches!(magic, 0xef53 | 0x5846_5342 | 0x9123_683e | 0x0102_1994)
}

#[cfg(target_os = "linux")]
fn mount_path(encoded: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let octal = bytes.get(index + 1..index + 4)?;
            if !octal.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                return None;
            }
            let value = (octal[0] - b'0') as u16 * 64
                + (octal[1] - b'0') as u16 * 8
                + (octal[2] - b'0') as u16;
            decoded.push(u8::try_from(value).ok()?);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(std::ffi::OsString::from_vec(decoded).into())
}

#[cfg(target_os = "linux")]
fn covering_mount(root: &Path, mountinfo: &str) -> Option<MountIdentity> {
    let mut covering: Option<MountIdentity> = None;
    let mut covering_points = BTreeSet::new();
    for line in mountinfo.lines() {
        let (before, after) = line.split_once(" - ")?;
        let mut fields = before.split_whitespace();
        let id = fields.next()?.parse().ok()?;
        let parent = fields.next()?.parse().ok()?;
        let device = fields.next()?.to_owned();
        let mount_root = mount_path(fields.next()?)?;
        let point = mount_path(fields.next()?)?;
        let filesystem = after.split_whitespace().next()?.to_owned();
        // A recursive source containing another mount cannot promise one complete notification
        // journal. Reject even local bind mounts, avoiding registration walks into remote trees
        // and detecting later mount additions without reading any directory contents.
        if point != root && point.starts_with(root) {
            return None;
        }
        if !root.starts_with(&point) {
            continue;
        }
        if !covering_points.insert(point.clone()) {
            return None; // stacked covering mounts are ambiguous even below a nearer mount
        }
        if covering.as_ref().is_some_and(|previous| {
            previous.point.components().count() > point.components().count()
        }) {
            continue;
        }
        covering = Some(MountIdentity {
            id,
            parent,
            device,
            root: mount_root,
            point,
            filesystem,
        });
    }
    covering.filter(|mount| {
        matches!(
            mount.filesystem.as_str(),
            "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "tmpfs"
        )
    })
}

#[cfg(target_os = "linux")]
fn root_handle(root: &Path) -> Option<(std::fs::File, RootIdentity)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    if !root.is_absolute() {
        return None;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .ok()?;
    for component in root.components() {
        let name = match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => std::ffi::CString::new(name.as_bytes()).ok()?,
            _ => return None,
        };
        // Safety: this live directory descriptor and NUL-terminated basename are valid for the
        // call. Each returned descriptor is newly owned and never follows a symlink component.
        let descriptor = unsafe {
            libc::openat(
                file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return None;
        }
        file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    }
    let metadata = file.metadata().ok()?;
    let identity = RootIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    Some((file, identity))
}

#[cfg(target_os = "linux")]
fn local_binding(root: &Path) -> Option<Arc<LocalBinding>> {
    use std::os::fd::AsRawFd;
    let (file, identity) = root_handle(root)?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // Safety: fstatfs initialises this correctly sized output on success and the descriptor lives
    // through the call. This probes the directory itself, not an ambient path or its contents.
    let success = unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) == 0 };
    if !success || !trusted_filesystem(unsafe { filesystem.assume_init() }.f_type as u32 as u64) {
        return None;
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mount = covering_mount(root, &mountinfo)?;
    Some(Arc::new(LocalBinding {
        broken: AtomicBool::new(false),
        root: root.into(),
        identity,
        mount,
        _root_file: file,
    }))
}

async fn binding_current(binding: Option<Arc<LocalBinding>>) -> bool {
    match binding {
        Some(binding) => tokio::task::spawn_blocking(move || binding.current())
            .await
            .unwrap_or(false),
        None => false,
    }
}

#[derive(Default)]
struct DirtyPaths {
    full: bool,
    trusted: bool,
    reconciling: bool,
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
        self.reconciling = true;
        if self.full {
            self.full = false;
            self.paths.clear();
            Vec::new()
        } else {
            std::mem::take(&mut self.paths).into_iter().collect()
        }
    }
    fn clean(&self) -> bool {
        self.trusted && !self.reconciling && !self.full && self.paths.is_empty()
    }
    fn reconciled(&mut self, outcome: ScanOutcome, full: bool, binding_current: bool) {
        self.reconciling = false;
        if !binding_current || !outcome.healthy {
            self.fallback();
        } else if !self.full && (full || self.trusted) {
            self.trusted = true;
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
        binding: Option<Arc<LocalBinding>>,
        signal: watch::Sender<u64>,
        token: Arc<()>,
        root: String,
    },
    /// A detached poll task marks its source watched here (nothing to keep alive).
    Poll(Arc<()>),
    /// Slot reserved while a local watcher is being registered off-thread. Reserving synchronously
    /// stops a second `ensure()` from double-spawning before the background setup lands.
    Pending(Arc<()>),
}

impl WatchEntry {
    fn token(&self) -> &Arc<()> {
        match self {
            Self::Local { token, .. } | Self::Poll(token) | Self::Pending(token) => token,
        }
    }
}

#[derive(Clone)]
struct Registration {
    id: SourceId,
    live: Arc<Mutex<HashMap<SourceId, WatchEntry>>>,
    token: Arc<()>,
}
impl Registration {
    fn current(&self) -> bool {
        self.live
            .lock()
            .unwrap()
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(entry.token(), &self.token))
    }
    fn publish(&self, entry: WatchEntry) -> bool {
        let mut live = self.live.lock().unwrap();
        if !live
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(entry.token(), &self.token))
        {
            return false;
        }
        live.insert(self.id, entry);
        true
    }
    fn remove(&self) {
        let mut live = self.live.lock().unwrap();
        if live
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(entry.token(), &self.token))
        {
            live.remove(&self.id);
        }
    }
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
    pub(crate) async fn trusted_clean(&self, id: SourceId) -> bool {
        if self.in_flight.lock().unwrap().contains(&id) {
            return false;
        }
        let state = {
            let live = self.live.lock().unwrap();
            match live.get(&id) {
                Some(WatchEntry::Local {
                    dirty,
                    binding: Some(binding),
                    signal,
                    root,
                    token,
                    ..
                }) => Some((
                    dirty.clone(),
                    binding.clone(),
                    signal.clone(),
                    root.clone(),
                    Registration {
                        id,
                        live: self.live.clone(),
                        token: token.clone(),
                    },
                )),
                _ => None,
            }
        };
        let Some((dirty, binding, signal, root, registration)) = state else {
            return false;
        };
        let store = self.store.clone();
        let valid = tokio::task::spawn_blocking(move || {
            binding.current()
                && matches!(store.get_source(&id), Ok(Some(info)) if info.watch && matches!(info.kind, SourceKind::LocalFs) && info.uri == root)
        }).await.unwrap_or(false);
        let mut pending = dirty.lock().unwrap();
        if !valid {
            pending.fallback();
            mark_dirty(&signal);
            return false;
        }
        let clean = pending.clean();
        drop(pending);
        registration.current() && clean && !self.in_flight.lock().unwrap().contains(&id)
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
        let registration = Registration {
            id,
            live: self.live.clone(),
            token: Arc::new(()),
        };
        // Reserve the slot under the lock so overlapping `ensure()` calls can't both spawn.
        {
            let mut live = self.live.lock().unwrap();
            if live.contains_key(&id) {
                return;
            }
            live.insert(id, WatchEntry::Pending(registration.token.clone()));
        }
        let info = match self.store.get_source(&id) {
            Ok(Some(i)) if i.watch => i,
            _ => {
                registration.remove();
                return;
            }
        };
        match info.kind {
            SourceKind::LocalFs => self.spawn_local(id, info.uri, registration),
            SourceKind::Sftp | SourceKind::Smb => {
                self.spawn_poll(id, registration);
            }
            SourceKind::Federated => {
                registration.remove();
            }
        }
    }

    /// Register a recursive OS watch for a local source **off the async runtime**. Adding a recursive
    /// inotify/FSEvents watch walks the whole subtree synchronously — seconds-to-forever on a huge or
    /// network-backed (CIFS/NFS) root — so doing it inline would stall `EmbeddedLibrary::open()` and
    /// every role with it. The slot is left `Pending` until the watcher lands (or is dropped on error).
    fn spawn_local(&self, id: SourceId, root: String, registration: Registration) {
        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let events = self.events.clone();
        let in_flight = self.in_flight.clone();
        let rt = self.rt.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch.clone();
        let coordinator = self.coordinator.clone();
        self.rt.spawn_blocking(move || {
            #[cfg(target_os = "linux")]
            let binding = match local_binding(Path::new(&root)) {
                Some(binding) => Some(binding),
                None => {
                    tracing::debug!(source = %id, "local notifications cannot cover source; polling instead");
                    spawn_poll_task(&rt, id, store, secrets, events, in_flight, governor, scratch, coordinator, registration);
                    return;
                }
            };
            // Other platforms retain notifications as hints, but without a verified filesystem
            // and root identity every hint requires full reconciliation and never a clean skip.
            #[cfg(not(target_os = "linux"))]
            let binding: Option<Arc<LocalBinding>> = None;
            // A watch channel retains one revision, not one item per callback. A 100k-file copy can
            // therefore make this counter race ahead, but it can never allocate a 100k-entry queue.
            let (tx, mut rx) = watch::channel(0_u64);
            let initial_scan = tx.clone();
            let dirty = Arc::new(Mutex::new(DirtyPaths::default()));
            let callback_dirty = dirty.clone();
            let callback_binding = binding.clone();
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
                            if let Some(binding) = &callback_binding {
                                binding.broken.store(true, Ordering::Release);
                            }
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
                            registration,
                        );
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
                    registration,
                );
                return;
            }
            if binding.as_ref().is_some_and(|binding| !binding.current()) {
                drop(watcher);
                spawn_poll_task(&rt, id, store, secrets, events, in_flight, governor, scratch, coordinator, registration);
                return;
            }

            // Reconcile once after registration. A file can be created after `add_source` returns
            // but before the off-thread OS watch is live; that event cannot be replayed by notify.
            // The initial delta is idempotent and closes that otherwise permanent missed-event gap.
            dirty.lock().unwrap().fallback();
            mark_dirty(&initial_scan);

            let task_dirty = dirty.clone();
            let task_registration = registration.clone();
            let task_binding = binding.clone();
            let task_root = root.clone();
            let task_rt = rt.clone();
            rt.spawn(async move {
                let fallback = PollPolicy::from_env().max;
                loop {
                    if !task_registration.current() {
                        break;
                    }
                    if !matches!(store.get_source(&id),Ok(Some(info)) if info.watch) {
                        let registration = task_registration.clone();
                        let _ = tokio::task::spawn_blocking(move || registration.remove()).await;
                        break;
                    }
                    let mut scopes = tokio::select! {
                        changed=wait_for_quiet(&mut rx,DEBOUNCE) => {
                            if !changed { break; }
                            task_dirty.lock().unwrap().take()
                        },
                        _=tokio::time::sleep(fallback) => {
                            task_dirty.lock().unwrap().take();
                            Vec::new()
                        },
                    };
                    let current = binding_current(task_binding.clone()).await;
                    let same_source = matches!(store.get_source(&id), Ok(Some(info)) if info.watch && matches!(info.kind, SourceKind::LocalFs) && info.uri == task_root);
                    if !task_registration.current() {
                        break;
                    }
                    if !same_source || (task_binding.is_some() && !current) {
                        task_dirty.lock().unwrap().fallback();
                        let registration = task_registration.clone();
                        let (rt, store, secrets, events, in_flight, governor, scratch, coordinator) = (task_rt.clone(), store.clone(), secrets.clone(), events.clone(), in_flight.clone(), governor.clone(), scratch.clone(), coordinator.clone());
                        let _ = tokio::task::spawn_blocking(move || spawn_poll_task(&rt, id, store, secrets, events, in_flight, governor, scratch, coordinator, registration)).await;
                        break;
                    }
                    if !current || !task_dirty.lock().unwrap().trusted {
                        scopes.clear();
                    }
                    let full = scopes.is_empty();
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
                        let current = binding_current(task_binding.clone()).await;
                        let same_source = matches!(store.get_source(&id), Ok(Some(info)) if info.watch && matches!(info.kind, SourceKind::LocalFs) && info.uri == task_root);
                        let current = current && same_source;
                        if !task_registration.current() {
                            break;
                        }
                        task_dirty.lock().unwrap().reconciled(outcome, full, current);
                        if task_binding.is_some() && !current {
                            let registration = task_registration.clone();
                            let (rt, store, secrets, events, in_flight, governor, scratch, coordinator) = (task_rt.clone(), store.clone(), secrets.clone(), events.clone(), in_flight.clone(), governor.clone(), scratch.clone(), coordinator.clone());
                            let _ = tokio::task::spawn_blocking(move || spawn_poll_task(&rt, id, store, secrets, events, in_flight, governor, scratch, coordinator, registration)).await;
                            break;
                        }
                    } else {
                        // Failed job creation or an overlapping producer must not discard the
                        // claimed dirty snapshot and expose a clean journal before reconciliation.
                        task_dirty.lock().unwrap().reconciled(ScanOutcome::default(), full, current);
                    }
                }
            });
            // Publish the live watcher, replacing the `Pending` reservation.
            registration.publish(
                WatchEntry::Local {
                    _watcher: watcher,
                    dirty,
                    binding,
                    signal: initial_scan,
                    token: registration.token.clone(),
                    root,
                },
            );
        });
    }

    fn spawn_poll(&self, id: SourceId, registration: Registration) {
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
            registration,
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
    registration: Registration,
) {
    if !registration.publish(WatchEntry::Poll(registration.token.clone())) {
        return;
    }
    rt.spawn(async move {
        let policy = PollPolicy::from_env();
        let mut interval = policy.base;
        loop {
            tokio::time::sleep(policy.delay(interval, id)).await;
            if !registration.current() {
                break;
            }
            if !matches!(store.get_source(&id),Ok(Some(info)) if info.watch) {
                registration.remove();
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

/// Submit background quick discovery for one source, unless one is already running for it.
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

    #[cfg(target_os = "linux")]
    #[test]
    fn only_known_local_filesystems_can_supply_a_trusted_notification_journal() {
        for magic in [0xef53, 0x5846_5342, 0x9123_683e, 0x0102_1994] {
            assert!(super::trusted_filesystem(magic));
        }
        // CIFS, SMB2, NFS, FUSE, overlay, 9p and an unknown future implementation.
        for magic in [
            0xff53_4d42,
            0xfe53_4d42,
            0x6969,
            0x6573_5546,
            0x794c_7630,
            0x0102_1997,
            0xdead_beef,
        ] {
            assert!(!super::trusted_filesystem(magic));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn descendant_mounts_and_unknown_covering_mounts_require_full_polling() {
        let root = std::path::Path::new("/assets");
        let local = "24 1 8:1 / / rw - ext4 /dev/sda1 rw\n";
        let original = super::covering_mount(root, local).unwrap();
        for filesystem in ["cifs", "nfs", "fuse.sshfs", "xfs"] {
            let nested = format!("{local}25 24 0:2 / /assets/shared rw - {filesystem} remote rw\n");
            assert!(super::covering_mount(root, &nested).is_none());
        }
        for filesystem in ["cifs", "nfs", "fuse.sshfs", "overlay", "unknown"] {
            let covering = format!("{local}25 24 0:2 / /assets rw - {filesystem} remote rw\n");
            assert!(super::covering_mount(root, &covering).is_none());
        }
        let sibling = format!("{local}25 24 0:2 / /assets-other/shared rw - cifs remote rw\n");
        assert_eq!(super::covering_mount(root, &sibling), Some(original));
        let escaped = format!("{local}25 24 0:2 / /assets\\040space/shared rw - cifs remote rw\n");
        assert!(super::covering_mount(std::path::Path::new("/assets space"), &escaped).is_none());
        assert!(super::covering_mount(root, "malformed mount state").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn changing_mount_identity_invalidates_an_otherwise_local_root() {
        let root = std::path::Path::new("/assets");
        let old = super::covering_mount(root, "24 1 8:1 / / rw - ext4 /dev/sda1 rw\n").unwrap();
        let changed = super::covering_mount(root, "25 1 8:1 / / rw - ext4 /dev/sda1 rw\n").unwrap();
        assert_ne!(old, changed);
        let bind = super::covering_mount(root, "24 1 8:1 / / rw - ext4 /dev/sda1 rw\n25 24 8:1 /original /assets rw - ext4 /dev/sda1 rw\n").unwrap();
        let rebound = super::covering_mount(root, "24 1 8:1 / / rw - ext4 /dev/sda1 rw\n25 24 8:1 /replacement /assets rw - ext4 /dev/sda1 rw\n").unwrap();
        assert_ne!(bind, rebound);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stacked_covering_mounts_are_rejected_regardless_of_record_order() {
        let root = std::path::Path::new("/assets");
        let base = "24 1 8:1 / / rw - ext4 /dev/sda1 rw\n";
        let stacked = "25 1 8:2 / / rw - ext4 /dev/sdb1 rw\n";
        let nearer = "26 24 8:1 /subtree /assets rw - ext4 /dev/sda1 rw\n";
        for records in [
            format!("{base}{nearer}{stacked}"),
            format!("{base}{stacked}{nearer}"),
            format!("{nearer}{base}{stacked}"),
        ] {
            assert!(super::covering_mount(root, &records).is_none());
        }
        let descendant_bind =
            format!("{base}25 24 8:1 /other /assets/bind rw - ext4 /dev/sda1 rw\n");
        assert!(super::covering_mount(root, &descendant_bind).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replacing_a_root_cannot_retrust_the_watcher_bound_to_its_old_inode() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let (held, before) = super::root_handle(&root).unwrap();
        assert_eq!(before, super::root_handle(&root).unwrap().1);
        let mut pending = super::DirtyPaths::default();
        let healthy = super::ScanOutcome {
            changed: false,
            healthy: true,
        };
        pending.reconciled(healthy, true, true);
        assert!(pending.trusted);
        std::fs::rename(&root, root.with_file_name("old-root")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let (_, replacement) = super::root_handle(&root).unwrap();
        assert_ne!(before, replacement);
        let pinned = held.metadata().unwrap();
        assert_eq!((pinned.dev(), pinned.ino()), (before.device, before.inode));
        // A successful scan of the replacement directory does not repair the old subscription.
        pending.reconciled(healthy, true, before == replacement);
        assert!(!pending.trusted);
        assert!(pending.full);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_and_symlinked_root_components_cannot_pass_identity_probes() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let actual = base.join("actual");
        std::fs::create_dir_all(actual.join("child")).unwrap();
        assert!(super::root_handle(&base.join("missing")).is_none());
        symlink(&actual, base.join("alias")).unwrap();
        assert!(super::root_handle(&base.join("alias")).is_none());
        assert!(super::root_handle(&base.join("alias/child")).is_none());
    }

    #[test]
    fn hint_only_and_partial_reconciliation_never_establish_new_trust() {
        let healthy = super::ScanOutcome {
            changed: false,
            healthy: true,
        };
        let mut pending = super::DirtyPaths::default();
        pending.reconciled(healthy, false, true);
        assert!(!pending.trusted);
        pending.reconciled(healthy, true, false);
        assert!(!pending.trusted);
        assert!(pending.full);
        pending.take();
        pending.reconciled(healthy, true, true);
        assert!(pending.trusted);
        pending.reconciled(super::ScanOutcome::default(), true, true);
        assert!(!pending.trusted);
    }

    #[test]
    fn claimed_dirty_work_stays_unclean_until_reconciliation_finishes() {
        let healthy = super::ScanOutcome {
            changed: false,
            healthy: true,
        };
        let mut pending = super::DirtyPaths::default();
        pending.reconciled(healthy, true, true);
        assert!(pending.clean());
        pending.add(
            std::path::Path::new("/assets"),
            std::path::Path::new("/assets/changed.png"),
        );
        assert!(!pending.clean());
        assert_eq!(pending.take(), vec!["changed.png"]);
        assert!(pending.paths.is_empty());
        assert!(
            !pending.clean(),
            "binding probes must not expose claimed work as clean"
        );
        pending.reconciled(healthy, false, true);
        assert!(pending.clean());
        pending.take();
        assert!(
            !pending.clean(),
            "periodic full reconciliation is pending too"
        );
        pending.reconciled(super::ScanOutcome::default(), true, true);
        assert!(!pending.clean());
        assert!(
            pending.full,
            "failed admission must retain authoritative fallback"
        );
    }

    #[test]
    fn an_old_registration_cannot_publish_or_remove_a_new_watcher_slot() {
        let id = dam_api::id::SourceId::new();
        let live = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let old = super::Registration {
            id,
            live: live.clone(),
            token: std::sync::Arc::new(()),
        };
        let new = super::Registration {
            id,
            live: live.clone(),
            token: std::sync::Arc::new(()),
        };
        live.lock()
            .unwrap()
            .insert(id, super::WatchEntry::Pending(new.token.clone()));
        assert!(!old.current());
        assert!(!old.publish(super::WatchEntry::Poll(old.token.clone())));
        old.remove();
        assert!(new.current());
        assert!(new.publish(super::WatchEntry::Poll(new.token.clone())));
        old.remove();
        assert!(new.current());
        new.remove();
        assert!(!new.current());
    }

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
