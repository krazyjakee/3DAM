//! Job lifecycle: create, progress, state, and listing — part of the `Store` impl, split out of `lib.rs`.
use super::*;
use crate::helpers::*;

impl Store {
    // ── jobs ───────────────────────────────────────────────────────────────

    /// Create a queued job. `sources` records every source the job will touch — the attribution a
    /// visibility ceiling is evaluated against (issue #42). Pass the *full* set: a job is observable
    /// by a restricted identity only when all of it is reachable, so an under-recorded job hides
    /// itself rather than leaking the paths of a source that was left out.
    pub fn create_job(
        &self,
        kind: JobKind,
        params_json: &str,
        total: Option<u64>,
        sources: &[SourceId],
    ) -> Result<JobId, LibError> {
        self.create_job_scoped(kind, params_json, total, sources, &[])
    }

    pub fn create_job_scoped(
        &self,
        kind: JobKind,
        params_json: &str,
        total: Option<u64>,
        sources: &[SourceId],
        collections: &[CollectionId],
    ) -> Result<JobId, LibError> {
        let id = JobId::new();
        let now = now_ms();
        let sources_json = encode_job_sources(sources);
        let collections_json = encode_job_collections(collections);
        // Automated callers are attributable from their stable internal request marker. Manual
        // callers currently have no identity at this storage seam, so remain honestly unknown.
        let initiator = if params_json.contains("\"watch\":true") {
            Some("Source watcher")
        } else if params_json.contains("\"auto\":true") {
            Some("Automation")
        } else {
            None
        };
        let conn = self.write();
        conn.execute(
            "INSERT INTO job (id, kind, state, params, progress, done, total, created_at, updated_at,
                              sources, initiator, collections)
             VALUES (?1, ?2, 'queued', ?3, 0, 0, ?4, ?5, ?5, ?6, ?7, ?8)",
            params![
                id.as_bytes().to_vec(),
                job_kind_str(kind),
                params_json,
                total.map(|t| t as i64),
                now,
                sources_json,
                initiator,
                collections_json,
            ],
        )
        .map_err(internal)?;
        Ok(id)
    }

    /// Persist a terminal structured report. Completing is conditional so a cancellation accepted
    /// concurrently with worker completion can never be overwritten by `Done`.
    pub fn finish_job(
        &self,
        id: &JobId,
        state: JobState,
        summary: &str,
        warnings: &[String],
        result: Option<&JobResult>,
    ) -> Result<bool, LibError> {
        if !matches!(state, JobState::Done | JobState::Cancelled) {
            return Err(LibError::BadRequest("invalid terminal result state".into()));
        }
        let warnings = serde_json::to_string(warnings).map_err(internal)?;
        let result = result
            .map(serde_json::to_string)
            .transpose()
            .map_err(internal)?;
        let conn = self.write();
        let changed = conn
            .execute(
                "UPDATE job SET state = ?2, summary = ?3, warnings = ?4, result = ?5,
                                error = NULL, current = NULL, updated_at = ?6
                 WHERE id = ?1 AND (?2 = 'cancelled' OR state <> 'cancelled')",
                params![
                    id.as_bytes().to_vec(),
                    job_state_str(state),
                    summary,
                    warnings,
                    result,
                    now_ms(),
                ],
            )
            .map_err(internal)?;
        Ok(changed != 0)
    }

    pub fn update_job_progress(
        &self,
        id: &JobId,
        state: JobState,
        done: u64,
        total: Option<u64>,
        current: Option<&str>,
    ) -> Result<(), LibError> {
        let progress = match total {
            Some(t) if t > 0 => (done as f64 / t as f64).min(1.0),
            _ => 0.0,
        };
        let conn = self.write();
        conn.execute(
            "UPDATE job SET state = ?2, done = ?3, total = ?4, current = ?5, progress = ?6, updated_at = ?7
             WHERE id = ?1 AND (?2 <> 'running' OR state <> 'cancelled')",
            params![
                id.as_bytes().to_vec(),
                job_state_str(state),
                done as i64,
                total.map(|t| t as i64),
                current,
                progress,
                now_ms(),
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn set_job_state(
        &self,
        id: &JobId,
        state: JobState,
        error: Option<&str>,
    ) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute(
            "UPDATE job SET state = ?2, error = ?3, updated_at = ?4
             WHERE id = ?1 AND (?2 NOT IN ('done', 'failed') OR state <> 'cancelled')",
            params![
                id.as_bytes().to_vec(),
                job_state_str(state),
                error,
                now_ms(),
            ],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Attach request attribution after submission. The server learns the authenticated actor at
    /// its boundary, while automatic/watch jobs are attributed at creation time.
    pub fn set_job_initiator(&self, id: &JobId, initiator: &str) -> Result<(), LibError> {
        let conn = self.write();
        conn.execute(
            "UPDATE job SET initiator = ?2 WHERE id = ?1",
            params![id.as_bytes().to_vec(), initiator],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Attach opaque in-app report/artifact links to a job. Routes are validated here before they
    /// become durable API data; future async convert/export manifests cannot persist a filesystem
    /// path, external URL, or traversal and have the web client turn it into a link.
    pub fn set_job_artifacts(&self, id: &JobId, artifacts: &[JobArtifact]) -> Result<(), LibError> {
        if artifacts
            .iter()
            .filter_map(|artifact| artifact.route.as_deref())
            .any(|route| !valid_artifact_route(route))
        {
            return Err(LibError::BadRequest(
                "job artifact routes must be safe application-relative paths".into(),
            ));
        }
        let encoded = serde_json::to_string(artifacts).map_err(internal)?;
        let conn = self.write();
        conn.execute(
            "UPDATE job SET result_artifacts = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), encoded, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    /// Persist a successful terminal report. Warnings deliberately live outside `error`: their
    /// presence means partial success, while `Failed` remains reserved for a whole-job failure.
    pub fn complete_job(
        &self,
        id: &JobId,
        summary: &str,
        warnings: &[String],
    ) -> Result<(), LibError> {
        let warnings_json = serde_json::to_string(warnings).map_err(internal)?;
        let conn = self.write();
        conn.execute(
            "UPDATE job SET state = 'done', summary = ?2, warnings = ?3, error = NULL,
                            updated_at = ?4 WHERE id = ?1 AND state <> 'cancelled'",
            params![id.as_bytes().to_vec(), summary, warnings_json, now_ms()],
        )
        .map_err(internal)?;
        Ok(())
    }

    pub fn get_job(&self, id: &JobId) -> Result<JobStatus, LibError> {
        let conn = self.write();
        conn.query_row(
            "SELECT id, kind, state, done, total, current, error, sources, created_at, updated_at,
                    summary, warnings, initiator, result_artifacts, result, collections
             FROM job WHERE id = ?1",
            params![id.as_bytes().to_vec()],
            Self::row_to_job,
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| LibError::NotFound(format!("job {id}")))
    }

    /// Lightweight event/list shape. The full structured result is intentionally fetched only by
    /// `get_job` when a caller opens one history detail.
    pub fn get_job_summary(&self, id: &JobId) -> Result<JobStatus, LibError> {
        let conn = self.write();
        conn.query_row(
            "SELECT id, kind, state, done, total, current, error, sources, created_at, updated_at,
                    summary, warnings, initiator, result_artifacts, NULL AS result, collections
             FROM job WHERE id = ?1",
            params![id.as_bytes().to_vec()],
            Self::row_to_job,
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| LibError::NotFound(format!("job {id}")))
    }

    /// List jobs newest-first, filtered to what `vis` may observe. A job outside the ceiling is
    /// simply absent — the same "unreachable reads as nonexistent" rule the asset queries follow.
    pub fn list_jobs(
        &self,
        req: &JobListRequest,
        vis: &Visibility,
    ) -> Result<Page<JobStatus>, LibError> {
        let limit = req.page.clamped(QUERY_MAX_LIMIT);
        let offset = decode_offset(req.page.after.as_ref())?;
        let conn = self.write();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, state, done, total, current, error, sources, created_at, updated_at,
                        summary, warnings, initiator, result_artifacts, NULL AS result, collections FROM job
                 ORDER BY created_at DESC, rowid DESC",
            )
            .map_err(internal)?;
        let rows = stmt.query_map([], Self::row_to_job).map_err(internal)?;
        let mut items = Vec::new();
        let mut raw_index = 0usize;
        let mut has_more = false;
        for r in rows {
            let job = r.map_err(internal)?;
            if raw_index < offset {
                raw_index += 1;
                continue;
            }
            let keep_kind = req.kinds.is_empty() || req.kinds.contains(&job.kind);
            let keep_state = req.state.map(|s| s == job.state).unwrap_or(true);
            if keep_kind && keep_state && vis.allows_job(&job) {
                if items.len() < limit as usize {
                    items.push(job);
                } else {
                    has_more = true;
                    break;
                }
            }
            raw_index += 1;
        }
        let next = if has_more {
            // The cursor is a physical row offset, not a visible-item count: inaccessible rows can
            // be interleaved, and counting only returned jobs would repeat/skip data on the next page.
            Some(Cursor(raw_index.to_string()))
        } else {
            None
        };
        Ok(Page::new(items, next))
    }

    fn row_to_job(r: &rusqlite::Row) -> rusqlite::Result<JobStatus> {
        let id = blob_to_job_id(&r.get::<_, Vec<u8>>(0)?);
        let kind_s: String = r.get(1)?;
        let state_s: String = r.get(2)?;
        let done: i64 = r.get(3)?;
        let total: Option<i64> = r.get(4)?;
        let current: Option<String> = r.get(5)?;
        let error: Option<String> = r.get(6)?;
        let sources: Option<String> = r.get(7)?;
        let created_at: i64 = r.get(8)?;
        let updated_at: i64 = r.get(9)?;
        let mut summary: Option<String> = r.get(10)?;
        let warnings: Option<String> = r.get(11)?;
        let initiator: Option<String> = r.get(12)?;
        let result_artifacts: Option<String> = r.get(13)?;
        let result: Option<String> = r.get(14)?;
        let collections: Option<String> = r.get(15)?;
        let mut decoded_warnings = decode_job_warnings(warnings.as_deref());
        let mut hard_error = error;
        // Before V16 completed scan/analyse notes occupied `error`. Preserve those reports while
        // keeping actual failed-job errors differentiated in the new API.
        if parse_job_state(&state_s) == JobState::Done && summary.is_none() {
            summary = hard_error.take();
            if summary.as_deref().is_some_and(|s| s.contains("skipped")) {
                decoded_warnings.push(summary.clone().unwrap_or_default());
            }
        }
        Ok(JobStatus {
            id,
            kind: parse_job_kind(&kind_s),
            state: parse_job_state(&state_s),
            progress: Progress {
                done: done as u64,
                total: total.map(|t| t as u64),
                current,
            },
            error: hard_error,
            summary,
            warnings: decoded_warnings,
            result_artifacts: decode_job_artifacts(result_artifacts.as_deref()),
            result: result.and_then(|value| serde_json::from_str(&value).ok()),
            created_at,
            updated_at,
            initiator,
            sources: decode_job_sources(sources.as_deref()),
            collections: decode_job_collections(collections.as_deref()),
        })
    }
}

fn decode_job_warnings(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

fn decode_job_artifacts(raw: Option<&str>) -> Vec<JobArtifact> {
    raw.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

/// Only a single-slash application route is linkable. Percent escapes are rejected rather than
/// decoded here so encoded separators/traversal cannot disagree with the browser/router decoder.
fn valid_artifact_route(route: &str) -> bool {
    let path = route.split(['?', '#']).next().unwrap_or_default();
    let known_surface = path == "/jobs"
        || path.starts_with("/jobs/")
        || path == "/reports"
        || path.starts_with("/reports/")
        || path == "/assets"
        || path.starts_with("/assets/");
    known_surface
        && route.starts_with('/')
        && !route.starts_with("//")
        && !route.contains('\\')
        && !route.contains('%')
        && !route.contains("://")
        && !path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

/// Job source attribution ↔ the `job.sources` TEXT column: a JSON array of canonical uuid strings.
/// JSON rather than a join table because the set is tiny, write-once at job creation, and only ever
/// read whole.
fn encode_job_sources(sources: &[SourceId]) -> String {
    let ids: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())
}

fn encode_job_collections(collections: &[CollectionId]) -> String {
    let ids: Vec<String> = collections.iter().map(|id| id.to_string()).collect();
    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())
}

/// Decode `job.sources`. NULL (a pre-V9 row) and anything unparseable read back as empty — the
/// unattributed case, which `Visibility::allows_job` treats as observable only at `Full`.
fn decode_job_sources(raw: Option<&str>) -> Vec<SourceId> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn decode_job_collections(raw: Option<&str>) -> Vec<CollectionId> {
    raw.and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|id| id.parse().ok())
        .collect()
}

fn job_kind_str(k: JobKind) -> &'static str {
    match k {
        JobKind::Scan => "scan",
        JobKind::Analyze => "analyse",
        JobKind::Convert => "convert",
        JobKind::Export => "export",
    }
}
fn parse_job_kind(s: &str) -> JobKind {
    match s {
        "analyse" => JobKind::Analyze,
        "convert" => JobKind::Convert,
        "export" => JobKind::Export,
        _ => JobKind::Scan,
    }
}
fn job_state_str(s: JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Paused => "paused",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}
fn parse_job_state(s: &str) -> JobState {
    match s {
        "running" => JobState::Running,
        "paused" => JobState::Paused,
        "done" => JobState::Done,
        "failed" => JobState::Failed,
        "cancelled" => JobState::Cancelled,
        _ => JobState::Queued,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::page::PageParams;
    use dam_api::service::VisibilityScope;
    use std::collections::BTreeSet;

    #[test]
    fn terminal_report_round_trips_separately_from_hard_errors() {
        let store = Store::open_in_memory().unwrap();
        let source = SourceId::new();
        let job = store
            .create_job(JobKind::Analyze, r#"{"auto":true}"#, Some(3), &[source])
            .unwrap();
        store
            .update_job_progress(&job, JobState::Done, 3, Some(3), None)
            .unwrap();
        store
            .complete_job(
                &job,
                "Analysed 2 of 3 item(s)",
                &["1 item could not be analysed; inspect source status".into()],
            )
            .unwrap();

        let status = store.get_job(&job).unwrap();
        assert_eq!(status.state, JobState::Done);
        assert_eq!(status.summary.as_deref(), Some("Analysed 2 of 3 item(s)"));
        assert_eq!(status.warnings.len(), 1);
        assert!(status.error.is_none());
        assert_eq!(status.initiator.as_deref(), Some("Automation"));
        assert!(status.updated_at >= status.created_at);
    }

    #[test]
    fn injected_sqlite_terminal_persistence_failures_are_returned() {
        let store = Store::open_in_memory().unwrap();
        let job = store
            .create_job(JobKind::Analyze, "{}", Some(1), &[])
            .unwrap();
        store.write().execute("DROP TABLE job", []).unwrap();

        assert!(store
            .set_job_state(&job, JobState::Failed, Some("analysis failed"))
            .is_err());
        assert!(store.complete_job(&job, "finished", &[]).is_err());
    }

    #[test]
    fn cancellation_wins_scan_and_analysis_completion_race() {
        let store = Store::open_in_memory().unwrap();
        let job = store
            .create_job(JobKind::Analyze, "{}", Some(1), &[])
            .unwrap();
        store
            .set_job_state(&job, JobState::Cancelled, None)
            .unwrap();

        store.complete_job(&job, "too late", &[]).unwrap();

        let status = store.get_job(&job).unwrap();
        assert_eq!(status.state, JobState::Cancelled);
        assert!(status.summary.is_none());
    }

    #[test]
    fn structured_result_survives_reload_and_cancel_wins_completion_race() {
        let data = std::env::temp_dir().join(format!("3dam-job-result-{}", JobId::new()));
        std::fs::create_dir_all(&data).unwrap();
        let collection = CollectionId::new();
        let job;
        let result = JobResult::Export(ExportReport {
            format: ExportFormat::Csv,
            output: "/tmp/manifest.csv".into(),
            assets: 12,
            files_written: 1,
        });
        {
            let store = Store::open(&data).unwrap();
            job = store
                .create_job_scoped(JobKind::Export, "{}", None, &[], &[collection])
                .unwrap();
            store
                .set_job_state(&job, JobState::Cancelled, None)
                .unwrap();
            assert!(!store
                .finish_job(&job, JobState::Done, "too late", &[], Some(&result))
                .unwrap());
            store
                .finish_job(
                    &job,
                    JobState::Cancelled,
                    "cancelled after output completed",
                    &[],
                    Some(&result),
                )
                .unwrap();
        }

        let store = Store::open(&data).unwrap();
        let status = store.get_job(&job).unwrap();
        assert_eq!(status.state, JobState::Cancelled);
        assert_eq!(status.collections, vec![collection]);
        match status.result.as_deref() {
            Some(JobResult::Export(report)) => assert_eq!(report.assets, 12),
            other => panic!("structured export result was not persisted: {other:?}"),
        }
        let listed = store
            .list_jobs(&JobListRequest::default(), &Visibility::Full)
            .unwrap();
        assert!(
            listed.items[0].result.is_none(),
            "history summaries must not eagerly hydrate large reports"
        );
        drop(store);
        std::fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn history_paging_counts_hidden_rows_without_skipping_visible_jobs() {
        let store = Store::open_in_memory().unwrap();
        let visible = SourceId::new();
        let hidden = SourceId::new();
        let oldest = store
            .create_job(JobKind::Scan, "{}", None, &[visible])
            .unwrap();
        let middle = store
            .create_job(JobKind::Scan, "{}", None, &[hidden])
            .unwrap();
        let newest = store
            .create_job(JobKind::Scan, "{}", None, &[visible])
            .unwrap();
        {
            let conn = store.write();
            for (id, at) in [(&oldest, 1_i64), (&middle, 2), (&newest, 3)] {
                conn.execute(
                    "UPDATE job SET created_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![id.as_bytes().to_vec(), at],
                )
                .unwrap();
            }
        }
        let vis = Visibility::Restricted(VisibilityScope {
            sources: BTreeSet::from([visible]),
            ..VisibilityScope::default()
        });
        let first = store
            .list_jobs(
                &JobListRequest {
                    page: PageParams {
                        after: None,
                        limit: 1,
                    },
                    ..JobListRequest::default()
                },
                &vis,
            )
            .unwrap();
        assert_eq!(first.items[0].id, newest);
        let second = store
            .list_jobs(
                &JobListRequest {
                    page: PageParams {
                        after: first.cursor,
                        limit: 1,
                    },
                    ..JobListRequest::default()
                },
                &vis,
            )
            .unwrap();
        assert_eq!(second.items[0].id, oldest);
    }

    #[test]
    fn artifact_routes_reject_external_and_filesystem_targets() {
        assert!(valid_artifact_route("/jobs?job=018f"));
        for unsafe_route in [
            "https://example.test/report",
            "//example.test/report",
            "/jobs/../secret",
            "/Users/operator/report.json",
            r"\server\share\report.json",
            "/jobs/%2e%2e/secret",
        ] {
            assert!(
                !valid_artifact_route(unsafe_route),
                "accepted {unsafe_route}"
            );
        }
    }
}
