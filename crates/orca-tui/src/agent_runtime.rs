use std::io;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};
use orca_runtime::runtime_host::{RuntimeHost, RuntimeHostHandle, ThreadOperationExecutor};

use crate::action_dispatcher::TuiActionDispatcher;
use crate::channels::USER_ACTION_CAPACITY;
use crate::operation_controller::TuiOperationController;
use crate::task_supervisor::{TuiTaskSpawner, TuiTaskSupervisor};
use crate::types::{TuiEvent, UserAction};

pub(crate) struct TuiAgentRuntime {
    controller: TuiOperationController,
    dispatcher: TuiActionDispatcher,
    agent: Option<JoinHandle<()>>,
    tasks: TuiTaskSupervisor,
    host: Option<RuntimeHost>,
}

impl TuiAgentRuntime {
    pub(crate) fn spawn_hosted(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        task_capacity: usize,
        controller: TuiOperationController,
        build_executor: impl FnOnce(TuiTaskSpawner) -> Arc<dyn ThreadOperationExecutor>,
        run: impl FnOnce(
            TuiOperationController,
            Receiver<UserAction>,
            TuiTaskSpawner,
            RuntimeHostHandle,
        ) + Send
        + 'static,
    ) -> io::Result<Self> {
        let tasks = TuiTaskSupervisor::new(task_capacity);
        let executor = build_executor(tasks.spawner());
        let host = RuntimeHost::start_with_executor(executor).map_err(runtime_host_error)?;
        Self::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            USER_ACTION_CAPACITY,
            USER_ACTION_CAPACITY,
            controller,
            host,
            tasks,
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
        tasks: TuiTaskSupervisor,
        run: impl FnOnce(
            TuiOperationController,
            Receiver<UserAction>,
            TuiTaskSpawner,
            RuntimeHostHandle,
        ) + Send
        + 'static,
    ) -> io::Result<Self> {
        let task_spawner = tasks.spawner();
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
            .spawn(move || run(agent_controller, command_rx, task_spawner, host_handle));
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
            tasks,
            host: Some(host),
        })
    }

    pub(crate) fn controller(&self) -> &TuiOperationController {
        &self.controller
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(agent) = self.agent.take() else {
            let dispatcher_result = self.dispatcher.shutdown();
            let tasks_result = self.tasks.shutdown();
            let host_result = self
                .host
                .take()
                .map_or(Ok(()), RuntimeHost::shutdown)
                .map_err(runtime_host_error);
            return dispatcher_result.and(tasks_result).and(host_result);
        };
        self.controller.shutdown();
        self.tasks.begin_shutdown();
        let dispatcher_result = self.dispatcher.shutdown();

        let agent_result = agent
            .join()
            .map_err(|_| io::Error::other("TUI agent controller panicked during shutdown"));
        let tasks_result = self.tasks.shutdown();
        let host_result = self
            .host
            .take()
            .map_or(Ok(()), RuntimeHost::shutdown)
            .map_err(runtime_host_error);
        dispatcher_result
            .and(agent_result)
            .and(tasks_result)
            .and(host_result)
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
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use orca_core::cancel::CancelToken;
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_runtime::runtime_host::{
        GenerationContext, HostedTurnRequest, ThreadOperationExecutor,
    };
    use orca_runtime::thread::RuntimeThread;

    use super::*;
    use crate::interaction_broker::TuiInteractionBroker;

    struct BlockingExecutor {
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    }

    impl ThreadOperationExecutor for BlockingExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<RunStatus> {
            self.started.store(true, Ordering::SeqCst);
            while !cancel.is_cancelled() {
                std::thread::yield_now();
            }
            self.finished.store(true, Ordering::SeqCst);
            Ok(RunStatus::Cancelled)
        }
    }

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
                .start_turn(HostedTurnRequest::new("block until shutdown"), io::sink())
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
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        ready_tx: crossbeam_channel::Sender<()>,
    ) -> TuiAgentRuntime {
        let controller = TuiOperationController::hosted(TuiInteractionBroker::default());
        TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx,
            1,
            controller,
            move |_| Arc::new(BlockingExecutor { started, finished }),
            move |controller, _commands, _tasks, host| {
                run_blocking_hosted_operation(controller, host, ready_tx)
            },
        )
        .expect("hosted agent runtime spawned")
    }

    #[test]
    fn shutdown_cancels_current_operation_and_joins_agent_thread() {
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let mut runtime = spawn_blocking_runtime(
            action_rx,
            event_tx,
            Arc::clone(&started),
            Arc::clone(&finished),
            ready_tx,
        );

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent started");
        runtime.shutdown().expect("agent runtime shutdown");
        assert!(started.load(Ordering::SeqCst));
        assert!(finished.load(Ordering::SeqCst));
    }

    #[test]
    fn drop_uses_the_same_cancel_and_join_path() {
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let runtime = spawn_blocking_runtime(
            action_rx,
            event_tx,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&finished),
            ready_tx,
        );

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent started");
        drop(runtime);
        assert!(finished.load(Ordering::SeqCst));
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
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let host =
            RuntimeHost::start_with_executor(Arc::new(BlockingExecutor { started, finished }))
                .expect("runtime host");
        let tasks = TuiTaskSupervisor::new(1);

        let mut runtime = TuiAgentRuntime::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            1,
            1,
            controller,
            host,
            tasks,
            move |controller, _commands, _tasks, host| {
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
