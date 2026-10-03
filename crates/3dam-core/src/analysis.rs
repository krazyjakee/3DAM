//! The automation brain (tech-spec 05): the analysis pass that turns a scanned asset into embeddings,
//! derived signals, and auto-tag/-category *suggestions*, plus the background job that drives it.
//!
//! **Model-free v1.** Per ADR 0006 / tech-spec 05 §2.3 the default build ships no model weights and
//! makes no network call, so the embedders here are deterministic descriptors (image = a normalised
//! low-res luminance grid from `dam-media`; audio/3D = normalised cheap-attribute stats). They sit
//! behind the same `EmbeddingSpace` seam the SigLIP/CLAP path will use, so a real model is a
//! `model_version` bump (§7), not a rewrite. Spaces are named honestly (`*-stats-v1`).
//!
//! Every stage is fail-soft (DESIGN_GUIDELINES §2): a bad decode degrades that one asset to "no
//! embedding" and the pass moves on — never an aborted job.

use crate::writer::BatchAccumulator;
use crate::{emit_progress, reliability};
use dam_api::dto::*;
use dam_api::event::{ChangeKind, LibraryEvent};
use dam_api::id::{AssetId, JobId, SourceId};
use dam_api::LibError;
use dam_store::{
    AnalysisBatchContext, AnalysisPlanSource, AnalysisPlanTarget, AnalysisWrite, AudioFeatureWrite,
    EmbeddingWrite, ImageAnalysis, Store, TagSuggestion,
};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::broadcast;

/// The current analysis pipeline version. Bump when any extractor's algorithm/output changes so the
/// Plan stage re-enqueues assets behind it (§7.2). One monotonic number covers the v1 extractor set.
/// V2: the audio classifier moved from a duration heuristic to measured DSP features (loopability,
/// tonality/key, tempo, envelope) — every audio asset re-analyses to get an honest class.
/// V3: the analyze pass also extracts continuous acoustic features (loudness/brightness/harmonicity,
/// issue #61), so audio re-analyses once more to populate them.
pub const PIPELINE_VERSION: i64 = 3;
const MAX_JOB_WARNING_DETAILS: u64 = 20;

/// Embedding-space ids (§2.1). Model-free descriptors in v1 — see module docs. One logical index per
/// media type; vectors from different spaces are never cross-ranked (§3.1).
///
/// This match is the **only** place the model-free space names are written. The analyse pass embeds
/// into it and [`crate::EmbeddedLibrary::embedding_spaces`] advertises it to peers, so a peer can
/// never gate cross-peer similarity on a name nothing was ever written under. Deliberately a match
/// rather than a `format!` over the media name: the mapping is not derivable (documents rank in a
/// hashed-text space, not a stats one), and being exhaustive means a sixth media type is a compile
/// error here rather than a silently missing space.
pub(crate) fn model_free_space(media: MediaType) -> &'static str {
    match media {
        MediaType::Image => "image-stats-v1",
        MediaType::Audio => "audio-stats-v1",
        MediaType::Model => "model-stats-v1",
        // Video: container shape only (see [`analyze_video`]) — a weak descriptor, honestly named.
        MediaType::Video => "video-stats-v1",
        // Documents: hashed bag-of-words over the extracted text (`dam_media::text_descriptor`).
        // Text is the one media type where the model-free descriptor is genuinely useful rather
        // than a placeholder, because lexical overlap *is* a real similarity signal for prose. The
        // model-backed text encoder is a later feature-gated bump into its own space (issue #47),
        // never a redefinition of this one.
        MediaType::Document => "text-hash-v1",
    }
}

/// Progress heartbeat for a pass that persists *nothing*. Progress normally rides along inside the
/// batch transaction that carries the rows it counts, so it costs no transaction of its own; a pass
/// whose every asset fails (an offline source, say) never fills a batch, and this is how often it
/// commits the counter anyway.
const PROGRESS_EVERY: u64 = 8;
pub(crate) const PLAN_BATCH_SIZE: usize = 256;
pub(crate) const PLAN_PREFETCH_BATCHES: usize = 2;
type SourceBackends =
    HashMap<dam_api::id::SourceId, Result<Arc<dyn dam_sources::FileSource>, String>>;

pub(crate) struct AnalysisRunPlan {
    pub current_version: i64,
    pub force: bool,
    pub assets: Vec<AssetId>,
    pub total: u64,
    pub end: Option<dam_store::AnalysisPlanCursor>,
}

fn bounded_plan_channel<T>() -> (std::sync::mpsc::SyncSender<T>, std::sync::mpsc::Receiver<T>) {
    std::sync::mpsc::sync_channel(PLAN_PREFETCH_BATCHES)
}

fn produce_analysis_batches(
    store: &Store,
    plan: &AnalysisRunPlan,
    cancel: &AtomicBool,
    sender: std::sync::mpsc::SyncSender<Result<dam_store::AnalysisPlanBatch, LibError>>,
) {
    let mut cursor = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let batch = store.analysis_target_batch(
            plan.current_version,
            plan.force,
            &plan.assets,
            cursor,
            plan.end,
            PLAN_BATCH_SIZE,
        );
        let next = batch.as_ref().ok().and_then(|batch| batch.next);
        let failed = batch.is_err();
        if sender.send(batch).is_err() || failed || next.is_none() {
            break;
        }
        cursor = next;
    }
}

/// One analysed asset waiting to be committed, with the little bit of context the post-commit
/// bookkeeping needs: `source_id` for its `AssetChanged` event, `path` for a warning or the job's
/// "currently working on" line. Neither is part of the write itself.
struct PendingAnalysis {
    write: AnalysisWrite,
    source_id: SourceId,
    path: String,
}

/// Roughly how much memory one pending write pins, for the accumulator's payload bound.
///
/// Deliberately an over-estimate of the *stored* form rather than a measurement of the Rust one:
/// what the bound protects is the transaction the batch turns into, and text and peaks are the only
/// two fields that can differ from their neighbours by three orders of magnitude.
fn payload_cost(write: &AnalysisWrite) -> usize {
    /// Attributes, image analysis and class are a few fixed-size columns; charge a flat row cost
    /// rather than walking them, so an ordinary image or model batch closes on the row bound.
    const ROW_COST: usize = 512;
    let mut cost = ROW_COST;
    cost += write.document_text.as_ref().map_or(0, String::len);
    // Peaks reach the catalog as a JSON array of floats, several bytes per sample rather than four.
    cost += write
        .audio_peaks
        .as_ref()
        .map_or(0, |peaks| peaks.len() * 8);
    for embedding in &write.embeddings {
        cost += embedding.vector.len() * std::mem::size_of::<f32>()
            + embedding.space_id.len()
            + embedding.extractor.len();
    }
    for tag in &write.tags {
        cost += ROW_COST + tag.name.len() + tag.extractor.len() + tag.explanation.len();
    }
    cost
}

/// A `Mutex` guard that survives a poisoned lock. A worker that panicked mid-analysis must not
/// wedge the whole pass: the accumulator and the warning list are both plain collections, so the
/// worst a poisoned one can hold is a partially-pushed batch, which the next flush picks up.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The bounded write stage for one analysis job (issue #138).
///
/// Every rayon worker submits into the same accumulator; the worker whose push crosses a bound
/// takes the pending vector under the lock, releases it, and performs the flush itself. There is no
/// writer thread — a second worker that fills the fresh vector before the first flush returns simply
/// blocks on the store's writer guard, and that is the backpressure.
///
/// Events are emitted from [`AnalysisSink::flush`] *after* `apply_analysis_batch` returns, never
/// from the compute path, so an `AssetChanged { Reanalyzed }` on the wire always describes a
/// committed row.
struct AnalysisSink<'a> {
    store: &'a Store,
    events: &'a broadcast::Sender<LibraryEvent>,
    /// The job's cancel flag, also *written* here: `outcome.job.state` is the only place a
    /// cancellation that landed while a batch was being filled becomes visible to this thread.
    cancel: &'a AtomicBool,
    job: JobId,
    total: u64,
    /// Targets whose outcome is final — committed or failed. Deliberately not "targets computed":
    /// progress is written inside the batch transaction, so counting work that has not been
    /// committed would let a crash leave the job claiming more than the catalog holds.
    done: AtomicU64,
    warnings: AtomicU64,
    warning_details: Mutex<Vec<String>>,
    pending: Mutex<BatchAccumulator<PendingAnalysis>>,
}

impl<'a> AnalysisSink<'a> {
    fn new(
        store: &'a Store,
        events: &'a broadcast::Sender<LibraryEvent>,
        cancel: &'a AtomicBool,
        job: JobId,
        total: u64,
    ) -> AnalysisSink<'a> {
        AnalysisSink {
            store,
            events,
            cancel,
            job,
            total,
            done: AtomicU64::new(0),
            warnings: AtomicU64::new(0),
            warning_details: Mutex::new(Vec::new()),
            pending: Mutex::new(BatchAccumulator::new()),
        }
    }

    /// Enqueue one analysed asset, flushing on this thread if that push closed the batch.
    fn submit(&self, item: PendingAnalysis) {
        let cost = payload_cost(&item.write);
        let batch = {
            let mut pending = lock(&self.pending);
            // `push` reports the size bounds; the age bound is checked on the same guard so a
            // trickle of small assets still commits promptly.
            (pending.push(item, cost) || pending.due_by_age()).then(|| pending.take())
        };
        if let Some(batch) = batch {
            self.flush(batch);
        }
    }

    /// A compute failure: the asset produced no write at all, so there is nothing to commit and it
    /// stays legitimately due for the next pass (§1.3 fail-soft, DESIGN_GUIDELINES §2).
    fn fail(&self, target: &AnalysisPlanTarget, error: &str) {
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        self.warn(&target.path, error);
        tracing::warn!(asset = %target.id, path = %target.path, error = %error, "analysis skipped asset");
        // A pass where *everything* fails (an offline source) enqueues nothing, so no flush would
        // ever carry its progress. Commit the counter on its own in that case, and take the chance
        // to drain anything the age bound has made due.
        if done.is_multiple_of(PROGRESS_EVERY) {
            let batch = {
                let mut pending = lock(&self.pending);
                (pending.is_empty() || pending.due_by_age()).then(|| pending.take())
            };
            if let Some(batch) = batch {
                self.flush(batch);
            }
        }
    }

    /// Record a per-asset warning against the job. The detail list is capped; the count is not.
    fn warn(&self, path: &str, error: &str) {
        let prior = self.warnings.fetch_add(1, Ordering::Relaxed);
        if prior < MAX_JOB_WARNING_DETAILS {
            lock(&self.warning_details).push(format!(
                "“{path}” could not be analysed; inspect its source status and retry"
            ));
        }
        tracing::debug!(%path, %error, "analysis warning");
    }

    /// Commit one batch — derived rows, vectors, suggestions and the version stamp for every asset
    /// in it — in a single transaction, then emit what it made true. An empty batch is legal and
    /// commits only the progress counters.
    fn flush(&self, batch: Vec<PendingAnalysis>) {
        let count = batch.len() as u64;
        let done = self.done.fetch_add(count, Ordering::Relaxed) + count;
        let current = batch.last().map(|item| item.path.clone());
        let mut writes = Vec::with_capacity(batch.len());
        let mut origins = Vec::with_capacity(batch.len());
        for item in batch {
            writes.push(item.write);
            origins.push((item.source_id, item.path));
        }
        let ctx = AnalysisBatchContext {
            job: self.job,
            state: JobState::Running,
            done,
            total: Some(self.total),
            current,
        };
        match self.store.apply_analysis_batch(&ctx, &writes) {
            Ok(outcome) => {
                for (result, (source_id, path)) in outcome.items.iter().zip(origins.iter()) {
                    match result {
                        // The version stamp is the last statement of an item, so `analysed` is the
                        // one honest signal that this asset's whole derivation is durable.
                        Ok(item) if item.analysed => {
                            reliability::publish_event(
                                self.events,
                                LibraryEvent::AssetChanged {
                                    id: item.id,
                                    source_id: Some(*source_id),
                                    kind: ChangeKind::Reanalyzed,
                                },
                                "publish analysed asset",
                            );
                        }
                        // Nothing was stamped, so the asset stays due — silently, by design.
                        Ok(_) => {}
                        Err(error) => {
                            self.warn(path, error);
                            tracing::warn!(%path, %error, "analysis write rolled back");
                        }
                    }
                }
                // A cancellation that landed while this batch was being filled is visible only in
                // the state the transaction just read back.
                if outcome.job.state == JobState::Cancelled {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                reliability::publish_event(
                    self.events,
                    LibraryEvent::JobProgress(outcome.job),
                    "publish analysis batch progress",
                );
            }
            Err(error) => {
                // Transaction-level failure: nothing in this batch landed, so every asset in it is
                // a warning and is re-planned by the next pass.
                let detail = error.to_string();
                for (_, path) in &origins {
                    self.warn(path, &detail);
                }
                reliability::retryable_store_write(
                    Err(error),
                    "persist analysis batch",
                    Some(&self.job),
                    None,
                );
            }
        }
    }

    /// Commit whatever is left. Called once the fan-out is finished — including on cancellation,
    /// where flushing before the terminal state is what keeps completed-but-unwritten work from
    /// being thrown away (an asset that never flushes simply never advances its analysis version
    /// and is re-planned).
    fn flush_remaining(&self) {
        let batch = lock(&self.pending).take();
        if !batch.is_empty() {
            self.flush(batch);
        }
    }
}

/// The background analysis job (mirrors `scan::run_scan`): the caller supplies only a stable plan
/// boundary and total; a bounded producer keyset-streams due targets while this consumer runs
/// Extract→Derive→Classify→Index and emits a `Reanalyzed` event per commit (§1.3 incremental).
///
/// Per-asset work is CPU-bound (decode → derive → classify → embed), so it fans out across the rayon
/// pool (ADR 0007, issue #67) — a whole-library pass now scales with cores instead of pinning one. The
/// async caller already handed off via `spawn_blocking`, so this stays a one-shot async→CPU hop.
///
/// Persistence is a bounded batch stage (issue #138): a worker produces an [`AnalysisWrite`] and
/// hands it to [`AnalysisSink`] rather than writing anything itself, so one analysed image costs a
/// share of one transaction instead of the ~28 it used to (image analysis + embedding + a
/// `suggest_tag` round trip per tag + the version stamp).
#[allow(clippy::too_many_arguments)] // the job runner's full context; a struct would just rename it
pub(crate) fn run_analyze(
    store: Arc<Store>,
    events: broadcast::Sender<LibraryEvent>,
    job: JobId,
    plan: AnalysisRunPlan,
    cancel: Arc<AtomicBool>,
    model: Option<Arc<dyn crate::semantic::SemanticModel>>,
    secrets: crate::credentials::SecretVault,
    pool: &rayon::ThreadPool,
    governor: &crate::resources::Governor,
    scratch: &Path,
) -> Result<(), LibError> {
    let total = plan.total;
    store.update_job_progress(&job, JobState::Running, 0, Some(total), None)?;
    // Shared across the rayon workers: the bounded write stage, and with it the completion and
    // warning counters (progress is committed with the rows it describes, so the counter belongs
    // to whatever writes them).
    let sink = AnalysisSink::new(&store, &events, &cancel, job, total);

    let mut backends = SourceBackends::new();
    let mut planner_error = None;

    // A planner may be one batch ahead, never one catalog ahead. A full channel blocks the producer
    // until the bounded rayon consumer releases capacity; dropping the receiver on cancellation
    // wakes a blocked send immediately.
    std::thread::scope(|scope| {
        let (sender, receiver) = bounded_plan_channel();
        let producer_store = store.clone();
        let producer_cancel = cancel.clone();
        scope.spawn(move || {
            produce_analysis_batches(&producer_store, &plan, &producer_cancel, sender);
        });

        while let Ok(batch) = receiver.recv() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let batch = match batch {
                Ok(batch) => batch,
                Err(error) => {
                    planner_error = Some(error.to_string());
                    break;
                }
            };
            open_batch_backends(&store, &secrets, &mut backends, batch.sources, scratch);

            // Run on the bounded background pool (not the global rayon pool) so a large pass leaves
            // cores free for interactive reads. Only this page and the small channel are resident.
            pool.install(|| {
                batch.targets.par_iter().for_each(|target| {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    governor.pace(cancel.as_ref());
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let outcome = match backends.get(&target.source_id) {
                        Some(Ok(source)) => analyze_one(
                            target,
                            model.as_deref(),
                            source.as_ref(),
                            governor,
                            scratch,
                            &cancel,
                        ),
                        Some(Err(error)) => Err(error.clone()),
                        None => Err("source backend missing".to_string()),
                    };
                    match outcome {
                        // Nothing is written or announced here: the asset joins the pending batch
                        // and its event follows the commit that makes it true.
                        Ok(write) => sink.submit(PendingAnalysis {
                            write,
                            source_id: target.source_id,
                            path: target.path.clone(),
                        }),
                        Err(_) if cancel.load(Ordering::Relaxed) => {}
                        Err(error) => sink.fail(target, &error),
                    }
                });
            });
        }
        drop(receiver);
    });

    // Flush before the terminal state, cancelled or not: work that is already computed is cheap to
    // keep and expensive to redo, and an asset that never reaches a commit is simply still due.
    sink.flush_remaining();
    let AnalysisSink {
        done,
        warnings,
        warning_details,
        ..
    } = sink;
    let done = done.load(Ordering::Relaxed);
    let warnings = warnings.load(Ordering::Relaxed);
    let mut warning_details = warning_details
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let omitted = warnings.saturating_sub(warning_details.len() as u64);
    if omitted > 0 {
        warning_details.push(format!(
            "{omitted} additional warning(s) omitted; inspect source status and server logs"
        ));
    }
    let terminal_result = if let Some(error) = planner_error {
        store.set_job_state(&job, JobState::Failed, Some(&error))
    } else if cancel.load(Ordering::Relaxed) {
        store.set_job_state(&job, JobState::Cancelled, None)
    } else {
        store.update_job_progress(&job, JobState::Running, done, Some(total), None)?;
        let analysed = done.saturating_sub(warnings);
        let summary = format!("Analysed {analysed} of {total} item(s)");
        store.complete_job(&job, &summary, &warning_details)
    };
    emit_progress(&store, &events, &job);
    tracing::info!(%job, done, warnings, "analysis finished");
    terminal_result
}

/// Rebuild one `FileSource` per distinct source in `targets` (issue #48).
///
/// The result is keyed by `source_id` so the fan-out is a map lookup, and holds the *error* for a
/// source that could not be opened rather than dropping it — otherwise its assets would silently
/// vanish from the pass instead of each reporting why they were skipped. The error is also written
/// to `source.last_error`, so the offline state surfaces in the sources list and not only in a log
/// line nobody reads.
fn open_batch_backends(
    store: &Store,
    secrets: &crate::credentials::SecretVault,
    backends: &mut SourceBackends,
    sources: Vec<AnalysisPlanSource>,
    scratch: &Path,
) {
    for source in sources {
        if backends.contains_key(&source.source_id) {
            continue;
        }
        let entry = store
            .get_source_connection(&source.source_id)
            .map_err(|error| error.to_string())
            .and_then(|connection| {
                secrets
                    .resolve(connection)
                    .map_err(|error| error.to_string())
            })
            .and_then(|connection| {
                dam_sources::open_source(&connection, scratch)
                    .map(Arc::from)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = &entry {
            reliability::retryable_store_write(
                store.set_source_error(&source.source_id, error),
                "record unavailable analysis source",
                None,
                Some(&source.source_id),
            );
            tracing::warn!(source = %source.source_id, %error, "source unavailable for analysis");
        };
        backends.insert(source.source_id, entry);
    }
}

/// Analyse one asset end-to-end: extract features, derive signals, classify → suggest, and collect
/// the whole lot into one [`AnalysisWrite`]. Fail-soft per stage.
///
/// This function no longer touches the store. Everything it derives — including the version stamp —
/// is accumulated and handed back, so the asset lands as a single all-or-nothing item inside a
/// batch savepoint (issue #138). That closes a real hole as well as saving transactions: a failure
/// between the old `set_image_analysis` and `mark_analysed` used to leave derived signals written
/// with the version gate unmoved, i.e. a half-derivation that the next pass would overwrite rather
/// than notice. A *compute* failure still returns `Err` from here, before any write exists at all,
/// and takes the caller's per-asset warning path unchanged.
///
/// Bytes arrive through `fs.fetch` (issue #48): in place for a local source — the same path the old
/// root-join produced, minus the chance of disagreeing with the scan about it — and a temp file for
/// SFTP/SMB. The temp is owned by `fetched` and deleted when this function returns, so peak scratch
/// usage across a remote pass is bounded by the background pool's width times the largest asset,
/// not by the size of the batch.
fn analyze_one(
    t: &AnalysisPlanTarget,
    model: Option<&dyn crate::semantic::SemanticModel>,
    fs: &dyn dam_sources::FileSource,
    governor: &crate::resources::Governor,
    scratch: &Path,
    cancel: &AtomicBool,
) -> Result<AnalysisWrite, String> {
    let (fetched, work) = governor
        .fetch(fs, &t.path, scratch, cancel)
        .map_err(|e| e.to_string())?;
    // Opaque decoders are protected by device concurrency; their entry read is budgeted before
    // execution. The fetched copy itself yields and cancels between every 256 KiB chunk.
    work.pace(
        std::fs::metadata(fetched.path())
            .map(|m| m.len())
            .unwrap_or(0),
    )
    .map_err(|e| e.to_string())?;
    let abs = fetched.path().to_path_buf();
    let det = dam_media::Detected {
        media: t.media,
        format: t.format.clone(),
    };
    let mut write = AnalysisWrite::new(t.id);
    match t.media {
        MediaType::Image => analyze_image(&mut write, &abs)?,
        MediaType::Audio => analyze_audio(&mut write, t, &abs, &det)?,
        MediaType::Model => analyze_model(&mut write, &abs, &det)?,
        MediaType::Video => analyze_video(&mut write, t, &abs, &det)?,
        MediaType::Document => analyze_document(&mut write, t, &abs, &det)?,
    }
    // Model-backed semantic embedding (semantic-search M4): when a model ships, also index the
    // shared text/media space so text queries can rank against it (M5 semantic mode). Additive to
    // the model-free space above — different `space_id`, never cross-ranked. `None` on the default
    // build, so this is a no-op there.
    if let Some(m) = model {
        if let Some(vector) = m.encode_asset(t.media, &abs) {
            write.embeddings.push(EmbeddingWrite {
                space_id: m.space_id(t.media),
                media: t.media,
                vector,
                extractor: "semantic@1".into(),
            });
        }
        // Zero-shot content labels — the semantic tier CLAP/SigLIP add on top of the model-free DSP
        // class (music/speech/sfx by timbre, genre, mood, instrument): tags pure DSP can't derive.
        // Suggested (not confirmed) so they share the accept/reject lifecycle. No-op when the model
        // has no taxonomy for this media (default trait impl returns empty).
        for (name, confidence) in m.zero_shot_labels(t.media, &abs) {
            write.tags.push(TagSuggestion {
                name,
                confidence,
                extractor: "semantic@1".into(),
                explanation: "The semantic model matched this content label.".into(),
            });
        }
    }
    // Filename-derived tag suggestions (semantic-search M2): the words in a name ("ak47", "lowpoly")
    // are a real, if weak, content signal. Suggested (not confirmed) so they flow through the same
    // accept/reject lifecycle as the media-derived tags — a reject is remembered.
    suggest_filename_tags(&mut write, t);
    // Written last by the store, after everything above (see `apply_analysis_batch`), so this asset
    // can never claim to be analysed at a version it only half reached.
    write.analysed_version = Some(PIPELINE_VERSION);
    Ok(write)
}

/// Suggest a tag for each meaningful whole word in the filename: alphanumeric runs of ≥3 chars that
/// aren't purely numeric and aren't the format/extension. Low confidence — a name is a weaker signal
/// than a decoded attribute.
fn suggest_filename_tags(w: &mut AnalysisWrite, t: &AnalysisPlanTarget) {
    let filename = Path::new(&t.path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&t.path);
    let fmt = t.format.to_lowercase();
    for run in filename.split(|c: char| !c.is_alphanumeric()) {
        let tok = run.to_lowercase();
        if tok.len() < 3 || tok == fmt || tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        w.tags.push(TagSuggestion {
            name: tok,
            confidence: 0.4,
            extractor: "filename@1".into(),
            explanation: "This term appears in the source filename.".into(),
        });
    }
}

fn analyze_image(w: &mut AnalysisWrite, abs: &Path) -> Result<(), String> {
    let f = dam_media::extract_image_features(abs).map_err(|e| e.to_string())?;
    // Derive: perceptual/tileability/colour signals (§5, §6).
    w.image = Some(ImageAnalysis {
        phash: f.phash,
        tileability: f.tileability,
        repeat_period: f.repeat_period.map(|p| p as i64),
        tile_class: f.tile_class.to_string(),
        dominant_colors: f.dominant_colors,
        class: f.class.to_string(),
    });
    // Index: the visual embedding (§3.1).
    w.embeddings.push(EmbeddingWrite {
        space_id: model_free_space(MediaType::Image).to_string(),
        media: MediaType::Image,
        vector: f.embedding,
        extractor: "image-stats@1".into(),
    });
    // Classify → suggest (§1.4): tileability + content class drive auto-tags with a confidence.
    let mut suggestions: Vec<(&str, f32)> = vec![(f.class, 0.7)];
    match f.tile_class {
        "seamless" => suggestions.push(("seamless", f.tileability.clamp(0.5, 1.0))),
        "tiled" => suggestions.push(("tileable", 0.8)),
        _ => {}
    }
    suggest_all(
        w,
        &suggestions,
        "Image analysis inferred this from visual and tiling signals.",
    );
    Ok(())
}

fn analyze_audio(
    w: &mut AnalysisWrite,
    t: &AnalysisPlanTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Audio(a) = dam_media::extract_metadata(abs, det) else {
        return Err("audio metadata unavailable".into());
    };
    // Model-free stats embedding: a normalised cheap-attribute vector (§2.3). Duration dominates.
    let dur = a.duration_ms.unwrap_or(0) as f32;
    w.embeddings.push(EmbeddingWrite {
        space_id: model_free_space(MediaType::Audio).to_string(),
        media: MediaType::Audio,
        vector: normalise(vec![
            (1.0 + dur).log10(),
            a.sample_rate.unwrap_or(0) as f32 / 48_000.0,
            a.channels.unwrap_or(0) as f32 / 2.0,
            a.bit_depth.unwrap_or(0) as f32 / 24.0,
        ]),
        extractor: "audio-stats@1".into(),
    });
    // Classify from *measured* DSP signals, not duration (§4.2). A full decode yields loopability
    // (authored `smpl` loop points, else a seamless wrap boundary), tonality + key, tempo, and
    // envelope shape — the orthogonal "does it loop" and "what is it" axes a length threshold
    // conflates. Fail-soft: if the decode fails we fall back to a neutral duration split so the asset
    // still gets *a* class (never silently "loop", the old bug).
    // Features and inspector peaks share this one bounded decode (issue #145). Keeping the combined
    // result alive through both persistence steps avoids retaining a second PCM buffer or opening
    // the source twice; only the compact outputs escape `dam-media`.
    let analysis = dam_media::extract_audio_analysis(abs, &det.format);
    let (class, conf, mut extra): (&str, f32, Vec<(&str, f32)>) = match analysis.as_ref() {
        Ok(analysis) => {
            let f = &analysis.features;
            let mut tags: Vec<(&str, f32)> = Vec::new();
            if f.is_loop {
                tags.push(("loop", 0.6));
            }
            tags.push(if f.tonal {
                ("tonal", 0.6)
            } else {
                ("atonal", 0.5)
            });
            if f.bpm.is_some() {
                tags.push(("rhythmic", 0.6));
            }
            tags.push(if f.sustained {
                ("sustained", 0.5)
            } else {
                ("transient", 0.5)
            });
            suggest_audio_extras(w, f);
            // The continuous acoustic features behind the inspector bars (issue #61).
            w.audio_features = Some(AudioFeatureWrite {
                loudness_lufs: f.loudness_lufs,
                brightness: f.brightness,
                harmonicity: f.harmonicity,
            });
            let conf = if f.loop_source == dam_media::LoopSource::Metadata {
                0.95
            } else {
                0.6
            };
            (f.class, conf, tags)
        }
        Err(e) => {
            tracing::warn!(asset = %t.id, error = %e, "audio feature extraction failed; duration fallback");
            let (c, cf) = match a.duration_ms {
                Some(ms) if ms < 2_000 => ("one_shot", 0.4),
                _ => ("sfx", 0.3),
            };
            (c, cf, Vec::new())
        }
    };
    w.class = Some((MediaType::Audio, class.to_string()));
    extra.insert(0, (class, conf));
    suggest_all(
        w,
        &extra,
        "Audio analysis inferred this from measured rhythm, timbre, and envelope signals.",
    );
    // Waveform peaks for the inspector (issue #73): derived during the same decode as features, so
    // no client or second server pass re-downloads + re-decodes the audio to draw the bars.
    match analysis {
        Ok(analysis) => w.audio_peaks = Some(analysis.waveform_peaks),
        Err(e) => tracing::warn!(asset = %t.id, error = %e, "waveform peak computation failed"),
    }
    Ok(())
}

fn analyze_model(
    w: &mut AnalysisWrite,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Model(m) = dam_media::extract_metadata(abs, det) else {
        return Err("model metadata unavailable".into());
    };
    // Refine the scan's cheap counts with exact ones where this build can (issue #49). The cheap
    // tier reads FBX/Collada/3DS structurally, which is right for a scan but has to assume
    // triangles for a compressed FBX index array or a `<vcount>`-less Collada polygon list, and has
    // nothing to say about `.blend`. A full Assimp import settles all of that — and *this* is the
    // tier allowed to pay for a decode, which is why it lives here and not in the scan.
    //
    // Fail-soft in both directions: a build without `model-convert`, or a file Assimp cannot read,
    // simply leaves the cheap answer in place. The refined attributes are written back before the
    // embedding is computed so the stored row and the vector agree.
    let m = match dam_media::extract_model_metadata_deep(abs, &det.format) {
        Ok(exact) => {
            w.attrs = Some(MediaAttributes::Model(exact.clone()));
            exact
        }
        Err(e) => {
            tracing::debug!(asset = %w.id, error = %e, "exact model counts unavailable");
            m
        }
    };
    // Model-free stats embedding from geometry counts + rig/anim/uv flags (§2.3, §4.2 corroboration).
    w.embeddings.push(EmbeddingWrite {
        space_id: model_free_space(MediaType::Model).to_string(),
        media: MediaType::Model,
        vector: normalise(vec![
            (1.0 + m.vertex_count.unwrap_or(0) as f32).log10(),
            (1.0 + m.triangle_count.unwrap_or(0) as f32).log10(),
            (1.0 + m.mesh_count.unwrap_or(0) as f32).log10(),
            m.material_count.unwrap_or(0) as f32 / 8.0,
            m.texture_count.unwrap_or(0) as f32 / 8.0,
            m.has_rig.unwrap_or(false) as u8 as f32,
            m.has_animation.unwrap_or(false) as u8 as f32,
            m.has_uvs.unwrap_or(false) as u8 as f32,
        ]),
        extractor: "model-stats@1".into(),
    });
    // Category guess from triangle budget (§5): a coarse low/mid/high-poly bucket.
    let class = match m.triangle_count {
        Some(tris) if tris < 2_000 => "prop_lowpoly",
        Some(tris) if tris < 50_000 => "prop",
        Some(_) => "prop_highpoly",
        None => "prop",
    };
    w.class = Some((MediaType::Model, class.to_string()));
    // Structural facts make good high-confidence suggestions (§1.4).
    let mut suggestions: Vec<(&str, f32)> = vec![(class, 0.6)];
    if m.has_rig.unwrap_or(false) {
        suggestions.push(("rigged", 0.95));
    }
    if m.has_animation.unwrap_or(false) {
        suggestions.push(("animated", 0.95));
    }
    if m.has_uvs.unwrap_or(false) {
        suggestions.push(("uv_mapped", 0.9));
    }
    suggest_all(
        w,
        &suggestions,
        "Model analysis inferred this from measured geometry and structure.",
    );
    Ok(())
}

/// Analyse a video: container-shape embedding, a duration-based class, and structural tags.
///
/// Be clear about what this descriptor is. It is built from duration, resolution, frame rate and
/// bitrate — the *shape* of the file, not its pictures — so "similar" here means "another clip of
/// roughly this length and format", not "another clip that looks like this". That is the same
/// bargain `model-stats-v1` strikes with triangle budgets, and it is the honest ceiling for a
/// media type whose decode backend may not even be installed (ADR 0015).
///
/// A visual descriptor is a real follow-up: the poster frame already goes through
/// `extract_image_features`, so a `video-frame-v1` space is mostly plumbing. It is deliberately not
/// folded into *this* space, because a space must be homogeneous — half the library having visual
/// vectors and half having shape vectors, depending on whether ffmpeg happened to be installed at
/// scan time, is exactly what the `space_id` seam exists to prevent.
fn analyze_video(
    w: &mut AnalysisWrite,
    t: &AnalysisPlanTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Video(v) = dam_media::extract_metadata(abs, det) else {
        return Err("video metadata unavailable".into());
    };
    // With no prober installed every field is None, which would make one identical vector for
    // every video in the library — worse than useless, since it would rank them all as perfect
    // matches for each other. Skip the embedding entirely rather than index a lie.
    if v.duration_ms.is_none() && v.width.is_none() {
        tracing::debug!(asset = %t.id, "no video metadata (no prober?); skipping embedding");
        return Ok(());
    }

    let secs = v.duration_ms.unwrap_or(0) as f32 / 1000.0;
    w.embeddings.push(EmbeddingWrite {
        space_id: model_free_space(MediaType::Video).to_string(),
        media: MediaType::Video,
        vector: normalise(vec![
            (1.0 + secs).log10(),
            (1.0 + v.width.unwrap_or(0) as f32).log10(),
            (1.0 + v.height.unwrap_or(0) as f32).log10(),
            v.fps.unwrap_or(0.0) / 60.0,
            (1.0 + v.bitrate.unwrap_or(0) as f32).log10() / 8.0,
            v.has_audio.unwrap_or(false) as u8 as f32,
        ]),
        extractor: "video-stats@1".into(),
    });

    // Duration is the one axis that reliably separates the kinds of video a game project holds.
    let class = match v.duration_ms {
        Some(ms) if ms < 5_000 => "sting",
        Some(ms) if ms < 60_000 => "clip",
        Some(_) => "cutscene",
        None => "clip",
    };
    w.class = Some((MediaType::Video, class.to_string()));

    let mut suggestions: Vec<(&str, f32)> = vec![(class, 0.6)];
    // A video with no audio track is very often a UI/VFX element or a video texture rather than a
    // watchable clip — a genuinely useful thing to be able to filter on.
    if v.has_audio == Some(false) {
        suggestions.push(("silent", 0.9));
    }
    if let (Some(w), Some(h)) = (v.width, v.height) {
        if w >= 3840 || h >= 2160 {
            suggestions.push(("4k", 0.95));
        } else if w >= 1920 || h >= 1080 {
            suggestions.push(("1080p", 0.95));
        }
    }
    suggest_all(
        w,
        &suggestions,
        "Video analysis inferred this from measured duration, dimensions, and track metadata.",
    );
    Ok(())
}

/// Analyse a document: extract its full text, index it for search, and embed it.
///
/// This is the only analyse path that writes to the FTS index rather than (or as well as) the
/// catalog, because for a document the *text is the content*. It is also the reason the batch
/// accumulator has a payload bound at all: the body text is capped at `MAX_TEXT_BYTES` (1 MiB) per
/// asset, so a batch of documents is three orders of magnitude heavier than a batch of images.
fn analyze_document(
    w: &mut AnalysisWrite,
    t: &AnalysisPlanTarget,
    abs: &Path,
    det: &dam_media::Detected,
) -> Result<(), String> {
    let MediaAttributes::Document(d) = dam_media::extract_metadata(abs, det) else {
        return Err("document metadata unavailable".into());
    };
    // Re-persist the cheap tier: word/page counts and the excerpt for a document scanned before
    // this pipeline version existed would otherwise stay empty until a rescan.
    w.attrs = Some(MediaAttributes::Document(d.clone()));

    // A scanned-image PDF with no text layer legitimately yields nothing. That is not an error —
    // it is a document we can describe but not read, and it stays findable by filename and tags.
    // The empty string is still *written*: this pass also runs when a document is replaced by an
    // edited version, and returning early here would leave the previous body indexed forever,
    // matching searches for prose the file no longer contains.
    let text = dam_media::extract_text(abs, &det.format).unwrap_or_default();
    if text.is_empty() {
        tracing::debug!(asset = %t.id, "no extractable text; indexing by name only");
    }

    // `text_descriptor` returns `None` for text with no usable tokens, so an unreadable document
    // never gets a vector — but a *stale* one from a previous version must not survive either.
    match dam_media::text_descriptor(&text) {
        Some(vector) => w.embeddings.push(EmbeddingWrite {
            space_id: model_free_space(MediaType::Document).to_string(),
            media: MediaType::Document,
            vector,
            extractor: "text-hash@1".into(),
        }),
        None => w
            .cleared_spaces
            .push(model_free_space(MediaType::Document).to_string()),
    }

    // Classify by what the document *is* to a project. Filename is the strongest signal here —
    // a `LICENSE` is a licence whatever its prose says — with a text fallback for the rest.
    let name = Path::new(&t.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let head: String = text.chars().take(600).collect::<String>().to_lowercase();
    let class = if name.contains("licen") || name.contains("copying") || name.contains("eula") {
        "license"
    } else if name.contains("readme") {
        "readme"
    } else if name.contains("changelog") || name.contains("changes") {
        "changelog"
    } else if name.contains("invoice") || name.contains("receipt") || name.contains("order") {
        "receipt"
    } else if head.contains("permission is hereby granted")
        || head.contains("all rights reserved")
        || head.contains("licensed under")
    {
        "license"
    } else {
        "document"
    };
    w.class = Some((MediaType::Document, class.to_string()));

    let mut suggestions: Vec<(&str, f32)> = vec![(class, 0.6)];
    if d.page_count.is_some_and(|p| p > 20) {
        suggestions.push(("long_form", 0.8));
    }
    suggest_all(
        w,
        &suggestions,
        "Document analysis inferred this from the filename and extracted text.",
    );
    w.document_text = Some(text);
    Ok(())
}

/// Suggest the audio tags that need an owned string (tempo bucket, musical key) — kept out of the
/// `&'static str` batch below. Tempo is bucketed to the nearest 5 BPM so near-identical estimates
/// collapse to one filterable tag.
fn suggest_audio_extras(w: &mut AnalysisWrite, f: &dam_media::AudioFeatures) {
    if let Some(bpm) = f.bpm {
        let bucket = ((bpm / 5.0).round() * 5.0) as i64;
        w.tags.push(TagSuggestion {
            name: format!("{bucket}bpm"),
            confidence: 0.5,
            extractor: "analyze@1".into(),
            explanation: "Measured tempo falls in this BPM bucket.".into(),
        });
    }
    if let Some(key) = f.key {
        w.tags.push(TagSuggestion {
            name: format!("key-{key}"),
            confidence: 0.5,
            extractor: "analyze@1".into(),
            explanation: "Pitch analysis detected this musical key.".into(),
        });
    }
}

/// Collect a batch of suggested tags onto the pending write. Each one used to be its own
/// transaction — interning the name, inserting the row, and rewriting the whole `asset_fts.tags`
/// column — which is where most of an analysed image's ~28 transactions went; the batch interns
/// each distinct name once and reindexes each asset once.
fn suggest_all(w: &mut AnalysisWrite, suggestions: &[(&str, f32)], explanation: &str) {
    for (name, confidence) in suggestions {
        w.tags.push(TagSuggestion {
            name: (*name).to_string(),
            confidence: *confidence,
            extractor: "analyze@1".into(),
            explanation: explanation.to_string(),
        });
    }
}

/// Owned-vector adapter over the shared in-place L2 normaliser (`dam_media::l2_normalise`), so the
/// stats embedders below can build a vector inline. A zero vector stays zero (cosine 0 = "no signal").
fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    dam_media::l2_normalise(&mut v);
    v
}

#[cfg(test)]
mod planner_tests {
    use super::*;

    #[test]
    fn planner_channel_applies_bounded_backpressure() {
        let (sender, receiver) = bounded_plan_channel();
        for value in 0..PLAN_PREFETCH_BATCHES {
            sender.try_send(value).unwrap();
        }
        assert!(matches!(
            sender.try_send(99),
            Err(std::sync::mpsc::TrySendError::Full(99))
        ));
        drop(receiver);
    }

    #[test]
    fn cancelled_planner_produces_no_batch() {
        let store = Store::open_in_memory().unwrap();
        let cancel = AtomicBool::new(true);
        let plan = AnalysisRunPlan {
            current_version: PIPELINE_VERSION,
            force: false,
            assets: Vec::new(),
            total: 0,
            end: None,
        };
        let (sender, receiver) = bounded_plan_channel();
        produce_analysis_batches(&store, &plan, &cancel, sender);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn blocked_planner_unblocks_when_cancelled_consumer_drops() {
        let (sender, receiver) = bounded_plan_channel();
        for value in 0..PLAN_PREFETCH_BATCHES {
            sender.try_send(value).unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let producer_cancel = cancel.clone();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            started_sender.send(()).unwrap();
            let result = sender.send(99);
            finished_sender
                .send((producer_cancel.load(Ordering::Relaxed), result.is_err()))
                .unwrap();
        });
        started_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("producer thread did not start");
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            matches!(
                finished_receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "producer was not actually blocked by channel backpressure"
        );
        cancel.store(true, Ordering::Relaxed);
        drop(receiver);
        let (observed_cancel, disconnected) = finished_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("blocked planner did not stop promptly after consumer cancellation");
        producer.join().unwrap();
        assert!(observed_cancel);
        assert!(disconnected);
    }
}
