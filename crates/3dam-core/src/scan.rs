//! The scan job: for each file source, walk it (local FS / SFTP / SMB behind one trait), resolve
//! each entry's bytes to a local path, hash (BLAKE3), detect media, upsert the row, and emit live
//! events + job progress. Runs on a blocking thread (tech-spec 14: DB + file I/O off the async
//! runtime). Fail-soft: an unreadable file degrades that item, an unreachable source pauses it,
//! never the whole job.
//!
//! Two modes (tech-spec 07 §2.1–§2.2). **Full** re-opens every file. **Delta** compares each entry's
//! cheap change token (size + mtime) to the stored row and only opens bytes for new/changed files;
//! either way, files that vanished are marked absent (non-destructive), never deleted.
//!
//! Persistence is **chunked** (issue #138): the walk fills a bounded buffer instead of writing per
//! file, and each flush is one probe transaction, then the per-entry file work with no store call in
//! it, then one write transaction carrying the whole chunk's rows *and* the job's progress. Events
//! are published from what that transaction returns, so `AssetAdded` still means "the row is there"
//! — it just arrives in bursts. See [`ScanChunker`].

use crate::{emit_progress, reliability};
use dam_api::dto::*;
use dam_api::event::LibraryEvent;
use dam_api::id::{JobId, SourceId};
use dam_api::LibError;
use dam_sources::{open_source, FileEntry};
use dam_store::{NewAsset, ScanBatchContext, ScanItemOutcome, ScanWrite, SourceChangeToken, Store};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// How many scanned rows ride in one write transaction (issue #138).
///
/// This is a **writer-lock-hold** bound, not a memory bound. A batch holds the single writer for
/// its whole duration, so the number that matters is the p99 wall time of one `apply_scan_batch`,
/// targeted under ~50 ms — comfortably where 128 header-only upserts land, while still amortising
/// the WAL frame flush across a hundred-odd assets instead of paying it per asset. Everything else
/// that writes queues behind the batch, and not all of those are themselves batched:
/// `Store::repair_aggregates` opens an *Immediate* transaction and would simply wait out a batch
/// that ran long, as would a favourite toggle from a user's click. Sizing this by "how many rows
/// fit in memory" would optimise the wrong resource.
///
/// The visible consequence, and the reason it is not larger: `AssetAdded` now arrives in bursts of
/// up to this many, one burst per commit, rather than one event per file.
const SCAN_CHUNK: usize = 128;

/// The age half of the flush rule: a chunk open for this long commits however few rows it holds.
///
/// A row-count-only bound would be a *throughput* bound, and scan throughput is set by the source,
/// not by us. A slow SFTP/SMB share delivers a few entries per second, so filling 128 rows can take
/// minutes — minutes of rows uncommitted, no `AssetAdded`, and no progress event, i.e. a frozen
/// grid and a stuck progress bar for a scan that is working perfectly. 200 ms still reads as live
/// to a human, and is long enough that a fast local walk fills whole chunks anyway.
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

const MAX_JOB_WARNING_DETAILS: usize = 20;

fn record_warning(total: &mut u64, details: &mut Vec<String>, message: String) {
    *total += 1;
    if details.len() < MAX_JOB_WARNING_DETAILS {
        details.push(message);
    }
}

fn reconciliation_is_authoritative(
    cancelled: bool,
    markers_complete: bool,
    listing_incomplete: bool,
) -> bool {
    !cancelled && markers_complete && !listing_incomplete
}

/// The one and only source enumeration call for a scan pass. Kept as a narrow seam so remote
/// backends have a regression test proving progress planning never adds a second network walk.
fn enumerate_source(
    source: &dyn dam_sources::FileSource,
    sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
) -> Result<(), LibError> {
    source.walk(sink)
}

/// Counters and warnings for one whole scan job, across every source it touches.
#[derive(Default)]
struct ScanTally {
    /// Detectable entries examined — the progress denominator, and deliberately *not* the number
    /// of rows written: a delta re-scan is mostly unchanged files, and a bar that only moved for
    /// re-ingested bytes would sit at zero through an entire healthy re-scan.
    examined: u64,
    /// Rows that actually landed (insert or reconcile).
    committed: u64,
    /// What has been reported as persisted progress, and may be reported again: examined entries
    /// whose durable effect is already committed, plus the ones that legitimately never had a row
    /// (delta skips, blocked hashes, unreadable files). An entry counts here only *after* the
    /// transaction carrying it committed, so persisted progress can never describe rows that are
    /// still in flight or that rolled back.
    settled: u64,
    skipped: u64,
    /// Items whose write failed. They stay out of `settled` on purpose — a failed row is a
    /// warning, not progress.
    failed: u64,
    warnings: u64,
    warning_details: Vec<String>,
    removed_total: u64,
    /// A cancellation observed in the job row read back after a batch committed (the flag itself is
    /// the usual signal; this catches a cancellation that only reached the database).
    cancelled: bool,
}

/// The display half of an `AssetAdded`, computed while the asset's bytes are still open and held
/// beside its [`ScanWrite`] until the commit hands back the id it needs.
struct Staged {
    path: String,
    name: String,
    media: MediaType,
    format: String,
    size: u64,
    key_attrs: SmallMap,
}

/// One source's chunked ingest: entries queue here until the chunk is full or stale, then flush as
/// probe → per-entry work → one write transaction → post-commit events (steps A–D of issue #138).
struct ScanChunker<'a> {
    store: &'a Store,
    events: &'a broadcast::Sender<LibraryEvent>,
    job: JobId,
    cancel: &'a AtomicBool,
    governor: &'a crate::resources::Governor,
    fs: &'a dyn dam_sources::FileSource,
    mode: ScanMode,
    total: Option<u64>,
    sid: SourceId,
    source_label: String,
    generation: i64,
    scanned_at: i64,
    pending: Vec<FileEntry>,
    last_flush: Instant,
    tally: &'a mut ScanTally,
    reconciliation_safe: bool,
    listing_incomplete: bool,
}

impl ScanChunker<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.tally.cancelled
    }

    fn warn(&mut self, message: String) {
        record_warning(
            &mut self.tally.warnings,
            &mut self.tally.warning_details,
            message,
        );
    }

    /// Queue one listed entry, committing when the chunk is full or has been open too long.
    fn push(&mut self, entry: FileEntry) {
        self.pending.push(entry);
        if self.pending.len() >= SCAN_CHUNK || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    /// The backend continued after a per-entry listing error, but its enumeration is not
    /// authoritative enough to infer that any unseen catalog path disappeared.
    fn note_listing_error(&mut self, error: &LibError) {
        self.listing_incomplete = true;
        self.warn(format!(
            "An entry in source “{}” could not be listed",
            self.source_label
        ));
        tracing::warn!(source = %self.sid, error = %error, "source entry unavailable");
    }

    /// Steps A–D for whatever is queued. Must be called once more after the walk ends and before
    /// `finish_source_scan`: flushing *after* reconciliation would hand the mark-missing pass a
    /// generation that never saw this chunk's paths.
    fn flush(&mut self) {
        let entries = std::mem::replace(&mut self.pending, Vec::with_capacity(SCAN_CHUNK));
        if !entries.is_empty() {
            let tokens = self.stamp(&entries);
            let (items, staged, current) = self.prepare(entries, tokens);
            self.commit(&items, staged, current);
        }
        self.last_flush = Instant::now();
    }

    /// **Step A** — one transaction stamps every listed path with this generation and hands back
    /// its prior change token.
    ///
    /// Every listed entry is stamped *before* `detect_for_ingest` ever sees it, exactly as the
    /// per-entry version did. Detection must not move above this: a file that was ingestible when
    /// it was catalogued and is not any more would go unstamped, and the reconciliation at the end
    /// of the walk would mark it missing while it sits right there on disk.
    fn stamp(&mut self, entries: &[FileEntry]) -> Vec<Option<SourceChangeToken>> {
        let paths: Vec<String> = entries.iter().map(|e| e.rel_path.clone()).collect();
        match self
            .store
            .probe_scan_chunk(&self.sid, self.generation, &paths)
        {
            Ok(tokens) => tokens,
            Err(error) => {
                // Unstamped paths cannot be told apart from vanished ones, so this walk gives up
                // its right to finalise missing rows — and, with no tokens, re-opens every entry.
                self.reconciliation_safe = false;
                self.warn(format!(
                    "{} listed path(s) in source “{}” could not be marked as seen",
                    paths.len(),
                    self.source_label
                ));
                tracing::warn!(source = %self.sid, error = %error, paths = paths.len(), "scan markers failed");
                vec![None; entries.len()]
            }
        }
    }

    /// **Step B** — the per-entry work, with no store call anywhere in it: detect, delta
    /// short-circuit, pacing, fetch, hash, cheap-tier metadata. Every fail-soft degradation the
    /// scan has lives here; the batch below only ever sees entries that got this far.
    ///
    /// Bytes are turned into a [`ScanWrite`] eagerly rather than held open, so a chunk never pins
    /// 128 fetched temp files (a remote source materialises each one into `scratch/`).
    fn prepare(
        &mut self,
        entries: Vec<FileEntry>,
        tokens: Vec<Option<SourceChangeToken>>,
    ) -> (Vec<ScanWrite>, Vec<Staged>, Option<String>) {
        let mut items = Vec::with_capacity(entries.len());
        let mut staged = Vec::with_capacity(entries.len());
        let mut current = None;
        for (fe, token) in entries.into_iter().zip(tokens) {
            // Cancellation stops the walk, but whatever this chunk has already computed is still
            // committed below: those rows are legitimately ingested, and their events would
            // otherwise be lies.
            if self.cancelled() {
                break;
            }
            // Detect by the logical path's extension (a remote temp file has a random name).
            let Some(det) = dam_media::detect_for_ingest(Path::new(&fe.rel_path)) else {
                continue; // unhandled type: skip (fail-soft, DG §6)
            };
            self.tally.examined += 1;
            current = Some(fe.rel_path.clone());
            // Delta: unchanged (same size + mtime) → never open bytes (§2.2).
            if self.mode == ScanMode::Delta && unchanged(token, &fe) {
                self.tally.skipped += 1;
                self.tally.settled += 1;
                continue;
            }
            // Good-neighbour pacing (tech-spec 14 §3.4), placed exactly where the bulk I/O starts:
            // everything above is directory metadata, everything below opens and hashes file bytes
            // — the reads that can blockade a slow HDD. Pausing here lets the walk finish cheap
            // entries while the disk recovers.
            self.governor.pace(self.cancel);
            if self.cancelled() {
                break;
            }
            // Materialise bytes locally from the backend's pinned/opened source handle.
            let fetched = match self.fs.fetch(&fe.rel_path) {
                Ok(f) => f,
                Err(error) => {
                    self.tally.settled += 1;
                    self.warn(format!(
                        "“{}” could not be read from source “{}”",
                        fe.rel_path, self.source_label
                    ));
                    tracing::warn!(path = %fe.rel_path, error = %error, "fetch failed");
                    continue;
                }
            };
            let abs = fetched.path();
            // Now that real bytes exist locally, settle the container extensions whose media type
            // the path alone can't determine (`.mp4`/`.mov`/`.m4v` — audio-only or video?). This is
            // the only point in the scan where that question is answerable, and it's asked once per
            // asset, before the row is written.
            let det = dam_media::refine_with_content(&det, abs).unwrap_or(det);
            let Some(hash) = hash_file(abs) else {
                self.tally.settled += 1;
                self.warn(format!(
                    "“{}” could not be hashed; check file readability",
                    fe.rel_path
                ));
                continue;
            };
            // CHEAP tier (tech-spec 04 §4): header-only media attributes, read here rather than
            // after the upsert because the bytes are open now and the batch takes no file I/O.
            // (A blocklisted hash therefore pays for its own header read — the block itself is
            // decided inside the transaction, issue #21.)
            let attrs = dam_media::extract_metadata(abs, &det);
            let filename = file_name(&fe.rel_path);
            staged.push(Staged {
                path: fe.rel_path.clone(),
                name: filename.clone(),
                media: det.media,
                format: det.format.clone(),
                // Whole-asset size: the file itself plus a model's external companion files
                // (textures/buffers), matching the catalog read path.
                size: fe.size + dependency_bytes(&attrs),
                key_attrs: key_attrs_of(&attrs),
            });
            items.push(ScanWrite {
                asset: NewAsset {
                    source_id: self.sid,
                    path: fe.rel_path,
                    filename,
                    content_hash: Some(hash),
                    size_bytes: Some(fe.size as i64),
                    source_modified_at: fe.modified_ms,
                    scanned_at: self.scanned_at,
                    media_type: det.media,
                    format: det.format,
                },
                attrs,
            });
        }
        (items, staged, current)
    }

    /// **Step C + D** — one write transaction for the chunk (rows, generation stamps and the job's
    /// progress together), then the events it earned, published only once those rows are durable.
    ///
    /// Called even for a chunk that produced no writes: the transaction still carries the progress
    /// update that keeps a mostly-unchanged delta re-scan's bar moving, and its read-back of the
    /// job row is how a cancellation that landed mid-batch is noticed.
    fn commit(&mut self, items: &[ScanWrite], staged: Vec<Staged>, current: Option<String>) {
        let ctx = ScanBatchContext {
            job: self.job,
            state: JobState::Running,
            done: self.tally.settled,
            total: self.total,
            current,
            generation: self.generation,
        };
        let outcome = match self.store.apply_scan_batch(&ctx, items) {
            Ok(outcome) => outcome,
            Err(error) => {
                // A transaction-level failure: nothing in this chunk landed. Existing rows keep the
                // stamp step A gave them, and nothing new exists to be reconciled, so the walk
                // stays authoritative — this is a per-chunk ingest degradation, not a state one.
                self.tally.failed += items.len() as u64;
                self.warn(format!(
                    "{} scanned item(s) from source “{}” could not be saved",
                    items.len(),
                    self.source_label
                ));
                tracing::warn!(source = %self.sid, error = %error, items = items.len(), "scan batch failed");
                return;
            }
        };
        for (result, item) in outcome.items.iter().zip(staged) {
            match result {
                Ok(ScanItemOutcome::Written {
                    id,
                    inserted,
                    generation_current,
                }) => {
                    self.tally.committed += 1;
                    self.tally.settled += 1;
                    if !generation_current {
                        // A newer concurrent scan superseded this generation. Its guarded finish
                        // owns missing reconciliation.
                        self.reconciliation_safe = false;
                    }
                    if *inserted {
                        reliability::publish_event(
                            self.events,
                            LibraryEvent::AssetAdded(AssetSummary {
                                id: *id,
                                name: item.name,
                                media: item.media,
                                format: item.format,
                                size: item.size,
                                license: LicenseBadge::default(),
                                top_tags: Vec::new(),
                                origin: Origin::Local,
                                key_attrs: item.key_attrs,
                                // A freshly-scanned asset is never a favourite yet.
                                favorite: false,
                                // Attribution for the ceiling check on the way out to
                                // subscribers (issue #42).
                                source_id: Some(self.sid),
                            }),
                            "publish scanned asset",
                        );
                    }
                }
                // The hash is blocklisted (issue #21) and nothing was written.
                Ok(ScanItemOutcome::Blocked) => {
                    self.tally.skipped += 1;
                    self.tally.settled += 1;
                }
                Err(error) => {
                    self.tally.failed += 1;
                    self.warn(format!("“{}” could not be added to the catalog", item.path));
                    tracing::warn!(path = %item.path, error = %error, "skipped asset");
                }
            }
        }
        if outcome.job.state == JobState::Cancelled {
            self.tally.cancelled = true;
        }
        // The batch wrote the progress row itself, so the event is published from what it read
        // back rather than from a second read of the same job.
        reliability::publish_event(
            self.events,
            LibraryEvent::JobProgress(outcome.job),
            "publish job progress",
        );
    }
}

/// A model's external companion files (textures/buffers), which the catalog counts as part of the
/// asset's size.
fn dependency_bytes(attrs: &MediaAttributes) -> u64 {
    match attrs {
        MediaAttributes::Model(m) => m.dependency_bytes.unwrap_or(0).max(0) as u64,
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)] // the job runner's full context; a struct would just rename it
pub(crate) fn run_scan(
    store: Arc<Store>,
    secrets: crate::credentials::SecretVault,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    sources: Vec<SourceInfo>,
    mode: ScanMode,
    cancel: Arc<AtomicBool>,
    governor: &crate::resources::Governor,
    scratch: &Path,
) -> Result<(), LibError> {
    // Use the catalog size as an estimate for either mode. A new/empty source is indeterminate.
    // Walking just to discover an exact denominator doubled local directory work and, worse, every
    // SFTP/SMB listing call. The final update replaces this estimate with the examined count.
    let estimated_total: u64 = sources.iter().map(|source| source.stats.asset_count).sum();
    let total = (estimated_total > 0).then_some(estimated_total);
    store.update_job_progress(&job, JobState::Running, 0, total, None)?;
    let mut tally = ScanTally::default();

    for src in sources {
        if src.kind == SourceKind::Federated {
            continue; // federated peers yield catalog rows, not bytes — not scanned here (phase 6)
        }
        let sid = src.id;
        let source_label = src.name.clone();

        // Rebuild from the persisted non-secret connection + its resolved host credential. An
        // unreachable host, locked store, or bad credential marks this source offline and moves on.
        let conn = match store
            .get_source_connection(&sid)
            .and_then(|connection| secrets.resolve(connection))
        {
            Ok(c) => c,
            Err(e) => {
                reliability::retryable_store_write(
                    store.set_source_error(&sid, &e.to_string()),
                    "record scan source open failure",
                    Some(&job),
                    Some(&sid),
                );
                record_warning(
                    &mut tally.warnings,
                    &mut tally.warning_details,
                    format!("Source “{source_label}” could not be opened; inspect its connection settings"),
                );
                continue;
            }
        };
        let fs = match open_source(&conn, scratch) {
            Ok(fs) => fs,
            Err(e) => {
                reliability::retryable_store_write(
                    store.set_source_error(&sid, &e.to_string()),
                    "record unavailable scan source",
                    Some(&job),
                    Some(&sid),
                );
                tracing::warn!(source = %sid, error = %e, "source unavailable");
                record_warning(
                    &mut tally.warnings,
                    &mut tally.warning_details,
                    format!("Source “{source_label}” is unavailable; inspect source status and credentials"),
                );
                continue;
            }
        };

        // The database owns reconciliation state. Each streamed path is an indexed point lookup +
        // generation stamp; only an exhaustively completed walk finalises unseen rows as missing.
        // No catalog-sized path map/set exists in this process.
        let generation = match store.begin_source_scan(&sid) {
            Ok(generation) => generation,
            Err(error) => {
                reliability::retryable_store_write(
                    store.set_source_error(&sid, &error.to_string()),
                    "record scan generation failure",
                    Some(&job),
                    Some(&sid),
                );
                record_warning(
                    &mut tally.warnings,
                    &mut tally.warning_details,
                    format!("Source “{source_label}” scan state could not be started"),
                );
                continue;
            }
        };
        let mut chunker = ScanChunker {
            store: &store,
            events: &events,
            job,
            cancel: &cancel,
            governor,
            fs: fs.as_ref(),
            mode,
            total,
            sid,
            source_label: source_label.clone(),
            generation,
            scanned_at: dam_store::now_ms(),
            pending: Vec::with_capacity(SCAN_CHUNK),
            last_flush: Instant::now(),
            tally: &mut tally,
            reconciliation_safe: true,
            listing_incomplete: false,
        };

        let walk_result = enumerate_source(fs.as_ref(), &mut |entry| {
            if chunker.cancelled() {
                return false;
            }
            match entry {
                Ok(fe) => chunker.push(fe),
                Err(error) => chunker.note_listing_error(&error),
            }
            true
        });
        // Before reconciliation, always: the tail chunk holds paths this generation has seen, and
        // finalising missing rows without them would mark files missing while they sit on disk.
        chunker.flush();
        let reconciliation_safe = chunker.reconciliation_safe;
        let listing_incomplete = chunker.listing_incomplete;
        drop(chunker);

        match walk_result {
            Ok(()) => {
                let cancelled = cancel.load(Ordering::Relaxed) || tally.cancelled;
                if reconciliation_is_authoritative(
                    cancelled,
                    reconciliation_safe,
                    listing_incomplete,
                ) {
                    match store.finish_source_scan(&sid, generation, dam_store::now_ms()) {
                        Ok(Some(n)) => tally.removed_total += n,
                        Ok(None) => {
                            tracing::debug!(source = %sid, generation, "scan superseded before reconciliation");
                        }
                        Err(e) => {
                            record_warning(
                                &mut tally.warnings,
                                &mut tally.warning_details,
                                format!(
                                    "Source “{source_label}” was scanned but missing-file status could not be updated"
                                ),
                            );
                            tracing::warn!(source = %sid, error = %e, "mark-missing failed");
                        }
                    }
                } else if !cancelled {
                    // Preserve fail-soft partial ingest, but neither infer removals nor publish a
                    // successful source timestamp for an incomplete/unstamped enumeration.
                    tracing::debug!(source = %sid, generation, "partial scan skipped reconciliation");
                }
            }
            Err(e) => {
                reliability::retryable_store_write(
                    store.set_source_error(&sid, &e.to_string()),
                    "record source walk failure",
                    Some(&job),
                    Some(&sid),
                );
                record_warning(
                    &mut tally.warnings,
                    &mut tally.warning_details,
                    format!(
                        "Source “{source_label}” could not be fully walked; inspect source status"
                    ),
                );
                tracing::warn!(source = %sid, error = %e, "source scan failed");
            }
        }
    }

    // Every chunk has flushed by now, cancelled or not: a cancelled scan keeps the rows it had
    // already computed (scan is idempotent and non-destructive, and their `AssetAdded` events are
    // already published — rolling back would make those events lie) and only then goes terminal.
    let examined = tally.examined;
    let terminal_result = if cancel.load(Ordering::Relaxed) || tally.cancelled {
        store.set_job_state(&job, JobState::Cancelled, None)
    } else {
        store.update_job_progress(
            &job,
            // Keep the job non-terminal until `complete_job` atomically installs its summary and
            // warnings. Publishing `Done` here lets a polling client observe a terminal row before
            // the report fields are written.
            JobState::Running,
            examined,
            Some(examined),
            None,
        )?;
        let mut notes = Vec::new();
        if tally.skipped > 0 {
            notes.push(format!("{} unchanged", tally.skipped));
        }
        if tally.removed_total > 0 {
            notes.push(format!("{} missing", tally.removed_total));
        }
        let omitted = tally
            .warnings
            .saturating_sub(tally.warning_details.len() as u64);
        if omitted > 0 {
            tally.warning_details.push(format!(
                "{omitted} additional warning(s) omitted; inspect source status and server logs"
            ));
        }
        let suffix = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        };
        let summary = format!("Scanned {examined} item(s){suffix}");
        store.complete_job(&job, &summary, &tally.warning_details)
    };
    emit_progress(&store, &events, &job);
    tracing::info!(
        %job,
        done = tally.committed,
        failed = tally.failed,
        skipped = tally.skipped,
        removed = tally.removed_total,
        warnings = tally.warnings,
        "scan finished"
    );
    terminal_result
}

/// A delta entry is unchanged when its size and mtime both match the stored change token.
fn unchanged(stored: Option<dam_store::SourceChangeToken>, fe: &FileEntry) -> bool {
    match stored {
        Some((size, modified)) => {
            size == Some(fe.size as i64) && modified == fe.modified_ms && fe.modified_ms.is_some()
        }
        None => false,
    }
}

/// A couple of display attributes for the live-added grid row, mirroring the store's grid map so a
/// freshly scanned asset shows its dimensions/duration/tris immediately (before any refetch).
pub(crate) fn key_attrs_of(attrs: &MediaAttributes) -> SmallMap {
    let mut m = SmallMap::new();
    match attrs {
        MediaAttributes::Image(i) => {
            if let (Some(w), Some(h)) = (i.width, i.height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
        }
        MediaAttributes::Audio(a) => {
            if let Some(ms) = a.duration_ms {
                let secs = ms as f64 / 1000.0;
                m.insert(
                    "duration".into(),
                    format!("{:.0}:{:02}", (secs / 60.0).floor(), (secs % 60.0) as i64),
                );
            }
        }
        MediaAttributes::Model(md) => {
            if let Some(t) = md.triangle_count {
                m.insert("tris".into(), t.to_string());
            }
        }
        MediaAttributes::Video(v) => {
            if let (Some(w), Some(h)) = (v.width, v.height) {
                m.insert("dimensions".into(), format!("{w}×{h}"));
            }
            if let Some(ms) = v.duration_ms {
                let secs = ms as f64 / 1000.0;
                m.insert(
                    "duration".into(),
                    format!("{:.0}:{:02}", (secs / 60.0).floor(), (secs % 60.0) as i64),
                );
            }
        }
        MediaAttributes::Document(d) => {
            if let Some(p) = d.page_count.filter(|p| *p > 0) {
                m.insert("pages".into(), p.to_string());
            }
            if let Some(w) = d.word_count.filter(|w| *w > 0) {
                m.insert("words".into(), w.to_string());
            }
        }
        MediaAttributes::None => {}
    }
    m
}

pub(crate) fn hash_file(path: &Path) -> Option<dam_api::id::ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    std::io::copy(&mut reader, &mut hasher).ok()?;
    Some(dam_api::id::ContentHash(*hasher.finalize().as_bytes()))
}

/// The final path component of a source-relative path (works for `/`-separated remote paths too).
pub(crate) fn file_name(rel_path: &str) -> String {
    rel_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(rel_path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_sources::{Fetched, FileSource};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RemoteListingFixture {
        entries: usize,
        walks: AtomicUsize,
    }

    impl FileSource for RemoteListingFixture {
        fn walk(
            &self,
            sink: &mut dyn FnMut(Result<FileEntry, LibError>) -> bool,
        ) -> Result<(), LibError> {
            self.walks.fetch_add(1, Ordering::Relaxed);
            for index in 0..self.entries {
                if !sink(Ok(FileEntry {
                    rel_path: format!("remote/item_{index:07}.png"),
                    size: 1,
                    modified_ms: Some(1),
                })) {
                    break;
                }
            }
            Ok(())
        }

        fn fetch(&self, _rel_path: &str) -> Result<Fetched, LibError> {
            Err(LibError::SourceUnavailable(
                "listing fixture has no content channel".into(),
            ))
        }
    }

    #[test]
    fn remote_listing_is_enumerated_once_and_streamed() {
        let source = RemoteListingFixture {
            entries: 100_000,
            walks: AtomicUsize::new(0),
        };
        let mut visited = 0usize;
        enumerate_source(&source, &mut |entry| {
            let entry = entry.unwrap();
            assert!(entry.rel_path.starts_with("remote/item_"));
            visited += 1;
            true
        })
        .unwrap();
        assert_eq!(visited, source.entries);
        assert_eq!(
            source.walks.load(Ordering::Relaxed),
            1,
            "remote scan regressed to a count prewalk plus ingest walk"
        );
    }

    #[test]
    fn cancel_or_partial_listing_never_authorizes_missing_reconciliation() {
        assert!(!reconciliation_is_authoritative(true, true, false));
        assert!(!reconciliation_is_authoritative(false, true, true));
        assert!(!reconciliation_is_authoritative(false, false, false));
        assert!(reconciliation_is_authoritative(false, true, false));
    }
}
