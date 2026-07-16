use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

const READONLY_TOOL_TIMEOUT_SECS: u64 = 120;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct RuntimeReadonlyToolInvocation {
    pub(crate) request: ToolRequest,
    pub(crate) cwd: PathBuf,
    pub(crate) mcp_registry: McpRegistry,
    pub(crate) output_truncation: ToolOutputTruncation,
}

trait RuntimeReadonlyToolExecutor: Send + Sync {
    fn execute(
        &self,
        invocation: &RuntimeReadonlyToolInvocation,
        cancel: &CancelToken,
    ) -> ToolResult;
}

struct DefaultRuntimeReadonlyToolExecutor;

impl RuntimeReadonlyToolExecutor for DefaultRuntimeReadonlyToolExecutor {
    fn execute(
        &self,
        invocation: &RuntimeReadonlyToolInvocation,
        cancel: &CancelToken,
    ) -> ToolResult {
        orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
            &invocation.request,
            &invocation.cwd,
            &[],
            &invocation.mcp_registry,
            &[],
            invocation.output_truncation,
            READONLY_TOOL_TIMEOUT_SECS,
            None,
            || cancel.is_cancelled(),
        )
    }
}

struct RuntimeToolTerminal {
    result: Mutex<Option<ToolResult>>,
}

impl RuntimeToolTerminal {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
        }
    }

    fn complete(&self, result: ToolResult) -> bool {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(result);
        true
    }

    fn take(&self) -> Option<ToolResult> {
        self.result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

pub(crate) struct RuntimeToolCallRuntime {
    handle: Handle,
    executor: Arc<dyn RuntimeReadonlyToolExecutor>,
    _owned_runtime: Option<Arc<tokio::runtime::Runtime>>,
}

impl RuntimeToolCallRuntime {
    pub(crate) fn for_current_execution() -> io::Result<Self> {
        match Handle::try_current() {
            Ok(handle) => Ok(Self {
                handle,
                executor: Arc::new(DefaultRuntimeReadonlyToolExecutor),
                _owned_runtime: None,
            }),
            Err(_) => {
                let runtime = Arc::new(
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .thread_name("orca-tool-call")
                        .build()
                        .map_err(|error| {
                            io::Error::other(format!("failed to start tool-call runtime: {error}"))
                        })?,
                );
                Ok(Self {
                    handle: runtime.handle().clone(),
                    executor: Arc::new(DefaultRuntimeReadonlyToolExecutor),
                    _owned_runtime: Some(runtime),
                })
            }
        }
    }

    #[cfg(test)]
    fn with_executor(executor: Arc<dyn RuntimeReadonlyToolExecutor>) -> io::Result<Self> {
        let mut runtime = Self::for_current_execution()?;
        runtime.executor = executor;
        Ok(runtime)
    }

    pub(crate) fn execute_readonly_batch(
        &self,
        invocations: Vec<RuntimeReadonlyToolInvocation>,
        max_parallel: usize,
        cancel: &CancelToken,
    ) -> Vec<ToolResult> {
        let executor = Arc::clone(&self.executor);
        let cancel = cancel.clone();
        self.handle.block_on(async move {
            let permits = Arc::new(Semaphore::new(max_parallel.max(1)));
            let mut tasks = Vec::with_capacity(invocations.len());

            for invocation in invocations {
                let request = invocation.request.clone();
                let task_request = request.clone();
                let task_cancel = cancel.clone();
                let task_executor = Arc::clone(&executor);
                let task_permits = Arc::clone(&permits);
                let started = Arc::new(AtomicBool::new(false));
                let task_started = Arc::clone(&started);
                let terminal = Arc::new(RuntimeToolTerminal::new());
                let task_terminal = Arc::clone(&terminal);
                let join = tokio::spawn(async move {
                    let acquire = Arc::clone(&task_permits).acquire_owned();
                    tokio::pin!(acquire);
                    let permit = loop {
                        if task_cancel.is_cancelled() {
                            let completed = task_terminal.complete(
                                ToolResult::cancelled_before_start(
                                    &task_request,
                                    "the read-only invocation was cancelled before dispatch",
                                ),
                            );
                            debug_assert!(completed, "tool terminal must complete once");
                            return;
                        }
                        tokio::select! {
                            permit = &mut acquire => {
                                match permit {
                                    Ok(permit) => break permit,
                                    Err(_) => {
                                        let completed = task_terminal.complete(
                                            ToolResult::failed_before_start(
                                                &task_request,
                                                "read-only tool concurrency gate closed",
                                                None,
                                            ),
                                        );
                                        debug_assert!(completed, "tool terminal must complete once");
                                        return;
                                    }
                                }
                            }
                            _ = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {}
                        }
                    };

                    if task_cancel.is_cancelled() {
                        drop(permit);
                        let completed = task_terminal.complete(
                            ToolResult::cancelled_before_start(
                                &task_request,
                                "the read-only invocation was cancelled before dispatch",
                            ),
                        );
                        debug_assert!(completed, "tool terminal must complete once");
                        return;
                    }
                    task_started.store(true, Ordering::Release);
                    let blocking_cancel = task_cancel.clone();
                    let blocking = tokio::task::spawn_blocking(move || {
                        task_executor.execute(&invocation, &blocking_cancel)
                    })
                    .await;
                    drop(permit);

                    let result = match blocking {
                        Ok(result) => result,
                        Err(error) => ToolResult::indeterminate_after_start(
                            &task_request,
                            format!(
                                "Read-only tool worker panicked after execution started: {error}. Inspect external state before retrying."
                            ),
                        ),
                    };
                    let completed = task_terminal.complete(result);
                    debug_assert!(completed, "tool terminal must complete once");
                });
                tasks.push((request, started, terminal, join));
            }

            let mut results = Vec::with_capacity(tasks.len());
            for (request, started, terminal, join) in tasks {
                if let Err(error) = join.await {
                    let result = if started.load(Ordering::Acquire) {
                        ToolResult::indeterminate_after_start(
                            &request,
                            format!(
                                "Read-only tool task stopped after execution started: {error}. Inspect external state before retrying."
                            ),
                        )
                    } else {
                        ToolResult::failed_before_start(
                            &request,
                            format!("read-only tool task stopped before dispatch: {error}"),
                            None,
                        )
                    };
                    let _ = terminal.complete(result);
                }
                results.push(terminal.take().unwrap_or_else(|| {
                    ToolResult::indeterminate(
                        &request,
                        "Read-only tool task ended without a terminal result. Inspect external state before retrying.",
                    )
                }));
            }
            results
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Barrier, Condvar};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolInvocationStarted, ToolName, ToolStatus, ToolTerminalSource};

    use super::*;

    struct CancelAwareExecutor {
        started: Arc<Barrier>,
    }

    impl RuntimeReadonlyToolExecutor for CancelAwareExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            cancel: &CancelToken,
        ) -> ToolResult {
            self.started.wait();
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::cancelled(&invocation.request, "cancelled in flight", None)
        }
    }

    struct OutOfOrderExecutor {
        both_started: Barrier,
        second_finished: (Mutex<bool>, Condvar),
        completion_order: Mutex<Vec<String>>,
    }

    impl RuntimeReadonlyToolExecutor for OutOfOrderExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            _cancel: &CancelToken,
        ) -> ToolResult {
            self.both_started.wait();
            if invocation.request.id == "first" {
                let (finished, wake) = &self.second_finished;
                let mut second_finished = finished
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                while !*second_finished {
                    second_finished = wake
                        .wait(second_finished)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                self.completion_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(invocation.request.id.clone());
            } else {
                self.completion_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(invocation.request.id.clone());
                let (finished, wake) = &self.second_finished;
                *finished
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
                wake.notify_all();
            }
            ToolResult::completed(&invocation.request, invocation.request.id.clone(), false)
        }
    }

    struct CompletionAfterCancellationExecutor {
        started: Arc<Barrier>,
    }

    impl RuntimeReadonlyToolExecutor for CompletionAfterCancellationExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            cancel: &CancelToken,
        ) -> ToolResult {
            self.started.wait();
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::completed(
                &invocation.request,
                "completed at cancellation".to_string(),
                false,
            )
        }
    }

    struct JoinTrackingExecutor {
        active: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
    }

    impl RuntimeReadonlyToolExecutor for JoinTrackingExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            _cancel: &CancelToken,
        ) -> ToolResult {
            self.active.fetch_add(1, Ordering::AcqRel);
            std::thread::sleep(Duration::from_millis(20));
            self.finished.fetch_add(1, Ordering::AcqRel);
            self.active.fetch_sub(1, Ordering::AcqRel);
            ToolResult::completed(&invocation.request, "joined".to_string(), false)
        }
    }

    struct AdmissionCancelExecutor {
        started: std::sync::mpsc::SyncSender<()>,
        calls: Arc<AtomicUsize>,
    }

    impl RuntimeReadonlyToolExecutor for AdmissionCancelExecutor {
        fn execute(
            &self,
            invocation: &RuntimeReadonlyToolInvocation,
            cancel: &CancelToken,
        ) -> ToolResult {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let _ = self.started.send(());
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ToolResult::cancelled(&invocation.request, "cancelled in flight", None)
        }
    }

    struct PanickingExecutor;

    impl RuntimeReadonlyToolExecutor for PanickingExecutor {
        fn execute(
            &self,
            _invocation: &RuntimeReadonlyToolInvocation,
            _cancel: &CancelToken,
        ) -> ToolResult {
            panic!("read-only fixture panic");
        }
    }

    fn invocation(id: &str) -> RuntimeReadonlyToolInvocation {
        RuntimeReadonlyToolInvocation {
            request: ToolRequest {
                id: id.to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some(id.to_string()),
                raw_arguments: None,
            },
            cwd: PathBuf::from("."),
            mcp_registry: McpRegistry::default(),
            output_truncation: ToolOutputTruncation::default(),
        }
    }

    #[test]
    fn in_flight_readonly_invocation_observes_cancellation_before_return() {
        let started = Arc::new(Barrier::new(2));
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(CancelAwareExecutor {
            started: Arc::clone(&started),
        }))
        .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_readonly_batch(vec![invocation("slow")], 1, &worker_cancel)
        });
        started.wait();
        cancel.cancel();

        let results = worker.join().expect("runtime worker");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ToolStatus::Cancelled);
        assert_eq!(results[0].terminal().started, ToolInvocationStarted::Yes);
    }

    #[test]
    fn readonly_results_preserve_provider_order_when_tasks_finish_out_of_order() {
        let executor = Arc::new(OutOfOrderExecutor {
            both_started: Barrier::new(2),
            second_finished: (Mutex::new(false), Condvar::new()),
            completion_order: Mutex::new(Vec::new()),
        });
        let runtime =
            RuntimeToolCallRuntime::with_executor(executor.clone()).expect("tool-call runtime");
        let results = runtime.execute_readonly_batch(
            vec![invocation("first"), invocation("second")],
            2,
            &CancelToken::new(),
        );

        assert_eq!(results[0].id, "first");
        assert_eq!(results[1].id, "second");
        assert_eq!(results[0].output.as_deref(), Some("first"));
        assert_eq!(results[1].output.as_deref(), Some("second"));
        assert_eq!(
            *executor
                .completion_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["second".to_string(), "first".to_string()]
        );
    }

    #[test]
    fn cancellation_before_permit_never_starts_waiting_invocation() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(AdmissionCancelExecutor {
            started: started_tx,
            calls: Arc::clone(&calls),
        }))
        .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_readonly_batch(
                vec![invocation("first"), invocation("second")],
                1,
                &worker_cancel,
            )
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("one invocation should acquire the permit");
        cancel.cancel();

        let results = worker.join().expect("runtime worker");
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result.terminal().started == ToolInvocationStarted::Yes)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.terminal().started == ToolInvocationStarted::No)
                .count(),
            1
        );
        assert!(
            results
                .iter()
                .all(|result| result.status == ToolStatus::Cancelled)
        );
    }

    #[test]
    fn observed_completion_wins_cancellation_race() {
        let started = Arc::new(Barrier::new(2));
        let runtime =
            RuntimeToolCallRuntime::with_executor(Arc::new(CompletionAfterCancellationExecutor {
                started: Arc::clone(&started),
            }))
            .expect("tool-call runtime");
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            runtime.execute_readonly_batch(vec![invocation("race")], 1, &worker_cancel)
        });
        started.wait();
        cancel.cancel();

        let results = worker.join().expect("runtime worker");
        assert_eq!(results[0].status, ToolStatus::Completed);
        assert_eq!(
            results[0].output.as_deref(),
            Some("completed at cancellation")
        );
    }

    #[test]
    fn batch_returns_only_after_every_worker_is_joined() {
        let active = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(JoinTrackingExecutor {
            active: Arc::clone(&active),
            finished: Arc::clone(&finished),
        }))
        .expect("tool-call runtime");

        let results = runtime.execute_readonly_batch(
            vec![invocation("first"), invocation("second")],
            2,
            &CancelToken::new(),
        );

        assert_eq!(results.len(), 2);
        assert_eq!(finished.load(Ordering::Acquire), 2);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn readonly_worker_panic_is_indeterminate_after_start() {
        let runtime = RuntimeToolCallRuntime::with_executor(Arc::new(PanickingExecutor))
            .expect("tool-call runtime");
        let results =
            runtime.execute_readonly_batch(vec![invocation("panic")], 1, &CancelToken::new());

        assert_eq!(results[0].status, ToolStatus::Indeterminate);
        assert_eq!(results[0].terminal().started, ToolInvocationStarted::Yes);
        assert_eq!(results[0].terminal().source, ToolTerminalSource::Observed);
    }

    #[test]
    fn runtime_tool_terminal_accepts_only_one_result() {
        let terminal = RuntimeToolTerminal::new();
        assert!(terminal.complete(ToolResult::completed(
            &invocation("once").request,
            "first".to_string(),
            false,
        )));
        assert!(!terminal.complete(ToolResult::failed(
            &invocation("once").request,
            "second",
            None,
        )));
        assert_eq!(terminal.take().unwrap().output.as_deref(), Some("first"));
    }
}
