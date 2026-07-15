use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use orca_core::cancel::{CancelToken, OperationCancellation, OperationId, OperationScope};
use orca_runtime::runtime_host::{InterruptOperationResult, OperationHandle};

use crate::interaction_broker::TuiInteractionBroker;
use crate::interaction_broker::TuiInteractionWaiter;
use crate::types::TuiInteractionKind;

pub(crate) trait TuiOperationInterrupt {
    fn interrupt_current(&self);
}

#[derive(Clone, Debug)]
pub(crate) struct TuiOperationController {
    local_cancellation: Option<OperationCancellation>,
    hosted: Arc<HostedOperationState>,
    broker: TuiInteractionBroker,
    background_current: Arc<Mutex<Option<OperationId>>>,
}

#[derive(Debug, Default)]
struct HostedOperationState {
    inner: Mutex<HostedOperationInner>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct HostedOperationInner {
    active: Option<Arc<OperationHandle>>,
    interrupt_requested: bool,
    background_requested: bool,
    shutdown: bool,
}

impl TuiOperationController {
    pub(crate) fn new(cancellation: OperationCancellation, broker: TuiInteractionBroker) -> Self {
        Self {
            local_cancellation: Some(cancellation),
            hosted: Arc::new(HostedOperationState::default()),
            broker,
            background_current: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn hosted(broker: TuiInteractionBroker) -> Self {
        Self {
            local_cancellation: None,
            hosted: Arc::new(HostedOperationState::default()),
            broker,
            background_current: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn start(&self) -> io::Result<TuiOperationScope> {
        let cancellation = self.local_cancellation.as_ref().ok_or_else(|| {
            io::Error::other("local TUI operation admission is unavailable in hosted mode")
        })?;
        let operation = cancellation.start();
        if let Err(error) = self.broker.activate(operation.id()) {
            cancellation.complete(operation.id());
            return Err(error);
        }
        *self.lock_background_current() = None;
        Ok(TuiOperationScope {
            controller: self.clone(),
            operation: Some(operation),
        })
    }

    #[cfg(test)]
    pub(crate) fn current_id(&self) -> Option<OperationId> {
        self.lock_hosted()
            .active
            .as_ref()
            .map(|operation| operation.id())
            .or_else(|| {
                self.local_cancellation
                    .as_ref()
                    .and_then(OperationCancellation::current_id)
            })
    }

    pub(crate) fn interrupt_current(&self) -> Option<OperationId> {
        let hosted = {
            let mut hosted = self.lock_hosted();
            if let Some(operation) = hosted.active.clone() {
                Some(operation)
            } else if self.local_cancellation.is_none() {
                if hosted.shutdown {
                    return None;
                }
                hosted.interrupt_requested = true;
                return None;
            } else {
                None
            }
        };
        let operation_id = if let Some(operation) = hosted {
            let operation_id = operation.id();
            match operation.interrupt() {
                Ok(
                    InterruptOperationResult::Requested { .. }
                    | InterruptOperationResult::AlreadyRequested { .. },
                ) => operation_id,
                Ok(
                    InterruptOperationResult::Stale { .. } | InterruptOperationResult::Idle { .. },
                )
                | Err(_) => return None,
            }
        } else {
            self.local_cancellation.as_ref()?.cancel_current()?
        };
        self.broker.interrupt(operation_id);
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
        Some(operation_id)
    }

    pub(crate) fn request_background_current(&self) -> bool {
        if self.local_cancellation.is_none() {
            let mut hosted = self.lock_hosted();
            if hosted.shutdown {
                return false;
            }
            if let Some(operation_id) = hosted.active.as_ref().map(|operation| operation.id()) {
                *self.lock_background_current() = Some(operation_id);
            } else {
                hosted.background_requested = true;
            }
            return true;
        }
        let Some(operation_id) = self
            .local_cancellation
            .as_ref()
            .and_then(OperationCancellation::current_id)
        else {
            return false;
        };
        *self.lock_background_current() = Some(operation_id);
        true
    }

    pub(crate) fn take_background_current(&self, operation_id: OperationId) -> bool {
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn shutdown(&self) {
        if let Some(cancellation) = &self.local_cancellation {
            cancellation.shutdown();
        }
        let hosted = {
            let mut hosted = self.lock_hosted();
            hosted.shutdown = true;
            hosted.active.clone()
        };
        if let Some(operation) = hosted {
            let _ = operation.interrupt();
        }
        self.hosted.changed.notify_all();
        self.broker.shutdown();
        *self.lock_background_current() = None;
    }

    #[cfg(test)]
    pub(crate) fn cancellation(&self) -> &OperationCancellation {
        self.local_cancellation
            .as_ref()
            .expect("local TUI cancellation is unavailable in hosted mode")
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.lock_hosted().shutdown
            || self
                .local_cancellation
                .as_ref()
                .is_some_and(OperationCancellation::is_shutdown)
    }

    pub(crate) fn install_hosted(&self, operation: Arc<OperationHandle>) -> io::Result<()> {
        let operation_id = operation.id();
        {
            let hosted = self.lock_hosted();
            if hosted.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI operation controller is shutting down",
                ));
            }
            if let Some(active) = hosted.active.as_ref() {
                return Err(io::Error::other(format!(
                    "TUI operation {:?} is still active",
                    active.id()
                )));
            }
        }
        self.broker.activate(operation_id)?;
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            self.broker.complete(operation_id);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "TUI operation controller is shutting down",
            ));
        }
        let interrupt_requested = hosted.interrupt_requested;
        let background_requested = hosted.background_requested;
        hosted.interrupt_requested = false;
        hosted.background_requested = false;
        hosted.active = Some(Arc::clone(&operation));
        *self.lock_background_current() = background_requested.then_some(operation_id);
        drop(hosted);
        self.hosted.changed.notify_all();
        if interrupt_requested {
            let _ = operation.interrupt();
            self.broker.interrupt(operation_id);
        }
        Ok(())
    }

    pub(crate) fn wait_for_hosted(
        &self,
        operation_id: OperationId,
        cancel: &CancelToken,
    ) -> io::Result<TuiTurnControl> {
        let mut hosted = self.lock_hosted();
        loop {
            if hosted.shutdown || cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "TUI hosted operation was cancelled before activation",
                ));
            }
            if let Some(active) = hosted.active.as_ref() {
                if active.id() != operation_id {
                    return Err(io::Error::other(format!(
                        "TUI hosted operation activation mismatch: expected {:?}, found {:?}",
                        operation_id,
                        active.id()
                    )));
                }
                return Ok(TuiTurnControl {
                    controller: self.clone(),
                    operation_id,
                    cancel: cancel.clone(),
                });
            }
            let (next, _) = self
                .hosted
                .changed
                .wait_timeout(hosted, Duration::from_millis(10))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            hosted = next;
        }
    }

    pub(crate) fn complete_hosted(&self, operation_id: OperationId) {
        self.broker.complete(operation_id);
        let mut hosted = self.lock_hosted();
        if hosted.active.as_ref().map(|operation| operation.id()) == Some(operation_id) {
            hosted.active = None;
        }
        hosted.interrupt_requested = false;
        hosted.background_requested = false;
        drop(hosted);
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
        self.hosted.changed.notify_all();
    }

    pub(crate) fn broker(&self) -> &TuiInteractionBroker {
        &self.broker
    }

    fn complete(&self, operation_id: OperationId) {
        self.broker.complete(operation_id);
        if let Some(cancellation) = &self.local_cancellation {
            cancellation.complete(operation_id);
        }
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
    }

    fn lock_background_current(&self) -> MutexGuard<'_, Option<OperationId>> {
        self.background_current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_hosted(&self) -> MutexGuard<'_, HostedOperationInner> {
        self.hosted
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for TuiOperationController {
    fn default() -> Self {
        Self::new(
            OperationCancellation::new(),
            TuiInteractionBroker::default(),
        )
    }
}

impl TuiOperationInterrupt for TuiOperationController {
    fn interrupt_current(&self) {
        let _ = TuiOperationController::interrupt_current(self);
    }
}

impl TuiOperationInterrupt for OperationCancellation {
    fn interrupt_current(&self) {
        let _ = self.cancel_current();
    }
}

pub(crate) struct TuiOperationScope {
    controller: TuiOperationController,
    operation: Option<OperationScope>,
}

impl TuiOperationScope {
    pub(crate) fn id(&self) -> OperationId {
        self.operation
            .as_ref()
            .expect("TUI operation scope available")
            .id()
    }

    pub(crate) fn token(&self) -> &CancelToken {
        self.operation
            .as_ref()
            .expect("TUI operation scope available")
            .token()
    }

    pub(crate) fn control(&self) -> TuiTurnControl {
        TuiTurnControl {
            controller: self.controller.clone(),
            operation_id: self.id(),
            cancel: self.token().clone(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.operation
            .as_ref()
            .expect("TUI operation scope available")
            .cancel();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuiTurnControl {
    controller: TuiOperationController,
    operation_id: OperationId,
    cancel: CancelToken,
}

impl TuiTurnControl {
    pub(crate) fn token(&self) -> &CancelToken {
        &self.cancel
    }

    pub(crate) fn register_interaction(
        &self,
        kind: TuiInteractionKind,
        request_id: impl Into<String>,
    ) -> io::Result<TuiInteractionWaiter> {
        self.controller
            .broker()
            .register(self.operation_id, kind, request_id)
    }

    pub(crate) fn take_background_current(&self) -> bool {
        self.controller.take_background_current(self.operation_id)
    }
}

impl Drop for TuiOperationScope {
    fn drop(&mut self) {
        if let Some(operation) = self.operation.take() {
            self.controller.complete(operation.id());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::TuiOperationController;
    use crate::types::TuiInteractionKind;

    #[test]
    fn dropping_operation_scope_clears_current_and_wakes_waiter() {
        let controller = TuiOperationController::default();
        let operation = controller.start().expect("start operation");
        let waiter = controller
            .broker()
            .register(operation.id(), TuiInteractionKind::Approval, "approval")
            .expect("register waiter");
        assert_eq!(controller.current_id(), Some(operation.id()));

        drop(operation);

        assert_eq!(controller.current_id(), None);
        assert!(matches!(
            waiter.wait(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted
        ));
    }

    #[test]
    fn old_scope_drop_cannot_clear_a_replacement_operation() {
        let controller = TuiOperationController::default();
        let first = controller.start().expect("start first");
        let second = controller.start().expect("start second");

        drop(first);

        assert_eq!(controller.current_id(), Some(second.id()));
        drop(second);
        assert_eq!(controller.current_id(), None);
    }

    #[test]
    fn background_current_turn_request_is_operation_scoped_and_one_shot() {
        let controller = TuiOperationController::default();
        assert!(!controller.request_background_current());
        let first = controller.start().expect("start first");
        assert!(controller.request_background_current());

        let second = controller.start().expect("start second");
        assert!(!controller.take_background_current(first.id()));
        assert!(!controller.take_background_current(second.id()));
        assert!(controller.request_background_current());
        assert!(controller.take_background_current(second.id()));
        assert!(!controller.take_background_current(second.id()));
    }
}
