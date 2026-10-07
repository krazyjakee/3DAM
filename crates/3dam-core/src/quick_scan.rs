//! Discovery-only scans and durable, separately scheduled byte verification.
//! A Quick job never calls fetch, content_stat, hashing, or a media parser.

use crate::{emit_progress, reliability};
use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent};
use dam_api::id::{ContentHash, JobId, SourceId};
use dam_api::LibError;
use dam_sources::{FileEntry, FileSource};
use dam_store::{NewAsset, PendingCommitOutcome, PendingDiscovery, PendingIngest, Store};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const CHUNK: usize = 128;
const REPORT_INTERVAL: Duration = Duration::from_millis(200);

/// Explicit target for discovery: one streamed listing and no changed payload
/// reads; on a healthy local SSD with a warm 10k-path tree, aim for <=2s excluding pressure
/// admission. Remote latency scales with directory pages and token requests.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_quick_scan(
    store: Arc<Store>,
    secrets: crate::credentials::SecretVault,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    sources: Vec<SourceInfo>,
    cancel: Arc<AtomicBool>,
    governor: &crate::resources::Governor,
    scratch: &Path,
    coordinator: &crate::scan_admission::Coordinator,
    manual: bool,
    scopes: &[String],
    source_override: Option<Arc<dyn FileSource>>,
) -> Result<crate::scan::ScanOutcome, LibError> {
    if source_override.is_some() && sources.len() != 1 {
        return Err(LibError::BadRequest(
            "a source override requires one source".into(),
        ));
    }
    let scopes = normalise_scopes(scopes)?;
    db_admission(&store, governor, &cancel, manual, 4096)?;
    store.update_job_progress(&job, JobState::Running, 0, None, None)?;
    let mut done = 0;
    let mut queued = 0;
    let mut removed = 0;
    let mut warnings = Vec::new();
    let mut was_cancelled = false;
    for source in sources {
        if source.kind == SourceKind::Federated {
            continue;
        }
        let Some(_lease) = coordinator.acquire(source.id, manual, &cancel) else {
            was_cancelled = true;
            break;
        };
        db_admission(&store, governor, &cancel, manual, 4096)?;
        let backend = match source_override.clone().map(Ok).unwrap_or_else(|| {
            let connection = secrets.resolve(store.get_source_connection(&source.id)?)?;
            dam_sources::open_source(&connection, scratch).map(Arc::from)
        }) {
            Ok(backend) => backend,
            Err(error) => {
                warn(
                    &mut warnings,
                    format!("Source “{}” could not be opened", source.name),
                );
                store.set_source_error(&source.id, &error.to_string())?;
                continue;
            }
        };
        db_admission(&store, governor, &cancel, manual, 4096)?;
        let generation = store.begin_source_scan(&source.id)?;
        store.set_source_scan_scope(&source.id, generation, &scopes)?;
        reliability::publish_event(
            &events,
            LibraryEvent::SourceState {
                id: source.id,
                state: SourceState::Scanning,
            },
            "quick discovery started",
        );
        let mut chunk = DiscoveryChunk {
            store: &store,
            events: &events,
            job,
            source: source.id,
            generation,
            cancel: &cancel,
            governor,
            manual,
            pending: Vec::with_capacity(CHUNK),
            done,
            queued,
            last_flush: Instant::now(),
            last_progress: Instant::now(),
            error: None,
        };
        let mut incomplete = false;
        let mut eligible = |path: &str| {
            if dam_media::detect_for_ingest(Path::new(path)).is_some() {
                return Ok(true);
            }
            db_admission(&store, governor, &cancel, manual, 4096)?;
            store.source_path_exists(&source.id, path)
        };
        let mut pace = || {
            governor
                .io
                .acquire_metadata(governor, &[backend.storage_path()], cancel.as_ref(), manual)?
                .pace_metadata()
        };
        let result = backend.walk_scoped(&scopes,&mut eligible,&mut pace,&mut |entry| {
            if cancel.load(Ordering::Relaxed) || chunk.error.is_some() { return false; }
            match entry {
                Ok(entry) => chunk.push(entry),
                Err(error) => { incomplete=true; tracing::warn!(source=%source.id,error=%error,"quick listing entry unavailable"); }
            }
            true
        });
        chunk.flush();
        done = chunk.done;
        queued = chunk.queued;
        let chunk_error = chunk.error.take();
        drop(chunk);
        let cancelled = cancel.load(Ordering::Relaxed)
            || store.get_job_summary(&job)?.state == JobState::Cancelled;
        was_cancelled |= cancelled;
        if cancelled {
            break;
        }
        if result.is_err() || incomplete || chunk_error.is_some() {
            warn(
                &mut warnings,
                format!(
                    "Source “{}” discovery was incomplete; missing paths were preserved",
                    source.name
                ),
            );
            if let Some(error) = chunk_error.or_else(|| result.err()) {
                db_admission(&store, governor, &cancel, manual, 4096)?;
                store.set_source_error(&source.id, &error.to_string())?;
            }
            reliability::publish_event(
                &events,
                LibraryEvent::SourceState {
                    id: source.id,
                    state: SourceState::Error("Discovery incomplete".into()),
                },
                "quick discovery incomplete",
            );
            continue;
        }
        db_admission(&store, governor, &cancel, manual, 4096)?;
        match store.finish_source_scan(&source.id, generation, dam_store::now_ms())? {
            Some(missing) => {
                removed += missing;
                db_admission(&store, governor, &cancel, manual, 4096)?;
                if !store.finish_pending_discovery(&source.id, generation, &scopes)? {
                    warn(
                        &mut warnings,
                        "Quick discovery was superseded before pending reconciliation".into(),
                    );
                }
            }
            None => warn(
                &mut warnings,
                "Quick discovery was superseded; missing paths were preserved".into(),
            ),
        }
        reliability::publish_event(
            &events,
            LibraryEvent::SourceState {
                id: source.id,
                state: SourceState::Online,
            },
            "quick discovery finished",
        );
    }
    let state = if was_cancelled || cancel.load(Ordering::Relaxed) {
        JobState::Cancelled
    } else {
        JobState::Done
    };
    if state == JobState::Done {
        db_admission(&store, governor, &cancel, manual, 4096)?;
    }
    store.update_job_progress(
        &job,
        if state == JobState::Done {
            JobState::Running
        } else {
            state
        },
        done,
        Some(done),
        None,
    )?;
    store.finish_job(&job,state,&format!("Discovered {done} paths; queued {queued} revisions for verification; {removed} missing"),&warnings,None)?;
    emit_progress(&store, &events, &job);
    tracing::info!(%job,paths=done,queued,missing=removed,source_bytes=0,"quick discovery finished without payload reads");
    Ok(crate::scan::ScanOutcome {
        changed: queued > 0 || removed > 0,
        healthy: warnings.is_empty() && state == JobState::Done,
    })
}

struct DiscoveryChunk<'a> {
    store: &'a Store,
    events: &'a broadcast::Sender<LibraryEvent>,
    job: JobId,
    source: SourceId,
    generation: i64,
    cancel: &'a AtomicBool,
    governor: &'a crate::resources::Governor,
    manual: bool,
    pending: Vec<PendingDiscovery>,
    done: u64,
    queued: u64,
    last_flush: Instant,
    last_progress: Instant,
    error: Option<LibError>,
}

impl DiscoveryChunk<'_> {
    fn push(&mut self, entry: FileEntry) {
        let detected = dam_media::detect_for_ingest(Path::new(&entry.rel_path));
        self.pending.push(PendingDiscovery {
            path: entry.rel_path,
            size: entry.size,
            modified_ms: entry.modified_ms,
            media: detected.as_ref().map(|det| det.media),
            format: detected.map(|det| det.format),
        });
        if self.pending.len() >= CHUNK || self.last_flush.elapsed() >= REPORT_INTERVAL {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.pending.is_empty() || self.error.is_some() || self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let entries = std::mem::take(&mut self.pending);
        let result = (|| {
            db_admission(
                self.store,
                self.governor,
                self.cancel,
                self.manual,
                (entries.len() as u64 * 256).max(4096),
            )?;
            let outcome =
                self.store
                    .apply_quick_discovery(&self.source, self.generation, &entries)?;
            if !outcome.generation_current {
                return Err(LibError::Conflict("quick discovery superseded".into()));
            }
            self.done += outcome.examined;
            self.queued += outcome.queued;
            for id in outcome.invalidated {
                reliability::publish_event(
                    self.events,
                    LibraryEvent::AssetChanged {
                        id,
                        source_id: Some(self.source),
                        kind: ChangeKind::Metadata,
                    },
                    "pending revision invalidated old metadata",
                );
            }
            // Unchanged chunks only update the disposable observation spool. Persist
            // progress by elapsed time, so a fast no-op pass cannot rewrite the catalog
            // job row once per 128 paths. Newly queued revisions still report promptly.
            if outcome.queued > 0 || self.last_progress.elapsed() >= REPORT_INTERVAL {
                db_admission(self.store, self.governor, self.cancel, self.manual, 4096)?;
                self.store.update_job_progress(
                    &self.job,
                    JobState::Running,
                    self.done,
                    None,
                    entries.last().map(|entry| entry.path.as_str()),
                )?;
                emit_progress(self.store, self.events, &self.job);
                self.last_progress = Instant::now();
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error);
        }
        self.last_flush = Instant::now();
    }
}

/// Resume durable pending revisions independently of discovery. Release source
/// admission across payload reads so a Quick job can finish while this hashes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pending_ingest(
    store: Arc<Store>,
    secrets: crate::credentials::SecretVault,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    sources: Vec<SourceInfo>,
    cancel: Arc<AtomicBool>,
    governor: &crate::resources::Governor,
    scratch: &Path,
    coordinator: &crate::scan_admission::Coordinator,
    manual: bool,
    scopes: &[String],
) -> Result<(), LibError> {
    let scopes = normalise_scopes(scopes)?;
    db_admission(&store, governor, &cancel, manual, 4096)?;
    store.update_job_progress(&job, JobState::Running, 0, None, None)?;
    let mut done = 0;
    let mut warnings = Vec::new();
    let mut cancelled = false;
    for source in sources {
        if source.kind == SourceKind::Federated {
            continue;
        }
        let Some(lease) = coordinator.acquire(source.id, manual, &cancel) else {
            cancelled = true;
            break;
        };
        db_admission(&store, governor, &cancel, manual, 4096)?;
        let backend = match secrets
            .resolve(store.get_source_connection(&source.id)?)
            .and_then(|connection| dam_sources::open_source(&connection, scratch))
        {
            Ok(backend) => backend,
            Err(error) => {
                warn(
                    &mut warnings,
                    format!(
                        "Source “{}” is unavailable; verification remains queued",
                        source.name
                    ),
                );
                store.set_source_error(&source.id, &error.to_string())?;
                continue;
            }
        };
        drop(lease);
        loop {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            db_admission(&store, governor, &cancel, manual, 4096)?;
            let targets = store.list_pending_ingest_scoped(&source.id, CHUNK, &scopes)?;
            if targets.is_empty() {
                break;
            }
            let mut matched = false;
            for target in targets {
                if !scope_contains(&target.path, &scopes) {
                    continue;
                }
                matched = true;
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                db_admission(&store, governor, &cancel, manual, 4096)?;
                store.update_job_progress(
                    &job,
                    JobState::Running,
                    done,
                    None,
                    Some(&target.path),
                )?;
                let heartbeat =
                    HeartbeatCancel::new(&cancel, &events, &store, store.get_job_summary(&job)?);
                let prepared = prepare_pending(
                    backend.as_ref(),
                    &target,
                    governor,
                    scratch,
                    &heartbeat,
                    manual,
                );
                let (asset, attrs) = match prepared {
                    Ok(Prepared::Ready(asset, attrs)) => (asset, attrs),
                    Ok(Prepared::Changed(stat)) => {
                        db_admission(&store, governor, &cancel, manual, 4096)?;
                        store.refresh_pending_ingest_token(&target, stat.len, stat.modified_ms)?;
                        continue;
                    }
                    Err(error) => {
                        if cancel.load(Ordering::Relaxed) {
                            cancelled = true;
                            break;
                        }
                        db_admission(&store, governor, &cancel, manual, 4096)?;
                        store.retry_pending_ingest(&target, &error.to_string())?;
                        warn(
                            &mut warnings,
                            format!("“{}” remains queued for verification", target.path),
                        );
                        continue;
                    }
                };
                let Some(_lease) = coordinator.acquire(source.id, manual, &cancel) else {
                    cancelled = true;
                    break;
                };
                db_admission(&store, governor, &cancel, manual, 4096)?;
                match store.commit_pending_ingest(&job, &target, &asset, &attrs)? {
                    PendingCommitOutcome::Written { id, inserted } => {
                        done += 1;
                        if inserted {
                            let asset = store.get_asset(&id)?;
                            reliability::publish_event(
                                &events,
                                LibraryEvent::AssetAdded(asset.summary),
                                "verified pending asset admitted",
                            );
                        } else {
                            reliability::publish_event(
                                &events,
                                LibraryEvent::AssetChanged {
                                    id,
                                    source_id: Some(source.id),
                                    kind: ChangeKind::Metadata,
                                },
                                "pending revision verified",
                            );
                        }
                    }
                    PendingCommitOutcome::Blocked { removed_asset } => {
                        done += 1;
                        if let Some(id) = removed_asset {
                            reliability::publish_event(
                                &events,
                                LibraryEvent::AssetRemoved {
                                    id,
                                    source_id: Some(source.id),
                                },
                                "blocked pending revision removed",
                            );
                        }
                    }
                    PendingCommitOutcome::Stale => {
                        store.retry_pending_ingest(
                            &target,
                            "verification superseded by discovery",
                        )?;
                    }
                    PendingCommitOutcome::Cancelled => {
                        cancelled = true;
                        break;
                    }
                }
                db_admission(&store, governor, &cancel, manual, 4096)?;
                store.update_job_progress(
                    &job,
                    JobState::Running,
                    done,
                    None,
                    Some(&target.path),
                )?;
                emit_progress(&store, &events, &job);
            }
            if cancelled || !matched {
                break;
            }
        }
        if cancelled {
            break;
        }
    }
    let state = if cancelled || cancel.load(Ordering::Relaxed) {
        JobState::Cancelled
    } else {
        JobState::Done
    };
    if state == JobState::Done {
        db_admission(&store, governor, &cancel, manual, 4096)?;
    }
    store.update_job_progress(
        &job,
        if state == JobState::Done {
            JobState::Running
        } else {
            state
        },
        done,
        Some(done),
        None,
    )?;
    store.finish_job(
        &job,
        state,
        &format!("Verified {done} pending revisions; retryable work remains durable"),
        &warnings,
        None,
    )?;
    emit_progress(&store, &events, &job);
    Ok(())
}

enum Prepared {
    Ready(NewAsset, Box<MediaAttributes>),
    Changed(dam_sources::ContentStat),
}

fn prepare_pending(
    source: &dyn FileSource,
    target: &PendingIngest,
    governor: &crate::resources::Governor,
    scratch: &Path,
    cancel: &HeartbeatCancel<'_>,
    manual: bool,
) -> Result<Prepared, LibError> {
    let before = source_stat(source, &target.path, governor, cancel, manual)?;
    if !stat_matches(target, before) {
        return Ok(Prepared::Changed(before));
    }
    cancel.phase("Reading", &target.path);
    let (fetched, work) = governor.fetch(source, &target.path, scratch, cancel)?;
    let captured = fetched.source_stat();
    if let Some(captured) = captured {
        if !stat_matches(target, captured) {
            return Ok(Prepared::Changed(captured));
        }
    }
    drop(work);
    let hash = match fetched.content_hash().and_then(ContentHash::from_hex) {
        Some(hash) => hash,
        None => {
            cancel.phase("Verifying", &target.path);
            hash_paced(fetched.path(), governor, cancel)?
        }
    };
    cancel.phase("Inspecting", &target.path);
    let detected = dam_media::Detected {
        media: target.media,
        format: target.format.clone(),
    };
    let work = governor
        .io
        .acquire(governor, &[Some(fetched.path())], cancel)?;
    let cancelled = || crate::resources::Cancellation::cancelled(cancel);
    let pace = |bytes: usize| work.pace(bytes as u64).is_ok();
    let budget = dam_media::MetadataBudget {
        cancelled: &cancelled,
        before_read: &pace,
        ..dam_media::MetadataBudget::default()
    };
    let metadata = dam_media::extract_ingest_metadata(fetched.path(), &detected, &budget);
    drop(work);
    let after = source_stat(source, &target.path, governor, cancel, manual)?;
    if before != after {
        return Ok(Prepared::Changed(after));
    }
    fetched.verify_unchanged()?;
    if crate::resources::Cancellation::cancelled(cancel) {
        return Err(LibError::Cancelled);
    }
    Ok(Prepared::Ready(
        NewAsset {
            source_id: target.source_id,
            path: target.path.clone(),
            filename: crate::scan::file_name(&target.path),
            content_hash: Some(hash),
            size_bytes: Some(target.size as i64),
            source_modified_at: target.modified_ms,
            scanned_at: dam_store::now_ms(),
            media_type: metadata.detected.media,
            format: metadata.detected.format,
        },
        Box::new(metadata.attributes),
    ))
}

fn hash_paced(
    path: &Path,
    governor: &crate::resources::Governor,
    cancel: &dyn crate::resources::Cancellation,
) -> Result<ContentHash, LibError> {
    use std::io::Read;
    let work = governor.io.acquire(governor, &[Some(path)], cancel)?;
    let mut file = std::fs::File::open(path).map_err(dam_api::internal)?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes = vec![0; crate::resources::io::CHUNK];
    loop {
        work.pace(bytes.len() as u64)?;
        let n = file.read(&mut bytes).map_err(dam_api::internal)?;
        if n == 0 {
            break;
        }
        hasher.update(&bytes[..n]);
    }
    Ok(ContentHash(*hasher.finalize().as_bytes()))
}

fn source_stat(
    source: &dyn FileSource,
    path: &str,
    governor: &crate::resources::Governor,
    cancel: &dyn crate::resources::Cancellation,
    manual: bool,
) -> Result<dam_sources::ContentStat, LibError> {
    let work = governor
        .io
        .acquire_metadata(governor, &[source.storage_path()], cancel, manual)?;
    work.pace_metadata()?;
    source.content_stat(path)
}

fn stat_matches(target: &PendingIngest, stat: dam_sources::ContentStat) -> bool {
    target.size == stat.len && target.modified_ms == stat.modified_ms
}

fn db_admission(
    store: &Store,
    governor: &crate::resources::Governor,
    cancel: &AtomicBool,
    manual: bool,
    bytes: u64,
) -> Result<(), LibError> {
    governor
        .io
        .acquire_metadata(governor, &[store.storage_path()], cancel, manual)?
        .pace(bytes)
}

struct Heartbeat {
    status: JobStatus,
    last: Instant,
}
struct HeartbeatCancel<'a> {
    cancel: &'a AtomicBool,
    events: &'a broadcast::Sender<LibraryEvent>,
    store: &'a Store,
    state: Mutex<Heartbeat>,
}
impl<'a> HeartbeatCancel<'a> {
    fn new(
        cancel: &'a AtomicBool,
        events: &'a broadcast::Sender<LibraryEvent>,
        store: &'a Store,
        status: JobStatus,
    ) -> Self {
        Self {
            cancel,
            events,
            store,
            state: Mutex::new(Heartbeat {
                status,
                last: Instant::now(),
            }),
        }
    }
    fn phase(&self, phase: &str, path: &str) {
        let mut state = self.state.lock().unwrap();
        state.status.progress.current = Some(format!("{phase}: {path}"));
        state.last = Instant::now();
        reliability::publish_event(
            self.events,
            LibraryEvent::JobProgress(state.status.clone()),
            "pending verification phase",
        );
    }
}
impl crate::resources::Cancellation for HeartbeatCancel<'_> {
    fn cancelled(&self) -> bool {
        if self.cancel.load(Ordering::Relaxed) {
            return true;
        }
        let mut state = self.state.lock().unwrap();
        if state.last.elapsed() >= REPORT_INTERVAL {
            state.last = Instant::now();
            // Payload work holds no catalog guard. A read-only cancellation check
            // avoids re-entering device admission from inside its pace callback.
            if self
                .store
                .get_job_summary(&state.status.id)
                .is_ok_and(|job| job.state == JobState::Cancelled)
            {
                self.cancel.store(true, Ordering::Relaxed);
                return true;
            }
            reliability::publish_event(
                self.events,
                LibraryEvent::JobProgress(state.status.clone()),
                "pending byte work heartbeat",
            );
        }
        false
    }
}

fn warn(warnings: &mut Vec<String>, message: String) {
    if warnings.len() < 20 {
        warnings.push(message);
    }
}
fn scope_contains(path: &str, scopes: &[String]) -> bool {
    scopes.is_empty()
        || scopes.iter().any(|scope| {
            scope.is_empty()
                || path == scope
                || path
                    .strip_prefix(scope)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
}
fn normalise_scopes(scopes: &[String]) -> Result<Vec<String>, LibError> {
    scopes
        .iter()
        .map(|scope| {
            let scope = scope.replace('\\', "/");
            if scope.starts_with('/')
                || scope
                    .split('/')
                    .any(|segment| segment == ".." || segment == ".")
                || scope.contains(':')
            {
                return Err(LibError::BadRequest(
                    "invalid source discovery scope".into(),
                ));
            }
            Ok(scope.trim_end_matches('/').to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct ListingOnly {
        entries: Vec<FileEntry>,
        fetches: AtomicUsize,
        stats: AtomicUsize,
        walks: AtomicUsize,
        incomplete: bool,
    }
    impl FileSource for ListingOnly {
        fn walk(
            &self,
            sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
        ) -> Result<(), LibError> {
            self.walks.fetch_add(1, Ordering::Relaxed);
            for entry in &self.entries {
                if !sink(Ok(entry.clone())) {
                    break;
                }
            }
            if self.incomplete {
                sink(Err(LibError::SourceUnavailable("listing gap".into())));
            }
            Ok(())
        }
        fn fetch(&self, _: &str) -> Result<dam_sources::Fetched, LibError> {
            self.fetches.fetch_add(1, Ordering::Relaxed);
            Err(LibError::Internal(
                "discovery must never fetch payloads".into(),
            ))
        }
        fn content_stat(&self, _: &str) -> Result<dam_sources::ContentStat, LibError> {
            self.stats.fetch_add(1, Ordering::Relaxed);
            Err(LibError::Internal(
                "discovery must only use listed tokens".into(),
            ))
        }
    }
    fn source(store: &Store, root: &Path) -> SourceInfo {
        store
            .add_source(
                &dam_sources::SourceConnection::LocalFs {
                    root: root.to_string_lossy().into_owned(),
                },
                "test",
                false,
            )
            .unwrap();
        store.list_sources().unwrap().remove(0)
    }
    fn governor() -> crate::resources::Governor {
        crate::resources::Governor::new(Some(0), Some(100.0))
    }
    fn coordinator() -> crate::scan_admission::Coordinator {
        crate::scan_admission::Coordinator::new(Arc::default())
    }

    #[test]
    fn multi_gigabyte_discovery_completes_without_source_payload_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let source = source(&store, dir.path());
        let fake = Arc::new(ListingOnly {
            entries: vec![FileEntry {
                rel_path: "huge.mp4".into(),
                size: 8 * 1024 * 1024 * 1024,
                modified_ms: Some(10),
            }],
            fetches: AtomicUsize::new(0),
            stats: AtomicUsize::new(0),
            walks: AtomicUsize::new(0),
            incomplete: false,
        });
        let job = store
            .create_job(JobKind::Scan, r#"{"mode":"quick"}"#, None, &[source.id])
            .unwrap();
        let (events, _) = broadcast::channel(32);
        let outcome = run_quick_scan(
            store.clone(),
            crate::credentials::SecretVault::memory(),
            events,
            job,
            vec![source.clone()],
            Arc::new(AtomicBool::new(false)),
            &governor(),
            dir.path(),
            &coordinator(),
            true,
            &[],
            Some(fake.clone()),
        )
        .unwrap();
        assert!(outcome.healthy && outcome.changed);
        assert_eq!(fake.fetches.load(Ordering::Relaxed), 0);
        assert_eq!(fake.stats.load(Ordering::Relaxed), 0);
        assert_eq!(fake.walks.load(Ordering::Relaxed), 1);
        assert_eq!(store.get_job(&job).unwrap().state, JobState::Done);
        assert_eq!(store.pending_ingest_count(&source.id).unwrap(), 1);
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 0);
    }

    #[test]
    fn incomplete_listing_never_prunes_previously_pending_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let source = source(&store, dir.path());
        let generation = store.begin_source_scan(&source.id).unwrap();
        store
            .apply_quick_discovery(
                &source.id,
                generation,
                &[PendingDiscovery {
                    path: "unseen.png".into(),
                    size: 10,
                    modified_ms: Some(1),
                    media: Some(MediaType::Image),
                    format: Some("png".into()),
                }],
            )
            .unwrap();
        let fake = Arc::new(ListingOnly {
            entries: vec![FileEntry {
                rel_path: "seen.png".into(),
                size: 10,
                modified_ms: Some(2),
            }],
            fetches: AtomicUsize::new(0),
            stats: AtomicUsize::new(0),
            walks: AtomicUsize::new(0),
            incomplete: true,
        });
        let job = store
            .create_job(JobKind::Scan, "{}", None, &[source.id])
            .unwrap();
        let (events, _) = broadcast::channel(32);
        let outcome = run_quick_scan(
            store.clone(),
            crate::credentials::SecretVault::memory(),
            events,
            job,
            vec![source.clone()],
            Arc::new(AtomicBool::new(false)),
            &governor(),
            dir.path(),
            &coordinator(),
            true,
            &[],
            Some(fake),
        )
        .unwrap();
        assert!(!outcome.healthy);
        assert!(store
            .pending_ingest_for_path(&source.id, "unseen.png")
            .unwrap()
            .is_some());
        assert!(store
            .pending_ingest_for_path(&source.id, "seen.png")
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_cancelled_discovery_does_not_reconcile_missing_pending_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let source = source(&store, dir.path());
        let generation = store.begin_source_scan(&source.id).unwrap();
        store
            .apply_quick_discovery(
                &source.id,
                generation,
                &[PendingDiscovery {
                    path: "unseen.png".into(),
                    size: 10,
                    modified_ms: Some(1),
                    media: Some(MediaType::Image),
                    format: Some("png".into()),
                }],
            )
            .unwrap();
        let job = store
            .create_job(JobKind::Scan, "{}", None, &[source.id])
            .unwrap();
        let (events, _) = broadcast::channel(32);
        let outcome = run_quick_scan(
            store.clone(),
            crate::credentials::SecretVault::memory(),
            events,
            job,
            vec![source.clone()],
            Arc::new(AtomicBool::new(true)),
            &governor(),
            dir.path(),
            &coordinator(),
            true,
            &[],
            None,
        );
        // Admission cancellation can return before the runner owns its source;
        // regardless, it cannot infer absence or consume the durable work.
        assert!(outcome.is_err() || !outcome.unwrap().healthy);
        assert!(store
            .pending_ingest_for_path(&source.id, "unseen.png")
            .unwrap()
            .is_some());
    }

    #[test]
    fn scoped_membership_uses_whole_path_components() {
        let scopes = normalise_scopes(&["Art/".into()]).unwrap();
        assert!(scope_contains("Art/a.png", &scopes));
        assert!(!scope_contains("Artist/a.png", &scopes));
        assert!(normalise_scopes(&["../outside".into()]).is_err());
        assert!(normalise_scopes(&["/absolute".into()]).is_err());
    }

    #[test]
    fn local_pending_verification_admits_hashed_attributes_after_discovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "alpha beta gamma").unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let source = source(&store, dir.path());
        let governor = governor();
        let coordinator = coordinator();
        let cancel = Arc::new(AtomicBool::new(false));
        let (events, _) = broadcast::channel(64);
        let quick = store
            .create_job(JobKind::Scan, r#"{"mode":"quick"}"#, None, &[source.id])
            .unwrap();
        run_quick_scan(
            store.clone(),
            crate::credentials::SecretVault::memory(),
            events.clone(),
            quick,
            vec![source.clone()],
            cancel.clone(),
            &governor,
            dir.path(),
            &coordinator,
            true,
            &[],
            None,
        )
        .unwrap();
        assert_eq!(store.pending_ingest_count(&source.id).unwrap(), 1);
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 0);
        let enrich = store
            .create_job(JobKind::Enrich, "{}", None, &[source.id])
            .unwrap();
        run_pending_ingest(
            store.clone(),
            crate::credentials::SecretVault::memory(),
            events,
            enrich,
            vec![source.clone()],
            cancel,
            &governor,
            dir.path(),
            &coordinator,
            false,
            &[],
        )
        .unwrap();
        assert_eq!(store.pending_ingest_count(&source.id).unwrap(), 0);
        assert_eq!(store.list_sources().unwrap()[0].stats.asset_count, 1);
        assert_eq!(store.get_job(&enrich).unwrap().progress.done, 1);
    }
}
