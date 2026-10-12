//! Shared lifetime protection for local data operations and maintenance.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

static CACHE_IO: Mutex<()> = Mutex::new(());

/// Serialize database/cache snapshots with transfer downloads and deletions
/// already running on worker threads. Acquire only within blocking I/O work.
pub fn lock_cache_io() -> MutexGuard<'static, ()> {
    CACHE_IO.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Default)]
pub struct DataOperationState(Arc<AtomicBool>);

impl DataOperationState {
    pub fn is_active(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn try_acquire(&self, maintenance_running: bool) -> Option<DataOperationGuard> {
        if maintenance_running
            || self
                .0
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }
        Some(DataOperationGuard {
            _lease: Arc::new(DataOperationLease(self.0.clone())),
        })
    }
}

/// Keep maintenance suspended until both the UI task and background I/O end,
/// including when a foreground task is cancelled while I/O is still running.
#[derive(Clone)]
pub struct DataOperationGuard {
    _lease: Arc<DataOperationLease>,
}

struct DataOperationLease(Arc<AtomicBool>);

impl Drop for DataOperationLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::{lock_cache_io, DataOperationState};

    #[test]
    fn cache_snapshot_waits_for_an_in_flight_transfer_operation() {
        let transfer_guard = lock_cache_io();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let snapshot = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _snapshot_guard = lock_cache_io();
            acquired_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            acquired_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(transfer_guard);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        snapshot.join().unwrap();
    }

    #[test]
    fn maintenance_and_other_storage_operations_prevent_acquisition() {
        let state = DataOperationState::default();
        assert!(state.try_acquire(true).is_none());
        assert!(!state.is_active());
        let guard = state.try_acquire(false).unwrap();
        assert!(state.is_active());
        assert!(state.try_acquire(false).is_none());
        drop(guard);
        assert!(!state.is_active());
        assert!(state.try_acquire(false).is_some());
    }

    #[test]
    fn cancelling_the_ui_task_keeps_maintenance_blocked_until_background_io_finishes() {
        let state = DataOperationState::default();
        let foreground = state.try_acquire(false).unwrap();
        let background = foreground.clone();
        drop(foreground);
        assert!(state.is_active());
        assert!(state.try_acquire(false).is_none());
        std::thread::spawn(move || drop(background)).join().unwrap();
        assert!(!state.is_active());
        assert!(state.try_acquire(false).is_some());
    }
}
