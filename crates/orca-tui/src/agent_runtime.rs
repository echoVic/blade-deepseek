use std::io;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;
use orca_core::cancel::OperationCancellation;

use crate::task_supervisor::{TuiTaskSpawner, TuiTaskSupervisor};
use crate::types::UserAction;

pub(crate) struct TuiAgentRuntime {
    cancellation: OperationCancellation,
    shutdown_tx: Sender<UserAction>,
    agent: Option<JoinHandle<()>>,
    tasks: TuiTaskSupervisor,
}

impl TuiAgentRuntime {
    pub(crate) fn spawn(
        shutdown_tx: Sender<UserAction>,
        task_capacity: usize,
        run: impl FnOnce(OperationCancellation, TuiTaskSpawner) + Send + 'static,
    ) -> io::Result<Self> {
        let tasks = TuiTaskSupervisor::new(task_capacity);
        let task_spawner = tasks.spawner();
        let cancellation = OperationCancellation::new();
        let agent_cancellation = cancellation.clone();
        let agent = thread::Builder::new()
            .name("orca-tui-agent".to_string())
            .spawn(move || run(agent_cancellation, task_spawner))?;
        Ok(Self {
            cancellation,
            shutdown_tx,
            agent: Some(agent),
            tasks,
        })
    }

    pub(crate) fn cancellation(&self) -> &OperationCancellation {
        &self.cancellation
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(agent) = self.agent.take() else {
            return self.tasks.shutdown();
        };
        self.cancellation.shutdown();
        self.tasks.begin_shutdown();
        let _ = self.shutdown_tx.try_send(UserAction::Cancel);

        let agent_result = agent
            .join()
            .map_err(|_| io::Error::other("TUI agent controller panicked during shutdown"));
        let tasks_result = self.tasks.shutdown();
        agent_result.and(tasks_result)
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
        let (action_tx, _action_rx) = crossbeam_channel::bounded(1);
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let mut runtime = TuiAgentRuntime::spawn(action_tx, 1, {
            let started = Arc::clone(&started);
            let finished = Arc::clone(&finished);
            move |cancellation, _tasks| {
                let operation = cancellation.start();
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
        let (action_tx, _action_rx) = crossbeam_channel::bounded(1);
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let runtime = TuiAgentRuntime::spawn(action_tx, 1, {
            let finished = Arc::clone(&finished);
            move |cancellation, _tasks| {
                let operation = cancellation.start();
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
        action_tx
            .send(UserAction::Interrupt)
            .expect("fill action mailbox");
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let mut runtime = TuiAgentRuntime::spawn(action_tx, 1, move |cancellation, _tasks| {
            let operation = cancellation.start();
            ready_tx.send(()).expect("ready signal");
            while !operation.token().is_cancelled() {
                std::thread::yield_now();
            }
        })
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

        // Keep the RED test bounded even when shutdown blocks on send.
        drop(action_rx);
        shutdown.join().expect("shutdown thread joined");
        result
            .expect("shutdown must not wait for action mailbox capacity")
            .expect("runtime shutdown");
    }
}
