use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use orca_core::cancel::{CancelToken, OperationCancellation, OperationId, OperationScope};

use crate::interaction_broker::TuiInteractionBroker;
use crate::interaction_broker::TuiInteractionWaiter;
use crate::types::TuiInteractionKind;

#[derive(Clone, Debug)]
pub(crate) struct TuiOperationController {
    cancellation: OperationCancellation,
    broker: TuiInteractionBroker,
    background_current: Arc<Mutex<Option<OperationId>>>,
}

impl TuiOperationController {
    pub(crate) fn new(cancellation: OperationCancellation, broker: TuiInteractionBroker) -> Self {
        Self {
            cancellation,
            broker,
            background_current: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn start(&self) -> io::Result<TuiOperationScope> {
        let operation = self.cancellation.start();
        if let Err(error) = self.broker.activate(operation.id()) {
            self.cancellation.complete(operation.id());
            return Err(error);
        }
        *self.lock_background_current() = None;
        Ok(TuiOperationScope {
            controller: self.clone(),
            operation: Some(operation),
        })
    }

    pub(crate) fn current_id(&self) -> Option<OperationId> {
        self.cancellation.current_id()
    }

    pub(crate) fn interrupt_current(&self) -> Option<OperationId> {
        let operation_id = self.cancellation.cancel_current()?;
        self.broker.interrupt(operation_id);
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
        Some(operation_id)
    }

    pub(crate) fn request_background_current(&self) -> bool {
        let Some(operation_id) = self.current_id() else {
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
        self.cancellation.shutdown();
        self.broker.shutdown();
        *self.lock_background_current() = None;
    }

    pub(crate) fn cancellation(&self) -> &OperationCancellation {
        &self.cancellation
    }

    pub(crate) fn broker(&self) -> &TuiInteractionBroker {
        &self.broker
    }

    fn complete(&self, operation_id: OperationId) {
        self.broker.complete(operation_id);
        self.cancellation.complete(operation_id);
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
}

impl Default for TuiOperationController {
    fn default() -> Self {
        Self::new(
            OperationCancellation::new(),
            TuiInteractionBroker::default(),
        )
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
