//! Integration tests for the ACP agent adapter layer.
//!
//! These tests drive `OrcaAcpAgent` directly (without the stdio transport) using
//! a scripted `ThreadOperationExecutor` to emit events that the ACP event
//! projector maps onto `SessionUpdate` notifications.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_client_protocol::{
    Agent, CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, SessionId, SessionNotification, SessionUpdate, StopReason,
};
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_core::thread_identity::TurnId;
use orca_runtime::acp::OrcaAcpAgent;
use orca_runtime::runtime_host::{
    GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
    ThreadOperationOutcome,
};
use orca_runtime::thread::RuntimeThread;
use tokio::sync::mpsc;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

struct OrcaHomeGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
    _home: tempfile::TempDir,
}

impl OrcaHomeGuard {
    fn new() -> Self {
        let lock = ORCA_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().expect("create temporary ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        // This guard serializes environment mutation until the host shuts down.
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        Self {
            previous,
            _lock: lock,
            _home: home,
        }
    }
}

impl Drop for OrcaHomeGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(previous) => std::env::set_var("ORCA_HOME", previous),
                None => std::env::remove_var("ORCA_HOME"),
            }
        }
    }
}

fn test_config(cwd: PathBuf) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: orca_core::approval_types::ApprovalMode::FullAuto,
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

// --- Scripted executor that emits events through the EventFactory ---

enum TestBehavior {
    EmitMessageAndComplete { message: String },
    WaitForCancel,
}

struct AcpTestExecutor {
    behaviors: Mutex<Vec<TestBehavior>>,
    calls: AtomicUsize,
    working_directories: Mutex<Vec<Option<PathBuf>>>,
}

impl AcpTestExecutor {
    fn new(behaviors: Vec<TestBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors),
            calls: AtomicUsize::new(0),
            working_directories: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn working_directories(&self) -> Vec<Option<PathBuf>> {
        self.working_directories.lock().unwrap().clone()
    }
}

impl ThreadOperationExecutor for AcpTestExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.working_directories
            .lock()
            .unwrap()
            .push(generation.config().cwd.clone());
        let behavior = self.behaviors.lock().unwrap().remove(0);
        match behavior {
            TestBehavior::EmitMessageAndComplete { message } => {
                let identity =
                    orca_core::thread_item_projection::ModelResponseIdentity::new(TurnId::new());
                let mut sink = EventSink::new(writer, generation.config().output_format)
                    .with_optional_observer(request.event_observer());
                sink.emit(events.assistant_message_delta(&identity, &message))?;
                Ok(RunStatus::Success.into())
            }
            TestBehavior::WaitForCancel => {
                let deadline = std::time::Instant::now() + TEST_TIMEOUT;
                while !cancel.is_cancelled() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "operation was not cancelled within timeout"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(RunStatus::Cancelled.into())
            }
        }
    }
}

// --- Helper to drain notifications from the channel ---

fn drain_notifications(
    rx: &mut mpsc::UnboundedReceiver<SessionNotification>,
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    while let Ok(notification) = rx.try_recv() {
        updates.push(notification.update);
    }
    updates
}

// --- Tests ---

#[test]
fn acp_initialize_returns_v1_with_load_session_capability() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor).expect("start host");
    let (note_tx, _note_rx) = mpsc::unbounded_channel::<SessionNotification>();
    let agent = OrcaAcpAgent::new(
        host.handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let response = local.block_on(&rt, async {
        agent
            .initialize(InitializeRequest::new(ProtocolVersion::V1))
            .await
            .expect("initialize")
    });

    assert_eq!(response.protocol_version, ProtocolVersion::V1);
    assert!(response.agent_capabilities.load_session);
    assert_eq!(
        response.agent_info.as_ref().map(|i| i.name.as_str()),
        Some("orca")
    );
    assert_eq!(
        response.agent_info.as_ref().map(|i| i.version.as_str()),
        Some("test")
    );

    host.shutdown().expect("shutdown");
}

#[test]
fn acp_new_session_and_prompt_produces_message_chunk_notification() {
    let _home = OrcaHomeGuard::new();
    let base_cwd = tempfile::tempdir().unwrap();
    let session_cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![
        TestBehavior::EmitMessageAndComplete {
            message: "Hello from Orca!".to_string(),
        },
    ]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, mut note_rx) = mpsc::unbounded_channel::<SessionNotification>();
    let agent = OrcaAcpAgent::new(
        host.handle(),
        test_config(base_cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let (session_id, stop_reason) = local.block_on(&rt, async {
        let session = agent
            .new_session(NewSessionRequest::new(session_cwd.path().to_path_buf()))
            .await
            .expect("new_session");

        let prompt_response = agent
            .prompt(PromptRequest::new(
                session.session_id.clone(),
                vec![ContentBlock::from("Say hello".to_string())],
            ))
            .await
            .expect("prompt");

        (session.session_id, prompt_response.stop_reason)
    });

    assert_eq!(stop_reason, StopReason::EndTurn);
    assert_eq!(executor.call_count(), 1);
    assert_eq!(
        executor.working_directories(),
        vec![Some(session_cwd.path().to_path_buf())]
    );

    let updates = drain_notifications(&mut note_rx);
    assert!(
        !updates.is_empty(),
        "should have received at least one session update"
    );
    let has_message_chunk = updates.iter().any(|update| {
        matches!(update, SessionUpdate::AgentMessageChunk(chunk)
            if matches!(&chunk.content, ContentBlock::Text(text) if text.text.contains("Hello from Orca!")))
    });
    assert!(
        has_message_chunk,
        "expected AgentMessageChunk with 'Hello from Orca!' in updates: {updates:?}"
    );

    drop(session_id);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_cancel_stops_in_flight_prompt() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![TestBehavior::WaitForCancel]));
    let host = RuntimeHost::start_with_executor(executor.clone()).expect("start host");
    let (note_tx, _note_rx) = mpsc::unbounded_channel::<SessionNotification>();
    let agent = OrcaAcpAgent::new(
        host.handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let stop_reason = local.block_on(&rt, async {
        let session = Agent::new_session(&agent, NewSessionRequest::new(cwd.path().to_path_buf()))
            .await
            .expect("new_session");

        let session_id_for_prompt = session.session_id.clone();
        let session_id_for_cancel = session.session_id.clone();

        // Poll prompt first so it enters start_turn, then issue cancellation
        // immediately. This exercises the pre-install cancellation window.
        let prompt_fut = Agent::prompt(
            &agent,
            PromptRequest::new(
                session_id_for_prompt,
                vec![ContentBlock::from("long running".to_string())],
            ),
        );
        let cancel_fut = async {
            Agent::cancel(&agent, CancelNotification::new(session_id_for_cancel))
                .await
                .expect("cancel");
        };

        // Pin the prompt and run both concurrently.
        tokio::pin!(prompt_fut);
        tokio::pin!(cancel_fut);

        // Drive both: prompt completes once the compensation interrupt lands.
        let (prompt_result, _) = tokio::join!(prompt_fut, cancel_fut);
        prompt_result.expect("prompt").stop_reason
    });

    assert_eq!(stop_reason, StopReason::Cancelled);
    assert_eq!(executor.call_count(), 1);
    host.shutdown().expect("shutdown");
}

#[test]
fn acp_prompt_on_unknown_session_returns_error() {
    let _home = OrcaHomeGuard::new();
    let cwd = tempfile::tempdir().unwrap();
    let executor = Arc::new(AcpTestExecutor::new(vec![]));
    let host = RuntimeHost::start_with_executor(executor).expect("start host");
    let (note_tx, _note_rx) = mpsc::unbounded_channel::<SessionNotification>();
    let agent = OrcaAcpAgent::new(
        host.handle(),
        test_config(cwd.path().to_path_buf()),
        note_tx,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();

    let result = local.block_on(&rt, async {
        agent
            .prompt(PromptRequest::new(
                SessionId::new("nonexistent-session"),
                vec![ContentBlock::from("hello".to_string())],
            ))
            .await
    });

    assert!(result.is_err(), "prompt on unknown session should fail");
    host.shutdown().expect("shutdown");
}
