use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use orca_core::approval_types::{
    ApprovalDecision, ApprovalMode, ApprovalRequest, ApprovalResolution,
};
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::conversation::Message;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventFactory, EventType, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::mcp_types::McpServerConfig;
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_core::task_types::TaskStatus;
use orca_mcp::McpRegistry;
use orca_runtime::controller::{ThreadTurnOutcome, ThreadTurnRequest};
use orca_runtime::history::{self, SessionTranscript};
use orca_runtime::lifecycle::{RuntimeApprovalHandler, RuntimeTaskStatus};
use orca_runtime::provider_stream::{
    RuntimeProviderSuspensionControl, RuntimeProviderSuspensionEvent,
};
use orca_runtime::runtime_host::{
    GenerationAdmissionResult, GenerationContext, GenerationFence, HostedGenerationHandlers,
    HostedOperationWriter, HostedTurnRequest, HostedWorkflowRequest, InterruptOperationResult,
    OperationOutcome, ResumeOperationResult, RuntimeHost, RuntimeHostError, RuntimeThreadMutation,
    RuntimeThreadStartRequest, RuntimeThreadState, SteerOperationResult, ThreadOperationExecutor,
    ThreadOperationOutcome,
};
use orca_runtime::thread::RuntimeThread;

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct OneShotProviderSuspension {
    requested: AtomicBool,
}

impl OneShotProviderSuspension {
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(true),
        }
    }
}

impl RuntimeProviderSuspensionControl for OneShotProviderSuspension {
    fn take_suspension_request(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

#[derive(Clone)]
struct ManualGate {
    state: Arc<(Mutex<GateState>, Condvar)>,
}

#[derive(Default)]
struct GateState {
    entered: bool,
    released: bool,
}

#[derive(Clone)]
struct CancelJoinGate {
    state: Arc<(Mutex<CancelJoinState>, Condvar)>,
}

#[derive(Default)]
struct CancelJoinState {
    entered: bool,
    cancel_seen: bool,
    released: bool,
    exited: bool,
}

impl CancelJoinGate {
    fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(CancelJoinState::default()), Condvar::new())),
        }
    }

    fn enter_wait_for_cancel_and_release(&self, cancel: &CancelToken) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap();
        state.entered = true;
        changed.notify_all();
        while !cancel.is_cancelled() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "generation was not cancelled");
            let (next, _) = changed
                .wait_timeout(state, remaining.min(Duration::from_millis(5)))
                .unwrap();
            state = next;
        }
        state.cancel_seen = true;
        changed.notify_all();
        while !state.released {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "cancelled generation was not released"
            );
            let (next, timed_out) = changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(
                !timed_out.timed_out(),
                "cancelled generation was not released"
            );
        }
        state.exited = true;
        changed.notify_all();
    }

    fn wait_until_entered(&self) {
        self.wait_until(|state| state.entered, "generation did not enter executor");
    }

    fn wait_until_cancel_seen(&self) {
        self.wait_until(
            |state| state.cancel_seen,
            "generation did not observe cancellation",
        );
    }

    fn release(&self) {
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap();
        state.released = true;
        changed.notify_all();
    }

    fn exited(&self) -> bool {
        self.state.0.lock().unwrap().exited
    }

    fn wait_until(&self, predicate: impl Fn(&CancelJoinState) -> bool, message: &str) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap();
        while !predicate(&state) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "{message}");
            let (next, timed_out) = changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timed_out.timed_out(), "{message}");
        }
    }
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
    WaitForCancelAndRelease { gate: CancelJoinGate },
    WaitForRelease { gate: ManualGate, status: RunStatus },
    EmitEvent { message: String, status: RunStatus },
    Panic,
}

struct ScriptedExecutor {
    behaviors: Mutex<VecDeque<TestBehavior>>,
    calls: AtomicUsize,
    generations: Mutex<Vec<(GenerationFence, bool)>>,
    steer_inputs: Mutex<Vec<Vec<String>>>,
    task_states: Mutex<Vec<(String, RuntimeTaskStatus)>>,
    cancelled_on_entry: Mutex<Vec<bool>>,
    approval_modes: Mutex<Vec<ApprovalMode>>,
}

impl ScriptedExecutor {
    fn new(behaviors: impl IntoIterator<Item = TestBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors.into_iter().collect()),
            calls: AtomicUsize::new(0),
            generations: Mutex::new(Vec::new()),
            steer_inputs: Mutex::new(Vec::new()),
            task_states: Mutex::new(Vec::new()),
            cancelled_on_entry: Mutex::new(Vec::new()),
            approval_modes: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn generations(&self) -> Vec<(GenerationFence, bool)> {
        self.generations.lock().unwrap().clone()
    }

    fn steer_inputs(&self) -> Vec<Vec<String>> {
        self.steer_inputs.lock().unwrap().clone()
    }

    fn task_states(&self) -> Vec<(String, RuntimeTaskStatus)> {
        self.task_states.lock().unwrap().clone()
    }

    fn cancelled_on_entry(&self) -> Vec<bool> {
        self.cancelled_on_entry.lock().unwrap().clone()
    }

    fn approval_modes(&self) -> Vec<ApprovalMode> {
        self.approval_modes.lock().unwrap().clone()
    }
}

impl ThreadOperationExecutor for ScriptedExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.cancelled_on_entry
            .lock()
            .unwrap()
            .push(cancel.is_cancelled());
        self.approval_modes
            .lock()
            .unwrap()
            .push(generation.config().approval_mode);
        self.generations
            .lock()
            .unwrap()
            .push((generation.fence(), generation.resumes_existing_turn()));
        let task = thread
            .lifecycle()
            .active_task()
            .expect("active generation task");
        self.task_states
            .lock()
            .unwrap()
            .push((task.id().to_string(), task.status()));
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
                Ok(RunStatus::Cancelled.into())
            }
            TestBehavior::WaitForCancelAndRelease { gate } => {
                gate.enter_wait_for_cancel_and_release(cancel);
                self.steer_inputs
                    .lock()
                    .unwrap()
                    .push(generation.drain_steer_inputs());
                thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
                Ok(RunStatus::Cancelled.into())
            }
            TestBehavior::WaitForRelease { gate, status } => {
                gate.enter_and_wait();
                self.steer_inputs
                    .lock()
                    .unwrap()
                    .push(generation.drain_steer_inputs());
                Ok(status.into())
            }
            TestBehavior::EmitEvent { message, status } => {
                EventSink::new(writer, generation.config().output_format)
                    .emit(&events.error(&message))?;
                Ok(status.into())
            }
            TestBehavior::Panic => panic!("scripted operation panic"),
        }
    }
}

#[derive(Clone, Default)]
struct SharedWriter {
    output: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone, Default)]
struct RecordingEventObserver {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl RecordingEventObserver {
    fn count(&self, event_type: EventType) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.event_type == event_type)
            .count()
    }

    fn events(&self) -> Vec<EventEnvelope> {
        self.events.lock().unwrap().clone()
    }
}

impl EventObserver for RecordingEventObserver {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
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

#[derive(Clone, Default)]
struct RecordingOutput {
    output: SharedWriter,
    generation_commits: Arc<Mutex<Vec<bool>>>,
}

impl RecordingOutput {
    fn generation_commits(&self) -> Vec<bool> {
        self.generation_commits.lock().unwrap().clone()
    }
}

impl Write for RecordingOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

impl HostedOperationWriter for RecordingOutput {
    fn finish_generation(&mut self, commit_terminal: bool) -> io::Result<()> {
        self.generation_commits
            .lock()
            .unwrap()
            .push(commit_terminal);
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

#[derive(Clone)]
struct AllowApprovalHandler {
    calls: Arc<AtomicUsize>,
}

impl RuntimeApprovalHandler for AllowApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        _request: &orca_core::tool_types::ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(ApprovalResolution {
            id: approval.id.clone(),
            decision: ApprovalDecision::Allow,
            reason: "generation-scoped approval allowed".to_string(),
        })
    }
}

#[derive(Clone)]
struct CancelledApprovalHandler {
    cancel: CancelToken,
    entered: Arc<AtomicBool>,
}

impl RuntimeApprovalHandler for CancelledApprovalHandler {
    fn resolve_interactive(
        &self,
        _approval: &ApprovalRequest,
        _request: &orca_core::tool_types::ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        self.entered.store(true, Ordering::Release);
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !self.cancel.is_cancelled() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "approval generation was not cancelled",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "approval generation interrupted",
        ))
    }
}

fn wait_until_true(flag: &AtomicBool, message: &str) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !flag.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(5));
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

fn wait_for_call_count(executor: &ScriptedExecutor, expected: usize) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while executor.call_count() != expected {
        assert!(
            Instant::now() < deadline,
            "executor did not reach {expected} calls"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
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
            generation: first.initial_generation(),
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
            active: second.initial_generation(),
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
fn hosted_turn_uses_per_turn_config_task_id_and_idle_snapshot() {
    let gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForRelease {
        gate: gate.clone(),
        status: RunStatus::Success,
    }]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));
    let initial = thread.snapshot().expect("read idle snapshot");
    assert_eq!(initial.thread_id(), thread.thread_id());
    assert_eq!(
        initial.active_task_id(),
        Some(format!("{}:task-1", initial.thread_id()).as_str())
    );

    let mut turn_config = test_config(PathBuf::from("."));
    turn_config.approval_mode = ApprovalMode::FullAuto;
    let operation = thread
        .start_turn_with_config(
            HostedTurnRequest::new("configured turn").with_task_id("turn-42"),
            io::sink(),
            turn_config,
        )
        .expect("start configured turn");
    gate.wait_until_entered();
    assert_eq!(
        thread
            .snapshot()
            .expect_err("running snapshot must be rejected"),
        RuntimeHostError::OperationActive {
            operation_id: operation.id(),
        }
    );

    gate.release();
    operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("configured turn terminal");
    assert_eq!(executor.approval_modes(), vec![ApprovalMode::FullAuto]);
    assert_eq!(executor.task_states()[0].0, "turn-42");
    assert_eq!(
        thread
            .snapshot()
            .expect("read completed snapshot")
            .active_task_id(),
        Some("turn-42")
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn actor_owned_start_preserves_preloaded_session_usage_and_injected_mcp_registry() {
    let cwd = tempfile::tempdir().unwrap();
    let transcript_path = cwd.path().join("resume.jsonl");
    let mut meta = history::create_meta(cwd.path(), "mock", None, "resumed thread");
    meta.session_id = "actor-resume-session".to_string();
    let mut meta_json = serde_json::to_value(&meta).expect("serialize session meta");
    meta_json
        .as_object_mut()
        .expect("session meta object")
        .insert(
            "type".to_string(),
            serde_json::Value::String("session.meta".to_string()),
        );
    std::fs::write(
        &transcript_path,
        format!("{}\n", serde_json::to_string(&meta_json).unwrap()),
    )
    .expect("write resume transcript");
    let baseline = UsageTotals {
        input_tokens: 120,
        output_tokens: 30,
        cache_tokens: 40,
        estimated_cost_usd: 0.25,
    };
    let transcript = SessionTranscript {
        meta,
        messages: vec![
            Message::system("original system".to_string()),
            Message::user("resumed prompt".to_string()),
        ],
        compactions: Vec::new(),
        summaries: Vec::new(),
        usage: Some(baseline),
        plan: None,
        completion_status: Some("success".to_string()),
        completion_error: None,
        path: transcript_path,
    };
    let mut config = test_config(cwd.path().to_path_buf());
    config.history_mode = HistoryMode::Resume("actor-resume-session".to_string());
    config.mcp_servers.push(McpServerConfig {
        name: "must-not-start".to_string(),
        command: Some("orca-missing-mcp-command".to_string()),
        ..Default::default()
    });
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "ignored title")
                .with_preloaded(transcript)
                .with_mcp_registry(McpRegistry::default()),
        )
        .expect("start resumed actor thread");

    assert_eq!(thread.thread_id(), "actor-resume-session");
    assert!(
        thread.startup_warnings().is_empty(),
        "the injected registry must bypass config-time MCP startup"
    );
    let snapshot = thread.snapshot().expect("read resumed snapshot");
    assert_eq!(snapshot.usage_totals(), baseline);
    assert!(snapshot.messages().iter().any(
        |message| matches!(message, Message::User { content, .. } if content == "resumed prompt")
    ));
    assert_eq!(
        thread
            .backtrack_last_user()
            .expect("backtrack actor-owned session"),
        Some("resumed prompt".to_string())
    );
    assert!(
        !thread
            .snapshot()
            .expect("read backtracked snapshot")
            .messages()
            .iter()
            .any(|message| matches!(message, Message::User { .. }))
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_idle_session_commands_mutate_only_actor_owned_state() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "idle mutation")
        .expect("start runtime thread");

    thread
        .mutate(RuntimeThreadMutation::SetModel(Some(
            "deepseek-v4-pro".to_string(),
        )))
        .expect("set actor model");
    thread
        .mutate(RuntimeThreadMutation::AddPinnedContext(
            "remember this".to_string(),
        ))
        .expect("add pinned context");
    thread
        .mutate(RuntimeThreadMutation::ReplaceGoalContext(
            "goal context".to_string(),
        ))
        .expect("replace goal context");
    thread
        .mutate(RuntimeThreadMutation::ReplaceSkillContext(Some(
            "skill context".to_string(),
        )))
        .expect("replace skill context");

    let snapshot = thread.snapshot().expect("read mutated snapshot");
    assert!(
        snapshot
            .conversation()
            .volatile
            .goal
            .as_deref()
            .is_some_and(|context| context.contains("goal context"))
    );
    assert!(
        snapshot
            .conversation()
            .volatile
            .skill
            .as_deref()
            .is_some_and(|context| context.contains("skill context"))
    );
    assert!(snapshot.messages().iter().any(|message| {
        matches!(message, Message::User { content, pinned: true } if content == "remember this")
    }));

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn idle_session_commands_are_rejected_while_operation_owns_thread() {
    let gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([TestBehavior::WaitForRelease {
        gate: gate.clone(),
        status: RunStatus::Success,
    }]));
    let (_cwd, host, thread) = start_scripted_thread(executor);
    let operation = thread
        .start_turn(HostedTurnRequest::new("active turn"), io::sink())
        .expect("start active turn");
    gate.wait_until_entered();

    assert_eq!(
        thread
            .mutate(RuntimeThreadMutation::AddPinnedContext(
                "must not race".to_string(),
            ))
            .expect_err("running mutation must be rejected"),
        RuntimeHostError::OperationActive {
            operation_id: operation.id(),
        }
    );
    assert_eq!(
        thread
            .backtrack_last_user()
            .expect_err("running backtrack must be rejected"),
        RuntimeHostError::OperationActive {
            operation_id: operation.id(),
        }
    );

    gate.release();
    operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("active turn terminal");
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn logical_turn_resume_waits_for_join_and_publishes_one_terminal() {
    let first_gate = CancelJoinGate::new();
    let second_gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::WaitForCancelAndRelease {
            gate: first_gate.clone(),
        },
        TestBehavior::WaitForRelease {
            gate: second_gate.clone(),
            status: RunStatus::Success,
        },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));

    let factory_generations = Arc::new(Mutex::new(Vec::new()));
    let observed_generations = Arc::clone(&factory_generations);
    let output = RecordingOutput::default();
    let operation = thread
        .start_turn_with_output(
            HostedTurnRequest::new("one logical turn").with_generation_handlers(
                move |generation, cancel| {
                    observed_generations
                        .lock()
                        .unwrap()
                        .push((generation, cancel.is_cancelled()));
                    HostedGenerationHandlers::default()
                },
            ),
            output.clone(),
        )
        .expect("start logical turn");
    first_gate.wait_until_entered();
    let first_generation = operation.initial_generation();
    assert_eq!(first_generation.operation_id(), operation.id());
    assert_eq!(first_generation.generation_id().as_u64(), 0);
    assert_eq!(
        thread
            .admit_generation(first_generation)
            .expect("admit first generation"),
        GenerationAdmissionResult::Accepted {
            generation: first_generation,
        }
    );
    assert_eq!(
        operation.resume().expect("reject uninterrupted resume"),
        ResumeOperationResult::NotInterrupted {
            generation: first_generation,
        }
    );

    assert_eq!(
        operation.interrupt().expect("interrupt first generation"),
        InterruptOperationResult::Requested {
            generation: first_generation,
        }
    );
    first_gate.wait_until_cancel_seen();
    assert_eq!(
        thread
            .admit_generation(first_generation)
            .expect("reject cancelled generation"),
        GenerationAdmissionResult::Rejected {
            requested: first_generation,
            active: Some(first_generation),
        }
    );
    assert_eq!(
        operation.steer("too late").expect("reject cancelled steer"),
        SteerOperationResult::Rejected {
            requested_operation_id: operation.id(),
            active: Some(first_generation),
        }
    );
    assert_eq!(
        operation.resume().expect("queue resume"),
        ResumeOperationResult::Queued {
            generation: first_generation,
        }
    );
    assert_eq!(
        operation.resume().expect("coalesce duplicate resume"),
        ResumeOperationResult::AlreadyQueued {
            generation: first_generation,
        }
    );
    assert_eq!(executor.call_count(), 1);
    assert!(operation.completion().try_terminal().is_none());

    first_gate.release();
    wait_for_call_count(&executor, 2);
    second_gate.wait_until_entered();
    assert!(first_gate.exited());
    let second_generation = match thread.state().expect("read resumed state") {
        RuntimeThreadState::Running { generation, .. } => generation,
        state => panic!("expected running generation, got {state:?}"),
    };
    assert_eq!(second_generation.operation_id(), operation.id());
    assert_eq!(second_generation.generation_id().as_u64(), 1);
    assert_eq!(
        thread
            .admit_generation(first_generation)
            .expect("reject replaced generation"),
        GenerationAdmissionResult::Rejected {
            requested: first_generation,
            active: Some(second_generation),
        }
    );
    assert_eq!(
        thread
            .admit_generation(second_generation)
            .expect("admit resumed generation"),
        GenerationAdmissionResult::Accepted {
            generation: second_generation,
        }
    );
    assert_eq!(
        operation
            .steer("steer resumed generation")
            .expect("steer resumed generation"),
        SteerOperationResult::Accepted {
            generation: second_generation,
        }
    );

    second_gate.release();
    let terminal = operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("logical turn terminal");
    assert_eq!(terminal.operation_id(), operation.id());
    assert_eq!(
        terminal.outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    assert_eq!(operation.completion().try_terminal(), Some(terminal));
    assert_eq!(
        thread
            .admit_generation(second_generation)
            .expect("reject completed generation"),
        GenerationAdmissionResult::Rejected {
            requested: second_generation,
            active: None,
        }
    );
    assert_eq!(
        executor.generations(),
        vec![(first_generation, false), (second_generation, true)]
    );
    let task_states = executor.task_states();
    assert_eq!(task_states.len(), 2);
    assert_eq!(task_states[0].0, task_states[1].0);
    assert_eq!(task_states[0].1, RuntimeTaskStatus::Running);
    assert_eq!(task_states[1].1, RuntimeTaskStatus::Running);
    assert_eq!(executor.cancelled_on_entry(), vec![false, false]);
    assert_eq!(
        executor.steer_inputs(),
        vec![
            Vec::<String>::new(),
            vec!["steer resumed generation".to_string()]
        ]
    );
    assert_eq!(
        *factory_generations.lock().unwrap(),
        vec![(first_generation, false), (second_generation, false)]
    );
    assert_eq!(output.generation_commits(), vec![false, true]);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn actor_owned_steer_is_operation_fenced_and_drained_once() {
    let first_gate = ManualGate::new();
    let second_gate = ManualGate::new();
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::WaitForRelease {
            gate: first_gate.clone(),
            status: RunStatus::Success,
        },
        TestBehavior::WaitForRelease {
            gate: second_gate.clone(),
            status: RunStatus::Success,
        },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));

    let first = thread
        .start_turn(HostedTurnRequest::new("first"), io::sink())
        .expect("start first turn");
    first_gate.wait_until_entered();
    let first_generation = first.initial_generation();
    assert_eq!(
        first.steer("steer once").expect("admit steer"),
        SteerOperationResult::Accepted {
            generation: first_generation,
        }
    );
    first_gate.release();
    assert_eq!(
        first
            .wait_timeout(TEST_TIMEOUT)
            .expect("first terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    assert_eq!(
        executor.steer_inputs(),
        vec![vec!["steer once".to_string()]]
    );

    let second = thread
        .start_turn(HostedTurnRequest::new("second"), io::sink())
        .expect("start second turn");
    second_gate.wait_until_entered();
    let second_generation = second.initial_generation();
    assert_eq!(
        first.steer("stale steer").expect("reject stale steer"),
        SteerOperationResult::Rejected {
            requested_operation_id: first.id(),
            active: Some(second_generation),
        }
    );
    second_gate.release();
    second.wait_timeout(TEST_TIMEOUT).expect("second terminal");
    assert_eq!(executor.steer_inputs().len(), 2);
    assert!(executor.steer_inputs()[1].is_empty());

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn shutdown_joins_current_generation_without_starting_queued_resume() {
    let gate = CancelJoinGate::new();
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::WaitForCancelAndRelease { gate: gate.clone() },
        TestBehavior::EmitEvent {
            message: "queued resume must not run".to_string(),
            status: RunStatus::Success,
        },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));
    let operation = thread
        .start_turn(HostedTurnRequest::new("shutdown"), io::sink())
        .expect("start turn");
    gate.wait_until_entered();
    operation.interrupt().expect("interrupt generation");
    gate.wait_until_cancel_seen();
    assert!(matches!(
        operation.resume().expect("queue resume"),
        ResumeOperationResult::Queued { .. }
    ));

    let release_gate = gate.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        release_gate.release();
    });
    thread.shutdown().expect("shutdown thread actor");
    releaser.join().expect("join generation releaser");

    assert!(gate.exited());
    assert_eq!(executor.call_count(), 1);
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
fn headless_session_rejects_generation_resume() {
    let gate = CancelJoinGate::new();
    let executor = Arc::new(ScriptedExecutor::new([
        TestBehavior::WaitForCancelAndRelease { gate: gate.clone() },
    ]));
    let (_cwd, host, thread) = start_scripted_thread(Arc::clone(&executor));
    let operation = thread
        .start_turn(HostedTurnRequest::headless_session("headless"), io::sink())
        .expect("start headless session");
    gate.wait_until_entered();
    let generation = operation.initial_generation();
    operation
        .interrupt()
        .expect("interrupt headless generation");
    gate.wait_until_cancel_seen();
    assert_eq!(
        operation.resume().expect("reject headless resume"),
        ResumeOperationResult::NotResumable { generation }
    );
    gate.release();
    operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("headless terminal");
    assert!(gate.exited());
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

#[test]
fn canonical_hosted_turn_emits_context_budget_before_turn_start() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "canonical context")
        .expect("start runtime thread");
    let output = SharedWriter::default();

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("reply from mock provider"),
            output.clone(),
        )
        .expect("start canonical context turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("canonical context terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let events = output.json_events();
    let context_index = events
        .iter()
        .position(|event| event["type"] == "context.updated")
        .expect("context budget event");
    let turn_index = events
        .iter()
        .position(|event| event["type"] == "turn.started")
        .expect("turn started event");
    assert!(context_index < turn_index);
    assert!(
        events[context_index]["payload"]["limit_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_hosted_bash_streams_output_before_tool_terminal() {
    let cwd = tempfile::tempdir().unwrap();
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "canonical shell output")
        .expect("start runtime thread");
    let output = SharedWriter::default();

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("bash printf before; sleep 0.1; printf after"),
            output.clone(),
        )
        .expect("start canonical shell turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("canonical shell terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let events = output.json_events();
    let turn_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| (event["type"] == "turn.started").then_some(index))
        .collect::<Vec<_>>();
    assert!(
        turn_indices.len() >= 2,
        "tool execution must require a follow-up provider turn"
    );
    assert!(turn_indices.iter().all(|index| {
        index
            .checked_sub(1)
            .is_some_and(|previous| events[previous]["type"] == "context.updated")
    }));
    let deltas = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == "tool.output.delta")
        .collect::<Vec<_>>();
    assert!(!deltas.is_empty(), "canonical bash must stream output");
    let streamed = deltas
        .iter()
        .filter_map(|(_, event)| event["payload"]["chunk"].as_str())
        .collect::<String>();
    assert!(streamed.contains("before"));
    assert!(streamed.contains("after"));
    let terminal_index = events
        .iter()
        .position(|event| event["type"] == "tool.call.completed")
        .expect("tool terminal");
    assert!(deltas.iter().all(|(index, _)| *index < terminal_index));

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_hosted_edit_emits_committed_diff() {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("notes.txt"), "old\nsame\n").unwrap();
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "canonical committed diff")
        .expect("start runtime thread");
    let output = SharedWriter::default();

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("edit notes.txt :: old => committed"),
            output.clone(),
        )
        .expect("start canonical edit turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("canonical edit terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let completed = output
        .json_events()
        .into_iter()
        .find(|event| event["type"] == "tool.call.completed" && event["payload"]["name"] == "edit")
        .expect("edit terminal");
    let diff = completed["payload"]["diff"]
        .as_str()
        .expect("committed diff");
    assert!(diff.contains("-old"));
    assert!(diff.contains("+committed"));
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("notes.txt")).unwrap(),
        "committed\nsame\n"
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_hosted_pinned_prompt_preserves_previous_user_backtrack_target() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "canonical prompt placement",
        )
        .expect("start runtime thread");

    let user_turn = thread
        .start_turn(
            HostedTurnRequest::new("first user turn").with_backtrack_target(true),
            io::sink(),
        )
        .expect("start user turn");
    assert_eq!(
        user_turn
            .wait_timeout(TEST_TIMEOUT)
            .expect("user turn terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let notification = thread
        .start_turn(
            HostedTurnRequest::new("<task-notification>done</task-notification>")
                .with_backtrack_target(false),
            io::sink(),
        )
        .expect("start pinned notification turn");
    assert_eq!(
        notification
            .wait_timeout(TEST_TIMEOUT)
            .expect("notification turn terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    assert_eq!(
        thread
            .backtrack_last_user()
            .expect("backtrack actor-owned session"),
        Some("first user turn".to_string())
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_hosted_task_metadata_owns_one_main_session_lifecycle() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "canonical task metadata",
        )
        .expect("start runtime thread");
    let output = SharedWriter::default();

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("reply from mock provider")
                .with_task_description("Workflow notification task-42"),
            output.clone(),
        )
        .expect("start tracked turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("tracked turn terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    let task_events = output
        .json_events()
        .into_iter()
        .filter(|event| event["type"] == "task.status.updated")
        .collect::<Vec<_>>();
    assert_eq!(task_events.len(), 2);
    assert_eq!(task_events[0]["payload"]["task"]["status"], "running");
    assert_eq!(task_events[1]["payload"]["task"]["status"], "completed");
    assert_eq!(
        task_events[0]["payload"]["task"]["id"],
        task_events[1]["payload"]["task"]["id"]
    );
    assert_eq!(
        task_events[0]["payload"]["task"]["description"],
        "Workflow notification task-42"
    );

    let tasks = thread.task_registry().list();
    let task = tasks
        .iter()
        .find(|task| task.description == "Workflow notification task-42")
        .expect("tracked main-session task");
    assert_eq!(task.task_type, orca_core::task_types::TaskType::MainSession);
    assert_eq!(task.status, orca_core::task_types::TaskStatus::Completed);
    assert_eq!(
        thread
            .snapshot()
            .expect("tracked turn snapshot")
            .active_task_id(),
        Some(task.id.as_str())
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_hosted_task_event_failure_closes_registry_task() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "canonical task event failure",
        )
        .expect("start runtime thread");
    let observer = Arc::new(|event: &orca_core::event_schema::EventEnvelope| {
        if event.event_type == EventType::TaskStatusUpdated {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "task event subscriber disconnected",
            ));
        }
        Ok(())
    });

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("reply from mock provider")
                .with_task_description("Tracked task with failed event")
                .with_event_observer(observer),
            io::sink(),
        )
        .expect("start tracked turn");
    assert!(matches!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("failed task event terminal")
            .outcome(),
        OperationOutcome::ExecutionFailed {
            kind: io::ErrorKind::BrokenPipe,
            ..
        }
    ));

    let task = thread
        .task_registry()
        .list()
        .into_iter()
        .find(|task| task.description == "Tracked task with failed event")
        .expect("failed tracked task");
    assert_eq!(task.status, orca_core::task_types::TaskStatus::Failed);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_hosted_default_turn_does_not_emit_main_session_task_events() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "canonical untracked turn",
        )
        .expect("start runtime thread");
    let output = SharedWriter::default();

    let operation = thread
        .start_turn(
            HostedTurnRequest::new("reply from mock provider"),
            output.clone(),
        )
        .expect("start untracked turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("untracked turn terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    assert!(
        output
            .json_events()
            .iter()
            .all(|event| event["type"] != "task.status.updated")
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn generation_scoped_approval_handler_controls_canonical_tool_execution() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "canonical approval")
        .expect("start runtime thread");
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let request =
        HostedTurnRequest::new("bash true").with_generation_handlers(move |_fence, _cancel| {
            HostedGenerationHandlers::default().with_approval_handler(Arc::new(
                AllowApprovalHandler {
                    calls: Arc::clone(&handler_calls),
                },
            ))
        });

    let output = SharedWriter::default();
    let operation = thread
        .start_turn(request, output.clone())
        .expect("start canonical approval turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("canonical approval terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(
        output
            .json_events()
            .into_iter()
            .filter(|event| {
                event["type"] == "tool.call.completed" && event["payload"]["status"] == "completed"
            })
            .count(),
        1
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn request_scoped_approval_handler_remains_the_hosted_fallback() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "approval fallback")
        .expect("start runtime thread");
    let calls = Arc::new(AtomicUsize::new(0));
    let request =
        HostedTurnRequest::new("bash true").with_approval_handler(Arc::new(AllowApprovalHandler {
            calls: Arc::clone(&calls),
        }));

    let operation = thread
        .start_turn(request, io::sink())
        .expect("start fallback approval turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("fallback approval terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn interrupting_generation_scoped_approval_wait_cancels_only_that_operation() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "canonical approval interrupt",
        )
        .expect("start runtime thread");
    let first_entered = Arc::new(AtomicBool::new(false));
    let entered = Arc::clone(&first_entered);
    let first =
        HostedTurnRequest::new("bash true").with_generation_handlers(move |_fence, cancel| {
            HostedGenerationHandlers::default().with_approval_handler(Arc::new(
                CancelledApprovalHandler {
                    cancel,
                    entered: Arc::clone(&entered),
                },
            ))
        });
    let output = SharedWriter::default();
    let operation = thread
        .start_turn(first, output.clone())
        .expect("start waiting approval turn");
    wait_until_true(&first_entered, "approval handler did not start waiting");
    assert!(matches!(
        operation.interrupt().expect("interrupt approval wait"),
        InterruptOperationResult::Requested { .. }
    ));
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("cancelled approval terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Cancelled)
    );
    let cancelled_tools = output
        .json_events()
        .into_iter()
        .filter(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["status"] == "cancelled"
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled_tools.len(), 1);
    assert_eq!(
        cancelled_tools[0]["payload"]["error"],
        "interactive approval was interrupted"
    );

    let second_calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&second_calls);
    let second =
        HostedTurnRequest::new("bash true").with_generation_handlers(move |_fence, _cancel| {
            HostedGenerationHandlers::default().with_approval_handler(Arc::new(
                AllowApprovalHandler {
                    calls: Arc::clone(&handler_calls),
                },
            ))
        });
    let next = thread
        .start_turn(second, io::sink())
        .expect("start fresh approval turn");
    assert_eq!(
        next.wait_timeout(TEST_TIMEOUT)
            .expect("fresh approval terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    assert_eq!(second_calls.load(Ordering::Acquire), 1);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn canonical_turn_can_suspend_one_in_flight_provider_without_committing_terminal_state() {
    let cwd = tempfile::tempdir().unwrap();
    let config = test_config(cwd.path().to_path_buf());
    let mut thread = RuntimeThread::start(&config, "canonical provider suspension")
        .expect("start runtime thread");
    let request = ThreadTurnRequest::new("mock_stream_delay_ms 100")
        .with_provider_suspension_control(Arc::new(OneShotProviderSuspension::new()));
    let mut events = EventFactory::new(thread.thread_id().to_string());

    let outcome = thread
        .run_request_with_event_factory_and_cancel_outcome(
            &config,
            &request,
            io::sink(),
            &mut events,
            CancelToken::new(),
        )
        .expect("run canonical turn until provider suspension");
    let ThreadTurnOutcome::ProviderSuspended {
        mut suspension,
        background_workflows,
    } = outcome
    else {
        panic!("canonical turn must return the in-flight provider handle");
    };
    assert!(background_workflows.is_empty());
    assert_eq!(thread.session().completion_error(), None);

    let mut completed = None;
    while completed.is_none() {
        match suspension
            .recv_timeout(TEST_TIMEOUT)
            .expect("suspended provider event")
        {
            RuntimeProviderSuspensionEvent::Step(_) => {}
            RuntimeProviderSuspensionEvent::Completed(response) => completed = Some(response),
        }
    }
    assert_eq!(
        completed
            .as_ref()
            .and_then(|response| response.assistant_content.as_deref()),
        Some("Mock slow stream started.Mock slow stream completed.")
    );
}

#[test]
fn runtime_host_owns_suspended_provider_and_releases_actor_for_the_next_turn() {
    let cwd = tempfile::tempdir().unwrap();
    let config = test_config(cwd.path().to_path_buf());
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "host-owned provider suspension")
        .expect("start hosted runtime thread");
    let request = HostedTurnRequest::new("mock_stream_delay_ms 500")
        .with_task_description("slow provider turn")
        .with_generation_handlers(|_fence, _cancel| {
            HostedGenerationHandlers::default()
                .with_provider_suspension_control(Arc::new(OneShotProviderSuspension::new()))
        });

    let first = thread
        .start_turn(request, io::sink())
        .expect("start suspendable turn");
    let terminal = first
        .wait_timeout(TEST_TIMEOUT)
        .expect("background handoff terminal");
    let task_id = match terminal.outcome() {
        OperationOutcome::Backgrounded { task_id } => task_id.clone(),
        other => panic!("expected host-owned background handoff, got {other:?}"),
    };
    let backgrounded = thread
        .task_registry()
        .get(&task_id)
        .expect("background task record");
    assert_eq!(backgrounded.status, TaskStatus::Running);
    assert!(backgrounded.is_backgrounded);

    let second = thread
        .start_turn(HostedTurnRequest::new("next foreground turn"), io::sink())
        .expect("actor must accept a foreground turn while provider is backgrounded");
    assert_eq!(
        second
            .wait_timeout(TEST_TIMEOUT)
            .expect("next foreground terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    wait_until_task_status(&thread, &task_id, TaskStatus::Completed);
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn provider_background_handoff_keeps_one_thread_event_sequence() {
    let cwd = tempfile::tempdir().unwrap();
    let config = test_config(cwd.path().to_path_buf());
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "provider event sequence")
        .expect("start hosted runtime thread");
    let observer = Arc::new(RecordingEventObserver::default());
    let request = HostedTurnRequest::new("mock_stream_delay_ms 100")
        .with_task_description("sequenced background provider")
        .with_event_observer(observer.clone())
        .with_generation_handlers(|_fence, _cancel| {
            HostedGenerationHandlers::default()
                .with_provider_suspension_control(Arc::new(OneShotProviderSuspension::new()))
        });

    let operation = thread
        .start_turn(request, io::sink())
        .expect("start suspendable turn");
    let task_id = match operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("background handoff terminal")
        .outcome()
    {
        OperationOutcome::Backgrounded { task_id } => task_id.clone(),
        other => panic!("expected host-owned background handoff, got {other:?}"),
    };
    wait_until_task_status(&thread, &task_id, TaskStatus::Completed);

    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        let terminal_observed = observer.events().iter().any(|event| {
            event.event_type == EventType::TaskStatusUpdated
                && event.payload["task"]["id"] == task_id
                && event.payload["task"]["status"] == "completed"
        });
        if terminal_observed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background provider terminal event was not observed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut sequences = observer
        .events()
        .into_iter()
        .filter(|event| event.run_id == thread.thread_id())
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    assert_eq!(
        sequences,
        (0..sequences.len() as u64).collect::<Vec<_>>(),
        "foreground and background provider events must share one sequence"
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn runtime_host_shutdown_cancels_and_joins_suspended_provider_work() {
    let cwd = tempfile::tempdir().unwrap();
    let config = test_config(cwd.path().to_path_buf());
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "shutdown suspended provider")
        .expect("start hosted runtime thread");
    let request = HostedTurnRequest::new("mock_stream_delay_ms 5000")
        .with_task_description("provider cancelled by host shutdown")
        .with_generation_handlers(|_fence, _cancel| {
            HostedGenerationHandlers::default()
                .with_provider_suspension_control(Arc::new(OneShotProviderSuspension::new()))
        });
    let operation = thread
        .start_turn(request, io::sink())
        .expect("start suspendable turn");
    let terminal = operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("background handoff terminal");
    let task_id = match terminal.outcome() {
        OperationOutcome::Backgrounded { task_id } => task_id.clone(),
        other => panic!("expected host-owned background handoff, got {other:?}"),
    };

    let started = Instant::now();
    host.shutdown().expect("shutdown joins background provider");
    assert!(
        started.elapsed() < TEST_TIMEOUT,
        "shutdown waited for the provider's full delay instead of cancelling it"
    );
    assert_eq!(
        thread
            .task_registry()
            .get(&task_id)
            .expect("settled background task")
            .status,
        TaskStatus::Stopped
    );
}

#[test]
fn runtime_host_commits_background_provider_usage_once() {
    let cwd = tempfile::tempdir().unwrap();
    let config = test_config(cwd.path().to_path_buf());
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "background usage ledger")
        .expect("start hosted runtime thread");
    let request = HostedTurnRequest::new("mock_stream_usage_delay_ms 100")
        .with_task_description("usage-bearing background turn")
        .with_generation_handlers(|_fence, _cancel| {
            HostedGenerationHandlers::default()
                .with_provider_suspension_control(Arc::new(OneShotProviderSuspension::new()))
        });
    let operation = thread
        .start_turn(request, io::sink())
        .expect("start usage-bearing turn");
    let task_id = match operation
        .wait_timeout(TEST_TIMEOUT)
        .expect("background handoff terminal")
        .outcome()
    {
        OperationOutcome::Backgrounded { task_id } => task_id.clone(),
        other => panic!("expected background handoff, got {other:?}"),
    };
    wait_until_task_status(&thread, &task_id, TaskStatus::Completed);

    let task_usage = thread
        .task_registry()
        .get(&task_id)
        .and_then(|task| task.usage)
        .expect("background task usage");
    assert_eq!(task_usage.input_tokens, 120);
    assert_eq!(task_usage.output_tokens, 30);
    let snapshot = thread.snapshot().expect("usage snapshot");
    assert_eq!(snapshot.usage_totals().input_tokens, 120);
    assert_eq!(snapshot.usage_totals().output_tokens, 30);

    let foreground = thread
        .start_turn(HostedTurnRequest::new("mock_usage"), io::sink())
        .expect("start foreground usage turn");
    assert_eq!(
        foreground
            .wait_timeout(TEST_TIMEOUT)
            .expect("foreground terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    let snapshot = thread.snapshot().expect("combined usage snapshot");
    assert_eq!(snapshot.usage_totals().input_tokens, 240);
    assert_eq!(snapshot.usage_totals().output_tokens, 60);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn runtime_host_owns_turn_launched_workflow_until_shutdown() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let cwd = tempfile::tempdir().unwrap();
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "host-owned turn workflow")
        .expect("start hosted runtime thread");
    let observer = Arc::new(RecordingEventObserver::default());
    let operation = thread
        .start_turn(
            HostedTurnRequest::new("workflow inline")
                .with_wait_for_background_workflows(false)
                .with_event_observer(observer.clone()),
            io::sink(),
        )
        .expect("start workflow turn");
    assert_eq!(
        operation
            .wait_timeout(TEST_TIMEOUT)
            .expect("workflow foreground terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    let workflow_task = thread
        .task_registry()
        .list()
        .into_iter()
        .find(|task| task.task_type == orca_core::task_types::TaskType::Workflow)
        .expect("background workflow task");
    assert_eq!(workflow_task.status, TaskStatus::Running);

    let next = thread
        .start_turn(HostedTurnRequest::new("next foreground turn"), io::sink())
        .expect("actor accepts next turn while workflow is running");
    assert_eq!(
        next.wait_timeout(TEST_TIMEOUT)
            .expect("next foreground terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );

    host.shutdown().expect("shutdown joins background workflow");
    assert_eq!(
        thread
            .task_registry()
            .get(&workflow_task.id)
            .expect("settled workflow task")
            .status,
        TaskStatus::Stopped
    );
    assert_eq!(observer.count(EventType::WorkflowFailed), 1);
}

#[test]
fn runtime_host_launches_saved_workflow_without_blocking_the_next_turn() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let cwd = tempfile::tempdir().unwrap();
    write_saved_workflow(cwd.path(), "slow-audit", "mock_stream_delay_ms 1200");
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "hosted saved workflow")
        .expect("start hosted runtime thread");
    let observer = Arc::new(RecordingEventObserver::default());
    let launched = thread
        .launch_workflow(
            HostedWorkflowRequest::new("slow-audit").with_event_observer(observer.clone()),
        )
        .expect("launch saved workflow");
    assert_eq!(
        thread
            .task_registry()
            .get(launched.task_id())
            .expect("running workflow task")
            .status,
        TaskStatus::Running
    );

    let next = thread
        .start_turn(HostedTurnRequest::new("next foreground turn"), io::sink())
        .expect("actor accepts next turn");
    assert_eq!(
        next.wait_timeout(TEST_TIMEOUT)
            .expect("next foreground terminal")
            .outcome(),
        &OperationOutcome::Completed(RunStatus::Success)
    );
    wait_until_task_status(&thread, launched.task_id(), TaskStatus::Completed);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while observer.count(EventType::WorkflowResultAvailable) == 0 {
        assert!(
            Instant::now() < deadline,
            "workflow terminal event was not published"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(observer.count(EventType::WorkflowResultAvailable), 1);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn runtime_host_shutdown_cancels_and_joins_saved_workflow() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let cwd = tempfile::tempdir().unwrap();
    write_saved_workflow(cwd.path(), "shutdown-audit", "mock_stream_delay_ms 5000");
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(config, "shutdown saved workflow")
        .expect("start hosted runtime thread");
    let launched = thread
        .launch_workflow(HostedWorkflowRequest::new("shutdown-audit"))
        .expect("launch saved workflow");
    let task_id = launched.task_id().to_string();

    let started = Instant::now();
    host.shutdown().expect("shutdown joins saved workflow");
    assert!(
        started.elapsed() < TEST_TIMEOUT,
        "shutdown waited for the workflow's full provider delay"
    );
    assert_eq!(
        thread
            .task_registry()
            .get(&task_id)
            .expect("settled workflow task")
            .status,
        TaskStatus::Stopped
    );
}

#[test]
fn runtime_host_rejects_saved_workflow_before_launch_when_background_capacity_is_exhausted() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let cwd = tempfile::tempdir().unwrap();
    write_saved_workflow(cwd.path(), "capacity-audit", "inspect repo");
    let host = RuntimeHost::start_with_background_capacity(0).expect("start runtime host");
    let thread = host
        .start_thread(test_config(cwd.path().to_path_buf()), "workflow capacity")
        .expect("start hosted runtime thread");

    let error = thread
        .launch_workflow(HostedWorkflowRequest::new("capacity-audit"))
        .expect_err("capacity exhaustion rejects launch");
    assert!(error.to_string().contains("capacity exhausted (0)"));
    assert!(thread.task_registry().list().is_empty());

    host.shutdown().expect("shutdown runtime host");
}

fn write_saved_workflow(cwd: &std::path::Path, name: &str, prompt: &str) {
    let workflow_dir = cwd.join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).unwrap();
    fs::write(
        workflow_dir.join(format!("{name}.js")),
        format!(
            "export const meta = {{ name: '{name}', description: 'Runtime host ownership test', phases: ['main'] }};\nexport default await phase('main', async () => agent('{prompt}'));"
        ),
    )
    .unwrap();
}

fn wait_until_task_status(
    thread: &orca_runtime::runtime_host::RuntimeThreadHandle,
    task_id: &str,
    expected: TaskStatus,
) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        if thread
            .task_registry()
            .get(task_id)
            .is_some_and(|task| task.status == expected)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} did not reach {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
