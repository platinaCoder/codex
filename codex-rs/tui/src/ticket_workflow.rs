//! Session-lifetime coordination for the repository ticket workflow.
//!
//! Ticket calls arrive through one loopback MCP endpoint, but they are not one
//! logical conversation.  Keep the short-lived scheduling and connection
//! state keyed by the authenticated caller tuple so a busy MAIN/SIDE caller
//! cannot hide, cancel, or consume another caller's work.

use crate::app_event_sender::AppEventSender;
use codex_app_server_client::AppServerRequestHandle;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

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
}

const MAX_ACTIVE_CALLS: usize = 64;
const MAX_ACTIVE_CALLS_PER_CALLER: usize = 8;

struct CallState {
    active: HashMap<CallerKey, ()>,
}

/// Session-owned coordination shared by every MCP request and notification
/// pump.  It deliberately contains no ticket authority; canonical assignment
/// and lifecycle checks remain in `TicketStore` and `dynamic_tools`.
pub(crate) struct TicketWorkflowCoordinator {
    connection: RwLock<Option<(AppServerRequestHandle, AppEventSender)>>,
    calls: Mutex<CallState>,
    retry_in_flight: AtomicBool,
    retry_requested: AtomicBool,
}

impl TicketWorkflowCoordinator {
    pub(crate) fn new(
        request_handle: AppServerRequestHandle,
        app_event_tx: AppEventSender,
    ) -> Arc<Self> {
        Arc::new(Self {
            connection: RwLock::new(Some((request_handle, app_event_tx))),
            calls: Mutex::new(CallState {
                active: HashMap::new(),
            }),
            retry_in_flight: AtomicBool::new(false),
            retry_requested: AtomicBool::new(false),
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
    pub(crate) fn claim_retry(&self) -> bool {
        if self.retry_in_flight.swap(true, Ordering::AcqRel) {
            self.retry_requested.store(true, Ordering::Release);
            false
        } else {
            true
        }
    }

    pub(crate) fn take_retry_request(&self) -> bool {
        self.retry_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn release_retry(&self) {
        self.retry_in_flight.store(false, Ordering::Release);
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

    #[test]
    fn admission_is_duplicate_safe_and_fair_across_threads() {
        let mut state = CallState {
            active: HashMap::new(),
        };
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
    fn admission_has_a_global_bound() {
        let mut state = CallState {
            active: HashMap::new(),
        };
        for index in 0..MAX_ACTIVE_CALLS {
            try_admit(&mut state, &caller(&format!("thread-{index}"), index)).unwrap();
        }
        assert!(matches!(
            try_admit(&mut state, &caller("overflow", 0)),
            Err(AdmissionError::GlobalLimit(MAX_ACTIVE_CALLS))
        ));
    }
}
