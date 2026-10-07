//! One cancellable admission lane per source, shared by submitted and automatic scans.

use dam_api::id::{JobId, SourceId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) type JobCancels = Arc<Mutex<HashMap<JobId, Arc<AtomicBool>>>>;

pub(crate) struct Coordinator {
    slots: Mutex<HashMap<SourceId, Arc<Slot>>>,
    pub(crate) cancels: JobCancels,
}

#[derive(Default)]
struct State {
    active: bool,
    manual_waiters: usize,
}

#[derive(Default)]
struct Slot {
    state: Mutex<State>,
    ready: Condvar,
}

pub(crate) struct Lease(Arc<Slot>);

impl Coordinator {
    pub(crate) fn new(cancels: JobCancels) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            cancels,
        }
    }

    pub(crate) fn acquire(
        &self,
        source: SourceId,
        manual: bool,
        cancel: &AtomicBool,
    ) -> Option<Lease> {
        let slot = self
            .slots
            .lock()
            .unwrap()
            .entry(source)
            .or_default()
            .clone();
        let mut state = slot.state.lock().unwrap();
        state.manual_waiters += usize::from(manual);
        loop {
            if cancel.load(Ordering::Relaxed) {
                state.manual_waiters -= usize::from(manual);
                slot.ready.notify_all();
                return None;
            }
            if !state.active && (manual || state.manual_waiters == 0) {
                state.manual_waiters -= usize::from(manual);
                state.active = true;
                drop(state);
                return Some(Lease(slot));
            }
            state = slot
                .ready
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap()
                .0;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().active = false;
        self.0.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_sources_progress_and_cancelled_waiters_do_not_take_a_lane() {
        let coordinator = Coordinator::new(Arc::default());
        let source = SourceId::new();
        let cancel = AtomicBool::new(false);
        let lease = coordinator.acquire(source, false, &cancel).unwrap();
        let independent = coordinator.acquire(SourceId::new(), true, &cancel).unwrap();
        assert!(coordinator
            .acquire(source, true, &AtomicBool::new(true))
            .is_none());
        drop(lease);
        assert!(coordinator.acquire(source, false, &cancel).is_some());
        drop(independent);
    }

    #[test]
    fn submitted_and_automatic_scans_cannot_overlap() {
        let coordinator = Coordinator::new(Arc::default());
        let source = SourceId::new();
        let cancel = AtomicBool::new(false);
        let first = coordinator.acquire(source, false, &cancel).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _second = coordinator.acquire(source, true, &cancel).unwrap();
                tx.send(()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
            drop(first);
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
    }
}
