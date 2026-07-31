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
        let id = JobId::new();
        let now = now_ms();
        let sources_json = encode_job_sources(sources);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO job (id, kind, state, params, progress, done, total, created_at, updated_at, sources)
             VALUES (?1, ?2, 'queued', ?3, 0, 0, ?4, ?5, ?5, ?6)",
            params![
                id.as_bytes().to_vec(),
                job_kind_str(kind),
                params_json,
                total.map(|t| t as i64),
                now,
                sources_json,
            ],
        )
        .map_err(internal)?;
        Ok(id)
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE job SET state = ?2, done = ?3, total = ?4, current = ?5, progress = ?6, updated_at = ?7
             WHERE id = ?1",
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE job SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
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

    pub fn get_job(&self, id: &JobId) -> Result<JobStatus, LibError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, kind, state, done, total, current, error, sources FROM job WHERE id = ?1",
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
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, state, done, total, current, error, sources FROM job
                 ORDER BY created_at DESC LIMIT ? OFFSET ?",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], Self::row_to_job)
            .map_err(internal)?;
        let mut items = Vec::new();
        for r in rows {
            let job = r.map_err(internal)?;
            let keep_kind = req.kinds.is_empty() || req.kinds.contains(&job.kind);
            let keep_state = req.state.map(|s| s == job.state).unwrap_or(true);
            if keep_kind && keep_state && vis.allows_job(&job) {
                items.push(job);
            }
        }
        let next = if items.len() == limit as usize {
            Some(Cursor((offset + items.len()).to_string()))
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
        Ok(JobStatus {
            id,
            kind: parse_job_kind(&kind_s),
            state: parse_job_state(&state_s),
            progress: Progress {
                done: done as u64,
                total: total.map(|t| t as u64),
                current,
            },
            error,
            sources: decode_job_sources(sources.as_deref()),
        })
    }
}

/// Job source attribution ↔ the `job.sources` TEXT column: a JSON array of canonical uuid strings.
/// JSON rather than a join table because the set is tiny, write-once at job creation, and only ever
/// read whole.
fn encode_job_sources(sources: &[SourceId]) -> String {
    let ids: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
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
