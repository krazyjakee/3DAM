//! Immediate and background export orchestration.

use crate::*;

impl EmbeddedLibrary {
    pub(crate) async fn export_impl(
        &self,
        ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<ExportReport, LibError> {
        if let Some(assets) = self.federated_export_assets(ctx, &req).await? {
            return tokio::task::spawn_blocking(move || export::run_federated_export(req, &assets))
                .await
                .map_err(|error| LibError::Internal(error.to_string()))?;
        }
        let vis = ctx.visibility.clone();
        self.db(move |s| export::run_export(s, req, &vis)).await
    }

    pub(crate) async fn submit_export_impl(
        &self,
        ctx: &AuthContext,
        req: ExportRequest,
    ) -> Result<JobId, LibError> {
        let federated_assets = self.federated_export_assets(ctx, &req).await?;
        let total = (!req.assets.is_empty()).then_some(req.assets.len() as u64);
        let params = serde_json::to_string(&req).unwrap_or_else(|_| "{}".into());
        let (sources, collections): (Vec<SourceId>, Vec<CollectionId>) =
            if let Some(scope) = ctx.visibility.restricted() {
                (
                    scope.sources.iter().copied().collect(),
                    scope.collections.iter().copied().collect(),
                )
            } else {
                let sources = self
                    .db(|store| {
                        Ok(store
                            .list_sources()?
                            .into_iter()
                            .map(|s| s.id)
                            .collect::<Vec<_>>())
                    })
                    .await?;
                (sources, Vec::new())
            };
        let job = self
            .db(move |store| {
                store.create_job_scoped(JobKind::Export, &params, total, &sources, &collections)
            })
            .await?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap().insert(job, cancel.clone());
        let store = self.store.clone();
        let events = self.events.clone();
        let vis = ctx.visibility.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = if let Some(assets) = federated_assets {
                if cancel.load(Ordering::Relaxed) {
                    Err(LibError::Cancelled)
                } else {
                    export::run_federated_export(req, &assets)
                }
            } else {
                export::run_export_with_checkpoint(&store, req, &vis, |done| {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(LibError::Cancelled);
                    }
                    reliability::retryable_store_write(
                        store.update_job_progress(
                            &job,
                            JobState::Running,
                            done,
                            total,
                            Some("Encoding manifest"),
                        ),
                        "persist export progress",
                        Some(&job),
                        None,
                    );
                    emit_progress(&store, &events, &job);
                    Ok(())
                })
            };
            match outcome {
                Ok(report) => {
                    let result = JobResult::Export(report.clone());
                    let state = if cancel.load(Ordering::Relaxed) {
                        JobState::Cancelled
                    } else {
                        JobState::Done
                    };
                    let summary = if state == JobState::Cancelled {
                        format!(
                            "Cancellation arrived after {} file(s) were committed; output was retained",
                            report.files_written
                        )
                    } else {
                        format!(
                            "Exported {} asset(s) to {} file(s)",
                            report.assets, report.files_written
                        )
                    };
                    let finished = reliability::required_background_write(
                        store.finish_job(&job, state, &summary, &[], Some(&result)),
                        "persist export terminal report",
                        &job,
                    );
                    if state == JobState::Done && matches!(finished, Some(false)) {
                        let summary = format!(
                            "Cancellation arrived after {} file(s) were committed; output was retained",
                            report.files_written
                        );
                        reliability::required_background_write(
                            store.finish_job(
                                &job,
                                JobState::Cancelled,
                                &summary,
                                &[],
                                Some(&result),
                            ),
                            "persist export cancellation race report",
                            &job,
                        );
                    }
                }
                Err(LibError::Cancelled) => {
                    reliability::required_background_write(
                        store.finish_job(
                            &job,
                            JobState::Cancelled,
                            "Export cancelled; staged output was removed",
                            &[],
                            None,
                        ),
                        "persist export cancellation",
                        &job,
                    );
                }
                Err(error) if cancel.load(Ordering::Relaxed) => {
                    reliability::required_background_write(
                        store.finish_job(
                            &job,
                            JobState::Cancelled,
                            "Export cancelled; staged output was removed",
                            &[],
                            None,
                        ),
                        "persist export cancellation after error",
                        &job,
                    );
                    tracing::debug!(%job, %error, "export stopped after cancellation");
                }
                Err(error) => {
                    reliability::required_background_write(
                        store.set_job_state(&job, JobState::Failed, Some(&error.to_string())),
                        "persist export failure",
                        &job,
                    );
                }
            }
            reliability::required_background_write(
                store.set_job_artifacts(
                    &job,
                    &[JobArtifact {
                        label: "Open export report".into(),
                        route: Some(format!("/jobs?job={job}")),
                    }],
                ),
                "persist export report artifact",
                &job,
            );
            emit_progress(&store, &events, &job);
        });
        Ok(job)
    }
}
