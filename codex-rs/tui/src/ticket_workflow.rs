//! Session-lifetime coordination for the repository ticket workflow.
//!
//! Ticket calls arrive through one loopback MCP endpoint, but they are not one
//! logical conversation.  Keep the short-lived scheduling and connection
//! state keyed by the authenticated caller tuple so a busy MAIN/SIDE caller
//! cannot hide, cancel, or consume another caller's work.

use crate::app_event_sender::AppEventSender;
use codex_app_server_client::AppServerRequestHandle;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::RwLock;

/// The authenticated identity attached to every loopback task-tool call.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CallerKey {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) call_id: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("ticket workflow caller is already executing call {0}")]
    DuplicateCall(String),
    #[error("ticket workflow caller queue is full (limit {0})")]
    CallerLimit(usize),
    #[error("ticket workflow queue is full (limit {0})")]
    GlobalLimit(usize),
    #[error("TUI is reconnecting; tool was not sent")]
    Reconnecting,
    #[error("task call ID was already consumed in this session: {0}")]
    ReplayedCall(String),
    #[error("caller replay ledger is full; start a new session before submitting more calls")]
    ReplayLedgerFull,
}

const MAX_ACTIVE_CALLS: usize = 64;
const MAX_ACTIVE_CALLS_PER_CALLER: usize = 8;
const MAX_RECORDED_CALLS_PER_CALLER: usize = 4096;
const MAX_RECORDED_CALLERS: usize = 128;

#[derive(Default)]
struct CallState {
    active: HashMap<CallerKey, ()>,
    // Fail closed instead of evicting IDs and allowing replayed mutations.
    consumed: HashMap<String, HashSet<(String, String)>>,
}

/// Session-owned coordination shared by every MCP request and notification
/// pump.  It deliberately contains no ticket authority; canonical assignment
/// and lifecycle checks remain in `TicketStore` and `dynamic_tools`.
pub(crate) struct TicketWorkflowCoordinator {
    connection: RwLock<Option<(AppServerRequestHandle, AppEventSender)>>,
    calls: Mutex<CallState>,
    retry: Mutex<RetryState>,
}

impl TicketWorkflowCoordinator {
    pub(crate) fn new(
        request_handle: AppServerRequestHandle,
        app_event_tx: AppEventSender,
    ) -> Arc<Self> {
        Arc::new(Self {
            connection: RwLock::new(Some((request_handle, app_event_tx))),
            calls: Mutex::new(CallState::default()),
            retry: Mutex::new(RetryState::default()),
        })
    }

    pub(crate) fn suspend(&self) {
        *self
            .connection
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn reconnect(&self, handle: AppServerRequestHandle, events: AppEventSender) {
        *self
            .connection
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some((handle, events));
    }

    /// Snapshot the current connection once.  The caller may then await
    /// external app-server work without holding coordinator state.
    pub(crate) fn connection(
        &self,
    ) -> Result<(AppServerRequestHandle, AppEventSender), AdmissionError> {
        self.connection
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(AdmissionError::Reconnecting)
    }

    pub(crate) fn admit(self: &Arc<Self>, caller: CallerKey) -> Result<CallPermit, AdmissionError> {
        let mut state = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        try_admit(&mut state, &caller)?;
        Ok(CallPermit {
            coordinator: Arc::clone(self),
            caller,
        })
    }

    fn release(&self, caller: &CallerKey) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active
            .remove(caller);
    }

    pub(crate) fn active_call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active
            .len()
    }

    /// Coalesce startup/reconnect/store-change retry requests.  Only the
    /// first caller runs the pump; subsequent callers mark it dirty and the
    /// pump runs once more after the current pass, without holding a lock over
    /// app-server or user/model waits.
    pub(crate) fn claim_retry(self: &Arc<Self>) -> Option<RetryPermit> {
        let mut state = self.retry.lock().unwrap_or_else(PoisonError::into_inner);
        if state.claim() {
            Some(RetryPermit {
                coordinator: Arc::clone(self),
                active: true,
            })
        } else {
            None
        }
    }
}

/// Claim, dirtying and finish must share one lock. Separate atomics can lose a
/// request between testing the dirty flag and releasing the running flag.
#[derive(Default)]
struct RetryState {
    running: bool,
    requested: bool,
}

impl RetryState {
    fn claim(&mut self) -> bool {
        if self.running {
            self.requested = true;
            false
        } else {
            self.running = true;
            self.requested = false;
            true
        }
    }

    fn finish_pass(&mut self) -> bool {
        if std::mem::take(&mut self.requested) {
            true
        } else {
            self.running = false;
            false
        }
    }
}

pub(crate) struct RetryPermit {
    coordinator: Arc<TicketWorkflowCoordinator>,
    active: bool,
}

impl RetryPermit {
    pub(crate) fn finish_pass(&mut self) -> bool {
        let again = self
            .coordinator
            .retry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finish_pass();
        self.active = again;
        again
    }
}

impl Drop for RetryPermit {
    fn drop(&mut self) {
        // Cancellation/panic/disconnection must not permanently wedge retries.
        // Durable undelivered events are reconciled by the next pump request.
        if self.active {
            self.coordinator
                .retry
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .running = false;
        }
    }
}

fn try_admit(state: &mut CallState, caller: &CallerKey) -> Result<(), AdmissionError> {
    if state.active.contains_key(caller) {
        return Err(AdmissionError::DuplicateCall(caller.call_id.clone()));
    }
    if state.active.len() >= MAX_ACTIVE_CALLS {
        return Err(AdmissionError::GlobalLimit(MAX_ACTIVE_CALLS));
    }
    let caller_active = state
        .active
        .keys()
        .filter(|key| key.thread_id == caller.thread_id)
        .count();
    if caller_active >= MAX_ACTIVE_CALLS_PER_CALLER {
        return Err(AdmissionError::CallerLimit(MAX_ACTIVE_CALLS_PER_CALLER));
    }
    if let Some(consumed) = state.consumed.get(&caller.thread_id) {
        if consumed.contains(&(caller.turn_id.clone(), caller.call_id.clone())) {
            return Err(AdmissionError::ReplayedCall(caller.call_id.clone()));
        }
        if consumed.len() >= MAX_RECORDED_CALLS_PER_CALLER {
            return Err(AdmissionError::ReplayLedgerFull);
        }
    } else if state.consumed.len() >= MAX_RECORDED_CALLERS {
        return Err(AdmissionError::ReplayLedgerFull);
    }
    state
        .consumed
        .entry(caller.thread_id.clone())
        .or_default()
        .insert((caller.turn_id.clone(), caller.call_id.clone()));
    state.active.insert(caller.clone(), ());
    Ok(())
}

pub(crate) struct CallPermit {
    coordinator: Arc<TicketWorkflowCoordinator>,
    caller: CallerKey,
}

impl Drop for CallPermit {
    fn drop(&mut self) {
        self.coordinator.release(&self.caller);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(thread_id: &str, call_id: usize) -> CallerKey {
        CallerKey {
            thread_id: thread_id.into(),
            turn_id: "turn".into(),
            call_id: format!("call-{call_id}"),
        }
    }

    fn disconnected_coordinator() -> Arc<TicketWorkflowCoordinator> {
        Arc::new(TicketWorkflowCoordinator {
            connection: RwLock::new(None),
            calls: Mutex::new(CallState::default()),
            retry: Mutex::new(RetryState::default()),
        })
    }

    #[tokio::test]
    async fn cancellation_releases_only_its_own_call_and_retry_permits() {
        let coordinator = disconnected_coordinator();
        let side = coordinator.admit(caller("side", 1)).unwrap();
        let main = coordinator.admit(caller("main", 1)).unwrap();
        let retry = coordinator.claim_retry().unwrap();
        assert!(coordinator.claim_retry().is_none());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _main = main;
            let _retry = retry;
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        assert_eq!(coordinator.active_call_count(), 2);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(coordinator.active_call_count(), 1);
        assert!(matches!(
            coordinator.admit(caller("side", 1)),
            Err(AdmissionError::DuplicateCall(_))
        ));
        let mut retry = coordinator
            .claim_retry()
            .expect("cancelled pump must be reclaimable");
        assert!(!retry.finish_pass());
        // An old finished permit must not clear a newly claimed pump on Drop.
        let next = coordinator.claim_retry().unwrap();
        drop(retry);
        assert!(coordinator.claim_retry().is_none());
        drop(next);
        drop(side);
        assert_eq!(coordinator.active_call_count(), 0);
    }

    #[test]
    fn retry_during_pass_is_coalesced_and_retry_after_finish_reclaims() {
        let mut retry = RetryState::default();
        assert!(retry.claim());
        for _ in 0..100 {
            assert!(!retry.claim());
        }
        assert!(retry.finish_pass());
        assert!(!retry.finish_pass());
        assert!(retry.claim());
        assert!(!retry.finish_pass());
    }

    #[test]
    fn concurrent_retry_and_finish_cannot_lose_request() {
        for _ in 0..100 {
            let retry = Arc::new(Mutex::new(RetryState::default()));
            assert!(retry.lock().unwrap().claim());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let other = Arc::clone(&retry);
            let other_barrier = Arc::clone(&barrier);
            let claimant = std::thread::spawn(move || {
                other_barrier.wait();
                other.lock().unwrap().claim()
            });
            barrier.wait();
            let run_again = retry.lock().unwrap().finish_pass();
            let newly_claimed = claimant.join().unwrap();
            assert_ne!(run_again, newly_claimed);
            assert!(retry.lock().unwrap().running);
        }
    }

    #[test]
    fn admission_is_duplicate_safe_and_fair_across_threads() {
        let mut state = CallState::default();
        let first = caller("main", 1);
        try_admit(&mut state, &first).unwrap();
        assert!(matches!(
            try_admit(&mut state, &first),
            Err(AdmissionError::DuplicateCall(_))
        ));
        for call_id in 2..=MAX_ACTIVE_CALLS_PER_CALLER {
            try_admit(&mut state, &caller("main", call_id)).unwrap();
        }
        assert!(matches!(
            try_admit(&mut state, &caller("main", 99)),
            Err(AdmissionError::CallerLimit(MAX_ACTIVE_CALLS_PER_CALLER))
        ));
        try_admit(&mut state, &caller("side", 1)).unwrap();
    }

    #[test]
    fn consumed_calls_cannot_replay_after_completion() {
        let coordinator = disconnected_coordinator();
        drop(coordinator.admit(caller("main", 1)).unwrap());
        assert!(matches!(
            coordinator.admit(caller("main", 1)),
            Err(AdmissionError::ReplayedCall(_))
        ));
        assert!(coordinator.admit(caller("side", 1)).is_ok());
    }

    #[test]
    fn admission_has_a_global_bound() {
        let mut state = CallState::default();
        for index in 0..MAX_ACTIVE_CALLS {
            try_admit(&mut state, &caller(&format!("thread-{index}"), index)).unwrap();
        }
        assert!(matches!(
            try_admit(&mut state, &caller("overflow", 0)),
            Err(AdmissionError::GlobalLimit(MAX_ACTIVE_CALLS))
        ));
    }
}
