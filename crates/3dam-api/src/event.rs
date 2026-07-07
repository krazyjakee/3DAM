//! Live-update events (tech-spec 03 §7). Carried over WebSocket when connected and over
//! in-memory channels when embedded — same types both sides.

use crate::dto::{AssetSummary, JobStatus, SourceState};
use crate::id::{AssetId, SourceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LibraryEvent {
    AssetAdded(AssetSummary),
    AssetChanged {
        id: AssetId,
        kind: ChangeKind,
    },
    AssetRemoved(AssetId),
    SourceState {
        id: SourceId,
        state: SourceState,
    },
    JobProgress(JobStatus),
    /// The whole catalog was reset by a maintenance wipe/factory-reset. Carries no ids — clients
    /// drop their caches and refetch everything rather than diffing thousands of removals.
    CatalogReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Reanalyzed,
    Retagged,
    LicenseSet,
    Metadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    Progress(crate::dto::Progress),
    Warning(crate::page::ItemWarning),
    Done(JobStatus),
    Failed { message: String },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SubscribeRequest {
    #[serde(default)]
    pub topics: Vec<EventTopic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTopic {
    Assets,
    Sources,
    Jobs,
    Analysis,
}
