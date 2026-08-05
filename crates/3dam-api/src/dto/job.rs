//! Background jobs: what starts them, and their state, progress, artifacts, and terminal result.

use super::convert::ConvertReport;
use super::export::ExportReport;
use crate::id::{CollectionId, JobId, SourceId};
use crate::page::PageParams;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanRequest {
    #[serde(default)]
    pub sources: Vec<SourceId>,
    #[serde(default)]
    pub mode: ScanMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    #[default]
    Full,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Scan,
    Analyze,
    Convert,
    Export,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    pub current: Option<String>,
}

/// A durable, safe-to-render result of a background job. `route` is an application-relative report
/// or asset route, never a filesystem path; convert/export can populate these when their async job
/// manifests land without changing the history contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobArtifact {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    pub progress: Progress,
    #[serde(default)]
    pub error: Option<String>,
    /// Human-readable terminal summary. Unlike `error`, this describes completed work and remains
    /// available in job history after the live progress event has passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Recoverable, per-item degradation. A done job with warnings is a partial success, not an
    /// unqualified success and not a hard failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_artifacts: Vec<JobArtifact>,
    /// Structured terminal output, persisted so history can reopen the complete report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<JobResult>>,
    /// Millisecond Unix timestamps persisted with the job row.
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// The account/token/automation identity which started the job, when it can be attributed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
    /// Every source this job touches, recorded when the job is created (issue #42). `Progress.current`
    /// names a live file path, so a restricted identity may only observe a job whose sources are
    /// *all* within its ceiling — see
    /// [`Visibility::allows_job`](crate::service::Visibility::allows_job).
    ///
    /// Empty means "unattributed" (a pre-attribution job row, or a server older than this field) and
    /// is therefore reachable only at `Visibility::Full`.
    #[serde(default)]
    pub sources: Vec<SourceId>,
    /// Collection grants used by a restricted job.
    #[serde(default)]
    pub collections: Vec<CollectionId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "report", rename_all = "snake_case")]
pub enum JobResult {
    Convert(ConvertReport),
    Export(ExportReport),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JobListRequest {
    #[serde(default)]
    pub kinds: Vec<JobKind>,
    #[serde(default)]
    pub state: Option<JobState>,
    #[serde(default)]
    pub page: PageParams,
}
