//! Live-update events (tech-spec 03 §7). Carried over WebSocket when connected and over
//! in-memory channels when embedded — same types both sides.

use crate::dto::{AssetSummary, JobStatus, SourceState};
use crate::id::{AssetId, SourceId};
use serde::{Deserialize, Serialize};

/// Every per-asset variant carries the asset's `source_id` alongside its id. That attribution is what
/// makes the stream filterable against a [`Visibility`](crate::service::Visibility) ceiling without a
/// database round-trip per event per subscriber — see
/// [`Visibility::allows_event`](crate::service::Visibility::allows_event). `AssetAdded` takes its
/// attribution from the summary it already carries; `AssetChanged`/`AssetRemoved` carry only an id,
/// and a removed asset's source is unrecoverable *after* the delete, so both inline it on the variant.
///
/// `None` is the honest "unattributed" case (an older peer, or a source that could not be resolved)
/// and reads as outside every restricted ceiling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LibraryEvent {
    AssetAdded(AssetSummary),
    AssetChanged {
        id: AssetId,
        #[serde(default)]
        source_id: Option<SourceId>,
        kind: ChangeKind,
    },
    AssetRemoved {
        id: AssetId,
        #[serde(default)]
        source_id: Option<SourceId>,
    },
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
    /// The user's free-text note was set or cleared (issue #81).
    NoteSet,
    /// A discussion message was posted, edited, or deleted (issue #82).
    ///
    /// Deliberately a `ChangeKind` on the existing `AssetChanged` rather than a new event variant
    /// with its own topic: `AssetChanged` already carries `source_id`, which is exactly what
    /// [`Visibility::allows_event`](crate::service::Visibility::allows_event) filters on. Reusing it
    /// means comment events cannot fan out to a subscriber who cannot see the asset — the leak is
    /// closed by construction rather than by a filter someone has to remember to write.
    Commented,
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
