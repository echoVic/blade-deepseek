use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel::{Receiver, Sender};
use orca_core::cancel::OperationId;

use crate::types::{TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};

#[derive(Clone, Debug)]
enum TuiInteractionTerminal {
    Response(TuiInteractionResponse),
    Interrupted,
    Shutdown,
}

#[derive(Debug)]
struct PendingInteraction {
    waiter_id: u64,
    sender: Sender<TuiInteractionTerminal>,
}

#[derive(Debug)]
struct TuiInteractionBrokerState {
    accepting: bool,
    active_operation: Option<OperationId>,
    next_waiter_id: u64,
    pending: HashMap<TuiInteractionKey, PendingInteraction>,
}

impl Default for TuiInteractionBrokerState {
    fn default() -> Self {
        Self {
            accepting: true,
            active_operation: None,
            next_waiter_id: 1,
            pending: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TuiInteractionBroker {
    state: Arc<Mutex<TuiInteractionBrokerState>>,
}

impl TuiInteractionBroker {
    pub(crate) fn activate(&self, operation_id: OperationId) -> io::Result<()> {
        let interrupted = {
            let mut state = self.lock_state();
            if !state.accepting {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI interaction broker is shut down",
                ));
            }
            if state.active_operation == Some(operation_id) {
                return Ok(());
            }
            state.active_operation = Some(operation_id);
            std::mem::take(&mut state.pending)
        };
        Self::wake(interrupted, TuiInteractionTerminal::Interrupted);
        Ok(())
    }

    pub(crate) fn register(
        &self,
        operation_id: OperationId,
        kind: TuiInteractionKind,
        request_id: impl Into<String>,
    ) -> io::Result<TuiInteractionWaiter> {
        let mut state = self.lock_state();
        if !state.accepting {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI interaction broker is shut down",
            ));
        }
        if state.active_operation != Some(operation_id) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "TUI interaction request targets an inactive operation",
            ));
        }
        let key = TuiInteractionKey::new(operation_id, request_id, kind);
        if state.pending.contains_key(&key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending TUI interaction key: {key:?}"),
            ));
        }
        let waiter_id = state.next_waiter_id;
        state.next_waiter_id = state.next_waiter_id.saturating_add(1);
        let (sender, receiver) = crossbeam_channel::bounded(1);
        state
            .pending
            .insert(key.clone(), PendingInteraction { waiter_id, sender });
        Ok(TuiInteractionWaiter {
            broker: self.clone(),
            key,
            waiter_id,
            receiver,
        })
    }

    pub(crate) fn respond(
        &self,
        key: &TuiInteractionKey,
        response: TuiInteractionResponse,
    ) -> io::Result<()> {
        if response.kind() != key.kind {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "TUI interaction response kind {:?} does not match {:?}",
                    response.kind(),
                    key.kind
                ),
            ));
        }
        let pending = {
            let mut state = self.lock_state();
            if !state.accepting {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI interaction broker is shut down",
                ));
            }
            if state.active_operation != Some(key.operation_id) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "TUI interaction response targets a stale operation",
                ));
            }
            state.pending.remove(key).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "TUI interaction response has no matching waiter",
                )
            })?
        };
        pending
            .sender
            .send(TuiInteractionTerminal::Response(response))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI interaction waiter disconnected before response",
                )
            })
    }

    pub(crate) fn interrupt(&self, operation_id: OperationId) {
        let interrupted = {
            let mut state = self.lock_state();
            if state.active_operation == Some(operation_id) {
                state.active_operation = None;
            }
            let keys = state
                .pending
                .keys()
                .filter(|key| key.operation_id == operation_id)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.pending.remove(&key).map(|pending| (key, pending)))
                .collect()
        };
        Self::wake(interrupted, TuiInteractionTerminal::Interrupted);
    }

    pub(crate) fn complete(&self, operation_id: OperationId) {
        self.interrupt(operation_id);
    }

    pub(crate) fn shutdown(&self) {
        let pending = {
            let mut state = self.lock_state();
            state.accepting = false;
            state.active_operation = None;
            std::mem::take(&mut state.pending)
        };
        Self::wake(pending, TuiInteractionTerminal::Shutdown);
    }

    fn abandon(&self, key: &TuiInteractionKey, waiter_id: u64) {
        let mut state = self.lock_state();
        if state
            .pending
            .get(key)
            .is_some_and(|pending| pending.waiter_id == waiter_id)
        {
            state.pending.remove(key);
        }
    }

    fn wake(
        pending: HashMap<TuiInteractionKey, PendingInteraction>,
        terminal: TuiInteractionTerminal,
    ) {
        for pending in pending.into_values() {
            debug_assert!(!matches!(terminal, TuiInteractionTerminal::Response(_)));
            let _ = pending.sender.send(terminal.clone());
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, TuiInteractionBrokerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug)]
pub(crate) struct TuiInteractionWaiter {
    broker: TuiInteractionBroker,
    key: TuiInteractionKey,
    waiter_id: u64,
    receiver: Receiver<TuiInteractionTerminal>,
}

impl TuiInteractionWaiter {
    pub(crate) fn key(&self) -> &TuiInteractionKey {
        &self.key
    }

    pub(crate) fn wait(self) -> io::Result<TuiInteractionResponse> {
        match self.receiver.recv() {
            Ok(TuiInteractionTerminal::Response(response)) => Ok(response),
            Ok(TuiInteractionTerminal::Interrupted) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "TUI interaction interrupted",
            )),
            Ok(TuiInteractionTerminal::Shutdown) | Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI interaction broker shut down while waiting",
            )),
        }
    }
}

impl Drop for TuiInteractionWaiter {
    fn drop(&mut self) {
        self.broker.abandon(&self.key, self.waiter_id);
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use std::sync::LazyLock;

    use orca_core::cancel::OperationIdAllocator;

    use super::TuiInteractionBroker;
    use crate::types::{TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse};

    fn operation_id() -> orca_core::cancel::OperationId {
        static IDS: LazyLock<OperationIdAllocator> = LazyLock::new(OperationIdAllocator::new);
        IDS.allocate()
    }

    #[test]
    fn duplicate_interaction_key_does_not_replace_original_waiter() {
        let broker = TuiInteractionBroker::default();
        let operation_id = operation_id();
        broker.activate(operation_id).expect("activate operation");
        let first = broker
            .register(operation_id, TuiInteractionKind::UserInput, "ask")
            .expect("register original waiter");

        let error = broker
            .register(operation_id, TuiInteractionKind::UserInput, "ask")
            .expect_err("duplicate must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        broker
            .respond(
                first.key(),
                TuiInteractionResponse::UserInput("original".to_string()),
            )
            .expect("respond to original waiter");
        assert_eq!(
            first.wait().expect("original response"),
            TuiInteractionResponse::UserInput("original".to_string())
        );
    }

    #[test]
    fn stale_operation_response_is_rejected_after_request_id_reuse() {
        let broker = TuiInteractionBroker::default();
        let first_operation = operation_id();
        let second_operation = operation_id();
        broker.activate(first_operation).expect("activate first");
        let first = broker
            .register(first_operation, TuiInteractionKind::UserInput, "reused")
            .expect("register first waiter");
        let stale_key = first.key().clone();

        broker.activate(second_operation).expect("activate second");
        assert!(matches!(first.wait(), Err(error) if error.kind() == io::ErrorKind::Interrupted));
        let second = broker
            .register(second_operation, TuiInteractionKind::UserInput, "reused")
            .expect("register reused id in second operation");

        let stale_error = broker
            .respond(
                &stale_key,
                TuiInteractionResponse::UserInput("stale".to_string()),
            )
            .expect_err("stale response must fail closed");
        assert_eq!(stale_error.kind(), io::ErrorKind::NotFound);
        broker
            .respond(
                second.key(),
                TuiInteractionResponse::UserInput("fresh".to_string()),
            )
            .expect("fresh response");
        assert_eq!(
            second.wait().expect("fresh waiter response"),
            TuiInteractionResponse::UserInput("fresh".to_string())
        );
    }

    #[test]
    fn response_kind_must_match_the_registered_interaction_kind() {
        let broker = TuiInteractionBroker::default();
        let operation_id = operation_id();
        broker.activate(operation_id).expect("activate operation");
        let waiter = broker
            .register(operation_id, TuiInteractionKind::McpElicitation, "mcp")
            .expect("register MCP waiter");

        let error = broker
            .respond(
                waiter.key(),
                TuiInteractionResponse::UserInput("wrong kind".to_string()),
            )
            .expect_err("wrong response kind must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        broker
            .respond(
                waiter.key(),
                TuiInteractionResponse::McpElicitation {
                    accepted: true,
                    content_json: Some("{}".to_string()),
                },
            )
            .expect("typed MCP response");
        assert!(matches!(
            waiter.wait(),
            Ok(TuiInteractionResponse::McpElicitation { accepted: true, .. })
        ));
    }

    #[test]
    fn interrupt_wakes_each_interaction_kind() {
        for kind in [
            TuiInteractionKind::Approval,
            TuiInteractionKind::Permission,
            TuiInteractionKind::UserInput,
            TuiInteractionKind::McpElicitation,
        ] {
            let broker = TuiInteractionBroker::default();
            let operation_id = operation_id();
            broker.activate(operation_id).expect("activate operation");
            let waiter = broker
                .register(operation_id, kind, "request")
                .expect("register waiter");

            broker.interrupt(operation_id);

            assert!(matches!(
                waiter.wait(),
                Err(error) if error.kind() == io::ErrorKind::Interrupted
            ));
        }
    }

    #[test]
    fn interrupt_deactivates_operation_and_rejects_late_registration() {
        let broker = TuiInteractionBroker::default();
        let operation_id = operation_id();
        broker.activate(operation_id).expect("activate operation");
        let waiter = broker
            .register(operation_id, TuiInteractionKind::Approval, "approval")
            .expect("register waiter");

        broker.interrupt(operation_id);

        assert!(matches!(
            waiter.wait(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted
        ));
        assert!(matches!(
            broker.register(operation_id, TuiInteractionKind::UserInput, "late"),
            Err(error) if error.kind() == io::ErrorKind::NotConnected
        ));
    }

    #[test]
    fn shutdown_wakes_all_waiters_and_rejects_late_work() {
        let broker = TuiInteractionBroker::default();
        let operation_id = operation_id();
        broker.activate(operation_id).expect("activate operation");
        let approval = broker
            .register(operation_id, TuiInteractionKind::Approval, "approval")
            .expect("register approval");
        let input = broker
            .register(operation_id, TuiInteractionKind::UserInput, "input")
            .expect("register input");
        let late_key = TuiInteractionKey::new(operation_id, "late", TuiInteractionKind::UserInput);

        broker.shutdown();

        for waiter in [approval, input] {
            assert!(matches!(
                waiter.wait(),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe
            ));
        }
        assert!(matches!(
            broker.register(operation_id, TuiInteractionKind::UserInput, "late"),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
        assert!(matches!(
            broker.respond(
                &late_key,
                TuiInteractionResponse::UserInput("late".to_string())
            ),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }
}
