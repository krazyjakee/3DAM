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
    // Struct-shaped, not `AssetRemoved(AssetId)`: with internal tagging (`tag = "type"`) serde
    // cannot serialize a newtype variant whose inner type is a scalar (AssetId is a hex string),
    // so the newtype form failed to serialize and every removal was silently dropped over WS.
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
    /// This subscriber could not keep up or was disconnected long enough to miss events. No
    /// per-item payload can make that gap safe: consumers must refresh their visible cache once.
    StreamLagged,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{
        AssetSummary, JobKind, JobState, JobStatus, LicenseBadge, MediaType, Origin, Progress,
    };
    use crate::id::JobId;

    fn asset_summary() -> AssetSummary {
        AssetSummary {
            id: AssetId::new(),
            name: "kick.wav".into(),
            media: MediaType::Audio,
            format: "wav".into(),
            size: 1024,
            license: LicenseBadge::default(),
            top_tags: vec!["drum".into()],
            origin: Origin::Local,
            key_attrs: Default::default(),
            favorite: false,
            source_id: Some(SourceId::new()),
        }
    }

    fn job_status() -> JobStatus {
        JobStatus {
            id: JobId::new(),
            kind: JobKind::Scan,
            state: JobState::Running,
            progress: Progress::default(),
            error: None,
            summary: None,
            warnings: Vec::new(),
            result_artifacts: Vec::new(),
            result: None,
            created_at: 0,
            updated_at: 0,
            initiator: None,
            sources: vec![SourceId::new()],
            collections: Vec::new(),
        }
    }

    /// Every `LibraryEvent` variant must serialize and round-trip. This guards the internal-tag
    /// pitfall: `#[serde(tag = "type")]` cannot serialize a *newtype* variant whose inner type is a
    /// scalar (e.g. `AssetRemoved(AssetId)` — AssetId is a string), which silently failed over WS.
    #[test]
    fn every_library_event_round_trips() {
        let events = vec![
            LibraryEvent::AssetAdded(asset_summary()),
            LibraryEvent::AssetChanged {
                id: AssetId::new(),
                source_id: Some(SourceId::new()),
                kind: ChangeKind::Reanalyzed,
            },
            LibraryEvent::AssetRemoved {
                id: AssetId::new(),
                source_id: Some(SourceId::new()),
            },
            // `None` attribution is a real wire case (unattributed / older peer) — cover it too.
            LibraryEvent::AssetRemoved {
                id: AssetId::new(),
                source_id: None,
            },
            LibraryEvent::SourceState {
                id: SourceId::new(),
                state: SourceState::Online,
            },
            LibraryEvent::JobProgress(job_status()),
            LibraryEvent::StreamLagged,
            LibraryEvent::CatalogReset,
        ];

        for ev in &events {
            // Must not error — the internally-tagged newtype-scalar case fails here.
            let json = serde_json::to_string(ev).expect("LibraryEvent must serialize");
            let back: LibraryEvent =
                serde_json::from_str(&json).expect("LibraryEvent must deserialize");
            // Re-serialize and compare wire form (LibraryEvent isn't PartialEq).
            let json2 = serde_json::to_string(&back).expect("round-trip must serialize");
            assert_eq!(json, json2, "round-trip changed the wire form for {ev:?}");
        }
    }

    /// The removal event carries its id under `id` (struct variant), internally tagged by `type`.
    #[test]
    fn asset_removed_wire_shape() {
        let id = AssetId::new();
        let json = serde_json::to_string(&LibraryEvent::AssetRemoved {
            id,
            source_id: None,
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "asset_removed");
        assert_eq!(v["id"], id.to_string());
    }
}
