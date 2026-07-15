use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::runtime_host::{
    HostedTurnRequest, InterruptOperationResult, OperationOutcome, RuntimeHost, RuntimeHostError,
    ThreadOperationExecutor,
};
use orca_runtime::thread::RuntimeThread;

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct ManualGate {
    state: Arc<(Mutex<GateState>, Condvar)>,
}

#[derive(Default)]
struct GateState {
    entered: bool,
    released: bool,
}

impl ManualGate {
    fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(GateState::default()), Condvar::new())),
        }
    }

    fn enter_and_wait(&self) {
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap();
        state.entered = true;
        changed.notify_all();
        while !state.released {
            state = changed.wait(state).unwrap();
        }
    }

    fn wait_until_entered(&self) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap();
        while !state.entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "operation did not enter executor");
            let (next, timed_out) = changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timed_out.timed_out(), "operation did not enter executor");
        }
    }

    fn release(&self) {
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap();
        state.released = true;
        changed.notify_all();
    }
}

enum TestBehavior {
    WaitForCancel { finished: Arc<AtomicBool> },
    WaitForRelease { gate: ManualGate, status: RunStatus },
    EmitEvent { message: String, status: RunStatus },
    Panic,
}

struct ScriptedExecutor {
    behaviors: Mutex<VecDeque<TestBehavior>>,
    calls: AtomicUsize,
}

impl ScriptedExecutor {
    fn new(behaviors: impl IntoIterator<Item = TestBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors.into_iter().collect()),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

impl ThreadOperationExecutor for ScriptedExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        config: &RunConfig,
        _request: &HostedTurnRequest,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<RunStatus> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let behavior = self
            .behaviors
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted executor behavior");
        match behavior {
            TestBehavior::WaitForCancel { finished } => {
                let deadline = Instant::now() + TEST_TIMEOUT;
                while !cancel.is_cancelled() {
                    assert!(Instant::now() < deadline, "operation was not cancelled");
                    std::thread::sleep(Duration::from_millis(5));
                }
                finished.store(true, Ordering::Release);
                Ok(RunStatus::Cancelled)
            }
            TestBehavior::WaitForRelease { gate, status } => {
                gate.enter_and_wait();
                Ok(status)
            }
            TestBehavior::EmitEvent { message, status } => {
                EventSink::new(writer, config.output_format).emit(&events.error(&message))?;
                Ok(status)
            }
            TestBehavior::Panic => panic!("scripted operation panic"),
        }
    }
}

#[derive(Clone, Default)]
struct SharedWriter {
    output: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn json_events(&self) -> Vec<serde_json::Value> {
        let output = self.output.lock().unwrap();
        String::from_utf8(output.clone())
            .expect("utf8 event output")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json event"))
            .collect()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DisconnectedWriter;

impl io::Write for DisconnectedWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "event subscriber disconnected",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn test_config(cwd: PathBuf) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).unwrap(),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        max_budget_usd: None,
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        update_check: false,
        desktop_notifications: false,
        auto_memory: false,
    }
}

fn start_scripted_thread(
    executor: Arc<ScriptedExecutor>,
) -> (
    tempfile::TempDir,
    RuntimeHost,
    orca_runtime::runtime_host::RuntimeThreadHandle,
) {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "runtime host test")
        .expect("start runtime thread");
    (cwd, host, thread)
}

#[test]
fn actor_owned_events_keep_one_contiguous_sequence_across_operations() {
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::EmitEvent {
            message: "first operation".to_string(),
            status: RunStatus::Success,
        },
        TestBehavior::EmitEvent {
            message: "second operation".to_string(),
            status: RunStatus::Success,
        },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(executor);
    let writer = SharedWriter::default();

    let first = thread
        .start_turn(HostedTurnRequest::new("first"), writer.clone())
        .expect("start first operation");
    assert_eq!(
        first
            .wait_timeout(TEST_TIMEOUT)
            .expect("first terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    let second = thread
        .start_turn(HostedTurnRequest::new("second"), writer.clone())
        .expect("start second operation");
    assert_eq!(
        second
            .wait_timeout(TEST_TIMEOUT)
            .expect("second terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let events = writer.json_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["seq"], 0);
    assert_eq!(events[1]["seq"], 1);
    assert_eq!(events[0]["run_id"], events[1]["run_id"]);
    assert!(
        events[0]["run_id"]
            .as_str()
            .is_some_and(|run_id| !run_id.is_empty())
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn headless_session_envelope_owns_events_and_hooks_once_in_order() {
    let cwd = tempfile::tempdir().unwrap();
    let hook_log = cwd.path().join("session-hooks.log");
    let quoted_hook_log = format!(
        "'{}'",
        hook_log.display().to_string().replace('\'', "'\\''")
    );
    let mut config = test_config(cwd.path().to_path_buf());
    config.hooks = vec![
        HookConfig {
            event: HookEvent::SessionStart,
            command: format!("printf 'session_start\\n' >> {quoted_hook_log}"),
            tool: None,
        },
        HookConfig {
            event: HookEvent::SessionEnd,
            command: format!("printf 'session_end\\n' >> {quoted_hook_log}"),
            tool: None,
        },
    ];
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::EmitEvent {
        message: "turn event".to_string(),
        status: RunStatus::Success,
    }]));
    let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
    let thread = host
        .start_thread(config, "headless session")
        .expect("start runtime thread");
    let writer = SharedWriter::default();

    let operation = thread
        .start_turn(
            HostedTurnRequest::headless_session("inspect repo"),
            writer.clone(),
        )
        .expect("start headless session");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("headless terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let events = writer.json_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session.started", "error", "session.completed"]
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        events
            .iter()
            .all(|event| event["run_id"] == events[0]["run_id"])
    );
    assert_eq!(
        std::fs::read_to_string(hook_log).expect("session hook log"),
        "session_start\nsession_end\n"
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn headless_session_writer_failure_is_typed_before_turn_execution() {
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::EmitEvent {
        message: "must not execute".to_string(),
        status: RunStatus::Success,
    }]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));

    let operation = thread
        .start_turn(
            HostedTurnRequest::headless_session("inspect repo"),
            DisconnectedWriter,
        )
        .expect("start failing headless session");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("writer failure terminal")
            .outcome(),
        &OperationOutcome::ExecutionFailed {
            kind: io::ErrorKind::BrokenPipe,
            message: "event subscriber disconnected".to_string(),
        }
    );
    assert_eq!(executor.call_count(), 0);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn host_shutdown_joins_headless_session_before_terminal_completion() {
    let finished = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForCancel {
        finished: Arc::clone(&finished),
    }]));
    let (_cwd, host, thread) = start_scripted_thread(executor);
    let writer = SharedWriter::default();
    let operation = thread
        .start_turn(
            HostedTurnRequest::headless_session("long headless session"),
            writer.clone(),
        )
        .expect("start headless session");

    host.shutdown().expect("shutdown runtime host");

    assert!(finished.load(Ordering::Acquire));
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("shutdown terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
    let events = writer.json_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session.started", "session.completed"]
    );
    assert_eq!(events[1]["payload"]["status"], "cancelled");
    assert_eq!(events[0]["seq"], 0);
    assert_eq!(events[1]["seq"], 1);
}

#[test]
fn operation_completion_is_independent_of_handle_lifetime() {
    let gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForRelease {
        gate: gate.clone(),
        status: RunStatus::Success,
    }]));
    let (_cwd, host, thread) = start_scripted_thread(executor);

    let operation = thread
        .start_turn(HostedTurnRequest::new("inspect repo"), io::sink())
        .expect("start turn");
    gate.wait_until_entered();
    let operation_id = operation.id();
    let completion = operation.completion();
    drop(operation);
    drop(thread);

    gate.release();
    let terminal = completion
        .wait_timeout(TEST_TIMEOUT)
        .expect("operation terminal");
    assert_eq!(terminal.operation_id(), operation_id);
    assert_eq!(
        terminal.outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn concurrent_start_is_rejected_without_replacing_active_operation() {
    let gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForRelease {
        gate: gate.clone(),
        status: RunStatus::Success,
    }]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));

    let active = thread
        .start_turn(HostedTurnRequest::new("first"), io::sink())
        .expect("start first turn");
    gate.wait_until_entered();

    let error = thread
        .start_turn(HostedTurnRequest::new("second"), io::sink())
        .expect_err("second turn must be rejected");
    assert_eq!(
        error,
        RuntimeHostError::OperationActive {
            operation_id: active.id(),
        }
    );
    assert_eq!(executor.call_count(), 1);

    gate.release();
    assert_eq!(
        active
            .wait_timeout(TEST_TIMEOUT)
            .expect("first operation terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn stale_interrupt_cannot_cancel_a_newer_operation() {
    let first_finished = Arc::new(AtomicBool::new(false));
    let second_gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::WaitForCancel {
            finished: Arc::clone(&first_finished),
        },
        TestBehavior::WaitForRelease {
            gate: second_gate.clone(),
            status: RunStatus::Success,
        },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(executor);

    let first = thread
        .start_turn(HostedTurnRequest::new("first"), io::sink())
        .expect("start first turn");
    assert_eq!(
        thread
            .interrupt_operation(first.id())
            .expect("interrupt first operation"),
        InterruptOperationResult::Requested {
            operation_id: first.id(),
        }
    );
    assert_eq!(
        first
            .wait_timeout(TEST_TIMEOUT)
            .expect("cancelled first terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
    assert!(first_finished.load(Ordering::Acquire));

    let second = thread
        .start_turn(HostedTurnRequest::new("second"), io::sink())
        .expect("start second turn");
    second_gate.wait_until_entered();
    assert_eq!(
        thread
            .interrupt_operation(first.id())
            .expect("reject stale interrupt"),
        InterruptOperationResult::Stale {
            requested_operation_id: first.id(),
            active_operation_id: second.id(),
        }
    );
    assert!(second.completion().try_terminal().is_none());

    second_gate.release();
    assert_eq!(
        second
            .wait_timeout(TEST_TIMEOUT)
            .expect("second operation terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn thread_shutdown_cancels_and_joins_active_operation() {
    let finished = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForCancel {
        finished: Arc::clone(&finished),
    }]));
    let (_cwd, host, thread) = start_scripted_thread(executor);

    let operation = thread
        .start_turn(HostedTurnRequest::new("long turn"), io::sink())
        .expect("start turn");
    thread.shutdown().expect("shutdown thread actor");

    assert!(finished.load(Ordering::Acquire));
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("shutdown terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn host_shutdown_cancels_and_joins_every_thread_actor() {
    let first_finished = Arc::new(AtomicBool::new(false));
    let second_finished = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::WaitForCancel {
            finished: Arc::clone(&first_finished),
        },
        TestBehavior::WaitForCancel {
            finished: Arc::clone(&second_finished),
        },
    ]));
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
    let first_thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "first thread")
        .expect("start first thread");
    let second_thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "second thread")
        .expect("start second thread");
    let first = first_thread
        .start_turn(HostedTurnRequest::new("first"), io::sink())
        .expect("start first operation");
    let second = second_thread
        .start_turn(HostedTurnRequest::new("second"), io::sink())
        .expect("start second operation");

    host.shutdown().expect("shutdown runtime host");

    assert!(first_finished.load(Ordering::Acquire));
    assert!(second_finished.load(Ordering::Acquire));
    assert_eq!(
        first
            .wait_timeout(TEST_TIMEOUT)
            .expect("first shutdown terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
    assert_eq!(
        second
            .wait_timeout(TEST_TIMEOUT)
            .expect("second shutdown terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
}

#[test]
fn dropping_host_cancels_and_joins_active_operation() {
    let finished = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForCancel {
        finished: Arc::clone(&finished),
    }]));
    let (_cwd, host, thread) = start_scripted_thread(executor);
    let operation = thread
        .start_turn(HostedTurnRequest::new("drop host"), io::sink())
        .expect("start operation");

    drop(host);

    assert!(finished.load(Ordering::Acquire));
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("drop terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
}

#[test]
fn event_subscriber_disconnect_is_failure_not_cancellation() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "disconnected subscriber",
        )
        .expect("start runtime thread");

    let operation = thread
        .start_turn(HostedTurnRequest::new("error"), DisconnectedWriter)
        .expect("start failing turn");
    let terminal = operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("execution error terminal");
    assert_eq!(
        terminal.outcome(),
        &OperationOutcome::ExecutionFailed {
            kind: io::ErrorKind::BrokenPipe,
            message: "event subscriber disconnected".to_string(),
        }
    );
    assert_eq!(operation.completion().try_terminal(), Some(terminal));

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn operation_panic_has_one_terminal_and_actor_reclaims_thread_state() {
    let gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::Panic,
        TestBehavior::WaitForRelease {
            gate: gate.clone(),
            status: RunStatus::Success,
        },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(executor);

    let panicked = thread
        .start_turn(HostedTurnRequest::new("panic"), io::sink())
        .expect("start panicking turn");
    let terminal = panicked.wait_timeout(TEST_TIMEOUT).expect("panic terminal");
    assert!(matches!(
        terminal.outcome(),
        OperationOutcome::Panicked { message } if message.contains("scripted operation panic")
    ));
    assert_eq!(panicked.completion().try_terminal(), Some(terminal));

    let next = thread
        .start_turn(HostedTurnRequest::new("next"), io::sink())
        .expect("actor remains usable after executor panic");
    gate.wait_until_entered();
    gate.release();
    assert_eq!(
        next.wait_timeout(TEST_TIMEOUT)
            .expect("next operation terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn default_executor_delegates_to_runtime_thread_turn_executor() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "legacy executor")
        .expect("start runtime thread");

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("reply from mock provider"),
            io::sink(),
        )
        .expect("start legacy turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("legacy operation terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    host.shutdown().expect("shutdown runtime host");
}
