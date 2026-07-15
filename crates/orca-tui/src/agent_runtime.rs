use std::io;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};
use orca_runtime::runtime_host::{RuntimeHost, RuntimeHostHandle};

use crate::action_dispatcher::TuiActionDispatcher;
use crate::channels::USER_ACTION_CAPACITY;
use crate::operation_controller::TuiOperationController;
use crate::types::{TuiEvent, UserAction};

pub(crate) struct TuiAgentRuntime {
    controller: TuiOperationController,
    dispatcher: TuiActionDispatcher,
    agent: Option<JoinHandle<()>>,
    host: Option<RuntimeHost>,
}

impl TuiAgentRuntime {
    pub(crate) fn spawn_hosted(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        task_capacity: usize,
        controller: TuiOperationController,
        run: impl FnOnce(TuiOperationController, Receiver<UserAction>, RuntimeHostHandle)
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let host = RuntimeHost::start_with_background_capacity(task_capacity)
            .map_err(runtime_host_error)?;
        Self::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            USER_ACTION_CAPACITY,
            USER_ACTION_CAPACITY,
            controller,
            host,
            run,
        )
    }

    fn spawn_with_dispatch_capacities(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        command_capacity: usize,
        backlog_capacity: usize,
        controller: TuiOperationController,
        host: RuntimeHost,
        run: impl FnOnce(TuiOperationController, Receiver<UserAction>, RuntimeHostHandle)
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let host_handle = host.handle();
        let (mut dispatcher, command_rx) = TuiActionDispatcher::spawn(
            action_rx,
            event_tx,
            controller.clone(),
            command_capacity,
            backlog_capacity,
        )?;
        let agent_controller = controller.clone();
        let agent = thread::Builder::new()
            .name("orca-tui-agent".to_string())
            .spawn(move || run(agent_controller, command_rx, host_handle));
        let agent = match agent {
            Ok(agent) => agent,
            Err(error) => {
                let _ = dispatcher.shutdown();
                return Err(error);
            }
        };
        Ok(Self {
            controller,
            dispatcher,
            agent: Some(agent),
            host: Some(host),
        })
    }

    pub(crate) fn controller(&self) -> &TuiOperationController {
        &self.controller
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(agent) = self.agent.take() else {
            let dispatcher_result = self.dispatcher.shutdown();
            let host_result = self
                .host
                .take()
                .map_or(Ok(()), RuntimeHost::shutdown)
                .map_err(runtime_host_error);
            return dispatcher_result.and(host_result);
        };
        self.controller.shutdown();
        let dispatcher_result = self.dispatcher.shutdown();

        let agent_result = agent
            .join()
            .map_err(|_| io::Error::other("TUI agent controller panicked during shutdown"));
        let host_result = self
            .host
            .take()
            .map_or(Ok(()), RuntimeHost::shutdown)
            .map_err(runtime_host_error);
        dispatcher_result.and(agent_result).and(host_result)
    }
}

fn runtime_host_error(error: orca_runtime::runtime_host::RuntimeHostError) -> io::Error {
    io::Error::other(error.to_string())
}

impl Drop for TuiAgentRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use orca_runtime::runtime_host::HostedTurnRequest;

    use super::*;
    use crate::interaction_broker::TuiInteractionBroker;

    fn run_blocking_hosted_operation(
        controller: TuiOperationController,
        host: RuntimeHostHandle,
        ready_tx: crossbeam_channel::Sender<()>,
    ) {
        let thread = host
            .start_thread(crate::test_support::test_run_config(), "agent runtime test")
            .expect("hosted test thread");
        let operation = Arc::new(
            thread
                .start_turn(
                    HostedTurnRequest::new("mock_stream_delay_ms 5000"),
                    io::sink(),
                )
                .expect("hosted test operation"),
        );
        controller
            .install_hosted(Arc::clone(&operation))
            .expect("install hosted test operation");
        ready_tx.send(()).expect("ready signal");
        operation.wait();
        controller.complete_hosted(operation.id());
    }

    fn spawn_blocking_runtime(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        ready_tx: crossbeam_channel::Sender<()>,
    ) -> TuiAgentRuntime {
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx,
            1,
            controller,
            move |controller, _commands, host| {
                run_blocking_hosted_operation(controller, host, ready_tx)
            },
        )
        .expect("hosted agent runtime spawned")
    }

    #[test]
    fn shutdown_cancels_current_operation_and_joins_agent_thread() {
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let mut runtime = spawn_blocking_runtime(action_rx, event_tx, ready_tx);

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent started");
        runtime.shutdown().expect("agent runtime shutdown");
    }

    #[test]
    fn drop_uses_the_same_cancel_and_join_path() {
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let runtime = spawn_blocking_runtime(action_rx, event_tx, ready_tx);

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent started");
        drop(runtime);
    }

    #[test]
    fn shutdown_does_not_wait_for_capacity_in_full_action_mailbox() {
        let (action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        action_tx
            .send(UserAction::Submit("fill command mailbox".to_string()))
            .expect("fill action mailbox");
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        let host = RuntimeHost::start().expect("runtime host");

        let mut runtime = TuiAgentRuntime::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            1,
            1,
            controller,
            host,
            move |controller, _commands, host| {
                run_blocking_hosted_operation(controller, host, ready_tx)
            },
        )
        .expect("agent runtime spawned");
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent started");

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let shutdown = std::thread::spawn(move || {
            let result = runtime.shutdown();
            done_tx.send(result).expect("shutdown result");
        });
        let result = done_rx.recv_timeout(Duration::from_secs(1));

        shutdown.join().expect("shutdown thread joined");
        result
            .expect("shutdown must not wait for action mailbox capacity")
            .expect("runtime shutdown");
    }
}
