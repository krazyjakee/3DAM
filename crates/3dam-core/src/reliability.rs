//! Shared policy for operations that are intentionally allowed to degrade.
//!
//! Catalog/job terminal writes do not belong here: callers propagate those failures. This module
//! is only for secondary status writes that can be retried on a later pass and lossy broadcasts
//! whose channel deliberately has no durability guarantee.

use dam_api::event::LibraryEvent;
use dam_api::id::{JobId, SourceId};
use dam_api::LibError;
use tokio::sync::broadcast;

pub(crate) fn retryable_store_write(
    result: Result<(), LibError>,
    operation: &'static str,
    job: Option<&JobId>,
    source: Option<&SourceId>,
) {
    if let Err(error) = result {
        tracing::warn!(
            operation,
            job = job.map(ToString::to_string),
            source = source.map(ToString::to_string),
            %error,
            "retryable persistence operation failed"
        );
    }
}

/// Publish a lossy live event. Durable state remains authoritative when there are no receivers.
/// The boolean is useful to tests and callers that want to count delivery; a closed channel is not
/// promoted to an operation failure.
pub(crate) fn publish_event(
    events: &broadcast::Sender<LibraryEvent>,
    event: LibraryEvent,
    operation: &'static str,
) -> bool {
    match events.send(event) {
        Ok(_) => true,
        Err(_) => {
            tracing::debug!(
                operation,
                "lossy event dropped because no receiver is attached"
            );
            false
        }
    }
}

pub(crate) fn background_job_failed(job: &JobId, operation: &'static str, error: &LibError) {
    tracing::error!(%job, operation, %error, "background job persistence failed");
}

/// A detached worker cannot return a persistence error to the request that already accepted it.
/// Treat such writes as required anyway: report failure at error severity and make the caller
/// explicitly branch on the missing value instead of discarding the `Result`.
pub(crate) fn required_background_write<T>(
    result: Result<T, LibError>,
    operation: &'static str,
    job: &JobId,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            background_job_failed(job, operation, &error);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_api::dto::JobState;

    #[test]
    fn closed_event_receiver_is_an_observable_nonfatal_drop() {
        let (events, receiver) = broadcast::channel(1);
        drop(receiver);

        assert!(!publish_event(
            &events,
            LibraryEvent::JobProgress(dam_api::dto::JobStatus {
                id: JobId::new(),
                kind: dam_api::dto::JobKind::Scan,
                state: JobState::Queued,
                progress: dam_api::dto::Progress {
                    done: 0,
                    total: None,
                    current: None,
                },
                error: None,
                sources: Vec::new(),
                collections: Vec::new(),
                created_at: 0,
                updated_at: 0,
                summary: None,
                warnings: Vec::new(),
                initiator: None,
                result_artifacts: Vec::new(),
                result: None,
            }),
            "test event",
        ));
    }
}
