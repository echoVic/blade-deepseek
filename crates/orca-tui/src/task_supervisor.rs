use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use orca_core::cancel::CancelToken;

struct SupervisedTask {
    name: String,
    cancel: CancelToken,
    handle: JoinHandle<()>,
}

struct SupervisorState {
    accepting: bool,
    capacity: usize,
    next_id: u64,
    tasks: HashMap<u64, SupervisedTask>,
    failures: Vec<String>,
}

pub(crate) struct TuiTaskSupervisor {
    state: Arc<Mutex<SupervisorState>>,
}

#[derive(Clone)]
pub(crate) struct TuiTaskSpawner {
    state: Arc<Mutex<SupervisorState>>,
}

impl TuiTaskSupervisor {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(SupervisorState {
                accepting: true,
                capacity,
                next_id: 1,
                tasks: HashMap::new(),
                failures: Vec::new(),
            })),
        }
    }

    pub(crate) fn spawner(&self) -> TuiTaskSpawner {
        TuiTaskSpawner {
            state: Arc::clone(&self.state),
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        let mut state = lock_state(&self.state);
        state.accepting = false;
        for task in state.tasks.values() {
            task.cancel.cancel();
        }
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        self.begin_shutdown();
        let tasks = {
            let mut state = lock_state(&self.state);
            state
                .tasks
                .drain()
                .map(|(_, task)| task)
                .collect::<Vec<_>>()
        };
        let mut failures = join_tasks(tasks);
        failures.append(&mut lock_state(&self.state).failures);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "TUI background task shutdown failed: {}",
                failures.join("; ")
            )))
        }
    }
}

impl Drop for TuiTaskSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

impl TuiTaskSpawner {
    pub(crate) fn spawn(
        &self,
        name: impl Into<String>,
        run: impl FnOnce(CancelToken) + Send + 'static,
    ) -> io::Result<()> {
        self.reap_finished();
        let name = name.into();
        let mut state = lock_state(&self.state);
        if !state.accepting {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI background task supervisor is shutting down",
            ));
        }
        if state.tasks.len() >= state.capacity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "TUI background task capacity exhausted ({})",
                    state.capacity
                ),
            ));
        }

        let id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let thread_name = format!("orca-tui-{name}-{id}");
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || run(worker_cancel))?;
        state.tasks.insert(
            id,
            SupervisedTask {
                name,
                cancel,
                handle,
            },
        );
        Ok(())
    }

    fn reap_finished(&self) {
        let tasks = {
            let mut state = lock_state(&self.state);
            let finished = state
                .tasks
                .iter()
                .filter_map(|(id, task)| task.handle.is_finished().then_some(*id))
                .collect::<Vec<_>>();
            finished
                .into_iter()
                .filter_map(|id| state.tasks.remove(&id))
                .collect::<Vec<_>>()
        };
        let failures = join_tasks(tasks);
        if !failures.is_empty() {
            lock_state(&self.state).failures.extend(failures);
        }
    }
}

fn lock_state(state: &Arc<Mutex<SupervisorState>>) -> MutexGuard<'_, SupervisorState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn join_tasks(tasks: Vec<SupervisedTask>) -> Vec<String> {
    let mut failures = Vec::new();
    for task in tasks {
        if task.handle.join().is_err() {
            failures.push(format!("task '{}' panicked", task.name));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn shutdown_cancels_and_joins_every_admitted_task() {
        let mut supervisor = TuiTaskSupervisor::new(2);
        let spawner = supervisor.spawner();
        let joined = Arc::new(AtomicUsize::new(0));

        for index in 0..2 {
            let joined = Arc::clone(&joined);
            spawner
                .spawn(format!("worker-{index}"), move |cancel| {
                    while !cancel.is_cancelled() {
                        std::thread::yield_now();
                    }
                    joined.fetch_add(1, Ordering::SeqCst);
                })
                .expect("task admitted");
        }

        supervisor.shutdown().expect("supervisor shutdown");
        assert_eq!(joined.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn capacity_is_bounded_and_shutdown_closes_admission() {
        let mut supervisor = TuiTaskSupervisor::new(1);
        let spawner = supervisor.spawner();
        spawner
            .spawn("held", move |cancel| {
                while !cancel.is_cancelled() {
                    std::thread::yield_now();
                }
            })
            .expect("first task admitted");

        assert!(spawner.spawn("overflow", |_| {}).is_err());
        supervisor.begin_shutdown();
        assert!(spawner.spawn("late", |_| {}).is_err());
        supervisor.shutdown().expect("supervisor shutdown");
    }

    #[test]
    fn completed_tasks_release_capacity_before_the_next_admission() {
        let mut supervisor = TuiTaskSupervisor::new(1);
        let spawner = supervisor.spawner();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        spawner
            .spawn("short", move |_| {
                done_tx.send(()).expect("completion signal");
            })
            .expect("short task admitted");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("short task completed");

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match spawner.spawn("replacement", |_| {}) {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => std::thread::yield_now(),
                Err(error) => panic!("completed task did not release capacity: {error}"),
            }
        }
        supervisor.shutdown().expect("supervisor shutdown");
    }
}
