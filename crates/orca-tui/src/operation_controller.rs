use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use orca_core::cancel::{CancelToken, OperationId};
use orca_runtime::provider_stream::RuntimeProviderSuspensionControl;
use orca_runtime::runtime_host::{InterruptOperationResult, OperationHandle};

use crate::interaction_broker::TuiInteractionBroker;
use crate::interaction_broker::TuiInteractionWaiter;
use crate::types::TuiInteractionKind;

pub(crate) trait TuiOperationInterrupt {
    fn interrupt_current(&self);
}

#[derive(Clone, Debug)]
pub(crate) struct TuiOperationController {
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
    pub(crate) fn hosted(broker: TuiInteractionBroker) -> Self {
        Self {
            hosted: Arc::new(HostedOperationState::default()),
            broker,
            background_current: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn current_id(&self) -> Option<OperationId> {
        self.lock_hosted()
            .active
            .as_ref()
            .map(|operation| operation.id())
    }

    pub(crate) fn interrupt_current(&self) -> Option<OperationId> {
        let hosted = {
            let mut hosted = self.lock_hosted();
            if let Some(operation) = hosted.active.clone() {
                operation
            } else {
                if hosted.shutdown {
                    return None;
                }
                hosted.interrupt_requested = true;
                return None;
            }
        };
        let operation_id = hosted.id();
        match hosted.interrupt() {
            Ok(
                InterruptOperationResult::Requested { .. }
                | InterruptOperationResult::AlreadyRequested { .. },
            ) => {}
            Ok(InterruptOperationResult::Stale { .. } | InterruptOperationResult::Idle { .. })
            | Err(_) => return None,
        };
        self.broker.interrupt(operation_id);
        let mut background = self.lock_background_current();
        if *background == Some(operation_id) {
            *background = None;
        }
        Some(operation_id)
    }

    pub(crate) fn request_background_current(&self) -> bool {
        let mut hosted = self.lock_hosted();
        if hosted.shutdown {
            return false;
        }
        if let Some(operation_id) = hosted.active.as_ref().map(|operation| operation.id()) {
            *self.lock_background_current() = Some(operation_id);
        } else {
            hosted.background_requested = true;
        }
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

    pub(crate) fn is_shutdown(&self) -> bool {
        self.lock_hosted().shutdown
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

impl TuiOperationInterrupt for TuiOperationController {
    fn interrupt_current(&self) {
        let _ = TuiOperationController::interrupt_current(self);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuiTurnControl {
    controller: TuiOperationController,
    operation_id: OperationId,
}

impl TuiTurnControl {
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

impl RuntimeProviderSuspensionControl for TuiTurnControl {
    fn take_suspension_request(&self) -> bool {
        TuiTurnControl::take_background_current(self)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use crate::test_support::HostedOperationHarness;
    use crate::types::TuiInteractionKind;

    #[test]
    fn completing_hosted_operation_clears_current_and_wakes_waiter() {
        let mut operation = HostedOperationHarness::start();
        let controller = operation.controller().clone();
        let waiter = controller
            .broker()
            .register(
                operation.operation().id(),
                TuiInteractionKind::Approval,
                "approval",
            )
            .expect("register waiter");
        assert_eq!(controller.current_id(), Some(operation.operation().id()));

        operation.finish();

        assert_eq!(controller.current_id(), None);
        assert!(matches!(
            waiter.wait(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted
        ));
    }

    #[test]
    fn hosted_controller_rejects_a_second_active_operation() {
        let first = HostedOperationHarness::start();
        let second = HostedOperationHarness::start();
        let controller = first.controller();

        let error = controller
            .install_hosted(second.operation_handle())
            .expect_err("second active operation must be rejected");

        assert!(error.to_string().contains("still active"));
        assert_eq!(controller.current_id(), Some(first.operation().id()));
    }

    #[test]
    fn background_current_turn_request_is_operation_scoped_and_one_shot() {
        let operation = HostedOperationHarness::start();
        let controller = operation.controller();
        assert!(controller.request_background_current());
        assert!(controller.take_background_current(operation.operation().id()));
        assert!(!controller.take_background_current(operation.operation().id()));
    }
}
