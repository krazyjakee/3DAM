//! The hosted-mode background pipeline (issue #71) — the "fat server" that proactively drains ingest
//! work so clients always hit ready data.
//!
//! Without this, thumbnails are lazy-on-request (`gen_thumbnail` renders the *first time a client
//! looks*) and analysis only runs when a client submits it. The vision is the opposite: the server
//! just works through the job loads in the background. This module wires that up:
//!
//! - **Chain on ingest.** Every completed scan (full or watch-driven delta) wakes a worker that runs
//!   the analysis pass and pre-renders thumbnails/previews for the newly-ingested assets.
//! - **One always-on worker.** A single task drains *all* due work to completion, then waits for the
//!   next wake — coalescing bursts (a `Notify` permit), so overlapping scans collapse into one drain.
//!   Because the worker awaits each drain, it never overlaps itself: natural backpressure (rule 5).
//! - **Idempotent + fail-soft.** Analysis dueness is version-gated (`PIPELINE_VERSION`) and thumbnail
//!   caches are content-keyed, so a re-drain of unchanged assets is a cheap no-op; a corrupt asset
//!   degrades to a per-item skip and never stalls the queue (rule 6).
//! - **Flag-gated.** `auto_thumbnail` / `auto_analyze` are runtime feature flags (rule 4); the server
//!   supplies them through [`PipelinePolicy`] so dam-core never depends on `server.db`.

use super::*;

/// The runtime policy a long-running server supplies so the pipeline honours the `auto_thumbnail` /
/// `auto_analyze` feature flags — read fresh on every drain, so a live admin toggle takes effect on
/// the next scan without a restart. Kept as a trait so dam-core needn't link the server's flag store.
pub trait PipelinePolicy: Send + Sync + 'static {
    /// Pre-generate thumbnails + model previews on ingest.
    fn auto_thumbnail(&self) -> bool;
    /// Run the analysis pass (embeddings, auto-tags, derived attrs) on ingest.
    fn auto_analyze(&self) -> bool;
}

/// The edge the pipeline pre-renders thumbnails at — the server's default (`?edge` unset) and the
/// MCP thumbnail size, so a freshly-connected client's first grid mostly hits warm cache. Clients
/// that ask for a different edge still fall back to on-demand generation for that slice.
pub(crate) const PREGEN_THUMB_EDGE: u32 = 256;
pub(crate) const DERIVATIVE_VERSION: i64 = 1;

impl EmbeddedLibrary {
    /// Start the always-on hosted-mode pipeline (issue #71). Long-running roles (`serve`) call this
    /// once after open; a run-and-exit CLI never does — there is nothing to keep draining, and a
    /// detached worker would keep the runtime alive past the command (mirrors `start_watchers`).
    ///
    /// Two tasks: a *listener* that wakes the worker on every scan-completed event, and the *worker*
    /// that drains any startup backlog immediately, then drains again on each wake.
    pub fn start_background_pipeline(self: &Arc<Self>, policy: Arc<dyn PipelinePolicy>) {
        // Shared with the engine (`pipeline_wake`) rather than private to this function, so an
        // ingest that is *not* a scan can ask for a drain too — upload (issue #80) writes its
        // catalog row directly and would otherwise never reach this worker.
        let notify = self.pipeline_wake.clone();

        // Listener: a scan finishing (full or watch-driven delta) means fresh assets to process. The
        // analysis pass emits `Analyze` job progress, not `Scan`, so this never re-triggers itself.
        let listener_notify = notify.clone();
        let mut rx = self.events.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(LibraryEvent::JobProgress(js))
                        if js.kind == JobKind::Scan && js.state == JobState::Done =>
                    {
                        listener_notify.notify_one();
                    }
                    Ok(_) => {}
                    // Fell behind the firehose under load — we may have missed a scan-done, so drain
                    // to be safe (the drain is idempotent, so an unnecessary wake is cheap).
                    Err(broadcast::error::RecvError::Lagged(_)) => listener_notify.notify_one(),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Worker: drain to completion, then wait for the next (coalesced) wake. `Notify` holds a
        // single permit, so scans that complete *during* a drain collapse into one follow-up pass —
        // and since a drain processes everything due, that one pass picks their assets up.
        let worker = self.clone();
        tokio::spawn(async move {
            worker.run_pipeline_drain(policy.as_ref()).await; // startup backlog from a prior run
            loop {
                notify.notified().await;
                worker.run_pipeline_drain(policy.as_ref()).await;
            }
        });
    }

    /// One drain pass: bring analysis, then thumbnails/previews, up to date for every ingested asset.
    /// Each half is independently flag-gated and fail-soft — a failure in one is logged and never
    /// blocks the other or the worker loop.
    async fn run_pipeline_drain(&self, policy: &dyn PipelinePolicy) {
        if policy.auto_analyze() {
            if let Err(e) = self.drain_analysis().await {
                tracing::warn!(error = %e, "background analysis drain failed");
            }
        }
        if policy.auto_thumbnail() {
            if let Err(e) = self.drain_thumbnails().await {
                tracing::warn!(error = %e, "background thumbnail drain failed");
            }
        }
    }

    /// Run the analysis pass over every asset behind the current pipeline version. Reuses the same
    /// job + runner as `submit_analyze`, so progress surfaces through the usual `Analyze` job events
    /// (both clients see the server working). Awaited to completion — the worker won't start a second
    /// overlapping analyze job.
    async fn drain_analysis(&self) -> Result<(), LibError> {
        let summary = self
            .db(|s| s.analysis_plan_summary(crate::analysis::PIPELINE_VERSION, false, &[]))
            .await?;
        if summary.total == 0 {
            return Ok(()); // everything already analysed — idempotent no-op
        }
        let total = summary.total;
        let end = summary.end;
        let touched = summary.sources;
        let job = self
            .db(move |s| s.create_job(JobKind::Analyze, "{\"auto\":true}", Some(total), &touched))
            .await?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let events = self.events.clone();
        let model = self.semantic.clone();
        let secrets = self.secrets.clone();
        let pool = self.bg_pool.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch();
        let outcome = tokio::task::spawn_blocking(move || {
            crate::analysis::run_analyze(
                store,
                events,
                job,
                crate::analysis::AnalysisRunPlan {
                    current_version: crate::analysis::PIPELINE_VERSION,
                    force: false,
                    assets: Vec::new(),
                    total,
                    end,
                },
                cancel,
                model,
                secrets,
                &pool,
                &governor,
                &scratch,
            )
        })
        .await;
        self.cancels.lock().unwrap().remove(&job);
        outcome.map_err(|e| LibError::Internal(e.to_string()))?
    }

    /// Pre-render each ingested asset's thumbnail (at [`PREGEN_THUMB_EDGE`]) and, for models, the
    /// interactive `DMSH` preview blob — skipping any already warm in the content-keyed cache. Runs
    /// on one blocking thread (bounded), off the request path. Fail-soft: an asset that can't be
    /// rendered is skipped, never fatal.
    async fn drain_thumbnails(&self) -> Result<(), LibError> {
        let mut cursor = None;
        let mut generated_total = 0u64;
        let mut failed_total = 0u64;
        loop {
            let batch = self
                .db(move |store| {
                    store.derivative_target_batch(
                        DERIVATIVE_VERSION,
                        cursor,
                        crate::analysis::PLAN_BATCH_SIZE,
                    )
                })
                .await?;
            let next = batch.next;
            if batch.targets.is_empty() && next.is_none() {
                break;
            }
            let targets = batch.targets;
            let data_dir = self.data_dir.clone();
            let secrets = self.secrets.clone();
            let governor = self.governor.clone();
            let cache = self.cache.clone();
            let (generated, failed) = self
                .run_background(move |store| {
                    let cancel = cache.background_cancel();
                    let mut generated = 0u64;
                    let mut failed = 0u64;
                    for t in targets {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        // Good-neighbour pacing (tech-spec 14 §3.4): pre-rendering is pure opportunism — it
                        // parks whenever the host is short on memory or CPU and resumes on recovery.
                        governor.pace(cancel);
                        let Ok(asset) = store.get_asset(&t.id) else {
                            continue; // vanished between listing and read — fail-soft
                        };
                        let is_model = asset.summary.media == MediaType::Model;
                        let key = if is_model {
                            format!("model-derivatives:{}:{PREGEN_THUMB_EDGE}", t.id)
                        } else {
                            format!("thumbnail:{}:{PREGEN_THUMB_EDGE}", t.id)
                        };
                        let warmed = cache.singleflight_blocking(key, |priority_cancel| {
                            crate::derivatives::warm_derivatives(
                                &cache,
                                &data_dir,
                                store,
                                &secrets,
                                &asset,
                                PREGEN_THUMB_EDGE,
                                &governor,
                                priority_cancel,
                            )
                        });
                        if warmed.is_some_and(|result| result.is_ok()) {
                            if store.mark_derivative_ready(
                                &t.id,
                                DERIVATIVE_VERSION,
                                t.content_hash,
                            )? {
                                generated += 1;
                            } else {
                                // Content changed while the derivative was rendering; the scan reset
                                // the new revision to pending, so never bless the stale cache key.
                                failed += 1;
                            }
                        } else {
                            // Leave the durable marker pending. The keyset cursor still advances, so a
                            // bad asset is attempted at most once in this drain and retries on a later
                            // wake rather than spinning in a tight loop.
                            failed += 1;
                        }
                    }
                    Ok((generated, failed))
                })
                .await?;
            generated_total += generated;
            failed_total += failed;
            let Some(next) = next else {
                break;
            };
            cursor = Some(next);
        }
        if generated_total > 0 || failed_total > 0 {
            tracing::info!(
                generated = generated_total,
                pending_failures = failed_total,
                "background pipeline warmed derivatives"
            );
        }
        Ok(())
    }
}
