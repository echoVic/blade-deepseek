use std::collections::VecDeque;
use std::io;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::operation_controller::TuiOperationController;
use crate::types::{TuiEvent, UserAction};

pub(crate) struct TuiActionDispatcher {
    shutdown_tx: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl TuiActionDispatcher {
    pub(crate) fn spawn(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        controller: TuiOperationController,
        command_capacity: usize,
        backlog_capacity: usize,
    ) -> io::Result<(Self, Receiver<UserAction>)> {
        if command_capacity == 0 || backlog_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TUI dispatcher capacities must be greater than zero",
            ));
        }
        let (command_tx, command_rx) = crossbeam_channel::bounded(command_capacity);
        let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded(1);
        let handle = thread::Builder::new()
            .name("orca-tui-action-dispatcher".to_string())
            .spawn(move || {
                run_dispatcher(
                    action_rx,
                    event_tx,
                    command_tx,
                    shutdown_rx,
                    controller,
                    backlog_capacity,
                )
            })?;
        Ok((
            Self {
                shutdown_tx,
                handle: Some(handle),
            },
            command_rx,
        ))
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let _ = self.shutdown_tx.try_send(());
        handle
            .join()
            .map_err(|_| io::Error::other("TUI action dispatcher panicked during shutdown"))
    }
}

impl Drop for TuiActionDispatcher {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn run_dispatcher(
    action_rx: Receiver<UserAction>,
    event_tx: Sender<TuiEvent>,
    command_tx: Sender<UserAction>,
    shutdown_rx: Receiver<()>,
    controller: TuiOperationController,
    backlog_capacity: usize,
) {
    let mut backlog = VecDeque::with_capacity(backlog_capacity);
    'dispatch: loop {
        while let Some(action) = backlog.pop_front() {
            match command_tx.try_send(action) {
                Ok(()) => {}
                Err(TrySendError::Full(action)) => {
                    backlog.push_front(action);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => break 'dispatch,
            }
        }

        if backlog.is_empty() {
            crossbeam_channel::select! {
                recv(shutdown_rx) -> _ => break,
                recv(action_rx) -> action => {
                    let Ok(action) = action else { break };
                    if !route_action(
                        action,
                        &command_tx,
                        &event_tx,
                        &controller,
                        &mut backlog,
                        backlog_capacity,
                    ) {
                        break;
                    }
                }
            }
        } else {
            crossbeam_channel::select! {
                recv(shutdown_rx) -> _ => break,
                recv(action_rx) -> action => {
                    let Ok(action) = action else { break };
                    if !route_action(
                        action,
                        &command_tx,
                        &event_tx,
                        &controller,
                        &mut backlog,
                        backlog_capacity,
                    ) {
                        break;
                    }
                }
                default(Duration::from_millis(2)) => {}
            }
        }
    }
    controller.shutdown();
}

fn route_action(
    action: UserAction,
    command_tx: &Sender<UserAction>,
    event_tx: &Sender<TuiEvent>,
    controller: &TuiOperationController,
    backlog: &mut VecDeque<UserAction>,
    backlog_capacity: usize,
) -> bool {
    match action {
        UserAction::RespondToInteraction { key, response } => {
            let _ = controller.broker().respond(&key, response);
        }
        UserAction::Interrupt => {
            controller.interrupt_current();
        }
        UserAction::BackgroundCurrentTurn => {
            controller.request_background_current();
        }
        UserAction::Cancel => return false,
        action => {
            if backlog.is_empty() {
                match command_tx.try_send(action) {
                    Ok(()) => return true,
                    Err(TrySendError::Full(action)) => backlog.push_back(action),
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            } else if backlog.len() < backlog_capacity {
                backlog.push_back(action);
            } else {
                reject_overflowed_action(event_tx, action);
            }
        }
    }
    true
}

fn reject_overflowed_action(event_tx: &Sender<TuiEvent>, action: UserAction) {
    let message = "TUI command queue is full; command rejected".to_string();
    match action {
        UserAction::Submit(prompt) | UserAction::SubmitWithMentions { prompt, .. } => {
            let _ = event_tx.try_send(TuiEvent::SubmissionRejected { prompt, message });
        }
        UserAction::SubmitWorkflowNotification(_)
        | UserAction::RunWorkflow { .. }
        | UserAction::Compact
        | UserAction::GoalShow
        | UserAction::GoalSet(_)
        | UserAction::GoalEdit(_)
        | UserAction::GoalClear
        | UserAction::GoalPause
        | UserAction::GoalResume => {
            let _ = event_tx.try_send(TuiEvent::OperationRejected(message));
        }
        _ => {
            let _ = event_tx.try_send(TuiEvent::Error(message));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use crossbeam_channel as mpsc;

    use super::TuiActionDispatcher;
    use crate::interaction_broker::TuiInteractionBroker;
    use crate::operation_controller::TuiOperationController;
    use crate::test_support::HostedOperationHarness;
    use crate::types::{TuiEvent, TuiInteractionKind, TuiInteractionResponse, UserAction};

    #[test]
    fn full_command_mailbox_does_not_block_interaction_response() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let operation = HostedOperationHarness::start();
        let controller = operation.controller().clone();
        let waiter = controller
            .broker()
            .register(
                operation.operation().id(),
                TuiInteractionKind::UserInput,
                "ask",
            )
            .expect("register waiter");
        let key = waiter.key().clone();
        let (mut dispatcher, command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, controller.clone(), 1, 1)
                .expect("spawn dispatcher");

        raw_tx
            .send(UserAction::Submit("first".to_string()))
            .expect("queue first command");
        raw_tx
            .send(UserAction::Submit("second".to_string()))
            .expect("queue second command");
        raw_tx
            .send(UserAction::RespondToInteraction {
                key,
                response: TuiInteractionResponse::UserInput("answer".to_string()),
            })
            .expect("queue interaction response");

        let (done_tx, done_rx) = mpsc::bounded(1);
        std::thread::spawn(move || done_tx.send(waiter.wait()).expect("wait result"));
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("response bypasses full command mailbox")
                .expect("interaction response"),
            TuiInteractionResponse::UserInput("answer".to_string())
        );
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::Submit(prompt)) if prompt == "first"
        ));
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::Submit(prompt)) if prompt == "second"
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn full_command_mailbox_does_not_block_interrupt() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let operation = HostedOperationHarness::start();
        let controller = operation.controller().clone();
        let waiter = controller
            .broker()
            .register(
                operation.operation().id(),
                TuiInteractionKind::Approval,
                "approval",
            )
            .expect("register waiter");
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, controller.clone(), 1, 1)
                .expect("spawn dispatcher");

        raw_tx
            .send(UserAction::Submit("first".to_string()))
            .expect("queue first command");
        raw_tx
            .send(UserAction::Submit("second".to_string()))
            .expect("queue second command");
        raw_tx.send(UserAction::Interrupt).expect("queue interrupt");

        let (done_tx, done_rx) = mpsc::bounded(1);
        std::thread::spawn(move || done_tx.send(waiter.wait()).expect("wait result"));
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_secs(1)),
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted
        ));
        assert!(operation.cancel_token().is_cancelled());
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn cancel_shuts_down_broker_and_dispatcher_without_command_capacity() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let operation = HostedOperationHarness::start();
        let controller = operation.controller().clone();
        let waiter = controller
            .broker()
            .register(
                operation.operation().id(),
                TuiInteractionKind::McpElicitation,
                "mcp",
            )
            .expect("register waiter");
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, controller.clone(), 1, 1)
                .expect("spawn dispatcher");
        raw_tx
            .send(UserAction::Submit("fill".to_string()))
            .expect("fill command mailbox");
        raw_tx.send(UserAction::Cancel).expect("queue cancel");

        assert!(matches!(
            waiter.wait(),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
        dispatcher.shutdown().expect("join dispatcher");
        assert!(matches!(
            controller
                .broker()
                .register(
                    operation.operation().id(),
                    TuiInteractionKind::UserInput,
                    "late",
                ),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    #[test]
    fn overflowed_submit_is_rejected_with_its_prompt() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let (mut dispatcher, _command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, controller, 1, 1)
                .expect("spawn dispatcher");

        raw_tx
            .send(UserAction::Submit("first".to_string()))
            .unwrap();
        raw_tx
            .send(UserAction::Submit("second".to_string()))
            .unwrap();
        raw_tx
            .send(UserAction::Submit("third".to_string()))
            .unwrap();

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::SubmissionRejected { prompt, message })
                if prompt == "third" && message.contains("queue is full")
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }
}
