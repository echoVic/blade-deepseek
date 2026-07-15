use std::io;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};
use orca_core::cancel::OperationCancellation;

use crate::action_dispatcher::TuiActionDispatcher;
use crate::channels::USER_ACTION_CAPACITY;
use crate::interaction_broker::TuiInteractionBroker;
use crate::operation_controller::TuiOperationController;
use crate::task_supervisor::{TuiTaskSpawner, TuiTaskSupervisor};
use crate::types::{TuiEvent, UserAction};

pub(crate) struct TuiAgentRuntime {
    controller: TuiOperationController,
    dispatcher: TuiActionDispatcher,
    agent: Option<JoinHandle<()>>,
    tasks: TuiTaskSupervisor,
}

impl TuiAgentRuntime {
    pub(crate) fn spawn(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        task_capacity: usize,
        run: impl FnOnce(TuiOperationController, Receiver<UserAction>, TuiTaskSpawner) + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            USER_ACTION_CAPACITY,
            USER_ACTION_CAPACITY,
            task_capacity,
            run,
        )
    }

    fn spawn_with_dispatch_capacities(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        command_capacity: usize,
        backlog_capacity: usize,
        task_capacity: usize,
        run: impl FnOnce(TuiOperationController, Receiver<UserAction>, TuiTaskSpawner) + Send + 'static,
    ) -> io::Result<Self> {
        let tasks = TuiTaskSupervisor::new(task_capacity);
        let task_spawner = tasks.spawner();
        let controller = TuiOperationController::new(
            OperationCancellation::new(),
            TuiInteractionBroker::default(),
        );
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
            .spawn(move || run(agent_controller, command_rx, task_spawner));
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
        })
    }

    pub(crate) fn cancellation(&self) -> &OperationCancellation {
        self.controller.cancellation()
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(agent) = self.agent.take() else {
            let dispatcher_result = self.dispatcher.shutdown();
            return dispatcher_result.and_then(|()| self.tasks.shutdown());
        };
        self.controller.shutdown();
        self.tasks.begin_shutdown();
        let dispatcher_result = self.dispatcher.shutdown();

        let agent_result = agent
            .join()
            .map_err(|_| io::Error::other("TUI agent controller panicked during shutdown"));
        let tasks_result = self.tasks.shutdown();
        dispatcher_result.and(agent_result).and(tasks_result)
    }
}

impl Drop for TuiAgentRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn shutdown_cancels_current_operation_and_joins_agent_thread() {
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let mut runtime = TuiAgentRuntime::spawn(action_rx, event_tx, 1, {
            let started = Arc::clone(&started);
            let finished = Arc::clone(&finished);
            move |controller, _commands, _tasks| {
                let operation = controller.start().expect("operation started");
                started.store(true, Ordering::SeqCst);
                ready_tx.send(()).expect("ready signal");
                while !operation.token().is_cancelled() {
                    std::thread::yield_now();
                }
                finished.store(true, Ordering::SeqCst);
            }
        })
        .expect("agent runtime spawned");

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

        let runtime = TuiAgentRuntime::spawn(action_rx, event_tx, 1, {
            let finished = Arc::clone(&finished);
            move |controller, _commands, _tasks| {
                let operation = controller.start().expect("operation started");
                ready_tx.send(()).expect("ready signal");
                while !operation.token().is_cancelled() {
                    std::thread::yield_now();
                }
                finished.store(true, Ordering::SeqCst);
            }
        })
        .expect("agent runtime spawned");

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

        let mut runtime = TuiAgentRuntime::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            1,
            1,
            1,
            move |controller, _commands, _tasks| {
                let operation = controller.start().expect("operation started");
                ready_tx.send(()).expect("ready signal");
                while !operation.token().is_cancelled() {
                    std::thread::yield_now();
                }
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
