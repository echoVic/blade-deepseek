use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::{CancelToken, OperationId, OperationIdAllocator};
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::{Conversation, Message};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventDraft, EventFactory, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink, observe_event};
use orca_core::hook_types::HookEvent;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::task_types::TaskStatus;
use orca_core::thread_identity::TurnId;
use orca_core::thread_item_projection::ModelResponseIdentity;
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
use crate::goal_actor::{GoalContinuationStatus, GoalRuntimeHandle};
use crate::hooks::HookContext;
use crate::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequestHandler, RuntimeTaskKind,
    RuntimeUserInputHandler, ThreadSteerHandle,
};
use crate::provider_stream::{
    RuntimeProviderSuspension, RuntimeProviderSuspensionControl, RuntimeProviderSuspensionEvent,
};
use crate::runtime_pending_interaction::RuntimePendingInteractionStore;
use crate::runtime_surface as surface;
use crate::tasks::{MainSessionTerminalUpdate, TaskRegistry};
use crate::thread::RuntimeThread;
use crate::thread_store::{SessionMeta, SessionStore, SessionTranscript};
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
    turn_id: TurnId,
    prompt: String,
    options: ControllerRunOptions,
    operation_kind: HostedOperationKind,
    task_description: Option<String>,
    backtrack_target: bool,
    allow_goal_tools: bool,
    track_goal_usage: bool,
    goal_turn_origin: orca_core::goal_runtime::GoalTurnOrigin,
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
    pending_interactions: Option<RuntimePendingInteractionStore>,
    usage_credit: UsageTotals,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostedOperationKind {
    Turn,
    GoalRun,
    ManualCompaction,
    BackgroundContinuation { task_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalContinuationAdmission {
    Admit {
        reason: orca_core::goal_runtime::GoalContinuationReason,
    },
    Reject {
        code: GoalContinuationRejectCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalContinuationRejectCode {
    GoalInactive,
    Cancelled,
    NonSuccessfulTurn,
    QueuedUserInput,
    PendingInteraction,
    ActiveWorkflow,
    PlanMode,
    DuplicateAdmission,
    PendingVerification,
    BudgetLimited,
    RuntimeUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GoalContinuationPreflight {
    cancelled: bool,
    successful_turn: bool,
    queued_user_input: bool,
    pending_interaction: bool,
    active_workflow: bool,
    plan_mode: bool,
    duplicate_admission: bool,
}

fn goal_continuation_preflight(
    input: GoalContinuationPreflight,
) -> Option<GoalContinuationAdmission> {
    let reject = |code, message: &str| GoalContinuationAdmission::Reject {
        code,
        message: message.to_string(),
    };
    if input.cancelled {
        return Some(reject(
            GoalContinuationRejectCode::Cancelled,
            "goal continuation rejected because the operation was cancelled",
        ));
    }
    if !input.successful_turn {
        return Some(reject(
            GoalContinuationRejectCode::NonSuccessfulTurn,
            "goal continuation rejected after a non-successful outer turn",
        ));
    }
    if input.queued_user_input {
        return Some(reject(
            GoalContinuationRejectCode::QueuedUserInput,
            "goal continuation yielded to queued user input",
        ));
    }
    if input.pending_interaction {
        return Some(reject(
            GoalContinuationRejectCode::PendingInteraction,
            "goal continuation waits for a pending user interaction",
        ));
    }
    if input.active_workflow {
        return Some(reject(
            GoalContinuationRejectCode::ActiveWorkflow,
            "goal continuation waits for active workflow ownership",
        ));
    }
    if input.plan_mode {
        return Some(reject(
            GoalContinuationRejectCode::PlanMode,
            "goal continuation is disabled while the runtime is in plan mode",
        ));
    }
    if input.duplicate_admission {
        return Some(reject(
            GoalContinuationRejectCode::DuplicateAdmission,
            "goal continuation was already admitted for this generation fence",
        ));
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostedOperationEnvelope {
    Turn,
    HeadlessSession,
}

impl HostedTurnRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            turn_id: TurnId::new(),
            prompt: prompt.into(),
            options: ControllerRunOptions::default(),
            operation_kind: HostedOperationKind::Turn,
            task_description: None,
            backtrack_target: false,
            allow_goal_tools: false,
            track_goal_usage: false,
            goal_turn_origin: orca_core::goal_runtime::GoalTurnOrigin::User,
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
            pending_interactions: None,
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

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = turn_id;
        self
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

    pub fn with_goal_turn_origin(
        mut self,
        origin: orca_core::goal_runtime::GoalTurnOrigin,
    ) -> Self {
        self.goal_turn_origin = origin;
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

    pub fn with_pending_interactions(
        mut self,
        pending_interactions: RuntimePendingInteractionStore,
    ) -> Self {
        self.pending_interactions = Some(pending_interactions);
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
        self.turn_id = continuation.response.identity.turn_id.clone();
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
            .with_turn_id(self.turn_id.clone())
            .with_prompt_placement(prompt_placement)
            .with_tool_mode(tool_mode)
            .with_goal_turn_origin(self.goal_turn_origin)
            .with_options(self.options)
            .with_session_completed_event(
                self.envelope == HostedOperationEnvelope::Turn
                    && self.emit_session_completed
                    && self.operation_kind != HostedOperationKind::GoalRun,
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

    pub fn event_observer(&self) -> Option<Arc<dyn EventObserver>> {
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
            && request.operation_kind() != &HostedOperationKind::GoalRun
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
            .run_request_with_event_factory_and_cancel_outcome_unbound(
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
    GoalControlFailed { message: String },
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
            Self::GoalControlFailed { message } => {
                write!(formatter, "goal control command failed: {message}")
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
pub enum PauseGoalRunResult {
    Requested {
        generation: GenerationFence,
    },
    AlreadyRequested {
        generation: GenerationFence,
    },
    NotGoalRun {
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
    prepared_record_meta: Option<SessionMeta>,
}

impl RuntimeThreadStartRequest {
    pub fn new(config: RunConfig, title: impl Into<String>) -> Self {
        Self {
            config,
            title: title.into(),
            preloaded: None,
            mcp_registry: None,
            prepared_record_meta: None,
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
        let mcp_registry = self
            .mcp_registry
            .unwrap_or_else(|| orca_mcp::initialize_registry(&self.config.mcp_servers));
        RuntimeThread::start_with_prepared_history(
            &self.config,
            self.title,
            self.preloaded,
            mcp_registry,
            self.prepared_record_meta,
        )
    }

    fn prepare(mut self) -> Result<PreparedRuntimeThreadStart, RuntimeHostError> {
        let (thread_id, path) = match self.config.history_mode.clone() {
            HistoryMode::Record => {
                let cwd = self.config.cwd.clone().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
                });
                let meta = SessionStore::new().create_meta_with_permissions(
                    &cwd,
                    self.config.provider.as_str(),
                    self.config.model.as_history_value(),
                    &self.title,
                    self.config.active_permission_profile.clone(),
                    self.config.approval_mode,
                    self.config.permission_rules.clone(),
                    self.config.additional_working_directories.clone(),
                );
                let path =
                    crate::history::prospective_session_path(&meta.session_id, meta.created_at);
                let thread_id = meta.session_id.clone();
                self.prepared_record_meta = Some(meta);
                (thread_id, path)
            }
            HistoryMode::Resume(selector) => {
                let transcript = match self.preloaded.take() {
                    Some(transcript) => transcript,
                    None => SessionStore::new()
                        .load_session(&selector)
                        .map_err(|error| RuntimeHostError::ThreadStartFailed {
                            message: error.to_string(),
                        })?,
                };
                let thread_id = transcript.meta.session_id.clone();
                let path = transcript.path.clone();
                self.preloaded = Some(transcript);
                (thread_id, path)
            }
            _ => {
                return Ok(PreparedRuntimeThreadStart {
                    request: self,
                    surface_owner: None,
                });
            }
        };
        let Ok(raw_thread_id) = uuid::Uuid::parse_str(&thread_id) else {
            return Ok(PreparedRuntimeThreadStart {
                request: self,
                surface_owner: None,
            });
        };
        let surface_thread_id = surface::SurfaceThreadId::try_from_bytes(*raw_thread_id.as_bytes())
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("invalid surface thread identity: {error:?}"),
            })?;
        let owner_lease = surface::ExclusiveOwnerLease::acquire_thread(
            path.with_extension("surface-owner.lock"),
            path.with_extension("surface-owner.epoch"),
            surface_thread_id.clone(),
            &HostSurfaceClock::now(),
        )
        .map_err(|error| RuntimeHostError::ThreadStartFailed {
            message: format!("failed to acquire typed surface owner lease: {error:?}"),
        })?;
        Ok(PreparedRuntimeThreadStart {
            request: self,
            surface_owner: Some(PreparedSurfaceOwner {
                thread_id,
                surface_thread_id,
                path,
                owner_lease,
            }),
        })
    }
}

struct PreparedRuntimeThreadStart {
    request: RuntimeThreadStartRequest,
    surface_owner: Option<PreparedSurfaceOwner>,
}

struct PreparedSurfaceOwner {
    thread_id: String,
    surface_thread_id: surface::SurfaceThreadId,
    path: std::path::PathBuf,
    owner_lease: surface::ExclusiveOwnerLease,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeThreadMutation {
    SetModel(Option<String>),
    AddPinnedContext(String),
    ReplaceSkillContext(Option<String>),
}

impl RuntimeThreadMutation {
    fn apply(self, thread: &mut RuntimeThread) {
        match self {
            Self::SetModel(model) => thread.session_mut().set_model(model.as_deref()),
            Self::AddPinnedContext(content) => thread.session_mut().add_pinned_context(content),
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
    conversation_records: Option<Vec<crate::thread_store::StoredConversationRecord>>,
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
            conversation_records: thread.session().conversation_records(),
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

    pub(crate) fn conversation_records(
        &self,
    ) -> Option<&[crate::thread_store::StoredConversationRecord]> {
        self.conversation_records.as_deref()
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

    pub fn pause_goal(&self) -> Result<PauseGoalRunResult, RuntimeHostError> {
        self.thread.pause_goal_run(self.operation_id)
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
    surface: surface::RuntimeSurfaceHandle,
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

    pub fn surface(&self) -> surface::RuntimeSurfaceHandle {
        self.surface.clone()
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

    pub fn pause_goal_run(
        &self,
        operation_id: OperationId,
    ) -> Result<PauseGoalRunResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::PauseGoalRun {
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

    pub fn goal_runtime(&self) -> Result<GoalRuntimeHandle, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::GoalRuntime { reply: reply_tx })?;
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

    #[cfg(test)]
    fn surface_actor_probe_for_test(
        &self,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<SurfaceActorTestProbe, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::SurfaceActorTestProbe {
            operation_id,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")
    }

    pub fn shutdown(&self) -> Result<(), RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        send_thread_shutdown(
            &self.command_tx,
            ThreadCommand::ShutdownThread {
                reply: Some(reply_tx),
                reason: surface::SurfaceShutdownReason::ThreadClose,
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
    SurfaceReserveOperation {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        intent: surface::OperationRequestIntent,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::ReservedOperationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfaceAdmitReserved {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::AdmissionOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfaceCancelOperation {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::CancelOperationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfaceWaitOperationTerminal {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        reply: SyncSender<
            Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
        >,
    },
    SurfaceRetryFinalization {
        client: surface::RuntimeSurfaceClientHandle,
        token: surface::RetryFinalizationToken,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::OperationTerminalAtCursor>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    #[cfg(test)]
    SurfaceActorTestProbe {
        operation_id: surface::SurfaceOperationId,
        reply: SyncSender<SurfaceActorTestProbe>,
    },
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
    PauseGoalRun {
        operation_id: OperationId,
        reply: SyncSender<Result<PauseGoalRunResult, RuntimeHostError>>,
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
    GoalRuntime {
        reply: SyncSender<Result<GoalRuntimeHandle, RuntimeHostError>>,
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
        reason: surface::SurfaceShutdownReason,
    },
}

#[cfg(test)]
struct SurfaceActorTestProbe {
    waiter_count: usize,
    legacy_completion: Option<OperationCompletion>,
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
    let host_incarnation =
        surface::HostIncarnation::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
            .expect("generated UUID is v7");
    let mut actors = HashMap::<String, ThreadActorEntry>::new();
    while let Some(command) = command_rx.recv().await {
        match command {
            HostCommand::StartThread { request, reply } => {
                let actor_config = request.config.clone();
                let actor_title = request.title.clone();
                let prepared = tokio::task::spawn_blocking(move || request.prepare()).await;
                let prepared = match prepared {
                    Ok(Ok(prepared)) => prepared,
                    Ok(Err(error)) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: error.to_string(),
                        }));
                        continue;
                    }
                };
                if prepared
                    .surface_owner
                    .as_ref()
                    .is_some_and(|owner| actors.contains_key(&owner.thread_id))
                {
                    let thread_id = &prepared.surface_owner.as_ref().unwrap().thread_id;
                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                        message: format!("duplicate runtime thread id: {thread_id}"),
                    }));
                    continue;
                }
                let PreparedRuntimeThreadStart {
                    request,
                    surface_owner,
                } = prepared;
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
                let (surface_handle, resident_surface) = if let Some(surface_owner) = surface_owner
                {
                    match bootstrap_recorded_surface(
                        &thread,
                        &actor_config,
                        &actor_title,
                        host_incarnation.clone(),
                        command_tx.clone(),
                        surface_owner,
                    ) {
                        Ok((handle, resident)) => (handle, Some(resident)),
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            continue;
                        }
                    }
                } else {
                    (unavailable_surface_handle(host_incarnation.clone()), None)
                };
                let handle = RuntimeThreadHandle {
                    thread_id: thread_id.clone(),
                    session_id,
                    startup_warnings,
                    task_registry,
                    mcp_registry,
                    command_tx: command_tx.clone(),
                    surface: surface_handle,
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
                        resident_surface,
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
                        .send(ThreadCommand::ShutdownThread {
                            reply: None,
                            reason: surface::SurfaceShutdownReason::HostShutdown,
                        })
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

struct HostSurfaceClock {
    id: surface::HostMonotonicClockId,
    wall_ms: i64,
}

impl HostSurfaceClock {
    fn now() -> Self {
        let wall_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        Self {
            id: surface::HostMonotonicClockId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            wall_ms,
        }
    }
}

impl surface::InjectedRuntimeClock for HostSurfaceClock {
    fn clock_id(&self) -> surface::HostMonotonicClockId {
        self.id.clone()
    }

    fn monotonic_tick(&self) -> u64 {
        0
    }

    fn wall_clock_ms(&self) -> i64 {
        self.wall_ms
    }
}

struct ThreadSurfaceDispatcher {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
}

impl ThreadSurfaceDispatcher {
    fn dispatch<T>(
        &self,
        make_command: impl FnOnce(
            SyncSender<Result<T, surface::SurfaceClientCommandError>>,
        ) -> ThreadCommand,
    ) -> Result<T, surface::SurfaceClientCommandError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(make_command(reply_tx))
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        reply_rx
            .recv()
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
    }
}

impl surface::RuntimeSurfaceCommandDispatcher for ThreadSurfaceDispatcher {
    fn reserve_operation(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        intent: surface::OperationRequestIntent,
    ) -> Result<
        surface::MutationReply<surface::ReservedOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfaceReserveOperation {
            client,
            request_id,
            intent,
            reply,
        })
    }

    fn admit_reserved(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
    ) -> Result<surface::MutationReply<surface::AdmissionOutput>, surface::SurfaceClientCommandError>
    {
        self.dispatch(|reply| ThreadCommand::SurfaceAdmitReserved {
            client,
            request_id,
            operation_id,
            admission_lease_id,
            reply,
        })
    }

    fn cancel_operation(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfaceCancelOperation {
            client,
            request_id,
            operation_id,
            reply,
        })
    }

    fn wait_operation_terminal(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError> {
        self.dispatch(|reply| ThreadCommand::SurfaceWaitOperationTerminal {
            client,
            request_id,
            operation_id,
            reply,
        })
    }

    fn retry_finalization(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        token: surface::RetryFinalizationToken,
    ) -> Result<
        surface::MutationReply<surface::OperationTerminalAtCursor>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfaceRetryFinalization {
            client,
            token,
            reply,
        })
    }
}

fn unavailable_surface_handle(
    host_incarnation: surface::HostIncarnation,
) -> surface::RuntimeSurfaceHandle {
    let thread_id = surface::SurfaceThreadId::try_from_bytes(*uuid::Uuid::new_v4().as_bytes())
        .expect("generated UUID is valid");
    let authority = surface::SurfaceAttachAuthority::new(
        host_incarnation.clone(),
        thread_id.clone(),
        surface::SurfaceAttachmentRole::Tui,
        surface::NonEmptySet::try_new(BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]))
            .expect("unavailable surface grant is non-empty"),
        surface::NonEmptySet::try_new(BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]))
            .expect("unavailable surface required grant is non-empty"),
        BTreeSet::new(),
    );
    surface::RuntimeSurfaceHandle::new(host_incarnation, thread_id, authority)
}

fn bootstrap_recorded_surface(
    thread: &RuntimeThread,
    config: &RunConfig,
    title: &str,
    host_incarnation: surface::HostIncarnation,
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    surface_owner: PreparedSurfaceOwner,
) -> Result<(surface::RuntimeSurfaceHandle, ResidentSurfaceState), RuntimeHostError> {
    let materialized_path = thread.session().surface_commit_path().ok_or_else(|| {
        RuntimeHostError::ThreadStartFailed {
            message: "typed runtime surface requires recorded session history".to_string(),
        }
    })?;
    if materialized_path != surface_owner.path || thread.thread_id() != surface_owner.thread_id {
        return Err(RuntimeHostError::ThreadStartFailed {
            message: "prepared surface owner identity changed during materialization".to_string(),
        });
    }
    let raw_thread_id = uuid::Uuid::parse_str(&surface_owner.thread_id)
        .expect("prepared surface thread identity was validated");
    let thread_id = surface_owner.surface_thread_id;
    let mut incarnation_bytes = *raw_thread_id.as_bytes();
    incarnation_bytes[6] = 0x70 | (incarnation_bytes[6] & 0x0f);
    incarnation_bytes[8] = 0x80 | (incarnation_bytes[8] & 0x3f);
    let incarnation = surface::SurfaceIncarnation::try_from_bytes(incarnation_bytes)
        .expect("normalized thread UUID is v7");

    let path = surface_owner.path;
    let owner_lease = surface_owner.owner_lease;
    let current_owner_epoch = surface::ThreadOwnerEpoch::new(owner_lease.owner_epoch());
    let initial_owner_epoch = current_owner_epoch.get().saturating_sub(1).max(1);
    let snapshot = initial_surface_snapshot(
        thread_id,
        incarnation,
        surface::ThreadOwnerEpoch::new(initial_owner_epoch),
        config,
        title,
    )?;
    let ledger = surface::JsonlSurfaceCommitLedger::new(path, snapshot.cursor.clone());
    let mut coordinator = surface::RuntimeCommitCoordinator::recover_with_owned_lease(
        ledger,
        surface::SurfaceReducerState::new(snapshot),
        owner_lease,
    )
    .map_err(|error| RuntimeHostError::ThreadStartFailed {
        message: format!("failed to recover typed runtime surface: {error:?}"),
    })?;
    if current_owner_epoch.get() > initial_owner_epoch {
        let materialization = surface::MaterializationCause::ColdOwnerTakeover {
            new_incarnation: surface::SurfaceIncarnation::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .expect("generated UUID is v7"),
            new_owner_epoch: current_owner_epoch,
        };
        coordinator
            .materialize_cold_owner_takeover(&materialization)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to materialize typed surface owner: {error:?}"),
            })?;
        let operation_ids = coordinator
            .state()
            .snapshot()
            .foreground_operation
            .iter()
            .chain(coordinator.state().snapshot().queued_operations.iter())
            .chain(coordinator.state().snapshot().operation_history.iter())
            .map(|operation| operation.operation_id.clone())
            .collect::<Vec<_>>();
        for operation_id in operation_ids {
            loop {
                let before = coordinator.state().snapshot().cursor.clone();
                coordinator
                    .recover_operation(&operation_id, &materialization)
                    .map_err(|error| RuntimeHostError::ThreadStartFailed {
                        message: format!("failed to reconcile typed operation: {error:?}"),
                    })?;
                let terminal = coordinator
                    .state()
                    .snapshot()
                    .foreground_operation
                    .iter()
                    .chain(coordinator.state().snapshot().queued_operations.iter())
                    .chain(coordinator.state().snapshot().operation_history.iter())
                    .find(|operation| operation.operation_id == operation_id)
                    .is_some_and(|operation| {
                        matches!(operation.phase, surface::OperationPhase::Terminal)
                    });
                if terminal {
                    break;
                }
                if coordinator.state().snapshot().cursor == before {
                    return Err(RuntimeHostError::ThreadStartFailed {
                        message: "typed operation recovery made no durable progress".to_string(),
                    });
                }
            }
        }
    }
    let authority = surface::SurfaceAttachAuthority::new(
        host_incarnation,
        coordinator.state().snapshot().thread.thread_id.clone(),
        surface::SurfaceAttachmentRole::Tui,
        surface::NonEmptySet::try_new(BTreeSet::from([
            surface::SurfaceCapability::ReadSnapshot,
            surface::SurfaceCapability::SubmitOperation,
            surface::SurfaceCapability::ControlBoundOperation,
            surface::SurfaceCapability::RepairThread,
        ]))
        .expect("production TUI grant is non-empty"),
        surface::NonEmptySet::try_new(BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]))
            .expect("production TUI required grant is non-empty"),
        BTreeSet::new(),
    );
    let hub = surface::SurfaceHub::from_authority(
        coordinator.state().snapshot().clone(),
        authority,
        surface::SurfaceHubConfig::default(),
    )
    .map_err(|error| RuntimeHostError::ThreadStartFailed {
        message: format!("failed to create typed runtime surface: {error:?}"),
    })?
    .with_dispatcher(Arc::new(ThreadSurfaceDispatcher { command_tx }));
    coordinator.bind_surface_hub(hub.clone()).map_err(|error| {
        RuntimeHostError::ThreadStartFailed {
            message: format!("failed to bind typed runtime surface: {error:?}"),
        }
    })?;
    let terminals = recovered_surface_terminals(&coordinator);
    Ok((
        surface::RuntimeSurfaceHandle::from_hub(hub.clone()),
        ResidentSurfaceState {
            coordinator,
            hub: hub.clone(),
            terminals,
            pending_terminal_commits: HashMap::new(),
            waiters: HashMap::new(),
        },
    ))
}

fn initial_surface_snapshot(
    thread_id: surface::SurfaceThreadId,
    incarnation: surface::SurfaceIncarnation,
    owner_epoch: surface::ThreadOwnerEpoch,
    config: &RunConfig,
    title: &str,
) -> Result<surface::SurfaceSnapshot, RuntimeHostError> {
    let cwd = config.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    });
    let cwd = surface::CanonicalPath::try_new(cwd).map_err(|error| {
        RuntimeHostError::ThreadStartFailed {
            message: format!("invalid surface cwd: {error:?}"),
        }
    })?;
    let now = surface::UnixMillis::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64,
    );
    let approval_mode = match config.approval_mode {
        ApprovalMode::Suggest => surface::SurfaceApprovalMode::Suggest,
        ApprovalMode::AutoEdit => surface::SurfaceApprovalMode::AutoEdit,
        ApprovalMode::FullAuto => surface::SurfaceApprovalMode::FullAuto,
        ApprovalMode::Plan => surface::SurfaceApprovalMode::Plan,
    };
    let settings = surface::SurfaceRuntimeSettings {
        model: surface::NonEmptyText::try_new(
            config
                .model
                .as_history_value()
                .unwrap_or_else(|| "runtime-default".to_string()),
        )
        .expect("runtime model is non-empty"),
        reasoning_effort: surface::SurfaceReasoningEffort::Max,
        approval_mode,
        cwd: cwd.clone(),
        workspace_roots: vec![cwd.clone()],
        active_permission_profile: None,
        permission_rules: surface::SurfacePermissionRuleSet {
            ordered_rules: Vec::new(),
            digest: surface::Sha256Digest::new([0; 32]),
        },
        additional_working_directories: Vec::new(),
        network_permissions: surface::SurfaceNetworkPermissions {
            enabled: None,
            domains: Vec::new(),
        },
        policy_epoch: surface::PolicyEpoch::try_new(1).expect("one is a valid revision"),
    };
    let cursor = surface::SurfaceCursor {
        thread_id: thread_id.clone(),
        incarnation,
        next_seq: surface::SequenceNumber::new(0),
        source_revision: surface::CursorSourceRevision::Recorded {
            durable_revision: surface::DurableRevision::try_new(1)
                .expect("one is a valid revision"),
        },
    };
    Ok(surface::SurfaceSnapshot {
        cursor,
        thread: surface::SurfaceThreadSnapshot {
            thread_id,
            owner_epoch,
            persistence: surface::ThreadPersistence::RecordedCatalogued,
            title: surface::DisplayText::new(title),
            metadata_revision: surface::SessionMetadataRevision::try_new(1)
                .expect("one is a valid revision"),
            created_at: now,
            updated_at: now,
            cwd: cwd.clone(),
            workspace_roots: vec![cwd],
            closed: false,
        },
        foreground_operation: None,
        queued_operations: Vec::new(),
        background_operations: Vec::new(),
        operation_history: Vec::new(),
        items: Vec::new(),
        assistant_streams: Vec::new(),
        tools: Vec::new(),
        plan: surface::SurfacePlanSnapshot {
            revision: surface::PlanRevision::try_new(1).expect("one is a valid revision"),
            explanation: None,
            items: Vec::new(),
            causative_generation: None,
        },
        usage: surface::SurfaceUsageSnapshot {
            revision: surface::UsageRevision::try_new(1).expect("one is a valid revision"),
            thread_total: surface::UsageTotals {
                input_tokens: 0,
                output_tokens: 0,
                cache_tokens: 0,
                estimated_cost_usd_micros: 0,
            },
            active_operation: None,
            goal: None,
            workflow: Vec::new(),
        },
        context: surface::SurfaceContextSnapshot {
            revision: surface::ContextRevision::try_new(1).expect("one is a valid revision"),
            used_tokens: 0,
            limit_tokens: 128_000,
            compaction: surface::CompactionState::Idle,
            fragments: Vec::new(),
            provider_replay: surface::ProviderReplayHealth::None,
        },
        interactions: Vec::new(),
        tasks: Vec::new(),
        workflows: Vec::new(),
        subagents: Vec::new(),
        goal: None,
        settings: surface::SurfaceSettingsSnapshot {
            host_revision: surface::SettingsRevision::try_new(1).expect("one is a valid revision"),
            thread_revision: surface::SettingsRevision::try_new(1)
                .expect("one is a valid revision"),
            effective: settings,
            pending: None,
            frozen_generation_revision: None,
        },
        mcp_catalog: surface::SurfaceMcpCatalogSnapshot {
            revision: surface::McpCatalogRevision::try_new(1).expect("one is a valid revision"),
            servers: Vec::new(),
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            diagnostics: Vec::new(),
        },
        pinned_context: surface::SurfacePinnedContextSnapshot {
            revision: surface::PinnedContextRevision::try_new(1).expect("one is a valid revision"),
            entries: Vec::new(),
        },
        session_health: surface::SurfaceSessionHealth {
            revision: surface::SessionHealthRevision::try_new(1).expect("one is a valid revision"),
            accepting_admission: true,
            issues: Vec::new(),
            closing: false,
            closed: false,
        },
    })
}

fn recovered_surface_terminals(
    coordinator: &surface::RuntimeCommitCoordinator<'static, surface::JsonlSurfaceCommitLedger>,
) -> HashMap<surface::SurfaceOperationId, surface::OperationTerminalAtCursor> {
    let mut terminals = HashMap::new();
    let Ok(recovered) = coordinator.ledger().recover_batches() else {
        return terminals;
    };
    for batch in recovered.committed {
        for envelope in batch.events.as_slice() {
            let surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal { record }) =
                &envelope.event
            else {
                continue;
            };
            terminals.insert(
                record.operation_id.clone(),
                surface::OperationTerminalAtCursor {
                    operation_id: record.operation_id.clone(),
                    terminal: record.terminal.clone(),
                    cursor: batch.cursor_after.clone(),
                    commit_class: batch.commit_class.clone(),
                    batch_digest: batch.batch_digest.clone(),
                },
            );
        }
    }
    terminals
}

fn surface_request_text(request: &surface::SurfaceInputRequest) -> String {
    request
        .blocks
        .as_slice()
        .iter()
        .filter_map(|block| match block {
            surface::SurfaceInputRequestBlock::Text { text }
            | surface::SurfaceInputRequestBlock::EmbeddedText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolve_surface_input(request: &surface::SurfaceInputRequest) -> Option<surface::SurfaceInput> {
    let blocks = request
        .blocks
        .as_slice()
        .iter()
        .map(|block| match block {
            surface::SurfaceInputRequestBlock::Text { text } => {
                Some(surface::SurfaceInputBlock::Text { text: text.clone() })
            }
            surface::SurfaceInputRequestBlock::Binding {
                binding:
                    surface::SurfaceInputBindingRequest::ExactCatalog {
                        kind,
                        identity,
                        observed_catalog_revision,
                        observed_settings_revision,
                        label,
                    },
            } => Some(surface::SurfaceInputBlock::Binding {
                binding: surface::SurfaceInputBinding {
                    kind: *kind,
                    identity: identity.clone(),
                    observed_catalog_revision: *observed_catalog_revision,
                    observed_settings_revision: *observed_settings_revision,
                    label: label.clone(),
                },
            }),
            surface::SurfaceInputRequestBlock::Binding {
                binding: surface::SurfaceInputBindingRequest::LegacyJsonlMention { .. },
            } => None,
            surface::SurfaceInputRequestBlock::ResourceLink {
                uri,
                name,
                description,
                mime,
            } => Some(surface::SurfaceInputBlock::ResourceLink {
                uri: uri.clone(),
                name: name.clone(),
                description: description.clone(),
                mime: mime.clone(),
            }),
            surface::SurfaceInputRequestBlock::EmbeddedText {
                uri,
                mime,
                text,
                digest,
            } => Some(surface::SurfaceInputBlock::EmbeddedText {
                uri: uri.clone(),
                mime: mime.clone(),
                text: text.clone(),
                digest: digest.clone(),
            }),
        })
        .collect::<Option<Vec<_>>>()?;
    Some(surface::SurfaceInput {
        blocks: surface::NonEmptyVec::try_new(blocks).ok()?,
        canonical_text: surface::DisplayText::new(surface_request_text(request)),
        bindings_digest: surface::Sha256Digest::new([0; 32]),
    })
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
    resident_surface: ResidentSurfaceSlot,
    surface_terminal_blocked: Option<String>,
}

struct ResidentSurfaceSlot(Option<ResidentSurfaceState>);

impl std::ops::Deref for ResidentSurfaceSlot {
    type Target = ResidentSurfaceState;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("typed surface state is present after client admission")
    }
}

impl std::ops::DerefMut for ResidentSurfaceSlot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_mut()
            .expect("typed surface state is present after client admission")
    }
}

struct ResidentSurfaceState {
    coordinator: surface::RuntimeCommitCoordinator<'static, surface::JsonlSurfaceCommitLedger>,
    hub: surface::SurfaceHub,
    terminals: HashMap<surface::SurfaceOperationId, surface::OperationTerminalAtCursor>,
    pending_terminal_commits: HashMap<surface::SurfaceOperationId, PendingSurfaceTerminalCommit>,
    waiters: HashMap<
        surface::SurfaceOperationId,
        Vec<
            SyncSender<
                Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
            >,
        >,
    >,
}

struct PendingSurfaceTerminalCommit {
    batch: surface::SurfaceCommitBatch,
    value: surface::OperationTerminalAtCursor,
    failure: surface::WaitOperationTerminalResult,
    legacy_completion: Option<OperationCompletion>,
    legacy_terminal: Option<OperationTerminal>,
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
    goal_admitted_generation: Option<GenerationFence>,
    goal_control: Option<ActiveGoalControl>,
    pending_goal_pause_event: Option<PendingGoalPauseEvent>,
    generation: ActiveGeneration,
    surface_operation: Option<surface::SurfaceOperationFence>,
    surface_terminalization: Option<surface::TerminalizationCause>,
}

struct ActiveGoalControl {
    session_id: String,
    runtime: GoalRuntimeHandle,
}

struct PendingGoalPauseEvent {
    goal_id: orca_core::goal_runtime::GoalId,
    goal_run_id: Option<orca_core::goal_runtime::GoalRunId>,
    outer_turn_id: Option<orca_core::goal_runtime::GoalOuterTurnId>,
    previous_state: orca_core::goal_runtime::GoalState,
    next_state: orca_core::goal_runtime::GoalState,
    reason: orca_core::goal_runtime::GoalPauseReason,
    message: String,
    reason_code: String,
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
    response_identity: ModelResponseIdentity,
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
    fn admits_surface_client(
        &self,
        client: &surface::RuntimeSurfaceClientHandle,
        capability: surface::SurfaceCapability,
    ) -> bool {
        self.resident_surface.0.as_ref().is_some_and(|resident| {
            resident.hub.admits_client(client)
                && client.grant().capabilities.as_set().contains(&capability)
        })
    }

    fn surface_operation_batch(
        &self,
        operation_id: &surface::SurfaceOperationId,
        patches: Vec<surface::OperationPatch>,
    ) -> surface::SurfaceCommitBatch {
        self.surface_operation_batch_with_commit_id(operation_id, patches, None)
    }

    fn surface_operation_batch_with_commit_id(
        &self,
        operation_id: &surface::SurfaceOperationId,
        patches: Vec<surface::OperationPatch>,
        commit_id: Option<surface::SurfaceCommitId>,
    ) -> surface::SurfaceCommitBatch {
        let generation_scope = patches.iter().find_map(|patch| match patch {
            surface::OperationPatch::GenerationReserved { generation } => {
                Some(generation.fence.clone())
            }
            surface::OperationPatch::GenerationStarted { fence, .. }
            | surface::OperationPatch::InputBindingsResolved { fence, .. }
            | surface::OperationPatch::InputBindingsFailed { fence, .. }
            | surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn { fence, .. },
            }
            | surface::OperationPatch::ModelRouteSelected { fence, .. }
            | surface::OperationPatch::GenerationStopped { fence, .. }
            | surface::OperationPatch::GenerationTransferred { fence, .. } => Some(fence.clone()),
            _ => None,
        });
        let events = patches
            .into_iter()
            .map(|patch| {
                let scope = match &patch {
                    surface::OperationPatch::GenerationReserved { generation } => {
                        surface::SurfaceScope::Generation {
                            fence: generation.fence.clone(),
                        }
                    }
                    surface::OperationPatch::GenerationStarted { fence, .. }
                    | surface::OperationPatch::InputBindingsResolved { fence, .. }
                    | surface::OperationPatch::InputBindingsFailed { fence, .. }
                    | surface::OperationPatch::ModelRouteSelected { fence, .. }
                    | surface::OperationPatch::GenerationStopped { fence, .. }
                    | surface::OperationPatch::GenerationTransferred { fence, .. } => {
                        surface::SurfaceScope::Generation {
                            fence: fence.clone(),
                        }
                    }
                    surface::OperationPatch::AgentLoopTurnStarted { turn } => {
                        surface::SurfaceScope::Generation {
                            fence: turn.fence.clone(),
                        }
                    }
                    surface::OperationPatch::FinalizationStarted { .. }
                        if generation_scope.is_some() =>
                    {
                        surface::SurfaceScope::Generation {
                            fence: generation_scope.clone().unwrap(),
                        }
                    }
                    _ => surface::SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    },
                };
                (scope, surface::SurfaceEvent::Operation(patch))
            })
            .collect();
        self.surface_event_batch_with_commit_id(events, commit_id)
    }

    fn surface_event_batch_with_commit_id(
        &self,
        events: Vec<(surface::SurfaceScope, surface::SurfaceEvent)>,
        commit_id: Option<surface::SurfaceCommitId>,
    ) -> surface::SurfaceCommitBatch {
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let cursor_before = snapshot.cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            surface::CursorSourceRevision::Recorded { durable_revision } => {
                surface::DurableRevision::try_new(durable_revision.get() + 1)
                    .expect("surface durable revision did not exhaust")
            }
            surface::CursorSourceRevision::Ephemeral { .. } => {
                unreachable!("recorded runtime surface used ephemeral cursor")
            }
        };
        let commit_class = surface::CommitClass::Recorded {
            thread_owner_epoch: snapshot.thread.owner_epoch,
            durable_revision,
            commit_id: commit_id.unwrap_or_else(|| {
                surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7")
            }),
        };
        let events = events
            .into_iter()
            .enumerate()
            .map(|(ordinal, (scope, event))| surface::SurfaceEventEnvelope {
                ordinal: ordinal as u32,
                event_id: surface::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: commit_class.clone(),
                scope,
                event,
            })
            .collect::<Vec<_>>();
        let event_count = events.len() as u32;
        let mut batch = surface::SurfaceCommitBatch {
            cursor_after: surface::SurfaceCursor {
                next_seq: surface::SequenceNumber::new(
                    cursor_before.next_seq.get() + event_count as u64,
                ),
                source_revision: surface::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before.clone()
            },
            cursor_before,
            commit_class,
            event_count,
            batch_digest: surface::Sha256Digest::new([0; 32]),
            events: surface::NonEmptyVec::try_new(events).expect("operation batch is non-empty"),
        };
        batch.batch_digest = surface::canonical_batch_digest(&batch);
        batch
    }

    fn committed_surface_mutation<T>(
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Operation {
                    thread_id: batch.cursor_after.thread_id.clone(),
                    operation_id,
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Operation,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("operation commit has one acknowledgement"),
            },
            value,
        }
    }

    fn reserve_surface_operation(
        &mut self,
        request_id: surface::SurfaceRequestId,
        intent: surface::OperationRequestIntent,
    ) -> Result<
        surface::MutationReply<surface::ReservedOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.resident_surface.pending_terminal_commits.is_empty() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if intent.correlation != surface::OperationIngressCorrelation::TuiUser
            || intent.kind != surface::OperationKind::UserTurn
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let input_request = match (&intent.replayability, intent.input.as_ref()) {
            (surface::ReplayabilityRequest::CaptureReplayableCapsule, Some(input))
                if resolve_surface_input(input).is_some() =>
            {
                input.clone()
            }
            _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation_id =
            surface::SurfaceOperationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let reservation_sequence =
            surface::SequenceNumber::new(snapshot.queued_operations.len() as u64 + 1);
        let lease = surface::ReservationLease::new(
            surface::SurfaceAdmissionLeaseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            operation_id.clone(),
            reservation_sequence,
            self.resident_surface
                .hub
                .authority()
                .host_incarnation()
                .clone(),
            surface::MonotonicInstant {
                clock_id: surface::HostMonotonicClockId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                tick: surface::MonotonicTick::new(0),
            },
        );
        let origin = surface::OperationOrigin::TuiUser;
        let settings = &snapshot.settings;
        let replayability = surface::Replayability::Replayable {
            capsule_digest: surface::Sha256Digest::new([0; 32]),
            request: Some(input_request),
            request_digest: Some(surface::Sha256Digest::new([0; 32])),
            cwd: settings.effective.cwd.clone(),
            workspace_roots: settings.effective.workspace_roots.clone(),
            settings_revision: settings.thread_revision,
            policy_epoch: settings.effective.policy_epoch,
            tool_schema_digest: surface::Sha256Digest::new([0; 32]),
        };
        let operation = surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: request_id.clone(),
            intent: surface::OperationIntent {
                origin,
                kind: intent.kind,
                initial_replayability: replayability,
                busy_disposition: surface::BusyDisposition::Queue,
                interrupt_settlement: surface::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: surface::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: settings.thread_revision,
                policy_epoch: settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: surface::Sha256Digest::new([0; 32]),
                settings_receipt: surface::OperationSettingsPreparationReceipt::Current {
                    settings_revision: settings.thread_revision,
                    policy_epoch: settings.effective.policy_epoch,
                },
            },
            phase: surface::OperationPhase::Requested,
            reservation: lease.clone(),
            ready_for_admission: false,
            initial_logical_turn_id: None,
            initial_input_item_id: None,
            generations: Vec::new(),
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        };
        let batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::Requested { operation }],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &batch,
            surface::ReservedOperationOutput {
                operation_id,
                lease,
                requested_cursor: batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    fn admit_surface_operation(
        &mut self,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
    ) -> Result<surface::MutationReply<surface::AdmissionOutput>, surface::SurfaceClientCommandError>
    {
        if !self.resident_surface.pending_terminal_commits.is_empty() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let operation = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if operation.reservation.lease_id != admission_lease_id {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if operation.intent.origin != surface::OperationOrigin::TuiUser
            || operation.intent.kind != surface::OperationKind::UserTurn
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let (input_request, request_digest) = match &operation.intent.initial_replayability {
            surface::Replayability::Replayable {
                request: Some(request),
                request_digest: Some(request_digest),
                ..
            } => (request.clone(), request_digest.clone()),
            _ => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
        };
        let resolved_input = resolve_surface_input(&input_request)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let logical_turn_id = TurnId::new();
        let fence = surface::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id: surface::SurfaceGenerationId::new(0),
        };
        let input_item_id = surface::SurfaceItemId::new();
        let presentation = surface::SurfaceInputPresentation::Visible {
            text: resolved_input.canonical_text.clone(),
        };
        let correlation_id =
            surface::SurfaceInputCorrelationId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let admitted_input = surface::AdmittedInput::PendingUser {
            item_id: input_item_id.clone(),
            presentation: presentation.clone(),
            correlation_id: correlation_id.clone(),
        };
        let generation_input = surface::GenerationInputState::Pending {
            input_item_id: input_item_id.clone(),
            presentation: presentation.clone(),
            correlation_id: correlation_id.clone(),
        };
        let generation = surface::GenerationRecord {
            fence: fence.clone(),
            logical_turn_id: logical_turn_id.clone(),
            input: generation_input,
            predecessor: None,
            attempt: surface::GenerationAttempt::Initial,
            goal_identity: None,
            replayability: operation.intent.initial_replayability.clone(),
            required_capabilities: operation.intent.required_capabilities.clone(),
            capability_fingerprint: operation.intent.capability_fingerprint.clone(),
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let admitted_batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Admitted {
                        operation_id: operation_id.clone(),
                        logical_turn_id: logical_turn_id.clone(),
                        input: admitted_input,
                        first_generation: generation,
                    }),
                ),
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Item(surface::ItemPatch::Added {
                        item: surface::SurfaceItem::UserMessage {
                            id: input_item_id.clone(),
                            turn_id: logical_turn_id.clone(),
                            input: surface::SurfaceUserInputState::Pending {
                                presentation: presentation.clone(),
                                correlation_id,
                            },
                            pinned: false,
                            origin: surface::SurfaceItemOrigin::UserInput,
                        },
                    }),
                ),
            ],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&admitted_batch)
            .map_err(|error| {
                eprintln!("orca: typed surface admission commit failed: {error:?}");
                surface::SurfaceClientCommandError::RuntimeUnavailable
            })?;

        let start_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let started_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::GenerationStarted {
                fence: fence.clone(),
                witness: surface::GenerationStartedWitness {
                    started_commit_id: start_commit_id.clone(),
                    settings_revision: operation.intent.settings_revision,
                    policy_epoch: operation.intent.policy_epoch,
                    durable_replayability_digest: surface::canonical_replayability_digest(
                        &operation.intent.initial_replayability,
                    ),
                    capability_fingerprint: operation.intent.capability_fingerprint.clone(),
                },
            }],
            Some(start_commit_id),
        );
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &started_batch)
            .map_err(|error| {
                eprintln!("orca: typed surface start commit failed: {error:?}");
                surface::SurfaceClientCommandError::RuntimeUnavailable
            })?;

        let resolved_fact = surface::SurfaceResolvedInputFact::Replayable {
            input: resolved_input.clone(),
            request_digest,
        };
        let resolved_batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::InputBindingsResolved {
                            fence: fence.clone(),
                            input_item_id: input_item_id.clone(),
                            fact: resolved_fact.clone(),
                        },
                    ),
                ),
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Item(surface::ItemPatch::InputResolved {
                        item_id: input_item_id,
                        fact: resolved_fact,
                    }),
                ),
            ],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &resolved_batch)
            .map_err(|error| {
                eprintln!("orca: typed surface input resolution commit failed: {error:?}");
                surface::SurfaceClientCommandError::RuntimeUnavailable
            })?;

        let legacy_task_id = format!("typed-user-turn-{}", uuid::Uuid::now_v7());
        let loop_started_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn {
                    turn_id: logical_turn_id.clone(),
                    fence: fence.clone(),
                    ordinal: 0,
                    task_id: surface::SurfaceTaskId::try_new(legacy_task_id.clone())
                        .expect("generated task id is non-empty"),
                    task_status: surface::SurfaceTaskRunningStatus::Running,
                },
            }],
        );
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &loop_started_batch)
            .map_err(|error| {
                eprintln!("orca: typed surface agent-loop start commit failed: {error:?}");
                surface::SurfaceClientCommandError::RuntimeUnavailable
            })?;

        let mut hosted_request = HostedTurnRequest::new(resolved_input.canonical_text.as_str());
        hosted_request.turn_id = logical_turn_id;
        hosted_request.task_id = Some(legacy_task_id);
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(hosted_request),
            writer: Box::new(PassthroughHostedOperationWriter::new(io::sink())),
            config: None,
            reply: start_tx,
        });
        start_rx
            .recv()
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let active = self
            .active
            .as_mut()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        active.surface_operation = Some(fence.clone());

        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &admitted_batch,
            surface::AdmissionOutput::Admitted {
                operation_id,
                first_generation: fence,
                admitted_cursor: admitted_batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    fn wait_surface_operation(
        &mut self,
        _request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        reply: SyncSender<
            Result<surface::WaitOperationTerminalResult, surface::SurfaceClientCommandError>,
        >,
    ) {
        if let Some(value) = self.resident_surface.terminals.get(&operation_id) {
            let _ = reply.send(Ok(surface::WaitOperationTerminalResult::Terminal {
                value: value.clone(),
            }));
            return;
        }
        if let Some(pending) = self
            .resident_surface
            .pending_terminal_commits
            .get(&operation_id)
        {
            let _ = reply.try_send(Ok(pending.failure.clone()));
            return;
        }
        let exists = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .foreground_operation
            .iter()
            .chain(
                self.resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .queued_operations
                    .iter(),
            )
            .chain(
                self.resident_surface
                    .coordinator
                    .state()
                    .snapshot()
                    .operation_history
                    .iter(),
            )
            .any(|operation| operation.operation_id == operation_id);
        if !exists {
            let _ = reply.send(Ok(surface::WaitOperationTerminalResult::UnknownOperation {
                operation_id,
            }));
            return;
        }
        self.resident_surface
            .waiters
            .entry(operation_id)
            .or_default()
            .push(reply);
    }

    fn cache_surface_terminal(&mut self, value: surface::OperationTerminalAtCursor) {
        let operation_id = value.operation_id.clone();
        self.resident_surface
            .terminals
            .insert(operation_id.clone(), value.clone());
        for waiter in self
            .resident_surface
            .waiters
            .remove(&operation_id)
            .unwrap_or_default()
        {
            let _ = waiter.send(Ok(surface::WaitOperationTerminalResult::Terminal {
                value: value.clone(),
            }));
        }
    }

    fn cache_surface_terminal_failure(&mut self, pending: PendingSurfaceTerminalCommit) {
        let operation_id = pending.value.operation_id.clone();
        let failure = pending.failure.clone();
        self.surface_terminal_blocked =
            Some("typed surface terminal commit failed and requires cold recovery".to_string());
        self.resident_surface
            .pending_terminal_commits
            .insert(operation_id.clone(), pending);
        for waiter in self
            .resident_surface
            .waiters
            .remove(&operation_id)
            .unwrap_or_default()
        {
            let _ = waiter.try_send(Ok(failure.clone()));
        }
    }

    fn retry_surface_finalization(
        &self,
        token: surface::RetryFinalizationToken,
    ) -> surface::MutationReply<surface::OperationTerminalAtCursor> {
        let exact_pending = self
            .resident_surface
            .pending_terminal_commits
            .get(token.operation_id())
            .is_some_and(|pending| {
                let exact = matches!(
                    &pending.failure,
                    surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        repair,
                        ..
                    } if repair == &token
                );
                if exact {
                    debug_assert!(pending.batch.batch_digest == pending.value.batch_digest);
                    if let Some(completion) = pending.legacy_completion.as_ref() {
                        debug_assert!(completion.try_terminal().is_none());
                    }
                    if let Some(terminal) = pending.legacy_terminal.as_ref() {
                        let _ = terminal.outcome();
                    }
                }
                exact
            });
        let code = if exact_pending {
            surface::SurfaceMutationErrorCode::IllegalState
        } else {
            surface::SurfaceMutationErrorCode::InvalidRequest
        };
        let message = if exact_pending {
            "durable operation is Finalizing; live retry is not authoritative"
        } else {
            "retry finalization token does not match resident pending terminal commit"
        };
        surface::MutationReply::Uncommitted {
            mutation: surface::UncommittedMutation::Invalid {
                request_id: token.request_id().clone(),
                target: Some(surface::MutationTarget::Operation {
                    thread_id: token.thread_id().clone(),
                    operation_id: token.operation_id().clone(),
                }),
                error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                    code,
                    message: surface::DisplayText::new(message),
                    winning_request_id: None,
                    current_revision: None,
                }),
            },
        }
    }

    fn terminalize_surface_reservation(
        &mut self,
        operation_id: surface::SurfaceOperationId,
        finalizer_reason: surface::ReservationFinalizerReason,
        terminal_reason: surface::NotAdmittedReason,
    ) -> Result<
        (
            surface::OperationTerminalAtCursor,
            surface::SurfaceCommitBatch,
        ),
        surface::SurfaceClientCommandError,
    > {
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = snapshot
            .queued_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let original_request_id = operation.request_id.clone();
        let thread_id = snapshot.thread.thread_id.clone();
        let thread_owner_epoch = snapshot.thread.owner_epoch;
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let finalization_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::Reservation(finalizer_reason),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }],
        );
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(
                operation_id.clone(),
                finalize_intent_id.clone(),
                &finalization_batch,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;

        let terminal = surface::OperationTerminal::NotAdmitted {
            reason: terminal_reason,
        };
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal: terminal.clone(),
                    usage: surface::UsageTotals {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_tokens: 0,
                        estimated_cost_usd_micros: 0,
                    },
                    source_diagnostic_digest: None,
                    settlement_receipts: Vec::new(),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(terminal_commit_id.clone()),
        );
        let terminal_result = self.resident_surface.coordinator.commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &terminal_batch,
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal,
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if terminal_result.is_err() {
            let repair = surface::RetryFinalizationToken::new(
                original_request_id,
                thread_id,
                operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                thread_owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            let failure = surface::WaitOperationTerminalResult::TerminalCommitFailure {
                operation_id,
                finalize_intent_id,
                commit_id: terminal_commit_id,
                repair,
            };
            self.cache_surface_terminal_failure(PendingSurfaceTerminalCommit {
                batch: terminal_batch,
                value,
                failure,
                legacy_completion: None,
                legacy_terminal: None,
            });
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.cache_surface_terminal(value.clone());
        Ok((value, terminal_batch))
    }

    fn terminalize_requested_operations_for_shutdown(
        &mut self,
        reason: surface::SurfaceShutdownReason,
    ) -> Result<(), RuntimeHostError> {
        let operation_ids = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .map(|operation| operation.operation_id.clone())
            .collect::<Vec<_>>();
        let (finalizer_reason, terminal_reason) = match reason {
            surface::SurfaceShutdownReason::HostShutdown => (
                surface::ReservationFinalizerReason::HostShutdown,
                surface::NotAdmittedReason::HostShutdown,
            ),
            surface::SurfaceShutdownReason::ThreadClose => (
                surface::ReservationFinalizerReason::ThreadClose,
                surface::NotAdmittedReason::ThreadClose,
            ),
        };
        for operation_id in operation_ids {
            let _ = self
                .terminalize_surface_reservation(
                    operation_id,
                    finalizer_reason.clone(),
                    terminal_reason,
                )
                .map_err(|error| RuntimeHostError::ThreadStartFailed {
                    message: format!(
                        "failed to terminalize typed reservation during shutdown: {error:?}"
                    ),
                })?;
        }
        Ok(())
    }

    fn finish_surface_operation(
        &mut self,
        active: &ActiveOperation,
        outcome: &OperationOutcome,
    ) -> Result<surface::OperationTerminalAtCursor, RuntimeHostError> {
        let fence = active.surface_operation.clone().ok_or_else(|| {
            RuntimeHostError::ThreadStartFailed {
                message: "typed surface operation fence is missing during finalization".to_string(),
            }
        })?;
        let (stop_reason, terminal) = match outcome {
            OperationOutcome::Completed(RunStatus::Success) => (
                surface::GenerationStopReason::Completed {
                    status: surface::GenerationCompletionStatus::Success,
                },
                surface::OperationTerminal::Succeeded {
                    usage: surface::UsageTotals {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_tokens: 0,
                        estimated_cost_usd_micros: 0,
                    },
                },
            ),
            OperationOutcome::Completed(RunStatus::Cancelled) => {
                let cause = active
                    .surface_terminalization
                    .unwrap_or(surface::TerminalizationCause::UserCancel);
                let terminal = match cause {
                    surface::TerminalizationCause::HostShutdown => {
                        surface::OperationTerminal::Shutdown {
                            reason: surface::SurfaceShutdownReason::HostShutdown,
                        }
                    }
                    surface::TerminalizationCause::ThreadClose => {
                        surface::OperationTerminal::Shutdown {
                            reason: surface::SurfaceShutdownReason::ThreadClose,
                        }
                    }
                    _ => surface::OperationTerminal::Cancelled {
                        reason: surface::CancelReason::User,
                    },
                };
                (surface::GenerationStopReason::Cancelled { cause }, terminal)
            }
            OperationOutcome::Panicked { message } => (
                surface::GenerationStopReason::Panicked {
                    message: surface::SafeDiagnosticText::try_new(message.clone()).unwrap_or_else(
                        |_| surface::SafeDiagnosticText::try_new("generation panicked").unwrap(),
                    ),
                },
                surface::OperationTerminal::Panicked {
                    message: surface::SafeDiagnosticText::try_new(message.clone()).unwrap_or_else(
                        |_| surface::SafeDiagnosticText::try_new("generation panicked").unwrap(),
                    ),
                },
            ),
            _ => {
                let message = surface::SafeDiagnosticText::try_new("foreground operation failed")
                    .expect("fixed diagnostic is bounded");
                (
                    surface::GenerationStopReason::ExecutionFailed {
                        class: surface::GenerationExecutionFailureClass::RuntimeInvariant,
                        message: message.clone(),
                    },
                    surface::OperationTerminal::Failed {
                        class: surface::FailureClass::RuntimeInvariant,
                        message,
                    },
                )
            }
        };
        let operation_id = fence.operation_id.clone();
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == operation_id)
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed surface operation is missing during finalization".to_string(),
            })?;
        let original_request_id = operation.request_id.clone();
        let thread_id = snapshot.thread.thread_id.clone();
        let thread_owner_epoch = snapshot.thread.owner_epoch;
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let stop_and_finalization_batch = self.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(surface::OperationPatch::GenerationStopped {
                        fence: fence.clone(),
                        reason: stop_reason.clone(),
                        usage_delta: surface::UsageTotals {
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_tokens: 0,
                            estimated_cost_usd_micros: 0,
                        },
                    }),
                ),
                (
                    surface::SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::FinalizationStarted {
                            operation_id: operation_id.clone(),
                            finalize_intent_id: finalize_intent_id.clone(),
                            terminal_commit_id: terminal_commit_id.clone(),
                            selected_cause: surface::OperationFinalizationCause::GenerationStop(
                                stop_reason,
                            ),
                            suspended_cause: None,
                            expected_settlements: Vec::new(),
                        },
                    ),
                ),
            ],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_live_generation_stop_disposition_batch(
                fence,
                operation_id.clone(),
                finalize_intent_id.clone(),
                &stop_and_finalization_batch,
            )
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!(
                    "typed surface generation stop and finalization start failed: {error:?}"
                ),
            })?;
        let usage = surface::UsageTotals {
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            estimated_cost_usd_micros: 0,
        };
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal: terminal.clone(),
                    usage,
                    source_diagnostic_digest: None,
                    settlement_receipts: Vec::new(),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(terminal_commit_id.clone()),
        );
        let terminal_result = self.resident_surface.coordinator.commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &terminal_batch,
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal,
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if let Err(error) = terminal_result {
            let repair = surface::RetryFinalizationToken::new(
                original_request_id,
                thread_id,
                operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                thread_owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            let failure = surface::WaitOperationTerminalResult::TerminalCommitFailure {
                operation_id: operation_id.clone(),
                finalize_intent_id,
                commit_id: terminal_commit_id,
                repair,
            };
            self.cache_surface_terminal_failure(PendingSurfaceTerminalCommit {
                batch: terminal_batch,
                value,
                failure,
                legacy_completion: Some(active.completion.clone()),
                legacy_terminal: Some(OperationTerminal {
                    operation_id: active.operation_id,
                    outcome: outcome.clone(),
                }),
            });
            return Err(RuntimeHostError::ThreadStartFailed {
                message: format!("typed surface terminal commit failed: {error:?}"),
            });
        }
        self.cache_surface_terminal(value.clone());
        Ok(value)
    }

    fn cancel_surface_before_admission(
        &mut self,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        let operation = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let (value, terminal_batch) = self.terminalize_surface_reservation(
            operation_id.clone(),
            surface::ReservationFinalizerReason::CancelledBeforeAdmission,
            surface::NotAdmittedReason::CancelledBeforeAdmission,
        )?;
        debug_assert_eq!(operation.operation_id, operation_id);
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id,
            &terminal_batch,
            surface::CancelOperationOutput::CancelledBeforeAdmission { terminal: value },
        ))
    }

    fn cancel_surface_running(
        &mut self,
        active: &mut ActiveOperation,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        let _fence = active
            .surface_operation
            .as_ref()
            .filter(|fence| fence.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let original_request_id = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .foreground_operation
            .as_ref()
            .map(|operation| operation.request_id.clone())
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::ControlIntentCommitted {
                operation_id: operation_id.clone(),
                request_id: original_request_id,
                intent: surface::PendingControlIntent::Terminalize {
                    operation_id: operation_id.clone(),
                    cause: surface::TerminalizationCause::UserCancel,
                },
            }],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        active.surface_terminalization = Some(surface::TerminalizationCause::UserCancel);
        active.generation.cancel.cancel();
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &batch,
            surface::CancelOperationOutput::Accepted {
                operation_id,
                accepted_cursor: batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    fn commit_surface_terminalization(
        &mut self,
        active: &mut ActiveOperation,
        cause: surface::TerminalizationCause,
    ) -> Result<(), RuntimeHostError> {
        let Some(fence) = active.surface_operation.as_ref() else {
            return Ok(());
        };
        let operation_id = fence.operation_id.clone();
        let original_request_id = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .foreground_operation
            .as_ref()
            .map(|operation| operation.request_id.clone())
            .ok_or_else(|| RuntimeHostError::ThreadStartFailed {
                message: "typed surface foreground operation is missing".to_string(),
            })?;
        let batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::ControlIntentCommitted {
                operation_id: operation_id.clone(),
                request_id: original_request_id,
                intent: surface::PendingControlIntent::Terminalize {
                    operation_id: operation_id.clone(),
                    cause,
                },
            }],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to commit typed shutdown intent: {error:?}"),
            })?;
        active.surface_terminalization = Some(cause);
        Ok(())
    }

    fn new(
        thread: RuntimeThread,
        config: RunConfig,
        handle: RuntimeThreadHandle,
        executor: Arc<dyn ThreadOperationExecutor>,
        background_capacity: usize,
        resident_surface: Option<ResidentSurfaceState>,
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
            resident_surface: ResidentSurfaceSlot(resident_surface),
            surface_terminal_blocked: None,
        }
    }

    async fn run(mut self, mut command_rx: tokio_mpsc::Receiver<ThreadCommand>) {
        loop {
            let Some(mut active) = self.active.take() else {
                tokio::select! {
                    biased;
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            if self.surface_terminal_blocked.is_some() {
                                std::future::pending::<()>().await;
                            }
                            self.shutdown_background_tasks().await;
                            break;
                        };
                        if let ThreadCommand::ShutdownThread { reply, reason } = command {
                            if let Some(message) = self.surface_terminal_blocked.as_ref() {
                                if let Some(reply) = reply {
                                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                                        message: message.clone(),
                                    }));
                                }
                                if reason == surface::SurfaceShutdownReason::HostShutdown {
                                    std::future::pending::<()>().await;
                                }
                                continue;
                            }
                            let typed_shutdown = self
                                .resident_surface
                                .0
                                .is_some()
                                .then(|| self.terminalize_requested_operations_for_shutdown(reason))
                                .transpose();
                            if let Err(error) = typed_shutdown {
                                self.surface_terminal_blocked = Some(error.to_string());
                                if let Some(reply) = reply {
                                    let _ = reply.send(Err(error));
                                }
                                if reason == surface::SurfaceShutdownReason::HostShutdown {
                                    std::future::pending::<()>().await;
                                }
                                continue;
                            }
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
                        Some(ThreadCommand::ShutdownThread { reply, reason }) => {
                            command_rx.close();
                            let pause_result = Self::pause_active_goal(
                                &mut active,
                                "goal run paused during runtime shutdown",
                            );
                            let terminalization = match reason {
                                surface::SurfaceShutdownReason::HostShutdown => {
                                    surface::TerminalizationCause::HostShutdown
                                }
                                surface::SurfaceShutdownReason::ThreadClose => {
                                    surface::TerminalizationCause::ThreadClose
                                }
                            };
                            if let Err(error) = self
                                .commit_surface_terminalization(&mut active, terminalization)
                            {
                                let result = (&mut active.generation.join).await;
                                let finish_result = self.finish_generation(active, result, false);
                                self.shutdown_background_tasks().await;
                                if let Err(finish_error) = finish_result {
                                    if let Some(reply) = reply.as_ref() {
                                        let _ = reply.send(Err(finish_error));
                                    }
                                    std::future::pending::<()>().await;
                                }
                                if let Some(reply) = reply.as_ref() {
                                    let _ = reply.send(Err(error));
                                }
                                break;
                            }
                            active.generation.cancel.cancel();
                            let result = (&mut active.generation.join).await;
                            let finish_result = self.finish_generation(active, result, false);
                            self.shutdown_background_tasks().await;
                            if let Err(error) = finish_result {
                                if let Some(reply) = reply.as_ref() {
                                    let _ = reply.send(Err(error));
                                }
                                std::future::pending::<()>().await;
                            }
                            if let Some(reply) = reply.as_ref() {
                                let _ = reply.send(pause_result);
                            }
                            break;
                        }
                        Some(ThreadCommand::PauseGoalRun {
                            operation_id,
                            reply,
                        }) => {
                            let result = Self::request_goal_pause(&mut active, operation_id);
                            let waits_for_join = matches!(
                                result,
                                Ok(PauseGoalRunResult::Requested { .. }
                                    | PauseGoalRunResult::AlreadyRequested { .. })
                            );
                            if waits_for_join {
                                let generation_result = (&mut active.generation.join).await;
                                if let Err(error) =
                                    self.finish_generation(active, generation_result, false)
                                {
                                    eprintln!("orca: operation finalization failed: {error}");
                                }
                            } else {
                                self.active = Some(active);
                            }
                            let _ = reply.send(result);
                        }
                        Some(command) => {
                            self.handle_running_command(command, &mut active);
                            self.active = Some(active);
                        }
                        None => {
                            let _ = Self::pause_active_goal(
                                &mut active,
                                "goal run paused because runtime command channel closed",
                            );
                            active.generation.cancel.cancel();
                            let result = (&mut active.generation.join).await;
                            let finish_result = self.finish_generation(active, result, false);
                            self.shutdown_background_tasks().await;
                            if let Err(error) = finish_result {
                                eprintln!("orca: operation finalization failed: {error}");
                                std::future::pending::<()>().await;
                            }
                            break;
                        }
                    }
                }
                result = &mut active.generation.join => {
                    if let Err(error) = self.finish_generation(active, result, true) {
                        eprintln!("orca: operation finalization failed: {error}");
                        self.surface_terminal_blocked = Some(error.to_string());
                    }
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
            ThreadCommand::SurfaceReserveOperation {
                client,
                request_id,
                intent,
                reply,
            } => {
                let result = if self
                    .admits_surface_client(&client, surface::SurfaceCapability::SubmitOperation)
                {
                    self.reserve_surface_operation(request_id, intent)
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceAdmitReserved {
                client,
                request_id,
                operation_id,
                admission_lease_id,
                reply,
            } => {
                let result = if self
                    .admits_surface_client(&client, surface::SurfaceCapability::SubmitOperation)
                {
                    self.admit_surface_operation(request_id, operation_id, admission_lease_id)
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceCancelOperation {
                client,
                request_id,
                operation_id,
                reply,
            } => {
                let result = if self.admits_surface_client(
                    &client,
                    surface::SurfaceCapability::ControlBoundOperation,
                ) {
                    self.cancel_surface_before_admission(request_id, operation_id)
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceWaitOperationTerminal {
                client,
                request_id,
                operation_id,
                reply,
                ..
            } => {
                if self.admits_surface_client(&client, surface::SurfaceCapability::ReadSnapshot) {
                    self.wait_surface_operation(request_id, operation_id, reply);
                } else {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::Unauthorized));
                }
            }
            ThreadCommand::SurfaceRetryFinalization {
                client,
                token,
                reply,
            } => {
                let result = if self
                    .admits_surface_client(&client, surface::SurfaceCapability::RepairThread)
                {
                    Ok(self.retry_surface_finalization(token))
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            #[cfg(test)]
            ThreadCommand::SurfaceActorTestProbe {
                operation_id,
                reply,
            } => {
                let _ = reply.send(SurfaceActorTestProbe {
                    waiter_count: self
                        .resident_surface
                        .waiters
                        .get(&operation_id)
                        .map_or(0, Vec::len),
                    legacy_completion: None,
                });
            }
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
                if request.operation_kind() == &HostedOperationKind::GoalRun {
                    request.task_description = None;
                    request.task_id = None;
                    request.main_session_task_id = None;
                }
                if request.allows_goal_tools() && state.thread.session().session_id().is_none() {
                    self.state = Some(state);
                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                        message: "goal tools require a persistent session before turn execution"
                            .to_string(),
                    }));
                    return;
                }
                let goal_control = if request.operation_kind() == &HostedOperationKind::GoalRun {
                    let Some(session_id) = state.thread.session().session_id().map(str::to_string)
                    else {
                        self.state = Some(state);
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: "goal run requires a persistent session".to_string(),
                        }));
                        return;
                    };
                    let runtime = match state.thread.goal_runtime_handle() {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            self.state = Some(state);
                            let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                                message: error.to_string(),
                            }));
                            return;
                        }
                    };
                    if let Err(error) = Self::publish_goal_recoveries(
                        &mut state,
                        &runtime,
                        request.event_observer().as_deref(),
                    ) {
                        self.state = Some(state);
                        let _ = reply.send(Err(error));
                        return;
                    }
                    Some(ActiveGoalControl {
                        session_id,
                        runtime,
                    })
                } else {
                    None
                };
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
                    goal_admitted_generation: None,
                    goal_control,
                    pending_goal_pause_event: None,
                    generation,
                    surface_operation: None,
                    surface_terminalization: None,
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
            ThreadCommand::PauseGoalRun {
                operation_id,
                reply,
            } => {
                let _ = reply.send(Ok(PauseGoalRunResult::Idle {
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
            ThreadCommand::GoalRuntime { reply } => {
                let result = self
                    .state
                    .as_mut()
                    .ok_or(RuntimeHostError::ThreadUnavailable)
                    .and_then(|state| {
                        let runtime = state.thread.goal_runtime_handle().map_err(|error| {
                            RuntimeHostError::ThreadStartFailed {
                                message: error.to_string(),
                            }
                        })?;
                        Self::publish_goal_recoveries(state, &runtime, None)?;
                        Ok(runtime)
                    });
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

    fn handle_running_command(&mut self, command: ThreadCommand, active: &mut ActiveOperation) {
        let generation = active.generation.context.fence();
        match command {
            ThreadCommand::SurfaceReserveOperation { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            }
            ThreadCommand::SurfaceAdmitReserved { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            }
            ThreadCommand::SurfaceCancelOperation {
                client,
                request_id,
                operation_id,
                reply,
            } => {
                let result = if self.admits_surface_client(
                    &client,
                    surface::SurfaceCapability::ControlBoundOperation,
                ) {
                    self.cancel_surface_running(active, request_id, operation_id)
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceWaitOperationTerminal {
                client,
                request_id,
                operation_id,
                reply,
                ..
            } => {
                if self.admits_surface_client(&client, surface::SurfaceCapability::ReadSnapshot) {
                    self.wait_surface_operation(request_id, operation_id, reply);
                } else {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::Unauthorized));
                }
            }
            ThreadCommand::SurfaceRetryFinalization {
                client,
                token,
                reply,
            } => {
                let result = if self
                    .admits_surface_client(&client, surface::SurfaceCapability::RepairThread)
                {
                    Ok(self.retry_surface_finalization(token))
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            #[cfg(test)]
            ThreadCommand::SurfaceActorTestProbe {
                operation_id,
                reply,
            } => {
                let legacy_completion = active
                    .surface_operation
                    .as_ref()
                    .filter(|fence| fence.operation_id == operation_id)
                    .map(|_| active.completion.clone());
                let _ = reply.send(SurfaceActorTestProbe {
                    waiter_count: self
                        .resident_surface
                        .waiters
                        .get(&operation_id)
                        .map_or(0, Vec::len),
                    legacy_completion,
                });
            }
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
                } else if let Err(error) =
                    Self::pause_active_goal(active, "goal run interrupted by user")
                {
                    let _ = reply.send(Err(error));
                    return;
                } else {
                    active.generation.cancel.cancel();
                    InterruptOperationResult::Requested { generation }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::PauseGoalRun {
                operation_id,
                reply,
            } => {
                let _ = reply.send(Self::request_goal_pause(active, operation_id));
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
            ThreadCommand::GoalRuntime { reply } => {
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

    fn pause_active_goal(
        active: &mut ActiveOperation,
        message: &str,
    ) -> Result<(), RuntimeHostError> {
        let Some(control) = active.goal_control.as_ref() else {
            return Ok(());
        };
        let runtime = control.runtime.clone();
        let session_id = control.session_id.clone();
        let previous =
            runtime
                .read(&session_id)
                .map_err(|error| RuntimeHostError::GoalControlFailed {
                    message: error.to_string(),
                })?;
        runtime
            .pause(
                &session_id,
                orca_core::goal_runtime::GoalPauseReason::User,
                message,
                chrono::Utc::now().timestamp(),
            )
            .map_err(|error| RuntimeHostError::GoalControlFailed {
                message: error.to_string(),
            })?;
        let next =
            runtime
                .read(&session_id)
                .map_err(|error| RuntimeHostError::GoalControlFailed {
                    message: error.to_string(),
                })?;
        if active.pending_goal_pause_event.is_none()
            && let (Some(previous), Some(next)) = (previous, next)
            && previous.state != next.state
            && let orca_core::goal_runtime::GoalState::Paused { reason, message } = &next.state
        {
            active.pending_goal_pause_event = Some(PendingGoalPauseEvent {
                goal_id: next.goal_id.clone(),
                goal_run_id: previous
                    .current_run
                    .as_ref()
                    .map(|run| run.goal_run_id.clone()),
                outer_turn_id: previous
                    .current_run
                    .as_ref()
                    .and_then(|run| run.outer_turn_id.clone()),
                previous_state: previous.state,
                next_state: next.state.clone(),
                reason: *reason,
                message: message.clone(),
                reason_code: next
                    .last_transition
                    .as_ref()
                    .map(|transition| transition.reason_code.clone())
                    .unwrap_or_else(|| "paused".to_string()),
            });
        }
        Ok(())
    }

    fn publish_pending_goal_pause_event(
        &self,
        state: &mut ThreadActorState,
        active: &mut ActiveOperation,
    ) {
        let Some(event) = active.pending_goal_pause_event.take() else {
            return;
        };
        let observer = active.request.event_observer();
        observe_runtime_event(
            observer.as_deref(),
            state.events.goal_transitioned(
                &event.goal_id,
                &event.previous_state,
                &event.next_state,
                &event.reason_code,
            ),
        );
        observe_runtime_event(
            observer.as_deref(),
            state.events.goal_paused(
                &event.goal_id,
                event.goal_run_id.as_ref(),
                event.outer_turn_id.as_ref(),
                event.reason,
                &event.message,
            ),
        );
    }

    fn publish_goal_recoveries(
        state: &mut ThreadActorState,
        runtime: &GoalRuntimeHandle,
        observer: Option<&dyn EventObserver>,
    ) -> Result<(), RuntimeHostError> {
        let Some(session_id) = state.thread.session().session_id().map(str::to_string) else {
            return Ok(());
        };
        let recoveries = runtime.take_recoveries(&session_id).map_err(|error| {
            RuntimeHostError::GoalControlFailed {
                message: error.to_string(),
            }
        })?;
        for recovery in recoveries {
            observe_runtime_event(
                observer,
                state.events.goal_recovered(
                    &recovery.goal_id,
                    &recovery.stale_goal_run_id,
                    recovery.outer_turn_id.as_ref(),
                    &recovery.recovered_state,
                ),
            );
        }
        Ok(())
    }

    fn request_goal_pause(
        active: &mut ActiveOperation,
        operation_id: OperationId,
    ) -> Result<PauseGoalRunResult, RuntimeHostError> {
        let generation = active.generation.context.fence();
        if operation_id != active.operation_id {
            return Ok(PauseGoalRunResult::Stale {
                requested_operation_id: operation_id,
                active: generation,
            });
        }
        if active.goal_control.is_none() {
            return Ok(PauseGoalRunResult::NotGoalRun { generation });
        }
        let already_requested = active.generation.cancel.is_cancelled();
        Self::pause_active_goal(active, "paused by user")?;
        active.generation.cancel.cancel();
        Ok(if already_requested {
            PauseGoalRunResult::AlreadyRequested { generation }
        } else {
            PauseGoalRunResult::Requested { generation }
        })
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
            let usage_delta =
                subtract_usage_totals(usage_totals_delta(usage_before, usage_after), usage_credit);
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
    ) -> Result<(), RuntimeHostError> {
        let outcome = match result {
            Ok(mut result) => {
                self.usage_ledger.add(result.usage_delta);
                self.publish_pending_goal_pause_event(&mut result.state, &mut active);
                let background_error = match &mut result.outcome {
                    GenerationTaskOutcome::Executed(outcome) => {
                        let required = outcome
                            .background_workflow_count()
                            .saturating_add(usize::from(outcome.suspends_provider()));
                        if active.surface_operation.is_some() && required > 0 {
                            let workflows = outcome.take_background_workflows();
                            cancel_and_join_background_workflows(
                                result.state.thread.session().task_registry(),
                                &result.state.events,
                                active.request.event_observer(),
                                workflows,
                            );
                            if let ThreadOperationOutcome::ProviderSuspended {
                                suspension, ..
                            } = outcome
                            {
                                cancel_and_join_provider_suspension(suspension);
                            }
                            Some(io::Error::other(
                                "typed foreground operation produced unmodeled background work",
                            ))
                        } else if let Err(error) = self.ensure_background_capacity(required) {
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
                    let replace_generation = active.surface_operation.is_none()
                        && allow_resume
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
                            return Ok(());
                        }
                    } else {
                        let goal_continuation = (active.request.operation_kind()
                            == &HostedOperationKind::GoalRun)
                            .then(|| {
                                self.goal_continuation_admission(
                                    &mut result.state,
                                    &mut active,
                                    &result.outcome,
                                )
                            });
                        if let Some((admission, objective)) = goal_continuation {
                            if let Some(session_id) = result
                                .state
                                .thread
                                .session()
                                .session_id()
                                .map(str::to_string)
                                && let Ok(handle) = result.state.thread.goal_runtime_handle()
                                && let Ok(Some(record)) = handle.read(&session_id)
                            {
                                let (admitted, reason) = match &admission {
                                    GoalContinuationAdmission::Admit { reason } => {
                                        (true, goal_continuation_reason_name(*reason))
                                    }
                                    GoalContinuationAdmission::Reject { code, .. } => {
                                        (false, goal_continuation_reject_name(*code))
                                    }
                                };
                                observe_runtime_event(
                                    active.request.event_observer().as_deref(),
                                    result.state.events.goal_continuation_admission(
                                        &record.goal_id,
                                        record.current_run.as_ref().map(|run| &run.goal_run_id),
                                        record
                                            .current_run
                                            .as_ref()
                                            .and_then(|run| run.outer_turn_id.as_ref()),
                                        admitted,
                                        reason,
                                        &record.state,
                                        record
                                            .current_run
                                            .as_ref()
                                            .map(|run| run.continuation_count)
                                            .unwrap_or_default(),
                                    ),
                                );
                            }
                            if let (GoalContinuationAdmission::Admit { .. }, Some(objective)) =
                                (admission, objective)
                            {
                                if let Err(error) = result.writer.finish_generation(false) {
                                    self.state = Some(result.state);
                                    let outcome = OperationOutcome::ExecutionFailed {
                                        kind: error.kind(),
                                        message: error.to_string(),
                                    };
                                    let completed = active.completion.complete(OperationTerminal {
                                        operation_id: active.operation_id,
                                        outcome,
                                    });
                                    debug_assert!(completed);
                                    return Ok(());
                                }
                                if let Some(task_id) = active.runtime_task_id.as_deref() {
                                    result
                                        .state
                                        .thread
                                        .lifecycle_mut()
                                        .start_task_with_id(RuntimeTaskKind::Agent, task_id);
                                }
                                let continuation = active
                                    .generation
                                    .context
                                    .fence()
                                    .generation_id()
                                    .as_u64()
                                    .saturating_add(1);
                                active.request.prompt = goal_continuation_prompt(
                                    &objective,
                                    usize::try_from(continuation).unwrap_or(usize::MAX),
                                );
                                active.request.turn_id = TurnId::new();
                                active.request.continuation = None;
                                active.request.goal_turn_origin =
                                    orca_core::goal_runtime::GoalTurnOrigin::Continuation;
                                active.request.resumes_existing_turn = false;
                                let context = GenerationContext::new(
                                    active.generation.context.fence().next(),
                                    active.steer_handle.clone(),
                                    false,
                                    HostedGenerationHandlers::default(),
                                    active.config.clone(),
                                );
                                active.generation = self.spawn_generation(
                                    result.state,
                                    &active.request,
                                    result.writer,
                                    context,
                                );
                                self.active = Some(active);
                                return Ok(());
                            }
                        }
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
                                    if active.request.operation_kind()
                                        == &HostedOperationKind::GoalRun
                                    {
                                        observe_runtime_event(
                                            active.request.event_observer().as_deref(),
                                            result.state.events.session_completed(status),
                                        );
                                    }
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
        if active.surface_operation.is_some() {
            self.finish_surface_operation(&active, &outcome)?;
        }
        let completed = active.completion.complete(OperationTerminal {
            operation_id: active.operation_id,
            outcome,
        });
        debug_assert!(completed, "operation terminal must complete exactly once");
        Ok(())
    }

    fn goal_continuation_admission(
        &self,
        state: &mut ThreadActorState,
        active: &mut ActiveOperation,
        outcome: &GenerationTaskOutcome,
    ) -> (GoalContinuationAdmission, Option<String>) {
        let fence = active.generation.context.fence();
        let successful_turn = matches!(
            outcome,
            GenerationTaskOutcome::Executed(ThreadOperationOutcome::Completed {
                status: RunStatus::Success,
                ..
            })
        );
        if let Some(rejection) = goal_continuation_preflight(GoalContinuationPreflight {
            cancelled: active.generation.cancel.is_cancelled(),
            successful_turn,
            queued_user_input: active.steer_handle.has_pending(),
            pending_interaction: active
                .request
                .pending_interactions
                .as_ref()
                .is_some_and(|pending| !pending.is_empty()),
            active_workflow: state.thread.session().has_active_workflows(),
            plan_mode: active.config.approval_mode == ApprovalMode::Plan,
            duplicate_admission: active.goal_admitted_generation == Some(fence),
        }) {
            if let GoalContinuationAdmission::Reject { code, message } = &rejection {
                self.persist_queued_goal_input(state, active, *code);
                self.pause_goal_after_rejected_admission(state, *code, message);
            }
            return (rejection, None);
        }
        let Some(session_id) = state.thread.session().session_id().map(str::to_string) else {
            return (
                GoalContinuationAdmission::Reject {
                    code: GoalContinuationRejectCode::RuntimeUnavailable,
                    message: "goal continuation requires a persistent session".to_string(),
                },
                None,
            );
        };
        let handle = match state.thread.goal_runtime_handle() {
            Ok(handle) => handle,
            Err(error) => {
                return (
                    GoalContinuationAdmission::Reject {
                        code: GoalContinuationRejectCode::RuntimeUnavailable,
                        message: error.to_string(),
                    },
                    None,
                );
            }
        };
        let snapshot = match handle.continuation_state(&session_id) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                return (
                    GoalContinuationAdmission::Reject {
                        code: GoalContinuationRejectCode::GoalInactive,
                        message: "goal continuation rejected because no goal exists".to_string(),
                    },
                    None,
                );
            }
            Err(error) => {
                return (
                    GoalContinuationAdmission::Reject {
                        code: GoalContinuationRejectCode::RuntimeUnavailable,
                        message: error.to_string(),
                    },
                    None,
                );
            }
        };
        match snapshot.status {
            GoalContinuationStatus::Ready => {
                active.goal_admitted_generation = Some(fence);
                (
                    GoalContinuationAdmission::Admit {
                        reason: orca_core::goal_runtime::GoalContinuationReason::Progress,
                    },
                    Some(snapshot.record.objective),
                )
            }
            GoalContinuationStatus::PendingVerification => (
                GoalContinuationAdmission::Reject {
                    code: GoalContinuationRejectCode::PendingVerification,
                    message: "goal continuation waits for terminal verification".to_string(),
                },
                None,
            ),
            GoalContinuationStatus::OuterTurnInFlight => (
                GoalContinuationAdmission::Reject {
                    code: GoalContinuationRejectCode::RuntimeUnavailable,
                    message: "goal continuation rejected because an outer turn is still in flight"
                        .to_string(),
                },
                None,
            ),
            GoalContinuationStatus::Inactive => {
                let code = if matches!(
                    snapshot.record.state,
                    orca_core::goal_runtime::GoalState::BudgetLimited
                ) {
                    GoalContinuationRejectCode::BudgetLimited
                } else {
                    GoalContinuationRejectCode::GoalInactive
                };
                (
                    GoalContinuationAdmission::Reject {
                        code,
                        message: format!(
                            "goal continuation rejected while state is {:?}",
                            snapshot.record.state
                        ),
                    },
                    None,
                )
            }
        }
    }

    fn persist_queued_goal_input(
        &self,
        state: &mut ThreadActorState,
        active: &ActiveOperation,
        code: GoalContinuationRejectCode,
    ) {
        if code != GoalContinuationRejectCode::QueuedUserInput {
            return;
        }
        for input in active.steer_handle.drain() {
            let message = Message::user(input);
            state
                .thread
                .session_mut()
                .conversation_mut()
                .messages
                .push(message.clone());
            state.thread.session_mut().append_message(&message);
        }
    }

    fn pause_goal_after_rejected_admission(
        &self,
        state: &mut ThreadActorState,
        code: GoalContinuationRejectCode,
        message: &str,
    ) {
        let reason = match code {
            GoalContinuationRejectCode::QueuedUserInput
            | GoalContinuationRejectCode::PendingInteraction
            | GoalContinuationRejectCode::PlanMode => {
                orca_core::goal_runtime::GoalPauseReason::User
            }
            GoalContinuationRejectCode::ActiveWorkflow => {
                orca_core::goal_runtime::GoalPauseReason::WaitingForWorkflow
            }
            GoalContinuationRejectCode::DuplicateAdmission => {
                orca_core::goal_runtime::GoalPauseReason::Infrastructure
            }
            _ => return,
        };
        if let Some(session_id) = state.thread.session().session_id().map(str::to_string)
            && let Ok(handle) = state.thread.goal_runtime_handle()
        {
            let _ = handle.pause(
                &session_id,
                reason,
                message.to_string(),
                chrono::Utc::now().timestamp(),
            );
        }
        state.thread.session_mut().replace_goal_context(None);
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
            response_identity: suspension.identity().clone(),
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

fn cancel_and_join_provider_suspension(suspension: &mut RuntimeProviderSuspension) {
    suspension.cancel();
    loop {
        match suspension.recv_timeout(Duration::from_millis(10)) {
            Ok(RuntimeProviderSuspensionEvent::Completed(_))
            | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(RuntimeProviderSuspensionEvent::Step(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
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
            let turn_request = request.thread_turn_request(generation);
            let event_observer = request.event_observer();
            if request.allows_goal_tools()
                && let Some(session_id) = thread.session().session_id().map(str::to_string)
            {
                let handle = thread.goal_runtime_handle().map_err(io::Error::other)?;
                let goal = handle
                    .project_thread_goal(&session_id)
                    .map_err(io::Error::other)?;
                thread.session_mut().replace_goal_context(
                    goal.as_ref()
                        .map(crate::agent_common::format_goal_mode_instructions),
                );
            } else if !request.allows_goal_tools() {
                thread.session_mut().replace_goal_context(None);
            }
            let binding = thread.begin_goal_turn(&turn_request)?;
            RuntimeThread::emit_goal_turn_started(
                binding.as_ref(),
                events,
                event_observer.as_deref(),
            );
            let usage_before = thread.session().aggregate_usage_totals();
            let outcome = executor.run_turn(thread, request, generation, events, writer, cancel);
            let status = match &outcome {
                Ok(ThreadOperationOutcome::Completed { status, .. }) => *status,
                Ok(ThreadOperationOutcome::ProviderSuspended { .. }) => RunStatus::ApprovalRequired,
                Err(_) => RunStatus::Failed,
            };
            let usage = crate::thread::goal_usage_delta(
                usage_before,
                thread.session().aggregate_usage_totals(),
            );
            thread.finish_goal_turn(
                binding.as_ref(),
                status,
                usage,
                Some(events),
                event_observer.as_deref(),
                generation.config(),
                cancel.clone(),
            );
            if request.allows_goal_tools()
                && let Some(session_id) = thread.session().session_id().map(str::to_string)
            {
                let keep_context = thread
                    .goal_runtime_handle()
                    .ok()
                    .and_then(|handle| handle.read(&session_id).ok().flatten())
                    .is_some_and(|record| record.state.should_continue());
                if !keep_context {
                    thread.session_mut().replace_goal_context(None);
                }
            }
            outcome
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
                        &context.response_identity,
                        buffered_steps.drain(..),
                    );
                    emit_provider_steps(
                        context.observer.as_deref(),
                        &mut events,
                        &context.response_identity,
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

    let usage = response.as_ref().and_then(|response| {
        provider_response_usage_totals(&response.response, context.model.as_deref())
    });
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
        if provider_response_requires_approval(&response.response) {
            status = RunStatus::ApprovalRequired;
            let _ = context
                .task_registry
                .approval_required_for_pending_provider_response_with_usage(
                    &context.task_id,
                    status.as_str().to_string(),
                    response,
                    usage,
                );
        } else if let Some(provider_error) = provider_response_error(&response.response) {
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
        emit_provider_steps(
            context.observer.as_deref(),
            &mut events,
            &context.response_identity,
            buffered_steps,
        );
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
    identity: &ModelResponseIdentity,
    steps: impl IntoIterator<Item = ProviderStep>,
) {
    for step in steps {
        match step {
            ProviderStep::ReasoningDelta(text) => {
                observe_runtime_event(observer, events.assistant_reasoning_delta(identity, &text));
            }
            ProviderStep::MessageDelta(text) => {
                observe_runtime_event(observer, events.assistant_message_delta(identity, &text));
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

fn goal_continuation_prompt(objective: &str, continuation: usize) -> String {
    format!(
        "[Goal continuation #{continuation}]\nContinue working on this persistent goal:\n{objective}\n\nWork from current evidence. Preserve the full objective, verify every requirement before completion, and use update_goal only with structured evidence."
    )
}

fn goal_continuation_reason_name(
    reason: orca_core::goal_runtime::GoalContinuationReason,
) -> &'static str {
    match reason {
        orca_core::goal_runtime::GoalContinuationReason::Initial => "initial",
        orca_core::goal_runtime::GoalContinuationReason::Progress => "progress",
        orca_core::goal_runtime::GoalContinuationReason::GapFeedback => "gap_feedback",
        orca_core::goal_runtime::GoalContinuationReason::Resume => "resume",
        orca_core::goal_runtime::GoalContinuationReason::WorkflowNotification => {
            "workflow_notification"
        }
    }
}

fn goal_continuation_reject_name(code: GoalContinuationRejectCode) -> &'static str {
    match code {
        GoalContinuationRejectCode::GoalInactive => "goal_inactive",
        GoalContinuationRejectCode::Cancelled => "cancelled",
        GoalContinuationRejectCode::NonSuccessfulTurn => "non_successful_turn",
        GoalContinuationRejectCode::QueuedUserInput => "queued_user_input",
        GoalContinuationRejectCode::PendingInteraction => "pending_interaction",
        GoalContinuationRejectCode::ActiveWorkflow => "active_workflow",
        GoalContinuationRejectCode::PlanMode => "plan_mode",
        GoalContinuationRejectCode::DuplicateAdmission => "duplicate_admission",
        GoalContinuationRejectCode::PendingVerification => "pending_verification",
        GoalContinuationRejectCode::BudgetLimited => "budget_limited",
        GoalContinuationRejectCode::RuntimeUnavailable => "runtime_unavailable",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::config::{
        ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc::{Receiver, SyncSender};
    use std::time::Instant;

    use crate::model_response::RuntimeModelResponse;

    const TERMINAL_FAILURE_CHILD_ENV: &str = "ORCA_RUNTIME_HOST_TERMINAL_FAILURE_CHILD";
    const PREPARED_TERMINAL_CHILD_ENV: &str = "ORCA_RUNTIME_HOST_PREPARED_TERMINAL_CHILD";
    const EMPTY_THREAD_OWNER_RECOVERY_CHILD_ENV: &str =
        "ORCA_RUNTIME_HOST_EMPTY_THREAD_OWNER_RECOVERY_CHILD";
    const RESERVATION_TERMINAL_FAILURE_CHILD_ENV: &str =
        "ORCA_RUNTIME_HOST_RESERVATION_TERMINAL_FAILURE_CHILD";
    const SURFACE_TEST_TIMEOUT: Duration = Duration::from_secs(5);

    struct GatedSuccessExecutor {
        entered: SyncSender<()>,
        release: Mutex<Receiver<()>>,
    }

    struct PanicExecutor;

    impl ThreadOperationExecutor for PanicExecutor {
        fn run_turn(
            &self,
            _thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            panic!("recovered terminal operation must not execute")
        }
    }

    impl ThreadOperationExecutor for GatedSuccessExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.entered.send(()).expect("report executor entry");
            self.release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("release successful executor");
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    fn surface_test_config(cwd: PathBuf, history_mode: HistoryMode) -> RunConfig {
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
            history_mode,
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

    fn surface_request_id() -> surface::SurfaceRequestId {
        surface::SurfaceRequestId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
            .expect("generated UUID is v7")
    }

    fn surface_user_turn_intent(
        snapshot: &surface::SurfaceSnapshot,
        text: &str,
    ) -> surface::OperationRequestIntent {
        surface::OperationRequestIntent {
            correlation: surface::OperationIngressCorrelation::TuiUser,
            kind: surface::OperationKind::UserTurn,
            input: Some(surface::SurfaceInputRequest {
                blocks: surface::NonEmptyVec::try_new(vec![
                    surface::SurfaceInputRequestBlock::Text {
                        text: surface::DisplayText::new(text),
                    },
                ])
                .unwrap(),
            }),
            replayability: surface::ReplayabilityRequest::CaptureReplayableCapsule,
            settings_preparation: surface::OperationSettingsPreparation::UseCurrent {
                expected_settings_revision: snapshot.settings.thread_revision,
                expected_policy_epoch: snapshot.settings.effective.policy_epoch,
            },
        }
    }

    fn committed_surface_value<T>(reply: surface::MutationReply<T>) -> T {
        match reply {
            surface::MutationReply::Committed { value, .. } => value,
            surface::MutationReply::Deferred { .. } => panic!("surface mutation was deferred"),
            surface::MutationReply::Uncommitted { .. } => {
                panic!("surface mutation was not committed")
            }
        }
    }

    fn terminal_failure_child_check(condition: bool, message: &str) {
        if !condition {
            eprintln!("{message}");
            std::process::exit(101);
        }
    }

    fn fresh_surface_attachment(
        handle: &surface::RuntimeSurfaceHandle,
    ) -> surface::FreshSurfaceAttachment {
        fresh_surface_attachment_with_capabilities(
            handle,
            BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
                surface::SurfaceCapability::ControlBoundOperation,
            ]),
        )
    }

    fn fresh_surface_attachment_with_capabilities(
        handle: &surface::RuntimeSurfaceHandle,
        requested_capabilities: BTreeSet<surface::SurfaceCapability>,
    ) -> surface::FreshSurfaceAttachment {
        match handle.attach_fresh(surface::FreshAttachRequest {
            request_id: surface_request_id(),
            role: surface::SurfaceAttachmentRole::Tui,
            requested_capabilities,
            interaction_capabilities: BTreeSet::new(),
        }) {
            surface::AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh TUI attachment failed"),
        }
    }

    #[test]
    fn live_terminal_append_failure_wakes_registered_waiter() {
        if let Some(phase) = std::env::var_os(TERMINAL_FAILURE_CHILD_ENV) {
            match phase.to_string_lossy().as_ref() {
                "failure" => run_terminal_failure_child(),
                "recovery" => run_terminal_failure_recovery_child(),
                phase => panic!("unknown terminal failure child phase: {phase}"),
            }
        }

        let home = tempfile::tempdir().unwrap();
        for phase in ["failure", "recovery"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("runtime_host::tests::live_terminal_append_failure_wakes_registered_waiter")
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(TERMINAL_FAILURE_CHILD_ENV, phase)
                .env("ORCA_HOME", home.path())
                .status()
                .expect("start terminal append failure child");
            assert!(
                status.success(),
                "terminal append failure child failed during {phase}"
            );
        }
    }

    #[test]
    fn prepared_terminal_recovery_advances_owner_before_next_reservation() {
        if let Some(phase) = std::env::var_os(PREPARED_TERMINAL_CHILD_ENV) {
            match phase.to_string_lossy().as_ref() {
                "failure" => run_prepared_terminal_failure_child(),
                "recovery" => run_prepared_terminal_recovery_child(),
                phase => panic!("unknown prepared terminal child phase: {phase}"),
            }
        }

        let home = tempfile::tempdir().unwrap();
        for phase in ["failure", "recovery"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "runtime_host::tests::prepared_terminal_recovery_advances_owner_before_next_reservation",
                )
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(PREPARED_TERMINAL_CHILD_ENV, phase)
                .env("ORCA_HOME", home.path())
                .status()
                .expect("start prepared Terminal recovery child");
            assert!(
                status.success(),
                "prepared Terminal recovery child failed during {phase}"
            );
        }
    }

    #[test]
    fn cold_resume_empty_thread_materializes_owner_before_first_reservation() {
        if std::env::var_os(EMPTY_THREAD_OWNER_RECOVERY_CHILD_ENV).is_some() {
            run_empty_thread_owner_recovery_child();
        }

        let home = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(
                "runtime_host::tests::cold_resume_empty_thread_materializes_owner_before_first_reservation",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(EMPTY_THREAD_OWNER_RECOVERY_CHILD_ENV, "1")
            .env("ORCA_HOME", home.path())
            .status()
            .expect("start empty thread owner recovery child");
        assert!(status.success(), "empty thread owner recovery child failed");
    }

    fn run_empty_thread_owner_recovery_child() -> ! {
        let cwd = tempfile::tempdir().unwrap();
        let first_host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start first runtime host");
        let first_thread = first_host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "empty thread owner recovery",
            )
            .expect("record empty runtime thread");
        let thread_id = first_thread.thread_id().to_string();
        let first_attachment = fresh_surface_attachment(&first_thread.surface());
        assert!(
            first_attachment
                .baseline
                .snapshot
                .foreground_operation
                .is_none()
        );
        assert!(
            first_attachment
                .baseline
                .snapshot
                .queued_operations
                .is_empty()
        );
        assert!(
            first_attachment
                .baseline
                .snapshot
                .operation_history
                .is_empty()
        );
        let previous_owner_epoch = first_attachment.baseline.snapshot.thread.owner_epoch;
        first_host
            .shutdown()
            .expect("shutdown empty runtime thread");

        let resumed_host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start resumed runtime host");
        let resumed_thread = resumed_host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "resume empty runtime thread",
            )
            .expect("cold Resume empty runtime thread");
        let attachment = fresh_surface_attachment(&resumed_thread.surface());
        let reserved = match attachment.client.reserve_operation(
            surface_request_id(),
            surface_user_turn_intent(
                &attachment.baseline.snapshot,
                "first operation after Resume",
            ),
        ) {
            Ok(reply) => committed_surface_value(reply),
            Err(error) => {
                eprintln!(
                    "first reservation under acquired owner epoch failed through stale materialization: {error:?}"
                );
                std::process::exit(101);
            }
        };
        terminal_failure_child_check(
            attachment.baseline.snapshot.thread.owner_epoch > previous_owner_epoch,
            "cold Resume did not durably materialize the acquired owner epoch",
        );
        let _ = attachment
            .client
            .cancel_operation(surface_request_id(), reserved.operation_id)
            .expect("terminalize first recovered reservation");
        resumed_host
            .shutdown()
            .expect("shutdown recovered empty thread");
        std::process::exit(0)
    }

    #[test]
    fn pre_admission_terminal_failure_wakes_waiters_and_blocks_admission() {
        if let Some(phase) = std::env::var_os(RESERVATION_TERMINAL_FAILURE_CHILD_ENV) {
            match phase.to_string_lossy().as_ref() {
                "failure" => run_reservation_terminal_failure_child(),
                "recovery" => run_reservation_terminal_recovery_child(),
                phase => panic!("unknown reservation terminal failure child phase: {phase}"),
            }
        }

        let home = tempfile::tempdir().unwrap();
        for phase in ["failure", "recovery"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "runtime_host::tests::pre_admission_terminal_failure_wakes_waiters_and_blocks_admission",
                )
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(RESERVATION_TERMINAL_FAILURE_CHILD_ENV, phase)
                .env("ORCA_HOME", home.path())
                .status()
                .expect("start reservation terminal failure child");
            assert!(
                status.success(),
                "reservation terminal failure child failed during {phase}"
            );
        }
    }

    type ReservationTerminalFailureFixture = (
        String,
        surface::SurfaceOperationId,
        surface::SurfaceFinalizeIntentId,
        surface::SurfaceCommitId,
    );

    fn reservation_terminal_failure_fixture_path() -> PathBuf {
        PathBuf::from(
            std::env::var_os("ORCA_HOME").expect("reservation terminal failure ORCA_HOME"),
        )
        .join("runtime-host-reservation-terminal-failure.json")
    }

    fn run_reservation_terminal_failure_child() -> ! {
        let cwd = tempfile::tempdir().unwrap();
        let host =
            RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "reservation terminal append failure",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "cancel before admit"),
                )
                .expect("reserve operation"),
        );
        let queued = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "remain queued"),
                )
                .expect("reserve queued operation"),
        );

        let waiter_client = attachment.client.clone();
        let waiter_operation_id = reserved.operation_id.clone();
        let (waiter_tx, waiter_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result =
                waiter_client.wait_operation_terminal(surface_request_id(), waiter_operation_id);
            let _ = waiter_tx.send(result);
        });
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        while thread
            .surface_actor_probe_for_test(reserved.operation_id.clone())
            .expect("probe registered waiter")
            .waiter_count
            != 1
        {
            if Instant::now() >= deadline {
                eprintln!("reservation terminal waiter was not registered");
                std::process::exit(101);
            }
            std::thread::yield_now();
        }

        surface::JsonlSurfaceCommitLedger::inject_terminal_append_failure_once(transcript_path);
        terminal_failure_child_check(
            matches!(
                attachment
                    .client
                    .cancel_operation(surface_request_id(), reserved.operation_id.clone()),
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            ),
            "pre-admission cancel unexpectedly committed Terminal",
        );
        let result = match waiter_rx.recv_timeout(SURFACE_TEST_TIMEOUT) {
            Ok(Ok(result)) => result,
            _ => {
                eprintln!("reservation terminal waiter did not receive failure");
                std::process::exit(101);
            }
        };
        terminal_failure_child_check(
            matches!(
                &result,
                surface::WaitOperationTerminalResult::TerminalCommitFailure {
                    operation_id,
                    ..
                } if operation_id == &reserved.operation_id
            ),
            "reservation waiter did not receive TerminalCommitFailure",
        );
        let replayed = attachment
            .client
            .wait_operation_terminal(surface_request_id(), reserved.operation_id.clone())
            .expect("replay reservation TerminalCommitFailure");
        terminal_failure_child_check(
            replayed == result,
            "later reservation waiter did not receive byte-identical failure",
        );
        let failed = fresh_surface_attachment(&surface);
        let operation = failed
            .baseline
            .snapshot
            .foreground_operation
            .iter()
            .chain(failed.baseline.snapshot.queued_operations.iter())
            .chain(failed.baseline.snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == reserved.operation_id)
            .expect("failed reservation remains visible");
        terminal_failure_child_check(
            matches!(operation.phase, surface::OperationPhase::Finalizing { .. }),
            "failed reservation did not remain Finalizing",
        );
        terminal_failure_child_check(
            operation.terminal.is_none(),
            "failed reservation fabricated Terminal",
        );
        terminal_failure_child_check(
            matches!(
                attachment.client.reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&failed.baseline.snapshot, "must reject"),
                ),
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            ),
            "reservation failure barrier accepted new reserve",
        );
        terminal_failure_child_check(
            matches!(
                attachment.client.admit_reserved(
                    surface_request_id(),
                    queued.operation_id,
                    queued.lease.lease_id,
                ),
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            ),
            "reservation failure barrier accepted queued admit",
        );
        let finalization = operation.finalization.as_ref().unwrap();
        std::fs::write(
            reservation_terminal_failure_fixture_path(),
            serde_json::to_vec(&(
                thread.thread_id().to_string(),
                reserved.operation_id,
                finalization.finalize_intent_id.clone(),
                finalization.terminal_commit_id.clone(),
            ))
            .unwrap(),
        )
        .expect("write reservation terminal failure fixture");
        terminal_failure_child_check(
            thread.shutdown().is_err(),
            "thread close crossed reservation terminal failure barrier",
        );
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        terminal_failure_child_check(
            matches!(
                shutdown_rx.recv_timeout(Duration::from_millis(500)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "host shutdown crossed reservation terminal failure barrier",
        );
        std::process::exit(0)
    }

    fn run_reservation_terminal_recovery_child() -> ! {
        let (thread_id, operation_id, finalize_intent_id, terminal_commit_id):
            ReservationTerminalFailureFixture = serde_json::from_slice(
            &std::fs::read(reservation_terminal_failure_fixture_path())
                .expect("read reservation terminal failure fixture"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start recovery runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover reservation Terminal",
            )
            .expect("resume failed reservation");
        let attachment = fresh_surface_attachment(&thread.surface());
        let operation = attachment
            .baseline
            .snapshot
            .foreground_operation
            .iter()
            .chain(attachment.baseline.snapshot.queued_operations.iter())
            .chain(attachment.baseline.snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == operation_id)
            .expect("recovered reservation operation");
        assert!(matches!(operation.phase, surface::OperationPhase::Terminal));
        let finalization = operation.finalization.as_ref().unwrap();
        assert_eq!(finalization.finalize_intent_id, finalize_intent_id);
        assert_eq!(finalization.terminal_commit_id, terminal_commit_id);
        assert!(matches!(
            operation.terminal.as_ref().map(|record| &record.terminal),
            Some(surface::OperationTerminal::NotAdmitted {
                reason: surface::NotAdmittedReason::CancelledBeforeAdmission,
            })
        ));
        host.shutdown().expect("shutdown recovered host");
        std::process::exit(0)
    }

    type PreparedTerminalFixture = (
        String,
        surface::SurfaceOperationId,
        surface::SurfaceFinalizeIntentId,
        surface::SurfaceCommitId,
        surface::ThreadOwnerEpoch,
    );

    fn prepared_terminal_fixture_path() -> PathBuf {
        PathBuf::from(std::env::var_os("ORCA_HOME").expect("prepared Terminal ORCA_HOME"))
            .join("runtime-host-prepared-terminal.json")
    }

    fn run_prepared_terminal_failure_child() -> ! {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let executor = Arc::new(GatedSuccessExecutor {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "prepared terminal owner transition",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "prepare Terminal"),
                )
                .expect("reserve operation"),
        );
        let admission = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    reserved.operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit operation"),
        );
        assert!(matches!(
            admission,
            surface::AdmissionOutput::Admitted { .. }
        ));
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("executor did not start");

        let waiter_client = attachment.client.clone();
        let waiter_operation_id = reserved.operation_id.clone();
        let (waiter_tx, waiter_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result =
                waiter_client.wait_operation_terminal(surface_request_id(), waiter_operation_id);
            let _ = waiter_tx.send(result);
        });
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        while thread
            .surface_actor_probe_for_test(reserved.operation_id.clone())
            .expect("probe registered waiter")
            .waiter_count
            != 1
        {
            if Instant::now() >= deadline {
                eprintln!("prepared Terminal waiter was not registered");
                std::process::exit(101);
            }
            std::thread::yield_now();
        }

        surface::JsonlSurfaceCommitLedger::inject_terminal_checkpoint_failure_once(transcript_path);
        release_tx.send(()).expect("release successful executor");
        let result = match waiter_rx.recv_timeout(SURFACE_TEST_TIMEOUT) {
            Ok(Ok(result)) => result,
            _ => {
                eprintln!("prepared Terminal waiter did not receive failure");
                std::process::exit(101);
            }
        };
        terminal_failure_child_check(
            matches!(
                result,
                surface::WaitOperationTerminalResult::TerminalCommitFailure { .. }
            ),
            "checkpoint failure did not surface TerminalCommitFailure",
        );
        let failed = fresh_surface_attachment(&surface);
        let operation = failed
            .baseline
            .snapshot
            .foreground_operation
            .iter()
            .chain(failed.baseline.snapshot.queued_operations.iter())
            .chain(failed.baseline.snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == reserved.operation_id)
            .expect("prepared Terminal operation remains visible");
        let finalization = operation
            .finalization
            .as_ref()
            .expect("prepared Terminal finalization record");
        std::fs::write(
            prepared_terminal_fixture_path(),
            serde_json::to_vec(&(
                thread.thread_id().to_string(),
                reserved.operation_id,
                finalization.finalize_intent_id.clone(),
                finalization.terminal_commit_id.clone(),
                failed.baseline.snapshot.thread.owner_epoch,
            ))
            .unwrap(),
        )
        .expect("write prepared Terminal fixture");
        std::process::exit(0)
    }

    fn run_prepared_terminal_recovery_child() -> ! {
        let (thread_id, operation_id, finalize_intent_id, terminal_commit_id, previous_owner_epoch):
            PreparedTerminalFixture = serde_json::from_slice(
            &std::fs::read(prepared_terminal_fixture_path())
                .expect("read prepared Terminal fixture"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start recovery runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover prepared Terminal",
            )
            .expect("resume prepared Terminal operation");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let operation = attachment
            .baseline
            .snapshot
            .foreground_operation
            .iter()
            .chain(attachment.baseline.snapshot.queued_operations.iter())
            .chain(attachment.baseline.snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == operation_id)
            .expect("recovered prepared Terminal operation");
        assert!(matches!(operation.phase, surface::OperationPhase::Terminal));
        let finalization = operation.finalization.as_ref().unwrap();
        assert_eq!(finalization.finalize_intent_id, finalize_intent_id);
        assert_eq!(finalization.terminal_commit_id, terminal_commit_id);
        assert!(
            attachment.baseline.snapshot.thread.owner_epoch > previous_owner_epoch,
            "cold recovery did not materialize the acquired owner epoch"
        );
        let next = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "next operation"),
                )
                .expect("reserve under recovered owner epoch"),
        );
        let _ = attachment
            .client
            .cancel_operation(surface_request_id(), next.operation_id)
            .expect("terminalize next reservation");
        host.shutdown().expect("shutdown recovered host");
        std::process::exit(0)
    }

    type TerminalFailureFixture = (
        String,
        surface::SurfaceOperationId,
        surface::SurfaceFinalizeIntentId,
        surface::SurfaceCommitId,
    );

    fn terminal_failure_fixture_path() -> PathBuf {
        PathBuf::from(std::env::var_os("ORCA_HOME").expect("terminal failure ORCA_HOME"))
            .join("runtime-host-terminal-failure.json")
    }

    fn run_terminal_failure_child() -> ! {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let executor = Arc::new(GatedSuccessExecutor {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "terminal append failure",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "finish durably"),
                )
                .expect("reserve operation"),
        );
        let queued = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "remain queued"),
                )
                .expect("reserve queued operation"),
        );
        let admission = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    reserved.operation_id.clone(),
                    reserved.lease.lease_id.clone(),
                )
                .expect("admit operation"),
        );
        assert!(matches!(
            admission,
            surface::AdmissionOutput::Admitted { .. }
        ));
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("executor did not start");
        let completion = thread
            .surface_actor_probe_for_test(reserved.operation_id.clone())
            .expect("probe active surface operation")
            .legacy_completion
            .expect("active legacy completion");

        let waiter_client = attachment.client.clone();
        let waiter_operation_id = reserved.operation_id.clone();
        let (waiter_tx, waiter_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result =
                waiter_client.wait_operation_terminal(surface_request_id(), waiter_operation_id);
            let _ = waiter_tx.send(result);
        });
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        loop {
            let probe = thread
                .surface_actor_probe_for_test(reserved.operation_id.clone())
                .expect("probe registered waiter");
            if probe.waiter_count == 1 {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("registered terminal waiter was not observed");
                std::process::exit(101);
            }
            std::thread::yield_now();
        }

        surface::JsonlSurfaceCommitLedger::inject_terminal_append_failure_once(transcript_path);
        release_tx.send(()).expect("release successful executor");
        let result = match waiter_rx.recv_timeout(SURFACE_TEST_TIMEOUT) {
            Ok(result) => result.expect("wait operation terminal command"),
            Err(error) => {
                eprintln!("registered terminal waiter was not woken: {error}");
                std::process::exit(101);
            }
        };
        terminal_failure_child_check(
            matches!(
                &result,
                surface::WaitOperationTerminalResult::TerminalCommitFailure {
                    operation_id,
                    ..
                } if operation_id == &reserved.operation_id
            ),
            "registered waiter did not receive TerminalCommitFailure",
        );
        let replayed = attachment
            .client
            .wait_operation_terminal(surface_request_id(), reserved.operation_id.clone())
            .expect("replay terminal commit failure");
        terminal_failure_child_check(
            replayed == result,
            "later waiter did not receive the byte-identical terminal commit failure",
        );
        terminal_failure_child_check(
            completion.try_terminal().is_none(),
            "legacy completion was set before durable Terminal",
        );
        let after_failure = fresh_surface_attachment(&surface);
        let operation = after_failure
            .baseline
            .snapshot
            .foreground_operation
            .iter()
            .chain(after_failure.baseline.snapshot.queued_operations.iter())
            .chain(after_failure.baseline.snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == reserved.operation_id)
            .expect("failed operation remains in snapshot");
        terminal_failure_child_check(
            matches!(operation.phase, surface::OperationPhase::Finalizing { .. }),
            "durable operation did not remain Finalizing",
        );
        terminal_failure_child_check(
            operation.terminal.is_none(),
            "durable operation exposed Terminal after failed append",
        );
        let repair = match &result {
            surface::WaitOperationTerminalResult::TerminalCommitFailure { repair, .. } => {
                repair.clone()
            }
            _ => unreachable!(),
        };
        terminal_failure_child_check(
            matches!(
                attachment.client.retry_finalization(repair.clone()),
                Err(surface::SurfaceClientCommandError::Unauthorized)
            ),
            "RetryFinalization bypassed RepairThread capability",
        );
        let repair_attachment = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::RepairThread,
            ]),
        );
        let exact_retry = repair_attachment
            .client
            .retry_finalization(repair)
            .expect("dispatch exact RetryFinalization");
        terminal_failure_child_check(
            matches!(
                exact_retry,
                surface::MutationReply::Uncommitted {
                    mutation: surface::UncommittedMutation::Invalid { ref error, .. },
                } if error.error().code == surface::SurfaceMutationErrorCode::IllegalState
            ),
            "exact live RetryFinalization did not fail closed in durable Finalizing",
        );
        let stale_retry = repair_attachment
            .client
            .retry_finalization(surface::RetryFinalizationToken::new(
                surface_request_id(),
                after_failure.baseline.snapshot.thread.thread_id.clone(),
                queued.operation_id.clone(),
                surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                after_failure.baseline.snapshot.thread.owner_epoch,
                surface::Sha256Digest::new([0; 32]),
            ))
            .expect("dispatch stale RetryFinalization");
        terminal_failure_child_check(
            matches!(stale_retry, surface::MutationReply::Uncommitted { .. }),
            "stale live RetryFinalization mutated the durable operation",
        );
        let after_retry = fresh_surface_attachment(&surface);
        terminal_failure_child_check(
            after_retry.baseline.cursor == after_failure.baseline.cursor,
            "RetryFinalization changed the durable cursor",
        );

        terminal_failure_child_check(
            matches!(
                attachment.client.reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&after_failure.baseline.snapshot, "must be rejected"),
                ),
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            ),
            "terminal commit barrier accepted a new reservation",
        );
        terminal_failure_child_check(
            matches!(
                attachment.client.admit_reserved(
                    surface_request_id(),
                    queued.operation_id.clone(),
                    queued.lease.lease_id,
                ),
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            ),
            "terminal commit barrier accepted queued admission",
        );
        let finalization = operation
            .finalization
            .as_ref()
            .expect("Finalizing operation has finalization record");
        std::fs::write(
            terminal_failure_fixture_path(),
            serde_json::to_vec(&(
                thread.thread_id().to_string(),
                reserved.operation_id.clone(),
                finalization.finalize_intent_id.clone(),
                finalization.terminal_commit_id.clone(),
            ))
            .unwrap(),
        )
        .expect("write terminal failure fixture");
        terminal_failure_child_check(
            thread.shutdown().is_err(),
            "thread close succeeded through terminal commit barrier",
        );
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        terminal_failure_child_check(
            matches!(
                shutdown_rx.recv_timeout(Duration::from_millis(500)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "host shutdown completed through terminal commit barrier",
        );
        std::process::exit(0)
    }

    fn run_terminal_failure_recovery_child() -> ! {
        let (thread_id, operation_id, finalize_intent_id, terminal_commit_id):
            TerminalFailureFixture = serde_json::from_slice(
            &std::fs::read(terminal_failure_fixture_path())
                .expect("read terminal failure fixture"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start recovery runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover terminal append failure",
            )
            .expect("resume Finalizing operation");
        let attachment = fresh_surface_attachment(&thread.surface());
        let operation = attachment
            .baseline
            .snapshot
            .foreground_operation
            .iter()
            .chain(attachment.baseline.snapshot.queued_operations.iter())
            .chain(attachment.baseline.snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == operation_id)
            .expect("recovered operation remains visible");
        assert!(matches!(operation.phase, surface::OperationPhase::Terminal));
        let finalization = operation
            .finalization
            .as_ref()
            .expect("recovered operation finalization record");
        assert_eq!(finalization.finalize_intent_id, finalize_intent_id);
        assert_eq!(finalization.terminal_commit_id, terminal_commit_id);
        assert_eq!(
            operation
                .terminal
                .as_ref()
                .expect("recovered durable Terminal")
                .finalize_intent_id,
            finalize_intent_id
        );
        let terminal = match attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id.clone())
            .expect("wait recovered Terminal")
        {
            surface::WaitOperationTerminalResult::Terminal { value } => value,
            _ => panic!("recovered operation did not return Terminal"),
        };
        assert!(matches!(
            terminal.commit_class,
            surface::CommitClass::Recorded { commit_id, .. }
                if commit_id == terminal_commit_id
        ));
        host.shutdown().expect("shutdown recovered host");
        std::process::exit(0)
    }

    #[test]
    fn goal_continuation_preflight_has_no_outer_turn_limit() {
        let baseline = GoalContinuationPreflight {
            cancelled: false,
            successful_turn: true,
            queued_user_input: false,
            pending_interaction: false,
            active_workflow: false,
            plan_mode: false,
            duplicate_admission: false,
        };
        let cases = [
            (
                GoalContinuationPreflight {
                    queued_user_input: true,
                    ..baseline
                },
                GoalContinuationRejectCode::QueuedUserInput,
            ),
            (
                GoalContinuationPreflight {
                    pending_interaction: true,
                    ..baseline
                },
                GoalContinuationRejectCode::PendingInteraction,
            ),
            (
                GoalContinuationPreflight {
                    plan_mode: true,
                    ..baseline
                },
                GoalContinuationRejectCode::PlanMode,
            ),
            (
                GoalContinuationPreflight {
                    duplicate_admission: true,
                    ..baseline
                },
                GoalContinuationRejectCode::DuplicateAdmission,
            ),
        ];

        for (input, expected) in cases {
            assert!(matches!(
                goal_continuation_preflight(input),
                Some(GoalContinuationAdmission::Reject { code, .. }) if code == expected
            ));
        }
        assert_eq!(goal_continuation_preflight(baseline), None);
    }

    #[test]
    fn background_continuation_reuses_pending_response_turn_identity() {
        let registry = TaskRegistry::new("background-continuation-identity".to_string());
        let task = registry.create_main_session("continue after approval".to_string());
        registry.mark_running(&task.id).expect("mark task running");
        registry
            .mark_backgrounded(&task.id)
            .expect("mark task backgrounded");

        let response_turn_id = TurnId::new();
        let response = RuntimeModelResponse::new(
            ProviderResponse {
                steps: vec![ProviderStep::ToolCall(ToolRequest {
                    id: "tool-1".to_string(),
                    name: ToolName::TaskList,
                    action: ActionKind::Read,
                    target: None,
                    raw_arguments: Some("{}".to_string()),
                })],
                assistant_content: Some("I need approval.".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            },
            response_turn_id.clone(),
        );
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                response,
            )
            .expect("persist pending response");
        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .expect("approve pending tool");

        let mut request = HostedTurnRequest::new("").with_operation_kind(
            HostedOperationKind::BackgroundContinuation {
                task_id: task.id.clone(),
            },
        );
        assert_ne!(request.turn_id(), &response_turn_id);

        request
            .prepare_background_continuation(&registry)
            .expect("prepare continuation");

        assert_eq!(request.turn_id(), &response_turn_id);
        assert_eq!(
            request
                .continuation
                .as_ref()
                .expect("runtime continuation")
                .response
                .identity
                .turn_id,
            response_turn_id
        );
    }
}
