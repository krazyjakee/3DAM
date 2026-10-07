//! Conversion and analysis job submission, status, cancellation, and event subscription.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn convert_impl(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<ConvertReport, LibError> {
        // Restricted contexts may convert only assets they can reach (the outputs land in a
        // server-side dir either way, which non-destructive §5.1 already confines).
        if !ctx.visibility.is_full() {
            for id in &req.inputs {
                self.require_asset_visible(ctx, id).await?;
            }
        }
        let scratch = self.scratch();
        let secrets = self.secrets.clone();
        self.db(move |s| convert::run_convert(s, &secrets, req, &scratch))
            .await
    }

    pub(crate) async fn submit_convert_impl(
        &self,
        ctx: &AuthContext,
        req: ConvertRequest,
    ) -> Result<JobId, LibError> {
        if !ctx.visibility.is_full() {
            let inputs = req.inputs.clone();
            let visibility = ctx.visibility.clone();
            let visible = self
                .db(move |store| store.assets_visible(&inputs, &visibility))
                .await?;
            if !visible {
                return Err(LibError::NotFound(
                    "one or more convert inputs are unavailable".into(),
                ));
            }
        }
        let total = req.inputs.len() as u64;
        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let (sources, collections): (Vec<SourceId>, Vec<CollectionId>) =
            if let Some(scope) = ctx.visibility.restricted() {
                (
                    scope.sources.iter().copied().collect(),
                    scope.collections.iter().copied().collect(),
                )
            } else {
                let inputs = req.inputs.clone();
                let sources = self.db(move |store| store.asset_sources(&inputs)).await?;
                (sources, Vec::new())
            };
        let job = self
            .db(move |store| {
                store.create_job_scoped(
                    JobKind::Convert,
                    &params,
                    Some(total),
                    &sources,
                    &collections,
                )
            })
            .await?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());
        let store = self.store.clone();
        let events = self.events.clone();
        let secrets = self.secrets.clone();
        let scratch = self.scratch();
        tokio::task::spawn_blocking(move || {
            let run = convert::run_convert_with_checkpoint(
                &store,
                &secrets,
                req,
                &scratch,
                |done, total, current| {
                    if cancel.load(Ordering::Relaxed) {
                        return false;
                    }
                    reliability::retryable_store_write(
                        store.update_job_progress(
                            &job,
                            JobState::Running,
                            done,
                            Some(total),
                            current,
                        ),
                        "persist convert progress",
                        Some(&job),
                        None,
                    );
                    emit_progress(&store, &events, &job);
                    !cancel.load(Ordering::Relaxed)
                },
            );
            match run {
                Ok(run) => {
                    let result = JobResult::Convert(run.report.clone());
                    let warnings = convert_warnings(&run.report);
                    let state = if run.cancelled || cancel.load(Ordering::Relaxed) {
                        JobState::Cancelled
                    } else {
                        JobState::Done
                    };
                    let summary = convert_summary(&run.report, state);
                    let finished = reliability::required_background_write(
                        store.finish_job(&job, state, &summary, &warnings, Some(&result)),
                        "persist convert terminal report",
                        &job,
                    );
                    if state == JobState::Done && matches!(finished, Some(false)) {
                        // Cancellation won the DB race after the worker sampled the flag. Preserve
                        // the itemized partial/full report without changing the Cancelled state.
                        let summary = convert_summary(&run.report, JobState::Cancelled);
                        reliability::required_background_write(
                            store.finish_job(
                                &job,
                                JobState::Cancelled,
                                &summary,
                                &warnings,
                                Some(&result),
                            ),
                            "persist convert cancellation race report",
                            &job,
                        );
                    }
                }
                Err(error) if cancel.load(Ordering::Relaxed) => {
                    reliability::required_background_write(
                        store.finish_job(
                            &job,
                            JobState::Cancelled,
                            "Convert cancelled before completion",
                            &[],
                            None,
                        ),
                        "persist convert cancellation",
                        &job,
                    );
                    tracing::debug!(%job, %error, "convert stopped after cancellation");
                }
                Err(error) => {
                    reliability::required_background_write(
                        store.set_job_state(&job, JobState::Failed, Some(&error.to_string())),
                        "persist convert failure",
                        &job,
                    );
                }
            }
            reliability::required_background_write(
                store.set_job_artifacts(
                    &job,
                    &[JobArtifact {
                        label: "Open convert report".into(),
                        route: Some(format!("/jobs?job={job}")),
                    }],
                ),
                "persist convert report artifact",
                &job,
            );
            emit_progress(&store, &events, &job);
        });
        Ok(job)
    }

    pub(crate) async fn submit_scan_impl(
        &self,
        ctx: &AuthContext,
        req: ScanRequest,
    ) -> Result<JobId, LibError> {
        self.submit_scan_inner(ctx, req, None).await
    }

    /// Optional writer-only SQL metrics for the manual single-catalog harness.
    #[doc(hidden)]
    pub fn enable_scan_sql_metrics(&self) {
        self.store.enable_scan_sql_metrics();
    }
    #[doc(hidden)]
    pub fn scan_sql_metrics(&self) -> dam_store::ScanSqlMetrics {
        self.store.scan_sql_metrics()
    }

    /// Exercise the production scan with a measured backend (manual performance harness).
    #[doc(hidden)]
    pub async fn submit_scan_with_source(
        &self,
        ctx: &AuthContext,
        req: ScanRequest,
        source: Arc<dyn dam_sources::FileSource>,
    ) -> Result<JobId, LibError> {
        self.submit_scan_inner(ctx, req, Some(source)).await
    }

    async fn submit_scan_inner(
        &self,
        ctx: &AuthContext,
        req: ScanRequest,
        source_override: Option<Arc<dyn dam_sources::FileSource>>,
    ) -> Result<JobId, LibError> {
        Self::require_full_visibility(ctx, "scanning")?;
        // Resolve target sources (all file sources when none specified; federated peers excluded).
        let all = self.db(|s| s.list_sources()).await?;
        let sources: Vec<SourceInfo> = if req.sources.is_empty() {
            all.into_iter()
                .filter(|s| s.kind != SourceKind::Federated)
                .collect()
        } else {
            all.into_iter()
                .filter(|s| req.sources.contains(&s.id) && s.kind != SourceKind::Federated)
                .collect()
        };
        if sources.is_empty() {
            return Err(LibError::BadRequest(
                "no scannable file sources selected".into(),
            ));
        }

        if source_override.is_some() && sources.len() != 1 {
            return Err(LibError::BadRequest(
                "measured scans require exactly one source".into(),
            ));
        }
        let mode = req.mode;
        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        // The resolved source set *is* the job's attribution (issue #42).
        let touched: Vec<SourceId> = sources.iter().map(|s| s.id).collect();
        let sources = if mode == ScanMode::Quick && source_override.is_none() {
            sources
                .into_iter()
                .filter(|source| !self.watchers.trusted_clean(source.id))
                .collect()
        } else {
            sources
        };
        let job = self
            .db(move |s| s.create_job(JobKind::Scan, &params, None, &touched))
            .await?;

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());

        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let events = self.events.clone();
        let governor = self.governor.clone();
        let scratch = self.scratch();
        let coordinator = self.scan_coordinator.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = if mode == ScanMode::Quick {
                quick_scan::run_quick_scan(
                    store.clone(),
                    secrets,
                    events.clone(),
                    job,
                    sources,
                    cancel,
                    &governor,
                    &scratch,
                    &coordinator,
                    true,
                    &[],
                    source_override,
                )
            } else {
                scan::run_scan(
                    store.clone(),
                    secrets,
                    events.clone(),
                    job,
                    sources,
                    mode,
                    cancel,
                    &governor,
                    &scratch,
                    &coordinator,
                    true,
                    &[],
                    source_override,
                )
            };
            if let Err(error) = outcome {
                reliability::background_job_failed(&job, "run scan", &error);
                reliability::required_background_write(
                    store.set_job_state(&job, JobState::Failed, Some(&error.to_string())),
                    "persist scan failure",
                    &job,
                );
                emit_progress(&store, &events, &job);
            }
            coordinator.cancels.lock().unwrap().remove(&job);
        });

        Ok(job)
    }

    pub(crate) async fn submit_analyze_impl(
        &self,
        ctx: &AuthContext,
        req: AnalyzeRequest,
    ) -> Result<JobId, LibError> {
        Self::require_full_visibility(ctx, "analysis")?;
        // Plan only cheap metadata up front; the worker keyset-streams bounded target pages.
        let assets = req.assets.clone();
        let force = req.force;
        let summary_assets = assets.clone();
        let summary = self
            .db(move |s| {
                s.analysis_plan_summary(analysis::PIPELINE_VERSION, force, &summary_assets)
            })
            .await?;
        if summary.total == 0 {
            return Err(LibError::BadRequest(
                "nothing to analyse (all assets are up to date; pass --force to re-run)".into(),
            ));
        }

        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let total = summary.total;
        let end = summary.end;
        let touched = summary.sources;
        let job = self
            .db(move |s| s.create_job(JobKind::Analyze, &params, Some(total), &touched))
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
        tokio::task::spawn_blocking(move || {
            if let Err(error) = analysis::run_analyze(
                store,
                events,
                job,
                analysis::AnalysisRunPlan {
                    current_version: analysis::PIPELINE_VERSION,
                    force,
                    assets,
                    total,
                    end,
                },
                cancel,
                model,
                secrets,
                &pool,
                &governor,
                &scratch,
            ) {
                reliability::background_job_failed(&job, "run analysis", &error);
            }
        });
        Ok(job)
    }

    pub(crate) async fn regenerate_thumbnails_impl(
        &self,
        ctx: &AuthContext,
        req: ThumbnailRegenRequest,
    ) -> Result<ThumbnailRegenReport, LibError> {
        if !ctx.visibility.is_full() {
            for id in &req.assets {
                self.require_asset_writable(ctx, id).await?;
            }
        }
        let cache = self.cache.clone();
        // Resolve each asset's content key inside the store lock, then purge its cache slice; the
        // next thumbnail read re-renders from source. A missing asset fails the whole request (the
        // caller passed a bad id) — per-item fail-soft applies to the file deletes, not the lookup.
        let assets = req.assets.clone();
        let report = self
            .db(move |s| {
                let mut report = ThumbnailRegenReport::default();
                for id in &req.assets {
                    let asset = s.get_asset(id)?;
                    let key = asset
                        .hash
                        .map(|h| h.to_hex())
                        .unwrap_or_else(|| asset.summary.id.to_string());
                    report.files_deleted += purge_asset_cache(&cache, &key);
                    report.assets += 1;
                }
                s.mark_derivatives_pending(&assets)?;
                Ok(report)
            })
            .await?;
        self.pipeline_wake.notify_one();
        Ok(report)
    }

    /// A job is readable when every source it touches is within the caller's ceiling — see
    /// [`Visibility::allows_job`]. Outside it the job is *absent*, not forbidden.
    ///
    /// This is what lets a share-based identity watch its own scan finish: the WebSocket delivers
    /// `JobProgress`, the client invalidates its jobs query, and the refetch has to agree with the
    /// event or the status bar would blink empty (issue #42).
    pub(crate) async fn get_job_impl(
        &self,
        ctx: &AuthContext,
        id: &JobId,
    ) -> Result<JobStatus, LibError> {
        let jid = *id;
        let job = self.db(move |s| s.get_job(&jid)).await?;
        if !ctx.visibility.allows_job(&job) {
            return Err(LibError::NotFound(format!("job {id}")));
        }
        Ok(job)
    }

    pub(crate) async fn list_jobs_impl(
        &self,
        ctx: &AuthContext,
        req: JobListRequest,
    ) -> Result<Page<JobStatus>, LibError> {
        let vis = ctx.visibility.clone();
        self.db(move |s| s.list_jobs(&req, &vis)).await
    }

    pub(crate) async fn cancel_job_impl(
        &self,
        ctx: &AuthContext,
        id: &JobId,
    ) -> Result<(), LibError> {
        // Read-then-write split, matching `require_asset_writable`: a job outside the ceiling is
        // absent (404), one inside it but cancellable only by an unrestricted identity is forbidden.
        // Cancelling is a library-wide act — a share grants the right to *watch* a job, not stop it.
        self.get_job(ctx, id).await?;
        Self::require_full_visibility(ctx, "cancelling a job")?;
        if let Some(flag) = self.cancels.lock().unwrap().get(id) {
            flag.store(true, Ordering::Relaxed);
        }
        // Reflect intent immediately; the running task also sets terminal state on exit.
        let id = *id;
        self.db(move |s| {
            let job = s.get_job(&id)?;
            if matches!(job.state, JobState::Queued | JobState::Running) {
                s.set_job_state(&id, JobState::Cancelled, None)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn subscribe_impl(
        &self,
        ctx: &AuthContext,
        _req: SubscribeRequest,
    ) -> Result<EventStream<LibraryEvent>, LibError> {
        let rx = self.events.subscribe();
        let vis = ctx.visibility.clone();
        // A bounded broadcast receiver reports lag explicitly. Turn that gap into a payload the
        // transport can forward so every consumer performs one resync instead of staying stale.
        //
        // Restricted subscribers get a per-event ceiling check rather than a blanket withhold
        // (issue #42). Every event now carries the attribution the check needs — `source_id` on the
        // per-asset variants, the touched `sources` on `JobStatus` — so the decision is a set lookup
        // on data already in hand, with no database round-trip per event per subscriber. The rule
        // itself lives in `Visibility::allows_event` so the engine and any future transport enforce
        // one definition, and so adding a `LibraryEvent` variant fails to compile until it is judged.
        let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |r| {
            let event = match r {
                Ok(ev) if vis.allows_event(&ev) => Some(ev),
                Ok(_) => None,
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {
                    Some(LibraryEvent::StreamLagged)
                }
            };
            async move { event }
        });
        Ok(Box::pin(stream))
    }
}
