use std::collections::HashMap;
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::cancel::{CancelToken, OperationId, OperationIdAllocator};
use orca_core::config::RunConfig;
use orca_core::conversation::{Conversation, Message};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventDraft, EventFactory, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink, observe_event};
use orca_core::hook_types::HookEvent;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::task_types::TaskStatus;
use orca_core::workflow_types::{WorkflowInput, WorkflowOutput};
use orca_mcp::{McpElicitationHandler, McpRegistry};
use serde_json::Value;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{self as tokio_mpsc, error::TrySendError};
use tokio::task::JoinHandle;

use crate::background_turn::RuntimeTurnContinuation;
use crate::controller::{
    ControllerRunOptions, RuntimeBackgroundWorkflows, ThreadTurnPromptPlacement, ThreadTurnRequest,
    ThreadTurnToolMode,
};
use crate::hooks::HookContext;
use crate::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequestHandler, RuntimeTaskKind,
    RuntimeUserInputHandler, ThreadSteerHandle,
};
use crate::provider_stream::{
    RuntimeProviderSuspension, RuntimeProviderSuspensionControl, RuntimeProviderSuspensionEvent,
};
use crate::tasks::{MainSessionTerminalUpdate, TaskRegistry};
use crate::thread::RuntimeThread;
use crate::thread_store::SessionTranscript;
use crate::workflow::runner::{WorkflowLaunchRequest, WorkflowRunner};
use crate::workflow_execution::BackgroundWorkflowRun;

pub const HOST_COMMAND_CAPACITY: usize = 16;
pub const THREAD_COMMAND_CAPACITY: usize = 16;
pub const HOST_BACKGROUND_TASK_CAPACITY: usize = 16;
const WORKFLOW_BACKGROUND_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub trait HostedOperationWriter: io::Write + Send + 'static {
    fn finish_generation(&mut self, commit_terminal: bool) -> io::Result<()>;
}

struct PassthroughHostedOperationWriter<W> {
    writer: W,
}

impl<W> PassthroughHostedOperationWriter<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: io::Write> io::Write for PassthroughHostedOperationWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: io::Write + Send + 'static> HostedOperationWriter for PassthroughHostedOperationWriter<W> {
    fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
        self.writer.flush()
    }
}

#[derive(Clone, Default)]
pub struct HostedGenerationHandlers {
    approval_handler: Option<Arc<dyn RuntimeApprovalHandler + Send + Sync>>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    user_input_handler: Option<Arc<dyn RuntimeUserInputHandler + Send + Sync>>,
    mcp_elicitation_handler: Option<Arc<dyn McpElicitationHandler + Send + Sync>>,
    provider_suspension_control: Option<Arc<dyn RuntimeProviderSuspensionControl>>,
}

impl fmt::Debug for HostedGenerationHandlers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostedGenerationHandlers")
            .field("approval_handler", &self.approval_handler.is_some())
            .field("permission_handler", &self.permission_handler.is_some())
            .field("user_input_handler", &self.user_input_handler.is_some())
            .field(
                "mcp_elicitation_handler",
                &self.mcp_elicitation_handler.is_some(),
            )
            .field(
                "provider_suspension_control",
                &self.provider_suspension_control.is_some(),
            )
            .finish()
    }
}

impl HostedGenerationHandlers {
    pub fn with_approval_handler(
        mut self,
        handler: Arc<dyn RuntimeApprovalHandler + Send + Sync>,
    ) -> Self {
        self.approval_handler = Some(handler);
        self
    }

    pub fn with_permission_handler(
        mut self,
        handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    ) -> Self {
        self.permission_handler = Some(handler);
        self
    }

    pub fn with_user_input_handler(
        mut self,
        handler: Arc<dyn RuntimeUserInputHandler + Send + Sync>,
    ) -> Self {
        self.user_input_handler = Some(handler);
        self
    }

    pub fn with_mcp_elicitation_handler(
        mut self,
        handler: Arc<dyn McpElicitationHandler + Send + Sync>,
    ) -> Self {
        self.mcp_elicitation_handler = Some(handler);
        self
    }

    pub fn with_provider_suspension_control(
        mut self,
        control: Arc<dyn RuntimeProviderSuspensionControl>,
    ) -> Self {
        self.provider_suspension_control = Some(control);
        self
    }
}

type HostedGenerationHandlerFactory =
    dyn Fn(GenerationFence, CancelToken) -> HostedGenerationHandlers + Send + Sync;

pub trait ThreadOperationExecutor: Send + Sync + 'static {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome>;
}

pub enum ThreadOperationOutcome {
    Completed {
        status: RunStatus,
        background_workflows: RuntimeBackgroundWorkflows,
    },
    ProviderSuspended {
        suspension: Box<RuntimeProviderSuspension>,
        background_workflows: RuntimeBackgroundWorkflows,
    },
}

impl From<RunStatus> for ThreadOperationOutcome {
    fn from(status: RunStatus) -> Self {
        Self::Completed {
            status,
            background_workflows: RuntimeBackgroundWorkflows::from_vec(Vec::new()),
        }
    }
}

impl ThreadOperationOutcome {
    fn background_workflow_count(&self) -> usize {
        match self {
            Self::Completed {
                background_workflows,
                ..
            }
            | Self::ProviderSuspended {
                background_workflows,
                ..
            } => background_workflows.len(),
        }
    }

    fn take_background_workflows(&mut self) -> RuntimeBackgroundWorkflows {
        match self {
            Self::Completed {
                background_workflows,
                ..
            }
            | Self::ProviderSuspended {
                background_workflows,
                ..
            } => std::mem::take(background_workflows),
        }
    }

    fn suspends_provider(&self) -> bool {
        matches!(self, Self::ProviderSuspended { .. })
    }
}

#[derive(Clone)]
pub struct HostedTurnRequest {
    prompt: String,
    options: ControllerRunOptions,
    operation_kind: HostedOperationKind,
    task_description: Option<String>,
    backtrack_target: bool,
    allow_goal_tools: bool,
    track_goal_usage: bool,
    emit_session_completed: bool,
    envelope: HostedOperationEnvelope,
    approval_handler: Option<Arc<dyn RuntimeApprovalHandler + Send + Sync>>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    user_input_handler: Option<Arc<dyn RuntimeUserInputHandler + Send + Sync>>,
    mcp_elicitation_handler: Option<Arc<dyn McpElicitationHandler + Send + Sync>>,
    event_observer: Option<Arc<dyn EventObserver>>,
    continuation: Option<RuntimeTurnContinuation>,
    resumes_existing_turn: bool,
    task_id: Option<String>,
    main_session_task_id: Option<String>,
    generation_handler_factory: Option<Arc<HostedGenerationHandlerFactory>>,
    usage_credit: UsageTotals,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostedOperationKind {
    Turn,
    ManualCompaction,
    BackgroundContinuation { task_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostedOperationEnvelope {
    Turn,
    HeadlessSession,
}

impl HostedTurnRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            options: ControllerRunOptions::default(),
            operation_kind: HostedOperationKind::Turn,
            task_description: None,
            backtrack_target: false,
            allow_goal_tools: false,
            track_goal_usage: false,
            emit_session_completed: true,
            envelope: HostedOperationEnvelope::Turn,
            approval_handler: None,
            permission_handler: None,
            user_input_handler: None,
            mcp_elicitation_handler: None,
            event_observer: None,
            continuation: None,
            resumes_existing_turn: false,
            task_id: None,
            main_session_task_id: None,
            generation_handler_factory: None,
            usage_credit: UsageTotals::default(),
        }
    }

    pub fn headless_session(prompt: impl Into<String>) -> Self {
        Self {
            envelope: HostedOperationEnvelope::HeadlessSession,
            emit_session_completed: false,
            ..Self::new(prompt)
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn with_options(mut self, options: ControllerRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_operation_kind(mut self, operation_kind: HostedOperationKind) -> Self {
        self.operation_kind = operation_kind;
        self
    }

    pub fn operation_kind(&self) -> &HostedOperationKind {
        &self.operation_kind
    }

    pub fn with_task_description(mut self, description: impl Into<String>) -> Self {
        self.task_description = Some(description.into());
        self
    }

    pub fn task_description(&self) -> Option<&str> {
        self.task_description.as_deref()
    }

    pub fn with_backtrack_target(mut self, backtrack_target: bool) -> Self {
        self.backtrack_target = backtrack_target;
        self
    }

    pub fn is_backtrack_target(&self) -> bool {
        self.backtrack_target
    }

    pub fn with_goal_tools(mut self, allow_goal_tools: bool) -> Self {
        self.allow_goal_tools = allow_goal_tools;
        self
    }

    pub fn allows_goal_tools(&self) -> bool {
        self.allow_goal_tools
    }

    pub fn with_goal_usage_tracking(mut self, track_goal_usage: bool) -> Self {
        self.track_goal_usage = track_goal_usage;
        self
    }

    pub fn tracks_goal_usage(&self) -> bool {
        self.track_goal_usage
    }

    pub fn with_wait_for_background_workflows(mut self, wait: bool) -> Self {
        self.options.wait_for_background_workflows = wait;
        self
    }

    pub fn with_session_completed_event(mut self, emit: bool) -> Self {
        self.emit_session_completed = emit;
        self
    }

    pub fn with_permission_handler(
        mut self,
        handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    ) -> Self {
        self.permission_handler = Some(handler);
        self
    }

    pub fn with_approval_handler(
        mut self,
        handler: Arc<dyn RuntimeApprovalHandler + Send + Sync>,
    ) -> Self {
        self.approval_handler = Some(handler);
        self
    }

    pub fn with_user_input_handler(
        mut self,
        handler: Arc<dyn RuntimeUserInputHandler + Send + Sync>,
    ) -> Self {
        self.user_input_handler = Some(handler);
        self
    }

    pub fn with_mcp_elicitation_handler(
        mut self,
        handler: Arc<dyn McpElicitationHandler + Send + Sync>,
    ) -> Self {
        self.mcp_elicitation_handler = Some(handler);
        self
    }

    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = Some(observer);
        self
    }

    pub fn with_continuation(mut self, continuation: RuntimeTurnContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    pub fn with_existing_turn_prompt(mut self) -> Self {
        self.resumes_existing_turn = true;
        self
    }

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    pub fn with_generation_handlers<F>(mut self, factory: F) -> Self
    where
        F: Fn(GenerationFence, CancelToken) -> HostedGenerationHandlers + Send + Sync + 'static,
    {
        self.generation_handler_factory = Some(Arc::new(factory));
        self
    }

    fn prepare_main_session_task(&mut self, registry: &TaskRegistry) -> Result<(), String> {
        let Some(description) = self.task_description.as_ref() else {
            return Ok(());
        };
        if let Some(task_id) = self.task_id.as_deref() {
            registry.mark_running(task_id)?;
            self.main_session_task_id = Some(task_id.to_string());
            return Ok(());
        }

        let task = registry.create_main_session(description.clone());
        if let Err(error) = registry.mark_running(&task.id) {
            let _ = registry.fail(
                &task.id,
                format!("failed to start main-session task: {error}"),
            );
            return Err(error);
        }
        self.task_id = Some(task.id.clone());
        self.main_session_task_id = Some(task.id);
        Ok(())
    }

    fn prepare_background_continuation(&mut self, registry: &TaskRegistry) -> Result<(), String> {
        let HostedOperationKind::BackgroundContinuation { task_id } = &self.operation_kind else {
            return Ok(());
        };
        let continuation =
            crate::background_turn::take_approved_background_turn_continuation(registry, task_id)?
                .ok_or_else(|| {
                    format!(
                        "background task {task_id} has no approved provider response to continue"
                    )
                })?;
        self.usage_credit = registry
            .get(task_id)
            .and_then(|task| task.usage)
            .unwrap_or_default();
        self.continuation = Some(continuation.into_runtime_turn_continuation());
        self.resumes_existing_turn = true;
        self.task_id = Some(task_id.clone());
        self.main_session_task_id = Some(task_id.clone());
        Ok(())
    }

    pub fn thread_turn_request(&self, generation: &GenerationContext) -> ThreadTurnRequest {
        let prompt_placement = if self.resumes_existing_turn || generation.resumes_existing_turn {
            ThreadTurnPromptPlacement::ExistingTurn
        } else if self.backtrack_target {
            ThreadTurnPromptPlacement::BacktrackableUser
        } else {
            ThreadTurnPromptPlacement::PinnedUser
        };
        let tool_mode = if self.allow_goal_tools {
            ThreadTurnToolMode::Goal
        } else {
            ThreadTurnToolMode::Standard
        };
        let mut request = ThreadTurnRequest::new(self.prompt.clone())
            .with_prompt_placement(prompt_placement)
            .with_tool_mode(tool_mode)
            .with_options(self.options)
            .with_session_completed_event(
                self.envelope == HostedOperationEnvelope::Turn && self.emit_session_completed,
            )
            .with_steer_handle(generation.steer_handle.clone());
        if let Some(handler) = generation
            .handlers
            .approval_handler
            .clone()
            .or_else(|| self.approval_handler.clone())
        {
            request = request.with_approval_handler(handler);
        }
        if let Some(handler) = generation
            .handlers
            .permission_handler
            .clone()
            .or_else(|| self.permission_handler.clone())
        {
            request = request.with_permission_handler(handler);
        }
        if let Some(handler) = generation
            .handlers
            .user_input_handler
            .clone()
            .or_else(|| self.user_input_handler.clone())
        {
            request = request.with_threaded_user_input_handler(handler);
        }
        if let Some(handler) = generation
            .handlers
            .mcp_elicitation_handler
            .clone()
            .or_else(|| self.mcp_elicitation_handler.clone())
        {
            request = request.with_mcp_elicitation_handler(handler);
        }
        if let Some(observer) = self.event_observer.clone() {
            request = request.with_event_observer(observer);
        }
        if let Some(continuation) = self.continuation.clone() {
            request = request.with_continuation(continuation);
        }
        if let Some(control) = generation.handlers.provider_suspension_control.clone() {
            request = request.with_provider_suspension_control(control);
        }
        if let Some(task_id) = self.main_session_task_id.as_deref() {
            request = request.with_main_session_task_id(task_id);
        }
        request
    }

    fn event_observer(&self) -> Option<Arc<dyn EventObserver>> {
        self.event_observer.clone()
    }

    fn is_resumable(&self) -> bool {
        self.envelope == HostedOperationEnvelope::Turn
    }
}

#[derive(Clone)]
pub struct HostedWorkflowRequest {
    name: String,
    args: Option<Value>,
    config: Option<RunConfig>,
    tool_use_id: Option<String>,
    event_observer: Option<Arc<dyn EventObserver>>,
}

impl HostedWorkflowRequest {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: None,
            config: None,
            tool_use_id: None,
            event_observer: None,
        }
    }

    pub fn with_args(mut self, args: Value) -> Self {
        self.args = Some(args);
        self
    }

    pub fn with_command_args(mut self, raw: &str) -> Result<Self, String> {
        self.args = Some(parse_hosted_workflow_args(raw)?);
        Ok(self)
    }

    pub fn with_config(mut self, config: RunConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_tool_use_id(mut self, tool_use_id: impl Into<String>) -> Self {
        self.tool_use_id = Some(tool_use_id.into());
        self
    }

    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = Some(observer);
        self
    }
}

impl fmt::Debug for HostedWorkflowRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostedWorkflowRequest")
            .field("name", &self.name)
            .field("args", &self.args)
            .field("tool_use_id", &self.tool_use_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct HostedWorkflowLaunch {
    task_id: String,
    run_id: String,
    workflow_name: String,
    tool_use_id: String,
    output: WorkflowOutput,
}

impl HostedWorkflowLaunch {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn workflow_name(&self) -> &str {
        &self.workflow_name
    }

    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }

    pub fn output(&self) -> &WorkflowOutput {
        &self.output
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GenerationId(u64);

impl GenerationId {
    pub fn as_u64(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GenerationFence {
    operation_id: OperationId,
    generation_id: GenerationId,
}

impl GenerationFence {
    fn initial(operation_id: OperationId) -> Self {
        Self {
            operation_id,
            generation_id: GenerationId(0),
        }
    }

    fn next(self) -> Self {
        Self {
            operation_id: self.operation_id,
            generation_id: self.generation_id.next(),
        }
    }

    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    pub fn generation_id(self) -> GenerationId {
        self.generation_id
    }

    #[cfg(test)]
    pub(crate) fn for_test(generation_id: u64) -> Self {
        Self {
            operation_id: OperationIdAllocator::new().allocate(),
            generation_id: GenerationId(generation_id),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GenerationContext {
    fence: GenerationFence,
    steer_handle: ThreadSteerHandle,
    resumes_existing_turn: bool,
    handlers: HostedGenerationHandlers,
    config: RunConfig,
}

impl GenerationContext {
    fn new(
        fence: GenerationFence,
        steer_handle: ThreadSteerHandle,
        resumes_existing_turn: bool,
        handlers: HostedGenerationHandlers,
        config: RunConfig,
    ) -> Self {
        Self {
            fence,
            steer_handle,
            resumes_existing_turn,
            handlers,
            config,
        }
    }

    pub fn fence(&self) -> GenerationFence {
        self.fence
    }

    pub fn resumes_existing_turn(&self) -> bool {
        self.resumes_existing_turn
    }

    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    pub fn drain_steer_inputs(&self) -> Vec<String> {
        self.steer_handle.drain()
    }
}

struct LegacyThreadOperationExecutor;

impl ThreadOperationExecutor for LegacyThreadOperationExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        if request.operation_kind() == &HostedOperationKind::ManualCompaction {
            let config = generation.config();
            let cwd = config.cwd.clone().unwrap_or(std::env::current_dir()?);
            let before_messages = thread.session().conversation().messages.len();
            let mut sink = EventSink::new(writer, config.output_format)
                .with_optional_observer(request.event_observer());
            sink.emit(events.context_compaction_started("manual", before_messages))?;
            let (before_messages, after_messages) =
                thread.session_mut().compact(config, &cwd, cancel);
            sink.emit(events.context_compacted(
                "manual",
                "manual",
                before_messages,
                after_messages,
                before_messages.saturating_sub(after_messages),
                "compacted context manually",
            ))?;
            return Ok(RunStatus::Success.into());
        }
        if request.operation_kind() != &HostedOperationKind::Turn
            && !matches!(
                request.operation_kind(),
                HostedOperationKind::BackgroundContinuation { .. }
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime executor received an unsupported operation kind",
            ));
        }
        thread
            .run_request_with_event_factory_and_cancel_outcome(
                generation.config(),
                &request.thread_turn_request(generation),
                writer,
                events,
                cancel.clone(),
            )
            .map(|outcome| match outcome {
                crate::controller::ThreadTurnOutcome::Completed {
                    status,
                    background_workflows,
                } => ThreadOperationOutcome::Completed {
                    status,
                    background_workflows,
                },
                crate::controller::ThreadTurnOutcome::ProviderSuspended {
                    suspension,
                    background_workflows,
                } => ThreadOperationOutcome::ProviderSuspended {
                    suspension,
                    background_workflows,
                },
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeHostError {
    HostUnavailable,
    ThreadUnavailable,
    MailboxFull { owner: &'static str },
    ResponseChannelClosed { owner: &'static str },
    OperationActive { operation_id: OperationId },
    ThreadStartFailed { message: String },
    WorkflowLaunchFailed { message: String },
    RuntimeStartFailed { message: String },
    ThreadActorPanicked { thread_id: String, message: String },
    SupervisorPanicked,
}

impl fmt::Display for RuntimeHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => formatter.write_str("runtime host is unavailable"),
            Self::ThreadUnavailable => formatter.write_str("runtime thread is unavailable"),
            Self::MailboxFull { owner } => write!(formatter, "{owner} command mailbox is full"),
            Self::ResponseChannelClosed { owner } => {
                write!(formatter, "{owner} response channel closed")
            }
            Self::OperationActive { operation_id } => {
                write!(formatter, "operation {operation_id:?} is already active")
            }
            Self::ThreadStartFailed { message } => {
                write!(formatter, "failed to start runtime thread: {message}")
            }
            Self::WorkflowLaunchFailed { message } => {
                write!(formatter, "failed to launch workflow: {message}")
            }
            Self::RuntimeStartFailed { message } => {
                write!(formatter, "failed to start runtime host: {message}")
            }
            Self::ThreadActorPanicked { thread_id, message } => {
                write!(
                    formatter,
                    "runtime thread actor {thread_id} panicked: {message}"
                )
            }
            Self::SupervisorPanicked => formatter.write_str("runtime host supervisor panicked"),
        }
    }
}

impl std::error::Error for RuntimeHostError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterruptOperationResult {
    Requested {
        generation: GenerationFence,
    },
    AlreadyRequested {
        generation: GenerationFence,
    },
    Stale {
        requested_operation_id: OperationId,
        active: GenerationFence,
    },
    Idle {
        requested_operation_id: OperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeOperationResult {
    Queued {
        generation: GenerationFence,
    },
    AlreadyQueued {
        generation: GenerationFence,
    },
    NotInterrupted {
        generation: GenerationFence,
    },
    NotResumable {
        generation: GenerationFence,
    },
    Stale {
        requested_operation_id: OperationId,
        active: GenerationFence,
    },
    Idle {
        requested_operation_id: OperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SteerOperationResult {
    Accepted {
        generation: GenerationFence,
    },
    Rejected {
        requested_operation_id: OperationId,
        active: Option<GenerationFence>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GenerationAdmissionResult {
    Accepted {
        generation: GenerationFence,
    },
    Rejected {
        requested: GenerationFence,
        active: Option<GenerationFence>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationPhase {
    Running,
    Interrupted,
    ResumeQueued,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeThreadState {
    Idle,
    Running {
        generation: GenerationFence,
        phase: GenerationPhase,
    },
    Unavailable,
}

pub struct RuntimeThreadStartRequest {
    config: RunConfig,
    title: String,
    preloaded: Option<SessionTranscript>,
    mcp_registry: Option<McpRegistry>,
}

impl RuntimeThreadStartRequest {
    pub fn new(config: RunConfig, title: impl Into<String>) -> Self {
        Self {
            config,
            title: title.into(),
            preloaded: None,
            mcp_registry: None,
        }
    }

    pub fn with_preloaded(mut self, preloaded: SessionTranscript) -> Self {
        self.preloaded = Some(preloaded);
        self
    }

    pub fn with_mcp_registry(mut self, mcp_registry: McpRegistry) -> Self {
        self.mcp_registry = Some(mcp_registry);
        self
    }

    fn start(self) -> io::Result<RuntimeThread> {
        match self.mcp_registry {
            Some(mcp_registry) => RuntimeThread::start_with_preloaded_and_mcp_registry(
                &self.config,
                self.title,
                self.preloaded,
                mcp_registry,
            ),
            None => RuntimeThread::start_with_preloaded(&self.config, self.title, self.preloaded),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeThreadMutation {
    SetModel(Option<String>),
    AddPinnedContext(String),
    ReplaceGoalContext(String),
    ReplaceSkillContext(Option<String>),
}

impl RuntimeThreadMutation {
    fn apply(self, thread: &mut RuntimeThread) {
        match self {
            Self::SetModel(model) => thread.session_mut().set_model(model.as_deref()),
            Self::AddPinnedContext(content) => thread.session_mut().add_pinned_context(content),
            Self::ReplaceGoalContext(content) => {
                thread.session_mut().replace_goal_context(content);
            }
            Self::ReplaceSkillContext(content) => {
                thread.session_mut().replace_skill_context(content);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeThreadSnapshot {
    thread_id: String,
    session_id: Option<String>,
    conversation: Conversation,
    usage_totals: UsageTotals,
    completion_error: Option<String>,
    has_active_workflows: bool,
    active_task_id: Option<String>,
}

impl RuntimeThreadSnapshot {
    fn from_thread(thread: &RuntimeThread, usage_totals: UsageTotals) -> Self {
        Self {
            thread_id: thread.thread_id().to_string(),
            session_id: thread.session().session_id().map(str::to_string),
            conversation: thread.session().conversation().clone(),
            usage_totals,
            completion_error: thread.session().completion_error().map(str::to_string),
            has_active_workflows: thread.session().has_active_workflows(),
            active_task_id: thread
                .lifecycle()
                .active_task()
                .map(|task| task.id().to_string()),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn messages(&self) -> &[Message] {
        &self.conversation.messages
    }

    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub fn usage_totals(&self) -> UsageTotals {
        self.usage_totals
    }

    pub fn completion_error(&self) -> Option<&str> {
        self.completion_error.as_deref()
    }

    pub fn has_active_workflows(&self) -> bool {
        self.has_active_workflows
    }

    pub fn active_task_id(&self) -> Option<&str> {
        self.active_task_id.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationOutcome {
    Completed(RunStatus),
    Backgrounded {
        task_id: String,
    },
    ExecutionFailed {
        kind: io::ErrorKind,
        message: String,
    },
    Panicked {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTerminal {
    operation_id: OperationId,
    outcome: OperationOutcome,
}

impl OperationTerminal {
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn outcome(&self) -> &OperationOutcome {
        &self.outcome
    }
}

#[derive(Clone)]
pub struct OperationCompletion {
    state: Arc<OperationCompletionState>,
}

struct OperationCompletionState {
    terminal: Mutex<Option<OperationTerminal>>,
    completed: Condvar,
}

impl OperationCompletion {
    fn new() -> Self {
        Self {
            state: Arc::new(OperationCompletionState {
                terminal: Mutex::new(None),
                completed: Condvar::new(),
            }),
        }
    }

    pub fn try_terminal(&self) -> Option<OperationTerminal> {
        self.lock_terminal().clone()
    }

    pub fn wait(&self) -> OperationTerminal {
        let mut terminal = self.lock_terminal();
        loop {
            if let Some(terminal) = terminal.clone() {
                return terminal;
            }
            terminal = self
                .state
                .completed
                .wait(terminal)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Option<OperationTerminal> {
        let terminal = self.lock_terminal();
        if terminal.is_some() {
            return terminal.clone();
        }
        let (terminal, _) = self
            .state
            .completed
            .wait_timeout_while(terminal, timeout, |terminal| terminal.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        terminal.clone()
    }

    fn complete(&self, terminal: OperationTerminal) -> bool {
        let mut current = self.lock_terminal();
        if current.is_some() {
            return false;
        }
        *current = Some(terminal);
        self.state.completed.notify_all();
        true
    }

    fn lock_terminal(&self) -> MutexGuard<'_, Option<OperationTerminal>> {
        self.state
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct OperationHandle {
    operation_id: OperationId,
    initial_generation: GenerationFence,
    thread: RuntimeThreadHandle,
    completion: OperationCompletion,
}

impl OperationHandle {
    pub fn id(&self) -> OperationId {
        self.operation_id
    }

    pub fn initial_generation(&self) -> GenerationFence {
        self.initial_generation
    }

    pub fn thread_id(&self) -> &str {
        self.thread.thread_id()
    }

    pub fn completion(&self) -> OperationCompletion {
        self.completion.clone()
    }

    pub fn interrupt(&self) -> Result<InterruptOperationResult, RuntimeHostError> {
        self.thread.interrupt_operation(self.operation_id)
    }

    pub fn resume(&self) -> Result<ResumeOperationResult, RuntimeHostError> {
        self.thread.resume_operation(self.operation_id)
    }

    pub fn steer(
        &self,
        input: impl Into<String>,
    ) -> Result<SteerOperationResult, RuntimeHostError> {
        self.thread.steer_operation(self.operation_id, input)
    }

    pub fn admit_generation(
        &self,
        generation: GenerationFence,
    ) -> Result<GenerationAdmissionResult, RuntimeHostError> {
        self.thread.admit_generation(generation)
    }

    pub fn wait(&self) -> OperationTerminal {
        self.completion.wait()
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Option<OperationTerminal> {
        self.completion.wait_timeout(timeout)
    }
}

impl fmt::Debug for OperationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationHandle")
            .field("thread_id", &self.thread_id())
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct RuntimeThreadHandle {
    thread_id: String,
    session_id: Option<String>,
    startup_warnings: Arc<Vec<String>>,
    task_registry: TaskRegistry,
    mcp_registry: McpRegistry,
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
}

impl RuntimeThreadHandle {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn startup_warnings(&self) -> &[String] {
        self.startup_warnings.as_slice()
    }

    pub fn task_registry(&self) -> TaskRegistry {
        self.task_registry.clone()
    }

    pub fn mcp_registry(&self) -> McpRegistry {
        self.mcp_registry.clone()
    }

    pub fn start_turn<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: io::Write + Send + 'static,
    {
        self.start_turn_inner(
            request,
            Box::new(PassthroughHostedOperationWriter::new(writer)),
            None,
        )
    }

    pub fn start_turn_with_config<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
        config: RunConfig,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: io::Write + Send + 'static,
    {
        self.start_turn_inner(
            request,
            Box::new(PassthroughHostedOperationWriter::new(writer)),
            Some(config),
        )
    }

    pub fn start_turn_with_output<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: HostedOperationWriter,
    {
        self.start_turn_inner(request, Box::new(writer), None)
    }

    pub fn start_turn_with_config_and_output<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
        config: RunConfig,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: HostedOperationWriter,
    {
        self.start_turn_inner(request, Box::new(writer), Some(config))
    }

    fn start_turn_inner(
        &self,
        request: HostedTurnRequest,
        writer: Box<dyn HostedOperationWriter>,
        config: Option<RunConfig>,
    ) -> Result<OperationHandle, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::StartTurn {
            request: Box::new(request),
            writer,
            config: config.map(Box::new),
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn launch_workflow(
        &self,
        request: HostedWorkflowRequest,
    ) -> Result<HostedWorkflowLaunch, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::LaunchWorkflow {
            request: Box::new(request),
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn interrupt_operation(
        &self,
        operation_id: OperationId,
    ) -> Result<InterruptOperationResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::InterruptOperation {
            operation_id,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn resume_operation(
        &self,
        operation_id: OperationId,
    ) -> Result<ResumeOperationResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ResumeOperation {
            operation_id,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn steer_operation(
        &self,
        operation_id: OperationId,
        input: impl Into<String>,
    ) -> Result<SteerOperationResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::SteerOperation {
            operation_id,
            input: input.into(),
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn admit_generation(
        &self,
        generation: GenerationFence,
    ) -> Result<GenerationAdmissionResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::AdmitGeneration {
            generation,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn state(&self) -> Result<RuntimeThreadState, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ReadState { reply: reply_tx })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn snapshot(&self) -> Result<RuntimeThreadSnapshot, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ReadSnapshot { reply: reply_tx })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn mutate(&self, mutation: RuntimeThreadMutation) -> Result<(), RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::MutateIdle {
            mutation,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn backtrack_last_user(&self) -> Result<Option<String>, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::BacktrackLastUser { reply: reply_tx })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn shutdown(&self) -> Result<(), RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        send_thread_shutdown(
            &self.command_tx,
            ThreadCommand::ShutdownThread {
                reply: Some(reply_tx),
            },
        )?;
        receive_reply(reply_rx, "runtime thread")?
    }

    fn try_send(&self, command: ThreadCommand) -> Result<(), RuntimeHostError> {
        match self.command_tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(RuntimeHostError::MailboxFull {
                owner: "runtime thread",
            }),
            Err(TrySendError::Closed(_)) => Err(RuntimeHostError::ThreadUnavailable),
        }
    }
}

impl fmt::Debug for RuntimeThreadHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeThreadHandle")
            .field("thread_id", &self.thread_id)
            .finish_non_exhaustive()
    }
}

pub struct RuntimeHost {
    command_tx: tokio_mpsc::Sender<HostCommand>,
    supervisor: Option<thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct RuntimeHostHandle {
    command_tx: tokio_mpsc::Sender<HostCommand>,
}

impl RuntimeHostHandle {
    pub fn start_thread(
        &self,
        config: RunConfig,
        title: impl Into<String>,
    ) -> Result<RuntimeThreadHandle, RuntimeHostError> {
        self.start_thread_with_request(RuntimeThreadStartRequest::new(config, title))
    }

    pub fn start_thread_with_request(
        &self,
        request: RuntimeThreadStartRequest,
    ) -> Result<RuntimeThreadHandle, RuntimeHostError> {
        start_thread_with_sender(&self.command_tx, request)
    }
}

impl RuntimeHost {
    pub fn start() -> Result<Self, RuntimeHostError> {
        Self::start_with_background_capacity(HOST_BACKGROUND_TASK_CAPACITY)
    }

    pub fn start_with_background_capacity(
        background_capacity: usize,
    ) -> Result<Self, RuntimeHostError> {
        Self::start_inner(Arc::new(LegacyThreadOperationExecutor), background_capacity)
    }

    pub fn start_with_executor(
        executor: Arc<dyn ThreadOperationExecutor>,
    ) -> Result<Self, RuntimeHostError> {
        Self::start_inner(executor, HOST_BACKGROUND_TASK_CAPACITY)
    }

    fn start_inner(
        executor: Arc<dyn ThreadOperationExecutor>,
        background_capacity: usize,
    ) -> Result<Self, RuntimeHostError> {
        let (command_tx, command_rx) = tokio_mpsc::channel(HOST_COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let supervisor = thread::Builder::new()
            .name("orca-runtime-host".to_string())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("orca-runtime-worker")
                    .build()
                    .map_err(|error| RuntimeHostError::RuntimeStartFailed {
                        message: error.to_string(),
                    });
                match runtime {
                    Ok(runtime) => {
                        let _ = ready_tx.send(Ok(()));
                        runtime.block_on(run_host_supervisor(
                            command_rx,
                            executor,
                            background_capacity,
                        ));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|error| RuntimeHostError::RuntimeStartFailed {
                message: error.to_string(),
            })?;

        match receive_reply(ready_rx, "runtime host") {
            Ok(Ok(())) => Ok(Self {
                command_tx,
                supervisor: Some(supervisor),
            }),
            Ok(Err(error)) | Err(error) => {
                let _ = supervisor.join();
                Err(error)
            }
        }
    }

    pub fn start_thread(
        &self,
        config: RunConfig,
        title: impl Into<String>,
    ) -> Result<RuntimeThreadHandle, RuntimeHostError> {
        self.start_thread_with_request(RuntimeThreadStartRequest::new(config, title))
    }

    pub fn start_thread_with_request(
        &self,
        request: RuntimeThreadStartRequest,
    ) -> Result<RuntimeThreadHandle, RuntimeHostError> {
        start_thread_with_sender(&self.command_tx, request)
    }

    pub fn handle(&self) -> RuntimeHostHandle {
        RuntimeHostHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    pub fn shutdown(mut self) -> Result<(), RuntimeHostError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), RuntimeHostError> {
        let Some(supervisor) = self.supervisor.take() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let send_result =
            send_host_shutdown(&self.command_tx, HostCommand::Shutdown { reply: reply_tx });
        let shutdown_result = match send_result {
            Ok(()) => receive_reply(reply_rx, "runtime host").and_then(|result| result),
            Err(error) => Err(error),
        };
        let join_result = supervisor
            .join()
            .map_err(|_| RuntimeHostError::SupervisorPanicked);
        shutdown_result.and(join_result)
    }
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

enum HostCommand {
    StartThread {
        request: Box<RuntimeThreadStartRequest>,
        reply: SyncSender<Result<RuntimeThreadHandle, RuntimeHostError>>,
    },
    Shutdown {
        reply: SyncSender<Result<(), RuntimeHostError>>,
    },
}

enum ThreadCommand {
    StartTurn {
        request: Box<HostedTurnRequest>,
        writer: Box<dyn HostedOperationWriter>,
        config: Option<Box<RunConfig>>,
        reply: SyncSender<Result<OperationHandle, RuntimeHostError>>,
    },
    LaunchWorkflow {
        request: Box<HostedWorkflowRequest>,
        reply: SyncSender<Result<HostedWorkflowLaunch, RuntimeHostError>>,
    },
    InterruptOperation {
        operation_id: OperationId,
        reply: SyncSender<Result<InterruptOperationResult, RuntimeHostError>>,
    },
    ResumeOperation {
        operation_id: OperationId,
        reply: SyncSender<Result<ResumeOperationResult, RuntimeHostError>>,
    },
    SteerOperation {
        operation_id: OperationId,
        input: String,
        reply: SyncSender<Result<SteerOperationResult, RuntimeHostError>>,
    },
    AdmitGeneration {
        generation: GenerationFence,
        reply: SyncSender<Result<GenerationAdmissionResult, RuntimeHostError>>,
    },
    ReadState {
        reply: SyncSender<Result<RuntimeThreadState, RuntimeHostError>>,
    },
    ReadSnapshot {
        reply: SyncSender<Result<RuntimeThreadSnapshot, RuntimeHostError>>,
    },
    MutateIdle {
        mutation: RuntimeThreadMutation,
        reply: SyncSender<Result<(), RuntimeHostError>>,
    },
    BacktrackLastUser {
        reply: SyncSender<Result<Option<String>, RuntimeHostError>>,
    },
    ShutdownThread {
        reply: Option<SyncSender<Result<(), RuntimeHostError>>>,
    },
}

struct ThreadActorEntry {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    join: JoinHandle<()>,
}

async fn run_host_supervisor(
    mut command_rx: tokio_mpsc::Receiver<HostCommand>,
    executor: Arc<dyn ThreadOperationExecutor>,
    background_capacity: usize,
) {
    let mut actors = HashMap::<String, ThreadActorEntry>::new();
    while let Some(command) = command_rx.recv().await {
        match command {
            HostCommand::StartThread { request, reply } => {
                let actor_config = request.config.clone();
                let started = tokio::task::spawn_blocking(move || request.start()).await;
                let thread = match started {
                    Ok(Ok(thread)) => thread,
                    Ok(Err(error)) => {
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: error.to_string(),
                        }));
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: error.to_string(),
                        }));
                        continue;
                    }
                };
                let thread_id = thread.thread_id().to_string();
                let session_id = thread.session().session_id().map(str::to_string);
                let task_registry = thread.session().task_registry().clone();
                let mcp_registry = thread.session().mcp_registry().clone();
                let startup_warnings = Arc::new(
                    thread
                        .session()
                        .mcp_registry()
                        .errors()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                );
                if actors.contains_key(&thread_id) {
                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                        message: format!("duplicate runtime thread id: {thread_id}"),
                    }));
                    continue;
                }
                let (command_tx, actor_rx) = tokio_mpsc::channel(THREAD_COMMAND_CAPACITY);
                let handle = RuntimeThreadHandle {
                    thread_id: thread_id.clone(),
                    session_id,
                    startup_warnings,
                    task_registry,
                    mcp_registry,
                    command_tx: command_tx.clone(),
                };
                let actor_handle = handle.clone();
                let actor_executor = Arc::clone(&executor);
                let join = tokio::spawn(async move {
                    ThreadActor::new(
                        thread,
                        actor_config,
                        actor_handle,
                        actor_executor,
                        background_capacity,
                    )
                    .run(actor_rx)
                    .await;
                });
                actors.insert(thread_id, ThreadActorEntry { command_tx, join });
                let _ = reply.send(Ok(handle));
            }
            HostCommand::Shutdown { reply } => {
                for actor in actors.values() {
                    let _ = actor
                        .command_tx
                        .send(ThreadCommand::ShutdownThread { reply: None })
                        .await;
                }
                let mut actor_error = None;
                for (thread_id, actor) in actors.drain() {
                    if let Err(error) = actor.join.await
                        && actor_error.is_none()
                    {
                        actor_error = Some(RuntimeHostError::ThreadActorPanicked {
                            thread_id,
                            message: error.to_string(),
                        });
                    }
                }
                let _ = reply.send(actor_error.map_or(Ok(()), Err));
                break;
            }
        }
    }
}

fn start_thread_with_sender(
    command_tx: &tokio_mpsc::Sender<HostCommand>,
    request: RuntimeThreadStartRequest,
) -> Result<RuntimeThreadHandle, RuntimeHostError> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    match command_tx.try_send(HostCommand::StartThread {
        request: Box::new(request),
        reply: reply_tx,
    }) {
        Ok(()) => receive_reply(reply_rx, "runtime host")?,
        Err(TrySendError::Full(_)) => Err(RuntimeHostError::MailboxFull {
            owner: "runtime host",
        }),
        Err(TrySendError::Closed(_)) => Err(RuntimeHostError::HostUnavailable),
    }
}

struct ThreadActor {
    state: Option<ThreadActorState>,
    config: RunConfig,
    handle: RuntimeThreadHandle,
    executor: Arc<dyn ThreadOperationExecutor>,
    operation_ids: OperationIdAllocator,
    active: Option<ActiveOperation>,
    background_tasks: HashMap<String, HostBackgroundTask>,
    background_capacity: usize,
    background_completion_tx: tokio_mpsc::UnboundedSender<String>,
    background_completion_rx: tokio_mpsc::UnboundedReceiver<String>,
    usage_ledger: RuntimeUsageLedger,
}

struct ThreadActorState {
    thread: RuntimeThread,
    events: EventFactory,
}

struct ActiveOperation {
    operation_id: OperationId,
    runtime_task_id: Option<String>,
    main_session_task_id: Option<String>,
    completion: OperationCompletion,
    request: HostedTurnRequest,
    config: RunConfig,
    steer_handle: ThreadSteerHandle,
    resume_queued: bool,
    generation: ActiveGeneration,
}

struct ActiveGeneration {
    context: GenerationContext,
    cancel: CancelToken,
    join: JoinHandle<OperationTaskResult>,
}

struct OperationTaskResult {
    state: ThreadActorState,
    writer: Box<dyn HostedOperationWriter>,
    outcome: GenerationTaskOutcome,
    usage_delta: UsageTotals,
}

enum GenerationTaskOutcome {
    Executed(ThreadOperationOutcome),
    ExecutionFailed {
        kind: io::ErrorKind,
        message: String,
    },
    Panicked {
        message: String,
    },
}

struct HostBackgroundTask {
    cancel: CancelToken,
    join: JoinHandle<()>,
}

struct ProviderBackgroundTaskContext {
    task_registry: TaskRegistry,
    history_writer: Option<crate::history::SessionWriter>,
    observer: Option<Arc<dyn EventObserver>>,
    events: EventFactory,
    model: Option<String>,
    task_id: String,
    usage_ledger: RuntimeUsageLedger,
}

struct WorkflowBackgroundTaskContext {
    task_registry: TaskRegistry,
    observer: Option<Arc<dyn EventObserver>>,
    events: EventFactory,
}

#[derive(Clone, Debug)]
struct RuntimeUsageLedger {
    totals: Arc<Mutex<UsageTotals>>,
}

impl RuntimeUsageLedger {
    fn new(totals: UsageTotals) -> Self {
        Self {
            totals: Arc::new(Mutex::new(totals)),
        }
    }

    fn add(&self, usage: UsageTotals) -> UsageTotals {
        let mut totals = self
            .totals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *totals = add_usage_totals(*totals, usage);
        *totals
    }

    fn totals(&self) -> UsageTotals {
        *self
            .totals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ThreadActor {
    fn new(
        thread: RuntimeThread,
        config: RunConfig,
        handle: RuntimeThreadHandle,
        executor: Arc<dyn ThreadOperationExecutor>,
        background_capacity: usize,
    ) -> Self {
        let usage_ledger = RuntimeUsageLedger::new(thread.session().aggregate_usage_totals());
        let events = thread.event_factory();
        let (background_completion_tx, background_completion_rx) = tokio_mpsc::unbounded_channel();
        Self {
            state: Some(ThreadActorState { thread, events }),
            config,
            handle,
            executor,
            operation_ids: OperationIdAllocator::new(),
            active: None,
            background_tasks: HashMap::new(),
            background_capacity,
            background_completion_tx,
            background_completion_rx,
            usage_ledger,
        }
    }

    async fn run(mut self, mut command_rx: tokio_mpsc::Receiver<ThreadCommand>) {
        loop {
            let Some(mut active) = self.active.take() else {
                tokio::select! {
                    biased;
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            self.shutdown_background_tasks().await;
                            break;
                        };
                        if let ThreadCommand::ShutdownThread { reply } = command {
                            self.shutdown_background_tasks().await;
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                            break;
                        }
                        self.handle_idle_command(command);
                    }
                    task_id = self.background_completion_rx.recv(), if !self.background_tasks.is_empty() => {
                        if let Some(task_id) = task_id {
                            self.reap_background_task(&task_id).await;
                        }
                    }
                }
                continue;
            };

            tokio::select! {
                biased;
                command = command_rx.recv() => {
                    match command {
                        Some(ThreadCommand::ShutdownThread { reply }) => {
                            active.generation.cancel.cancel();
                            let result = (&mut active.generation.join).await;
                            self.finish_generation(active, result, false);
                            self.shutdown_background_tasks().await;
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                            break;
                        }
                        Some(command) => {
                            self.handle_running_command(command, &mut active);
                            self.active = Some(active);
                        }
                        None => {
                            active.generation.cancel.cancel();
                            let result = (&mut active.generation.join).await;
                            self.finish_generation(active, result, false);
                            self.shutdown_background_tasks().await;
                            break;
                        }
                    }
                }
                result = &mut active.generation.join => {
                    self.finish_generation(active, result, true);
                }
                task_id = self.background_completion_rx.recv(), if !self.background_tasks.is_empty() => {
                    if let Some(task_id) = task_id {
                        self.reap_background_task(&task_id).await;
                    }
                    self.active = Some(active);
                }
            }
        }
    }

    fn handle_idle_command(&mut self, command: ThreadCommand) {
        match command {
            ThreadCommand::StartTurn {
                request,
                writer,
                config,
                reply,
            } => {
                let Some(mut state) = self.state.take() else {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                    return;
                };
                let operation_id = self.operation_ids.allocate();
                let initial_generation = GenerationFence::initial(operation_id);
                let completion = OperationCompletion::new();
                let mut request = *request;
                let config = config
                    .map(|config| *config)
                    .unwrap_or_else(|| self.config.clone());
                if let Err(error) =
                    request.prepare_background_continuation(state.thread.session().task_registry())
                {
                    self.state = Some(state);
                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed { message: error }));
                    return;
                }
                if let Err(error) =
                    request.prepare_main_session_task(state.thread.session().task_registry())
                {
                    self.state = Some(state);
                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                        message: format!("failed to prepare main-session task: {error}"),
                    }));
                    return;
                }
                if let Some(task_id) = request.task_id.as_deref() {
                    state
                        .thread
                        .lifecycle_mut()
                        .start_task_with_id(RuntimeTaskKind::Agent, task_id);
                }
                let runtime_task_id = state
                    .thread
                    .lifecycle()
                    .active_task()
                    .map(|task| task.id().to_string());
                let main_session_task_id = request.main_session_task_id.clone();
                let steer_handle = ThreadSteerHandle::default();
                let generation = self.spawn_generation(
                    state,
                    &request,
                    writer,
                    GenerationContext::new(
                        initial_generation,
                        steer_handle.clone(),
                        request.resumes_existing_turn,
                        HostedGenerationHandlers::default(),
                        config.clone(),
                    ),
                );
                self.active = Some(ActiveOperation {
                    operation_id,
                    runtime_task_id,
                    main_session_task_id,
                    completion: completion.clone(),
                    request,
                    config,
                    steer_handle,
                    resume_queued: false,
                    generation,
                });
                let _ = reply.send(Ok(OperationHandle {
                    operation_id,
                    initial_generation,
                    thread: self.handle.clone(),
                    completion,
                }));
            }
            ThreadCommand::LaunchWorkflow { request, reply } => {
                let result = self.launch_hosted_workflow(*request);
                let _ = reply.send(result);
            }
            ThreadCommand::InterruptOperation {
                operation_id,
                reply,
            } => {
                let _ = reply.send(Ok(InterruptOperationResult::Idle {
                    requested_operation_id: operation_id,
                }));
            }
            ThreadCommand::ResumeOperation {
                operation_id,
                reply,
            } => {
                let _ = reply.send(Ok(ResumeOperationResult::Idle {
                    requested_operation_id: operation_id,
                }));
            }
            ThreadCommand::SteerOperation {
                operation_id,
                reply,
                ..
            } => {
                let _ = reply.send(Ok(SteerOperationResult::Rejected {
                    requested_operation_id: operation_id,
                    active: None,
                }));
            }
            ThreadCommand::AdmitGeneration { generation, reply } => {
                let _ = reply.send(Ok(GenerationAdmissionResult::Rejected {
                    requested: generation,
                    active: None,
                }));
            }
            ThreadCommand::ReadState { reply } => {
                let state = if self.state.is_some() {
                    RuntimeThreadState::Idle
                } else {
                    RuntimeThreadState::Unavailable
                };
                let _ = reply.send(Ok(state));
            }
            ThreadCommand::ReadSnapshot { reply } => {
                let result = self
                    .state
                    .as_ref()
                    .map(|state| {
                        RuntimeThreadSnapshot::from_thread(
                            &state.thread,
                            self.usage_ledger.totals(),
                        )
                    })
                    .ok_or(RuntimeHostError::ThreadUnavailable);
                let _ = reply.send(result);
            }
            ThreadCommand::MutateIdle { mutation, reply } => {
                let result = self
                    .state
                    .as_mut()
                    .ok_or(RuntimeHostError::ThreadUnavailable)
                    .map(|state| mutation.apply(&mut state.thread));
                let _ = reply.send(result);
            }
            ThreadCommand::BacktrackLastUser { reply } => {
                let result = self
                    .state
                    .as_mut()
                    .ok_or(RuntimeHostError::ThreadUnavailable)
                    .map(|state| state.thread.session_mut().backtrack_last_user());
                let _ = reply.send(result);
            }
            ThreadCommand::ShutdownThread { .. } => unreachable!("shutdown handled by actor loop"),
        }
    }

    fn handle_running_command(&self, command: ThreadCommand, active: &mut ActiveOperation) {
        let generation = active.generation.context.fence();
        match command {
            ThreadCommand::StartTurn { reply, .. } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::LaunchWorkflow { reply, .. } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::InterruptOperation {
                operation_id,
                reply,
            } => {
                let result = if operation_id != active.operation_id {
                    InterruptOperationResult::Stale {
                        requested_operation_id: operation_id,
                        active: generation,
                    }
                } else if active.generation.cancel.is_cancelled() {
                    InterruptOperationResult::AlreadyRequested { generation }
                } else {
                    active.generation.cancel.cancel();
                    InterruptOperationResult::Requested { generation }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::ResumeOperation {
                operation_id,
                reply,
            } => {
                let result = if operation_id != active.operation_id {
                    ResumeOperationResult::Stale {
                        requested_operation_id: operation_id,
                        active: generation,
                    }
                } else if !active.request.is_resumable() {
                    ResumeOperationResult::NotResumable { generation }
                } else if !active.generation.cancel.is_cancelled() {
                    ResumeOperationResult::NotInterrupted { generation }
                } else if active.resume_queued {
                    ResumeOperationResult::AlreadyQueued { generation }
                } else {
                    active.resume_queued = true;
                    ResumeOperationResult::Queued { generation }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::SteerOperation {
                operation_id,
                input,
                reply,
            } => {
                let accepts = operation_id == active.operation_id
                    && !active.generation.join.is_finished()
                    && !active.generation.cancel.is_cancelled()
                    && !active.resume_queued;
                let result = if accepts {
                    active.steer_handle.push(input);
                    SteerOperationResult::Accepted { generation }
                } else {
                    SteerOperationResult::Rejected {
                        requested_operation_id: operation_id,
                        active: Some(generation),
                    }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::AdmitGeneration {
                generation: requested,
                reply,
            } => {
                let accepts = requested == generation
                    && !active.generation.join.is_finished()
                    && !active.generation.cancel.is_cancelled()
                    && !active.resume_queued;
                let result = if accepts {
                    GenerationAdmissionResult::Accepted { generation }
                } else {
                    GenerationAdmissionResult::Rejected {
                        requested,
                        active: Some(generation),
                    }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::ReadState { reply } => {
                let phase = if active.resume_queued {
                    GenerationPhase::ResumeQueued
                } else if active.generation.cancel.is_cancelled() {
                    GenerationPhase::Interrupted
                } else {
                    GenerationPhase::Running
                };
                let _ = reply.send(Ok(RuntimeThreadState::Running { generation, phase }));
            }
            ThreadCommand::ReadSnapshot { reply } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::MutateIdle { reply, .. } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::BacktrackLastUser { reply } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::ShutdownThread { .. } => unreachable!("shutdown handled by actor loop"),
        }
    }

    fn spawn_generation(
        &self,
        state: ThreadActorState,
        request: &HostedTurnRequest,
        mut writer: Box<dyn HostedOperationWriter>,
        mut context: GenerationContext,
    ) -> ActiveGeneration {
        let executor = Arc::clone(&self.executor);
        let task_request = request.clone();
        let cancel = CancelToken::new();
        if let Some(factory) = request.generation_handler_factory.as_ref() {
            context.handlers = factory(context.fence(), cancel.clone());
        }
        let task_context = context.clone();
        let task_cancel = cancel.clone();
        let usage_credit = if context.fence().generation_id().as_u64() == 0 {
            request.usage_credit
        } else {
            UsageTotals::default()
        };
        let join = tokio::task::spawn_blocking(move || {
            let mut state = state;
            let started_at = Instant::now();
            let usage_before = state.thread.session().aggregate_usage_totals();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_hosted_operation(
                    executor.as_ref(),
                    &mut state.thread,
                    &mut state.events,
                    &task_request,
                    &task_context,
                    writer.as_mut(),
                    &task_cancel,
                )
            }));
            let outcome = match outcome {
                Ok(Ok(outcome)) => GenerationTaskOutcome::Executed(outcome),
                Ok(Err(error)) => GenerationTaskOutcome::ExecutionFailed {
                    kind: error.kind(),
                    message: error.to_string(),
                },
                Err(payload) => GenerationTaskOutcome::Panicked {
                    message: panic_message(payload),
                },
            };
            let usage_after = state.thread.session().aggregate_usage_totals();
            let usage_delta = subtract_usage_totals(
                usage_totals_delta(usage_before, usage_after),
                usage_credit,
            );
            account_goal_usage_for_generation(
                &state,
                &task_request,
                usage_delta,
                started_at.elapsed().as_secs() as i64,
            );
            OperationTaskResult {
                state,
                writer,
                outcome,
                usage_delta,
            }
        });
        ActiveGeneration {
            context,
            cancel,
            join,
        }
    }

    fn finish_generation(
        &mut self,
        mut active: ActiveOperation,
        result: Result<OperationTaskResult, tokio::task::JoinError>,
        allow_resume: bool,
    ) {
        let outcome = match result {
            Ok(mut result) => {
                self.usage_ledger.add(result.usage_delta);
                let background_error = match &mut result.outcome {
                    GenerationTaskOutcome::Executed(outcome) => {
                        let required = outcome
                            .background_workflow_count()
                            .saturating_add(usize::from(outcome.suspends_provider()));
                        if let Err(error) = self.ensure_background_capacity(required) {
                            let workflows = outcome.take_background_workflows();
                            cancel_and_join_background_workflows(
                                result.state.thread.session().task_registry(),
                                &result.state.events,
                                active.request.event_observer(),
                                workflows,
                            );
                            Some(error)
                        } else {
                            let workflows = outcome.take_background_workflows();
                            self.spawn_workflow_background_tasks(
                                result.state.thread.session().task_registry().clone(),
                                &result.state.events,
                                active.request.event_observer(),
                                workflows,
                            );
                            None
                        }
                    }
                    GenerationTaskOutcome::ExecutionFailed { .. }
                    | GenerationTaskOutcome::Panicked { .. } => None,
                };
                if let Some(error) = background_error {
                    let _ = result.writer.finish_generation(true);
                    self.state = Some(result.state);
                    OperationOutcome::ExecutionFailed {
                        kind: error.kind(),
                        message: error.to_string(),
                    }
                } else {
                    let replace_generation = allow_resume
                        && active.resume_queued
                        && active.request.is_resumable()
                        && matches!(
                            result.outcome,
                            GenerationTaskOutcome::Executed(ThreadOperationOutcome::Completed {
                                status: RunStatus::Cancelled,
                                ..
                            })
                        );
                    if replace_generation {
                        if let Err(error) = result.writer.finish_generation(false) {
                            self.state = Some(result.state);
                            OperationOutcome::ExecutionFailed {
                                kind: error.kind(),
                                message: error.to_string(),
                            }
                        } else {
                            let _ = active.steer_handle.drain();
                            if let Some(task_id) = active.runtime_task_id.as_deref() {
                                result
                                    .state
                                    .thread
                                    .lifecycle_mut()
                                    .start_task_with_id(RuntimeTaskKind::Agent, task_id);
                            }
                            let context = GenerationContext::new(
                                active.generation.context.fence().next(),
                                active.steer_handle.clone(),
                                true,
                                HostedGenerationHandlers::default(),
                                active.config.clone(),
                            );
                            active.generation = self.spawn_generation(
                                result.state,
                                &active.request,
                                result.writer,
                                context,
                            );
                            active.resume_queued = false;
                            self.active = Some(active);
                            return;
                        }
                    } else {
                        let writer_error = result.writer.finish_generation(true).err();
                        if let Some(error) = writer_error {
                            self.state = Some(result.state);
                            OperationOutcome::ExecutionFailed {
                                kind: error.kind(),
                                message: error.to_string(),
                            }
                        } else {
                            match result.outcome {
                                GenerationTaskOutcome::Executed(
                                    ThreadOperationOutcome::Completed { status, .. },
                                ) => {
                                    self.state = Some(result.state);
                                    OperationOutcome::Completed(status)
                                }
                                GenerationTaskOutcome::Executed(
                                    ThreadOperationOutcome::ProviderSuspended {
                                        suspension, ..
                                    },
                                ) => match self.spawn_provider_background_task(
                                    &active,
                                    &mut result.state,
                                    suspension,
                                ) {
                                    Ok(task_id) => {
                                        self.state = Some(result.state);
                                        OperationOutcome::Backgrounded { task_id }
                                    }
                                    Err(error) => {
                                        self.state = Some(result.state);
                                        OperationOutcome::ExecutionFailed {
                                            kind: error.kind(),
                                            message: error.to_string(),
                                        }
                                    }
                                },
                                GenerationTaskOutcome::ExecutionFailed { kind, message } => {
                                    self.state = Some(result.state);
                                    OperationOutcome::ExecutionFailed { kind, message }
                                }
                                GenerationTaskOutcome::Panicked { message } => {
                                    self.state = Some(result.state);
                                    OperationOutcome::Panicked { message }
                                }
                            }
                        }
                    }
                }
            }
            Err(error) => OperationOutcome::Panicked {
                message: error.to_string(),
            },
        };
        let completed = active.completion.complete(OperationTerminal {
            operation_id: active.operation_id,
            outcome,
        });
        debug_assert!(completed, "operation terminal must complete exactly once");
    }

    fn launch_hosted_workflow(
        &mut self,
        request: HostedWorkflowRequest,
    ) -> Result<HostedWorkflowLaunch, RuntimeHostError> {
        self.ensure_background_capacity(1).map_err(|error| {
            RuntimeHostError::WorkflowLaunchFailed {
                message: error.to_string(),
            }
        })?;
        let Some(mut state) = self.state.take() else {
            return Err(RuntimeHostError::ThreadUnavailable);
        };
        let result = self.launch_hosted_workflow_with_state(&mut state, request);
        self.state = Some(state);
        result
    }

    fn launch_hosted_workflow_with_state(
        &mut self,
        state: &mut ThreadActorState,
        request: HostedWorkflowRequest,
    ) -> Result<HostedWorkflowLaunch, RuntimeHostError> {
        let HostedWorkflowRequest {
            name,
            args,
            config,
            tool_use_id,
            event_observer,
        } = request;
        let tool_use_id =
            tool_use_id.unwrap_or_else(|| format!("workflow-{}", uuid::Uuid::new_v4()));
        let tool_request = orca_core::tool_types::ToolRequest {
            id: tool_use_id.clone(),
            name: orca_core::tool_types::ToolName::Workflow,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some(name.clone()),
            raw_arguments: serde_json::to_string(&WorkflowInput {
                name: Some(name.clone()),
                args: args.clone(),
                ..Default::default()
            })
            .ok(),
        };
        observe_runtime_event(
            event_observer.as_deref(),
            state.events.tool_call_requested(&tool_request),
        );

        let config = config.unwrap_or_else(|| self.config.clone());
        if !config.workflows.enabled {
            let message = "workflows are disabled".to_string();
            let failed =
                orca_core::tool_types::ToolResult::failed(&tool_request, message.clone(), None);
            observe_runtime_event(
                event_observer.as_deref(),
                state.events.tool_call_completed(&failed),
            );
            return Err(RuntimeHostError::WorkflowLaunchFailed { message });
        }
        let cwd = config
            .cwd
            .clone()
            .unwrap_or(std::env::current_dir().map_err(|error| {
                RuntimeHostError::WorkflowLaunchFailed {
                    message: error.to_string(),
                }
            })?);
        let task_registry = state.thread.session().task_registry().clone();
        let session_dir = cwd
            .join(".orca")
            .join("workflow-sessions")
            .join(task_registry.session_id());
        let runner = WorkflowRunner::new(config, task_registry.clone(), session_dir);
        let launch = match runner.launch_background(WorkflowLaunchRequest::from(WorkflowInput {
            name: Some(name),
            args,
            ..Default::default()
        })) {
            Ok(launch) => launch,
            Err(error) => {
                let message = error.to_string();
                let failed =
                    orca_core::tool_types::ToolResult::failed(&tool_request, message.clone(), None);
                observe_runtime_event(
                    event_observer.as_deref(),
                    state.events.tool_call_completed(&failed),
                );
                return Err(RuntimeHostError::WorkflowLaunchFailed { message });
            }
        };
        let response = HostedWorkflowLaunch {
            task_id: launch.task_id.clone(),
            run_id: launch.run_id.clone(),
            workflow_name: launch.workflow_name.clone(),
            tool_use_id: tool_use_id.clone(),
            output: launch.output.clone(),
        };
        observe_runtime_event(
            event_observer.as_deref(),
            state.events.workflow_started(
                &launch.task_id,
                &launch.run_id,
                &launch.workflow_name,
                &launch.phases,
            ),
        );
        if let Some(task) = task_registry
            .list()
            .into_iter()
            .find(|task| task.id == launch.task_id)
        {
            observe_runtime_event(
                event_observer.as_deref(),
                state.events.task_status_updated(&task),
            );
        }
        if let Ok(output) = serde_json::to_string(&launch.output) {
            let completed =
                orca_core::tool_types::ToolResult::completed(&tool_request, output, false);
            observe_runtime_event(
                event_observer.as_deref(),
                state.events.tool_call_completed(&completed),
            );
        }

        self.spawn_workflow_background_tasks(
            task_registry,
            &state.events,
            event_observer,
            RuntimeBackgroundWorkflows::from_vec(vec![BackgroundWorkflowRun::new(
                launch,
                Some(tool_use_id),
            )]),
        );
        Ok(response)
    }

    fn ensure_background_capacity(&self, additional: usize) -> io::Result<()> {
        if self.background_tasks.len().saturating_add(additional) > self.background_capacity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "runtime host background task capacity exhausted ({})",
                    self.background_capacity
                ),
            ));
        }
        Ok(())
    }

    fn spawn_workflow_background_tasks(
        &mut self,
        task_registry: TaskRegistry,
        events: &EventFactory,
        observer: Option<Arc<dyn EventObserver>>,
        workflows: RuntimeBackgroundWorkflows,
    ) {
        for workflow in workflows.into_inner() {
            let task_id = workflow.task_id.clone();
            let completion_task_id = task_id.clone();
            let completion_tx = self.background_completion_tx.clone();
            let cancel = CancelToken::new();
            let worker_cancel = cancel.clone();
            let context = WorkflowBackgroundTaskContext {
                task_registry: task_registry.clone(),
                observer: observer.clone(),
                events: events.fork(),
            };
            let join = tokio::task::spawn_blocking(move || {
                let panic_registry = context.task_registry.clone();
                let panic_observer = context.observer.clone();
                let mut panic_events = context.events.fork();
                let panic_task_id = workflow.task_id.clone();
                let panic_run_id = workflow.run_id.clone();
                let panic_workflow_name = workflow.workflow_name.clone();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_workflow_background_task(workflow, context, &worker_cancel)
                }));
                if let Err(payload) = outcome {
                    let message = panic_message(payload);
                    let _ = panic_registry.fail(&panic_task_id, message.clone());
                    emit_workflow_task_status(
                        panic_observer.as_deref(),
                        &mut panic_events,
                        &panic_registry,
                        &panic_task_id,
                    );
                    observe_runtime_event(
                        panic_observer.as_deref(),
                        panic_events.workflow_failed(
                            &panic_task_id,
                            &panic_run_id,
                            &panic_workflow_name,
                            None,
                            &message,
                        ),
                    );
                }
                let _ = completion_tx.send(completion_task_id);
            });
            self.background_tasks
                .insert(task_id, HostBackgroundTask { cancel, join });
        }
    }

    fn spawn_provider_background_task(
        &mut self,
        active: &ActiveOperation,
        state: &mut ThreadActorState,
        suspension: Box<RuntimeProviderSuspension>,
    ) -> io::Result<String> {
        let task_id = active
            .main_session_task_id
            .clone()
            .ok_or_else(|| io::Error::other("provider suspension requires a main-session task"))?;
        if self.background_tasks.len() >= self.background_capacity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "runtime host background task capacity exhausted ({})",
                    self.background_capacity
                ),
            ));
        }

        let task_registry = state.thread.session().task_registry().clone();
        task_registry
            .mark_backgrounded(&task_id)
            .map_err(io::Error::other)?;
        emit_task_status_update(
            active.request.event_observer(),
            &mut state.events,
            &task_registry,
            &task_id,
        )?;

        let history_writer = state.thread.session_mut().writer_mut().cloned();
        let context = ProviderBackgroundTaskContext {
            task_registry,
            history_writer,
            observer: active.request.event_observer(),
            events: state.events.fork(),
            model: suspension.model().map(str::to_string),
            task_id: task_id.clone(),
            usage_ledger: self.usage_ledger.clone(),
        };
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let completion_tx = self.background_completion_tx.clone();
        let completion_task_id = task_id.clone();
        let join = tokio::task::spawn_blocking(move || {
            let panic_registry = context.task_registry.clone();
            let panic_task_id = context.task_id.clone();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_provider_background_task(*suspension, context, &worker_cancel)
            }));
            if let Err(payload) = outcome {
                let _ = panic_registry.apply_main_session_terminal_update(
                    &panic_task_id,
                    MainSessionTerminalUpdate::Failed {
                        error: panic_message(payload),
                    },
                    None,
                );
            }
            let _ = completion_tx.send(completion_task_id);
        });
        self.background_tasks
            .insert(task_id.clone(), HostBackgroundTask { cancel, join });
        Ok(task_id)
    }

    async fn reap_background_task(&mut self, task_id: &str) {
        if let Some(task) = self.background_tasks.remove(task_id) {
            let _ = task.join.await;
        }
    }

    async fn shutdown_background_tasks(&mut self) {
        for task in self.background_tasks.values() {
            task.cancel.cancel();
        }
        for (_, task) in self.background_tasks.drain() {
            let _ = task.join.await;
        }
    }
}

fn cancel_and_join_background_workflows(
    task_registry: &TaskRegistry,
    events: &EventFactory,
    observer: Option<Arc<dyn EventObserver>>,
    workflows: RuntimeBackgroundWorkflows,
) {
    for workflow in workflows.into_inner() {
        let cancel = CancelToken::new();
        cancel.cancel();
        run_workflow_background_task(
            workflow,
            WorkflowBackgroundTaskContext {
                task_registry: task_registry.clone(),
                observer: observer.clone(),
                events: events.fork(),
            },
            &cancel,
        );
    }
}

fn parse_hosted_workflow_args(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed).map_err(|error| error.to_string())?;
        if value.is_object() {
            return Ok(value);
        }
        return Err("workflow args JSON must be an object".to_string());
    }

    let mut object = serde_json::Map::new();
    for part in trimmed.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("workflow arg `{part}` must use key=value"));
        };
        if key.trim().is_empty() {
            return Err("workflow arg key cannot be empty".to_string());
        }
        let parsed_value =
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()));
        object.insert(key.to_string(), parsed_value);
    }
    Ok(Value::Object(object))
}

fn run_workflow_background_task(
    workflow: BackgroundWorkflowRun,
    context: WorkflowBackgroundTaskContext,
    cancel: &CancelToken,
) {
    let BackgroundWorkflowRun {
        task_id,
        run_id,
        workflow_name,
        handle,
        tool_use_id,
        ..
    } = workflow;
    let mut events = context.events;
    let mut stop_requested = false;
    while !handle.is_finished() {
        if cancel.is_cancelled() && !stop_requested {
            let _ = context.task_registry.request_stop(&task_id);
            stop_requested = true;
        }
        observe_runtime_event(
            context.observer.as_deref(),
            events.workflow_tasks_updated(&context.task_registry.list()),
        );
        thread::sleep(WORKFLOW_BACKGROUND_POLL_INTERVAL);
    }

    let joined = handle.join();
    emit_workflow_task_status(
        context.observer.as_deref(),
        &mut events,
        &context.task_registry,
        &task_id,
    );
    let task_status = context.task_registry.get(&task_id).map(|task| task.status);
    match joined {
        Ok(Ok(result)) if task_status == Some(TaskStatus::Completed) => {
            observe_runtime_event(
                context.observer.as_deref(),
                events.workflow_completed(&task_id, &run_id, &workflow_name),
            );
            observe_runtime_event(
                context.observer.as_deref(),
                events.workflow_result_available(
                    &task_id,
                    &run_id,
                    &workflow_name,
                    tool_use_id.as_deref(),
                    "completed",
                    &result.status_line,
                ),
            );
        }
        Ok(Ok(result)) => {
            observe_runtime_event(
                context.observer.as_deref(),
                events.workflow_failed(
                    &task_id,
                    &run_id,
                    &workflow_name,
                    tool_use_id.as_deref(),
                    &result.status_line,
                ),
            );
        }
        Ok(Err(error)) => {
            observe_runtime_event(
                context.observer.as_deref(),
                events.workflow_failed(
                    &task_id,
                    &run_id,
                    &workflow_name,
                    tool_use_id.as_deref(),
                    &error.to_string(),
                ),
            );
        }
        Err(_) => {
            let _ = context
                .task_registry
                .fail(&task_id, "workflow thread panicked".to_string());
            emit_workflow_task_status(
                context.observer.as_deref(),
                &mut events,
                &context.task_registry,
                &task_id,
            );
            observe_runtime_event(
                context.observer.as_deref(),
                events.workflow_failed(
                    &task_id,
                    &run_id,
                    &workflow_name,
                    tool_use_id.as_deref(),
                    "workflow thread panicked",
                ),
            );
        }
    }
}

fn emit_workflow_task_status(
    observer: Option<&dyn EventObserver>,
    events: &mut EventFactory,
    task_registry: &TaskRegistry,
    task_id: &str,
) {
    let tasks = task_registry.list();
    if let Some(task) = tasks.iter().find(|task| task.id == task_id) {
        observe_runtime_event(observer, events.task_status_updated(task));
    }
    observe_runtime_event(observer, events.workflow_tasks_updated(&tasks));
}

fn run_hosted_operation(
    executor: &dyn ThreadOperationExecutor,
    thread: &mut RuntimeThread,
    events: &mut EventFactory,
    request: &HostedTurnRequest,
    generation: &GenerationContext,
    writer: &mut (dyn io::Write + Send),
    cancel: &CancelToken,
) -> io::Result<ThreadOperationOutcome> {
    match request.envelope {
        HostedOperationEnvelope::Turn => {
            executor.run_turn(thread, request, generation, events, writer, cancel)
        }
        HostedOperationEnvelope::HeadlessSession => run_headless_session(
            executor, thread, events, request, generation, writer, cancel,
        ),
    }
}

fn run_headless_session(
    executor: &dyn ThreadOperationExecutor,
    thread: &mut RuntimeThread,
    events: &mut EventFactory,
    request: &HostedTurnRequest,
    generation: &GenerationContext,
    writer: &mut (dyn io::Write + Send),
    cancel: &CancelToken,
) -> io::Result<ThreadOperationOutcome> {
    let config = generation.config();
    let cwd_path = config.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = cwd_path.display().to_string();
    let mut sink = EventSink::new(writer, config.output_format)
        .with_optional_observer(request.event_observer());
    sink.emit(events.session_started(
        &cwd,
        config.approval_mode.as_str(),
        config.provider.as_str(),
        config.verifier.as_deref(),
    ))?;
    if let Err(error) = thread.session().hooks().run(
        HookEvent::SessionStart,
        HookContext {
            cwd: &cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        sink.emit(events.error(&format!("session_start hook failed: {error}")))?;
    }

    let outcome = executor.run_turn(
        thread,
        request,
        generation,
        events,
        sink.writer_mut(),
        cancel,
    )?;

    let status = match &outcome {
        ThreadOperationOutcome::Completed { status, .. } => *status,
        ThreadOperationOutcome::ProviderSuspended { .. } => RunStatus::Success,
    };
    if let Err(error) = thread.session().hooks().run(
        HookEvent::SessionEnd,
        HookContext {
            cwd: &cwd,
            session_status: Some(status.as_str()),
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        sink.emit(events.error(&format!("session_end hook failed: {error}")))?;
    }
    if matches!(outcome, ThreadOperationOutcome::Completed { .. }) {
        sink.emit(events.session_completed(status))?;
    }
    Ok(outcome)
}

fn run_provider_background_task(
    mut suspension: RuntimeProviderSuspension,
    mut context: ProviderBackgroundTaskContext,
    cancel: &CancelToken,
) {
    let mut events = context.events;
    let mut buffered_steps = Vec::new();
    let mut cancelled = false;
    let mut response = None;
    let mut disconnected = false;

    loop {
        if !cancelled
            && (cancel.is_cancelled() || context.task_registry.is_cancelled(&context.task_id))
        {
            cancelled = true;
            suspension.cancel();
        }
        match suspension.recv_timeout(Duration::from_millis(10)) {
            Ok(RuntimeProviderSuspensionEvent::Step(step)) => {
                if cancelled {
                    continue;
                }
                if background_task_is_foregrounded(&context.task_registry, &context.task_id) {
                    emit_provider_steps(
                        context.observer.as_deref(),
                        &mut events,
                        buffered_steps.drain(..),
                    );
                    emit_provider_steps(
                        context.observer.as_deref(),
                        &mut events,
                        std::iter::once(step),
                    );
                } else if background_step_is_visible(&step) {
                    buffered_steps.push(step);
                }
            }
            Ok(RuntimeProviderSuspensionEvent::Completed(completed)) => {
                response = Some(completed);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                disconnected = true;
                break;
            }
        }
    }

    let usage = response
        .as_ref()
        .and_then(|response| provider_response_usage_totals(response, context.model.as_deref()));
    if let Some(usage) = usage {
        let totals = context.usage_ledger.add(usage);
        observe_runtime_event(context.observer.as_deref(), events.usage_updated(totals));
    }
    let was_backgrounded = context
        .task_registry
        .get(&context.task_id)
        .is_some_and(|task| task.is_backgrounded);
    let mut status = RunStatus::Failed;
    let mut error = None;

    if cancelled {
        status = RunStatus::Cancelled;
        let _ = context.task_registry.stop_with_usage(
            &context.task_id,
            status.as_str().to_string(),
            usage,
        );
    } else if disconnected {
        error = Some("provider stream ended without a response".to_string());
        let _ = context.task_registry.apply_main_session_terminal_update(
            &context.task_id,
            MainSessionTerminalUpdate::Failed {
                error: error.clone().expect("background provider error"),
            },
            usage,
        );
    } else if let Some(response) = response {
        if provider_response_requires_approval(&response) {
            status = RunStatus::ApprovalRequired;
            let _ = context
                .task_registry
                .approval_required_for_pending_provider_response_with_usage(
                    &context.task_id,
                    status.as_str().to_string(),
                    response,
                    usage,
                );
        } else if let Some(provider_error) = provider_response_error(&response) {
            error = Some(provider_error);
            let _ = context.task_registry.apply_main_session_terminal_update(
                &context.task_id,
                MainSessionTerminalUpdate::Failed {
                    error: error.clone().expect("background provider error"),
                },
                usage,
            );
        } else {
            status = RunStatus::Success;
            let _ = context.task_registry.apply_main_session_terminal_update(
                &context.task_id,
                MainSessionTerminalUpdate::Completed {
                    result: status.as_str().to_string(),
                },
                usage,
            );
        }
    }

    if let Some(writer) = &mut context.history_writer {
        let _ = writer.append_background_task_provider_response(
            &context.task_id,
            status.as_str(),
            error.as_deref(),
            usage,
        );
    }
    emit_task_status_update(
        context.observer.clone(),
        &mut events,
        &context.task_registry,
        &context.task_id,
    )
    .ok();
    if !was_backgrounded {
        emit_provider_steps(context.observer.as_deref(), &mut events, buffered_steps);
        if let Some(error) = error.as_deref() {
            observe_runtime_event(context.observer.as_deref(), events.error(error));
        }
        observe_runtime_event(
            context.observer.as_deref(),
            events.session_completed(status),
        );
    }
}

fn emit_task_status_update(
    observer: Option<Arc<dyn EventObserver>>,
    events: &mut EventFactory,
    task_registry: &TaskRegistry,
    task_id: &str,
) -> io::Result<()> {
    let task = task_registry
        .list()
        .into_iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "background task not found"))?;
    observe_event(observer.as_deref(), events.task_status_updated(&task))
}

fn background_task_is_foregrounded(task_registry: &TaskRegistry, task_id: &str) -> bool {
    task_registry
        .get(task_id)
        .is_some_and(|task| task.status == TaskStatus::Running && !task.is_backgrounded)
}

fn background_step_is_visible(step: &ProviderStep) -> bool {
    matches!(
        step,
        ProviderStep::ReasoningDelta(_)
            | ProviderStep::MessageDelta(_)
            | ProviderStep::ToolCallProgress(_)
    )
}

fn emit_provider_steps(
    observer: Option<&dyn EventObserver>,
    events: &mut EventFactory,
    steps: impl IntoIterator<Item = ProviderStep>,
) {
    for step in steps {
        match step {
            ProviderStep::ReasoningDelta(text) => {
                observe_runtime_event(observer, events.assistant_reasoning_delta(&text));
            }
            ProviderStep::MessageDelta(text) => {
                observe_runtime_event(observer, events.assistant_message_delta(&text));
            }
            ProviderStep::ToolCallProgress(progress) => {
                observe_runtime_event(observer, events.tool_call_progress(&progress));
            }
            _ => {}
        }
    }
}

fn observe_runtime_event(observer: Option<&dyn EventObserver>, event: EventDraft) {
    let _ = observe_event(observer, event);
}

fn provider_response_requires_approval(response: &ProviderResponse) -> bool {
    !response.tool_calls.is_empty()
        || response
            .steps
            .iter()
            .any(|step| matches!(step, ProviderStep::ToolCall(_)))
}

fn provider_response_error(response: &ProviderResponse) -> Option<String> {
    response.steps.iter().find_map(|step| match step {
        ProviderStep::Error(error) => Some(error.clone()),
        _ => None,
    })
}

fn provider_response_usage_totals(
    response: &ProviderResponse,
    model: Option<&str>,
) -> Option<UsageTotals> {
    let usage = response.usage.filter(|usage| !usage.is_empty())?;
    let mut tracker = crate::cost::CostTracker::new(model);
    Some(tracker.add_usage(usage))
}

fn account_goal_usage_for_generation(
    state: &ThreadActorState,
    request: &HostedTurnRequest,
    usage_delta: UsageTotals,
    elapsed_secs: i64,
) {
    if !request.tracks_goal_usage() {
        return;
    }
    let Some(session_id) = state.thread.session().session_id() else {
        return;
    };
    // Best-effort ledger telemetry: swallow accounting failures so the
    // generation task keeps its happy path (this file does not use tracing).
    let _ = crate::goals::GoalStore::load_default().account_usage(
        session_id,
        usage_delta.total_tokens() as i64,
        elapsed_secs,
    );
}

fn usage_totals_delta(before: UsageTotals, after: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        cache_tokens: after.cache_tokens.saturating_sub(before.cache_tokens),
        estimated_cost_usd: (after.estimated_cost_usd - before.estimated_cost_usd).max(0.0),
    }
}

fn add_usage_totals(left: UsageTotals, right: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cache_tokens: left.cache_tokens.saturating_add(right.cache_tokens),
        estimated_cost_usd: left.estimated_cost_usd + right.estimated_cost_usd,
    }
}

fn subtract_usage_totals(total: UsageTotals, credit: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: total.input_tokens.saturating_sub(credit.input_tokens),
        output_tokens: total.output_tokens.saturating_sub(credit.output_tokens),
        cache_tokens: total.cache_tokens.saturating_sub(credit.cache_tokens),
        estimated_cost_usd: (total.estimated_cost_usd - credit.estimated_cost_usd).max(0.0),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "operation executor panicked".to_string()
}

fn receive_reply<T>(
    receiver: mpsc::Receiver<T>,
    owner: &'static str,
) -> Result<T, RuntimeHostError> {
    receiver
        .recv()
        .map_err(|_| RuntimeHostError::ResponseChannelClosed { owner })
}

fn send_host_shutdown(
    sender: &tokio_mpsc::Sender<HostCommand>,
    mut command: HostCommand,
) -> Result<(), RuntimeHostError> {
    loop {
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Closed(_)) => return Err(RuntimeHostError::HostUnavailable),
        }
    }
}

fn send_thread_shutdown(
    sender: &tokio_mpsc::Sender<ThreadCommand>,
    mut command: ThreadCommand,
) -> Result<(), RuntimeHostError> {
    loop {
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Closed(_)) => return Err(RuntimeHostError::ThreadUnavailable),
        }
    }
}
