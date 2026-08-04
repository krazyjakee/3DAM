//! The bounded write stage's accumulator (issue #138, tech-spec 14 §4).
//!
//! Scan and analysis both used to persist one asset through a handful of independent `&self` store
//! calls, each one re-taking the writer and autocommitting. `dam-store`'s batch API replaced that
//! with "a slice in, one transaction"; this is the piece on the engine side that decides *how big a
//! slice*, and it is deliberately the only piece — there is no writer thread and no channel. A
//! producer (a rayon worker) pushes its finished unit of work under the accumulator's mutex; the
//! push that crosses a bound hands the pending vector straight back to that worker, which releases
//! the lock and performs the flush itself. Other workers keep computing into the fresh vector and
//! block on the store's own writer guard if they cross a bound before the first flush returns.
//! That blocking *is* the backpressure the issue asks for, with no shutdown ordering to get wrong.
//!
//! A bound is not a tuning knob here, it is a promise about a specific resource:
//!
//! * **rows** bound the writer-lock hold time,
//! * **bytes** bound resident memory,
//! * **age** bounds how long finished work can stay invisible (and un-durable).
//!
//! All three are checked, because each one alone is insufficient — see the constants below.

use std::time::{Duration, Instant};

/// Rows per flush.
///
/// This is a **writer-lock-hold** bound before it is a throughput knob. Every item in a batch runs
/// inside one `Immediate` transaction on the store's single write connection, so the batch size is
/// exactly how long every other writer waits: an interactive tag edit, a note, the next flush, and
/// notably [`dam_store::Store::repair_aggregates`], which takes an `Immediate` transaction of its
/// own and would simply wait out however long a batch takes. 64 analysed assets — derived rows,
/// vectors, tag suggestions and one FTS reindex apiece — commit well inside the ~50 ms p99 hold we
/// are willing to impose on those; the per-transaction overhead they amortise (one WAL flush and
/// one guard hand-off instead of ~28 per asset) is already almost entirely captured by 64.
pub(crate) const MAX_BATCH_ROWS: usize = 64;

/// Payload bytes per flush.
///
/// Required, not an optimisation: a row bound is not a memory bound. An analysis write carries
/// `document_text` up to `dam_media::MAX_TEXT_BYTES` (1 MiB) per asset, plus a waveform peak array
/// that reaches the catalog as JSON floats, so a batch of 64 documents would pin ~64 MiB of
/// accumulator — on top of whatever the rayon workers still hold — before the row bound so much as
/// noticed. 8 MiB keeps a pending batch comfortably smaller than the thumbnail cache's own
/// footprint while still holding every batch of ordinary images/audio (a few KiB each) at the row
/// bound, where the amortisation lives.
pub(crate) const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// How long a partial batch may sit unwritten.
///
/// The tail of a pass, a trickle from the watcher, or a handful of very slow decodes will never
/// reach either size bound; without an age bound their results would stay invisible (no event, no
/// progress) and non-durable until the job ended. 250 ms is below the ~500 ms at which a stalled
/// progress bar starts reading as a hang, and far above the cost of the extra commits it can cause
/// (at worst four a second, and only while the pipeline is running that slowly).
pub(crate) const MAX_BATCH_AGE: Duration = Duration::from_millis(250);

/// A pending batch and its three bounds.
///
/// Not itself synchronised — callers wrap it in a `Mutex` and hold that lock for the push/take
/// only, never across the flush.
pub(crate) struct BatchAccumulator<T> {
    pending: Vec<T>,
    bytes: usize,
    /// When the *oldest* pending item arrived, i.e. when the age clock started. `None` while empty,
    /// so an idle accumulator is never "due".
    opened: Option<Instant>,
    max_rows: usize,
    max_bytes: usize,
    max_age: Duration,
}

impl<T> BatchAccumulator<T> {
    pub(crate) fn new() -> BatchAccumulator<T> {
        BatchAccumulator::with_bounds(MAX_BATCH_ROWS, MAX_BATCH_BYTES, MAX_BATCH_AGE)
    }

    pub(crate) fn with_bounds(
        max_rows: usize,
        max_bytes: usize,
        max_age: Duration,
    ) -> BatchAccumulator<T> {
        BatchAccumulator {
            pending: Vec::new(),
            bytes: 0,
            opened: None,
            max_rows,
            max_bytes,
            max_age,
        }
    }

    /// Add one finished unit of work, charging `cost_bytes` of payload for it. Returns whether the
    /// batch is now due by *size* — the caller is expected to [`take`](Self::take) and flush it.
    ///
    /// The size bounds are checked after the push rather than before it, so a single item larger
    /// than `max_bytes` still flushes (as a batch of one) instead of wedging the accumulator.
    pub(crate) fn push(&mut self, item: T, cost_bytes: usize) -> bool {
        if self.pending.is_empty() {
            self.opened = Some(Instant::now());
        }
        self.pending.push(item);
        self.bytes = self.bytes.saturating_add(cost_bytes);
        self.pending.len() >= self.max_rows || self.bytes >= self.max_bytes
    }

    /// Whether the oldest pending item has waited out the age bound. Always false when empty:
    /// there is nothing to make durable, and an "always due" idle accumulator would turn every
    /// caller's due-check into a commit.
    pub(crate) fn due_by_age(&self) -> bool {
        self.opened
            .is_some_and(|since| since.elapsed() >= self.max_age)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Hand the pending batch to the caller and reset every bound, including the age clock — the
    /// next item starts a fresh window.
    pub(crate) fn take(&mut self) -> Vec<T> {
        self.bytes = 0;
        self.opened = None;
        std::mem::take(&mut self.pending)
    }
}

impl<T> Default for BatchAccumulator<T> {
    fn default() -> BatchAccumulator<T> {
        BatchAccumulator::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_bound_closes_a_batch() {
        let mut acc: BatchAccumulator<u32> = BatchAccumulator::new();
        for n in 0..MAX_BATCH_ROWS as u32 - 1 {
            assert!(!acc.push(n, 1), "batch closed early at {n}");
        }
        assert!(acc.push(99, 1), "the row bound did not close the batch");
        assert_eq!(acc.take().len(), MAX_BATCH_ROWS);
        assert!(acc.is_empty());
    }

    /// The reason the byte bound is not optional: nine one-megabyte documents are nowhere near the
    /// 64-row bound, and a row-only accumulator would hold every one of them resident.
    #[test]
    fn the_payload_bound_closes_a_batch_the_row_bound_would_not() {
        let mut acc: BatchAccumulator<u32> = BatchAccumulator::new();
        let document = 1024 * 1024;
        let mut pushed = 0;
        while !acc.push(pushed, document) {
            pushed += 1;
            assert!(
                pushed < MAX_BATCH_ROWS as u32,
                "the payload bound never fired"
            );
        }
        let batch = acc.take();
        assert_eq!(batch.len(), MAX_BATCH_BYTES / document);
        assert!(
            batch.len() < MAX_BATCH_ROWS,
            "the row bound, not the payload bound, closed this batch"
        );
    }

    /// A single item over the whole byte budget must still flush rather than wedge the batch.
    #[test]
    fn an_oversized_item_flushes_alone() {
        let mut acc: BatchAccumulator<u32> = BatchAccumulator::new();
        assert!(acc.push(1, MAX_BATCH_BYTES * 4));
        assert_eq!(acc.take().len(), 1);
    }

    #[test]
    fn the_age_bound_only_applies_to_a_non_empty_batch() {
        let mut acc: BatchAccumulator<u32> = BatchAccumulator::with_bounds(
            MAX_BATCH_ROWS,
            MAX_BATCH_BYTES,
            Duration::from_millis(1),
        );
        assert!(!acc.due_by_age(), "an empty accumulator is never due");
        assert!(!acc.push(1, 1), "one small item is not due by size");
        std::thread::sleep(Duration::from_millis(5));
        assert!(acc.due_by_age(), "the age bound did not fire");
        // Taking the batch restarts the clock, so the next partial batch gets its own window.
        assert_eq!(acc.take().len(), 1);
        assert!(!acc.due_by_age());
    }
}
