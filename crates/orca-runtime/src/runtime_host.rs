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
use orca_core::thread_item_projection::{CompletedModelItem, ModelResponseIdentity};
use orca_core::workflow_types::{WorkflowInput, WorkflowOutput};
use orca_mcp::{McpElicitationHandler, McpRegistry};
use serde_json::Value;
use sha2::{Digest, Sha256};
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
const SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS: usize = 3;

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
    provider_response_ingress: Option<Arc<dyn surface::RuntimeProviderResponseIngress>>,
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
            .field(
                "provider_response_ingress",
                &self.provider_response_ingress.is_some(),
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

    pub fn with_provider_response_ingress(
        mut self,
        ingress: Arc<dyn surface::RuntimeProviderResponseIngress>,
    ) -> Self {
        self.provider_response_ingress = Some(ingress);
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
        if let Some(ingress) = generation.handlers.provider_response_ingress.clone() {
            request = request.with_provider_response_ingress(ingress);
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

    pub fn user_input_handler(&self) -> Option<&dyn RuntimeUserInputHandler> {
        self.handlers
            .user_input_handler
            .as_deref()
            .map(|handler| handler as &dyn RuntimeUserInputHandler)
    }

    pub fn mcp_elicitation_handler(&self) -> Option<&(dyn McpElicitationHandler + Send + Sync)> {
        self.handlers.mcp_elicitation_handler.as_deref()
    }
}

struct RuntimeSurfaceUserInputHandler {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    fence: surface::SurfaceOperationFence,
}

impl RuntimeUserInputHandler for RuntimeSurfaceUserInputHandler {
    fn request_user_input(
        &self,
        request: &crate::lifecycle::RuntimeUserInputRequest,
    ) -> io::Result<Option<String>> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceRequestUserInput {
                fence: self.fence.clone(),
                request: request.clone(),
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "runtime interaction mailbox is full",
                ),
                TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime interaction actor is unavailable",
                ),
            })?;
        reply_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "runtime interaction actor closed while waiting for user input",
            )
        })?
    }
}

struct RuntimeSurfaceMcpElicitationHandler {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    fence: surface::SurfaceOperationFence,
}

struct RuntimeSurfaceApprovalHandler {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    fence: surface::SurfaceOperationFence,
}

impl RuntimeApprovalHandler for RuntimeSurfaceApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &orca_core::approval_types::ApprovalRequest,
        request: &orca_core::tool_types::ToolRequest,
    ) -> io::Result<orca_core::approval_types::ApprovalResolution> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceRequestToolApproval {
                fence: self.fence.clone(),
                approval: approval.clone(),
                request: request.clone(),
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "runtime interaction mailbox is full",
                ),
                TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime interaction actor is unavailable",
                ),
            })?;
        reply_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "runtime interaction actor closed while waiting for tool approval",
            )
        })?
    }
}

struct RuntimeSurfacePermissionHandler {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    fence: surface::SurfaceOperationFence,
}

impl RuntimePermissionRequestHandler for RuntimeSurfacePermissionHandler {
    fn request_permissions(
        &self,
        request: &crate::runtime_permission::RuntimePermissionRequest,
    ) -> io::Result<crate::runtime_permission::RuntimePermissionResponse> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceRequestPermission {
                fence: self.fence.clone(),
                request: request.clone(),
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "runtime interaction mailbox is full",
                ),
                TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime interaction actor is unavailable",
                ),
            })?;
        reply_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "runtime interaction actor closed while waiting for permission response",
            )
        })?
    }
}

impl McpElicitationHandler for RuntimeSurfaceMcpElicitationHandler {
    fn handle_elicitation(
        &self,
        request: orca_mcp::McpElicitationRequest,
    ) -> Result<orca_mcp::McpElicitationResponse, String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceRequestMcpElicitation {
                fence: self.fence.clone(),
                request,
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "runtime interaction mailbox is full".to_string(),
                TrySendError::Closed(_) => "runtime interaction actor is unavailable".to_string(),
            })?;
        reply_rx.recv().map_err(|_| {
            "runtime interaction actor closed while waiting for MCP elicitation".to_string()
        })?
    }
}

#[derive(Debug)]
struct RuntimeSurfaceProviderResponseIngress {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    fence: surface::SurfaceOperationFence,
}

impl surface::RuntimeProviderResponseIngress for RuntimeSurfaceProviderResponseIngress {
    fn commit_response(
        &self,
        response: &crate::model_response::RuntimeModelResponse,
    ) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceCommitProviderResponse {
                fence: self.fence.clone(),
                response: response.clone(),
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "runtime semantic ingress mailbox is full",
                ),
                TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime semantic ingress actor is unavailable",
                ),
            })?;
        reply_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "runtime semantic ingress actor closed before commit acknowledgement",
            )
        })?
    }

    fn commit_provider_step(
        &self,
        identity: &orca_core::thread_item_projection::ModelResponseIdentity,
        step: &ProviderStep,
    ) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceCommitProviderStep {
                fence: self.fence.clone(),
                identity: identity.clone(),
                step: step.clone(),
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "runtime semantic ingress mailbox is full",
                ),
                TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime semantic ingress actor is unavailable",
                ),
            })?;
        reply_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "runtime semantic ingress actor closed before commit acknowledgement",
            )
        })?
    }

    fn commit_tool_results(&self, results: &[orca_core::tool_types::ToolResult]) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.command_tx
            .try_send(ThreadCommand::SurfaceCommitToolResults {
                fence: self.fence.clone(),
                results: results.to_vec(),
                reply: reply_tx,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "runtime semantic ingress mailbox is full",
                ),
                TrySendError::Closed(_) => io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime semantic ingress actor is unavailable",
                ),
            })?;
        reply_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "runtime semantic ingress actor closed before commit acknowledgement",
            )
        })?
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

fn surface_history_messages(
    messages: &[Message],
) -> Result<Vec<crate::runtime_surface::SurfaceHistoryMessage>, RuntimeHostError> {
    messages
        .iter()
        .map(|message| match message {
            Message::System { content, .. } => {
                Ok(crate::runtime_surface::SurfaceHistoryMessage::System {
                    role: crate::runtime_surface::SurfaceHistorySystemRole::System,
                    content: crate::runtime_surface::DisplayText::new(content.clone()),
                })
            }
            Message::User { content, .. } => {
                Ok(crate::runtime_surface::SurfaceHistoryMessage::User {
                    role: crate::runtime_surface::SurfaceHistoryUserRole::User,
                    content: crate::runtime_surface::DisplayText::new(content.clone()),
                })
            }
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => Ok(crate::runtime_surface::SurfaceHistoryMessage::Assistant {
                role: crate::runtime_surface::SurfaceHistoryAssistantRole::Assistant,
                content: content
                    .as_ref()
                    .map(|value| crate::runtime_surface::DisplayText::new(value.clone())),
                reasoning_content: reasoning_content
                    .as_ref()
                    .map(|value| crate::runtime_surface::DisplayText::new(value.clone())),
                tool_calls: tool_calls
                    .iter()
                    .map(|tool_call| {
                        crate::runtime_surface::SurfaceDataValue::Object(vec![
                            crate::runtime_surface::SurfaceDataProperty {
                                name: crate::runtime_surface::DisplayText::new("id"),
                                value: Box::new(crate::runtime_surface::SurfaceDataValue::String(
                                    crate::runtime_surface::DisplayText::new(tool_call.id.clone()),
                                )),
                            },
                            crate::runtime_surface::SurfaceDataProperty {
                                name: crate::runtime_surface::DisplayText::new("name"),
                                value: Box::new(crate::runtime_surface::SurfaceDataValue::String(
                                    crate::runtime_surface::DisplayText::new(
                                        tool_call.function_name.clone(),
                                    ),
                                )),
                            },
                            crate::runtime_surface::SurfaceDataProperty {
                                name: crate::runtime_surface::DisplayText::new("arguments"),
                                value: Box::new(crate::runtime_surface::SurfaceDataValue::String(
                                    crate::runtime_surface::DisplayText::new(
                                        tool_call.arguments.clone(),
                                    ),
                                )),
                            },
                        ])
                    })
                    .collect(),
            }),
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                let id = crate::runtime_surface::SurfaceHistoryId::try_new(tool_call_id.clone())
                    .map_err(|error| RuntimeHostError::ThreadStartFailed {
                        message: format!("invalid persisted tool call id: {error:?}"),
                    })?;
                Ok(crate::runtime_surface::SurfaceHistoryMessage::Tool {
                    role: crate::runtime_surface::SurfaceHistoryToolRole::Tool,
                    tool_call_id: id,
                    content: crate::runtime_surface::DisplayText::new(content.clone()),
                })
            }
        })
        .collect()
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

    pub(crate) fn acp_surface(&self) -> Option<surface::RuntimeSurfaceHandle> {
        let authority = surface::SurfaceAttachAuthority::new(
            self.surface.host_incarnation().clone(),
            self.surface.thread_id().clone(),
            surface::SurfaceAttachmentRole::Acp,
            surface::NonEmptySet::try_new(BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
                surface::SurfaceCapability::ControlBoundOperation,
                surface::SurfaceCapability::RespondGrantedInteraction,
                surface::SurfaceCapability::RepairThread,
            ]))
            .expect("ACP surface grant is non-empty"),
            surface::NonEmptySet::try_new(BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
            ]))
            .expect("ACP surface required grant is non-empty"),
            BTreeSet::from([
                surface::SurfaceInteractionKind::ToolApproval,
                surface::SurfaceInteractionKind::PermissionRequest,
                surface::SurfaceInteractionKind::UserInput,
                surface::SurfaceInteractionKind::McpElicitation,
            ]),
        );
        self.surface.with_authority(authority)
    }

    #[allow(dead_code)]
    pub(crate) fn jsonl_surface(&self) -> Option<surface::RuntimeSurfaceHandle> {
        self.jsonl_surface_with_connection(None)
    }

    pub(crate) fn jsonl_surface_for_connection(
        &self,
        connection_id: surface::SurfaceConnectionId,
    ) -> Option<surface::RuntimeSurfaceHandle> {
        self.jsonl_surface_with_connection(Some(connection_id))
    }

    fn jsonl_surface_with_connection(
        &self,
        connection_id: Option<surface::SurfaceConnectionId>,
    ) -> Option<surface::RuntimeSurfaceHandle> {
        let authority = surface::SurfaceAttachAuthority::new(
            self.surface.host_incarnation().clone(),
            self.surface.thread_id().clone(),
            surface::SurfaceAttachmentRole::Jsonl,
            surface::NonEmptySet::try_new(BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
                surface::SurfaceCapability::ControlBoundOperation,
                surface::SurfaceCapability::RespondGrantedInteraction,
                surface::SurfaceCapability::RepairThread,
            ]))
            .expect("JSONL surface grant is non-empty"),
            surface::NonEmptySet::try_new(BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
            ]))
            .expect("JSONL surface required grant is non-empty"),
            BTreeSet::from([
                surface::SurfaceInteractionKind::ToolApproval,
                surface::SurfaceInteractionKind::PermissionRequest,
                surface::SurfaceInteractionKind::UserInput,
                surface::SurfaceInteractionKind::McpElicitation,
            ]),
        );
        let authority = match connection_id {
            Some(connection_id) => authority.with_connection_id(connection_id),
            None => authority,
        };
        self.surface.with_authority(authority)
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

    pub fn read_surface_history(
        &self,
    ) -> Result<Vec<crate::runtime_surface::SurfaceHistoryMessage>, RuntimeHostError> {
        let snapshot = self.snapshot()?;
        surface_history_messages(snapshot.messages())
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

    #[cfg(test)]
    fn suspend_surface_operation_for_test(
        &self,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::SurfaceSuspendOperationForTest {
            operation_id,
            reply: reply_tx,
        })
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        receive_reply(reply_rx, "runtime thread")
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
    }

    #[cfg(test)]
    fn respond_surface_interaction_for_test(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        selector: surface::InteractionSelector,
        response: surface::BoundInteractionResponse,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::SurfaceRespondInteraction {
            client,
            request_id,
            selector,
            response,
            reply: reply_tx,
        })
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        receive_reply(reply_rx, "runtime thread")
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
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
        match receive_reply(reply_rx, "runtime thread")? {
            ThreadShutdownAck::Complete => Ok(()),
            ThreadShutdownAck::Retry => Err(RuntimeHostError::ThreadStartFailed {
                message: "runtime thread shutdown is retrying a prepared terminalization"
                    .to_string(),
            }),
            ThreadShutdownAck::Failed(error) => Err(error),
        }
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
    host_incarnation: surface::HostIncarnation,
}

#[derive(Clone)]
pub struct RuntimeHostHandle {
    command_tx: tokio_mpsc::Sender<HostCommand>,
    host_incarnation: surface::HostIncarnation,
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

    pub(crate) fn host_incarnation(&self) -> &surface::HostIncarnation {
        &self.host_incarnation
    }
}

impl RuntimeHost {
    pub fn start() -> Result<Self, RuntimeHostError> {
        Self::start_with_background_capacity(HOST_BACKGROUND_TASK_CAPACITY)
    }

    pub fn start_with_background_capacity(
        background_capacity: usize,
    ) -> Result<Self, RuntimeHostError> {
        Self::start_inner(
            Arc::new(LegacyThreadOperationExecutor),
            background_capacity,
            surface::SurfaceHubConfig::default(),
        )
    }

    pub fn start_with_executor(
        executor: Arc<dyn ThreadOperationExecutor>,
    ) -> Result<Self, RuntimeHostError> {
        Self::start_inner(
            executor,
            HOST_BACKGROUND_TASK_CAPACITY,
            surface::SurfaceHubConfig::default(),
        )
    }

    #[cfg(test)]
    fn start_with_executor_and_surface_config(
        executor: Arc<dyn ThreadOperationExecutor>,
        surface_hub_config: surface::SurfaceHubConfig,
    ) -> Result<Self, RuntimeHostError> {
        Self::start_inner(executor, HOST_BACKGROUND_TASK_CAPACITY, surface_hub_config)
    }

    fn start_inner(
        executor: Arc<dyn ThreadOperationExecutor>,
        background_capacity: usize,
        surface_hub_config: surface::SurfaceHubConfig,
    ) -> Result<Self, RuntimeHostError> {
        let host_incarnation =
            surface::HostIncarnation::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated host incarnation is v7");
        let supervisor_host_incarnation = host_incarnation.clone();
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
                            surface_hub_config,
                            supervisor_host_incarnation,
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
                host_incarnation,
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
            host_incarnation: self.host_incarnation.clone(),
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
    SurfaceDetach {
        client: surface::RuntimeSurfaceClientHandle,
        request: surface::DetachRequest,
        reply: SyncSender<surface::DetachResult>,
    },
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
    SurfaceAdmitReservedWithOutput {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        writer: Box<dyn HostedOperationWriter>,
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
    SurfaceResumeOperation {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        expected_last_generation: surface::SurfaceGenerationId,
        resume_source: surface::ResumeSourceWitness,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::ResumeOperationOutput>,
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
    SurfaceUpdateSettings {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        expected_thread_revision: surface::SettingsRevision,
        patch: surface::NonEmptyVec<surface::RuntimeSettingsPatch>,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::SettingsMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfacePinnedContextMutation {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::PinnedContextAction,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::PinnedContextMutationOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfaceCommitProviderResponse {
        fence: surface::SurfaceOperationFence,
        response: crate::model_response::RuntimeModelResponse,
        reply: SyncSender<io::Result<()>>,
    },
    SurfaceCommitProviderStep {
        fence: surface::SurfaceOperationFence,
        identity: orca_core::thread_item_projection::ModelResponseIdentity,
        step: ProviderStep,
        reply: SyncSender<io::Result<()>>,
    },
    SurfaceCommitToolResults {
        fence: surface::SurfaceOperationFence,
        results: Vec<orca_core::tool_types::ToolResult>,
        reply: SyncSender<io::Result<()>>,
    },
    SurfaceRequestToolApproval {
        fence: surface::SurfaceOperationFence,
        approval: orca_core::approval_types::ApprovalRequest,
        request: orca_core::tool_types::ToolRequest,
        reply: SyncSender<io::Result<orca_core::approval_types::ApprovalResolution>>,
    },
    SurfaceRequestPermission {
        fence: surface::SurfaceOperationFence,
        request: crate::runtime_permission::RuntimePermissionRequest,
        reply: SyncSender<io::Result<crate::runtime_permission::RuntimePermissionResponse>>,
    },
    SurfaceRequestUserInput {
        fence: surface::SurfaceOperationFence,
        request: crate::lifecycle::RuntimeUserInputRequest,
        reply: SyncSender<io::Result<Option<String>>>,
    },
    SurfaceRequestMcpElicitation {
        fence: surface::SurfaceOperationFence,
        request: orca_mcp::McpElicitationRequest,
        reply: SyncSender<Result<orca_mcp::McpElicitationResponse, String>>,
    },
    #[cfg(test)]
    SurfaceRespondInteraction {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        selector: surface::InteractionSelector,
        response: surface::BoundInteractionResponse,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::RespondInteractionOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfaceRespondInteractionById {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::RespondInteractionOutput>,
                surface::SurfaceClientCommandError,
            >,
        >,
    },
    SurfaceRespondInteractionByIdWithPolicy {
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
        policy: surface::BrokerInteractionAnswerPolicy,
        reply: SyncSender<
            Result<
                surface::MutationReply<surface::RespondInteractionOutput>,
                surface::SurfaceClientCommandError,
            >,
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
    SurfaceSuspendOperationForTest {
        operation_id: surface::SurfaceOperationId,
        reply: SyncSender<Result<(), surface::SurfaceClientCommandError>>,
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
        reply: Option<SyncSender<ThreadShutdownAck>>,
        reason: surface::SurfaceShutdownReason,
    },
}

enum ThreadShutdownAck {
    Complete,
    Retry,
    Failed(RuntimeHostError),
}

#[cfg(test)]
struct SurfaceActorTestProbe {
    waiter_count: usize,
    legacy_completion: Option<OperationCompletion>,
    exact_interaction_selector: Option<surface::InteractionSelector>,
    secret_bearing_interaction_count: usize,
    pending_capability_loss: Option<PendingCapabilityLossTestProbe>,
    pending_terminalization: Option<PendingTerminalizationTestProbe>,
    interaction_admission_closed: bool,
}

#[cfg(test)]
struct PendingCapabilityLossTestProbe {
    attachment_id: surface::SurfaceAttachmentId,
    commit_id: surface::SurfaceCommitId,
    cursor_after: surface::SurfaceCursor,
    batch_digest: surface::Sha256Digest,
}

#[cfg(test)]
struct PendingTerminalizationTestProbe {
    commit_id: surface::SurfaceCommitId,
    cursor_before: surface::SurfaceCursor,
    cursor_after: surface::SurfaceCursor,
    batch_digest: surface::Sha256Digest,
}

struct ThreadActorEntry {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    join: JoinHandle<()>,
}

enum HostShutdownActorState {
    NeedsDispatch,
    Awaiting(mpsc::Receiver<ThreadShutdownAck>),
    Settled,
}

struct HostShutdownActor {
    thread_id: String,
    state: HostShutdownActorState,
}

async fn run_host_supervisor(
    mut command_rx: tokio_mpsc::Receiver<HostCommand>,
    executor: Arc<dyn ThreadOperationExecutor>,
    background_capacity: usize,
    surface_hub_config: surface::SurfaceHubConfig,
    host_incarnation: surface::HostIncarnation,
) {
    let mut actors = HashMap::<String, ThreadActorEntry>::new();
    while let Some(command) = command_rx.recv().await {
        match command {
            HostCommand::StartThread { request, reply } => {
                let mut actor_config = request.config.clone();
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
                let mut thread = match started {
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
                let (capability_change_tx, capability_change_rx) = tokio_mpsc::channel(1);
                let (surface_handle, resident_surface) = if let Some(surface_owner) = surface_owner
                {
                    match bootstrap_recorded_surface(
                        &mut thread,
                        &actor_config,
                        &actor_title,
                        host_incarnation.clone(),
                        command_tx.clone(),
                        capability_change_tx.clone(),
                        surface_owner,
                        surface_hub_config,
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
                if let Some(resident) = resident_surface.as_ref() {
                    if let Err(error) = hydrate_run_config_from_surface_settings(
                        &mut actor_config,
                        &resident.coordinator.state().snapshot().settings.effective,
                    ) {
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: format!("failed to restore runtime settings: {error:?}"),
                        }));
                        continue;
                    }
                }
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
                    .run(actor_rx, capability_change_rx)
                    .await;
                });
                actors.insert(thread_id, ThreadActorEntry { command_tx, join });
                let _ = reply.send(Ok(handle));
            }
            HostCommand::Shutdown { reply } => {
                let mut shutdown_actors = actors
                    .keys()
                    .cloned()
                    .map(|thread_id| HostShutdownActor {
                        thread_id,
                        state: HostShutdownActorState::NeedsDispatch,
                    })
                    .collect::<Vec<_>>();
                shutdown_actors.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
                let mut actor_error = None;
                while shutdown_actors
                    .iter()
                    .any(|actor| !matches!(actor.state, HostShutdownActorState::Settled))
                {
                    for shutdown_actor in &mut shutdown_actors {
                        if !matches!(shutdown_actor.state, HostShutdownActorState::NeedsDispatch) {
                            continue;
                        }
                        let actor = actors
                            .get(&shutdown_actor.thread_id)
                            .expect("selected runtime actor remains registered");
                        if actor.join.is_finished() {
                            shutdown_actor.state = HostShutdownActorState::Settled;
                            continue;
                        }
                        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
                        match actor.command_tx.try_send(ThreadCommand::ShutdownThread {
                            reply: Some(reply_tx),
                            reason: surface::SurfaceShutdownReason::HostShutdown,
                        }) {
                            Ok(()) => {
                                shutdown_actor.state = HostShutdownActorState::Awaiting(reply_rx);
                            }
                            Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Closed(_)) => {
                                shutdown_actor.state = HostShutdownActorState::Settled;
                            }
                        }
                    }
                    for shutdown_actor in &mut shutdown_actors {
                        let HostShutdownActorState::Awaiting(reply_rx) = &shutdown_actor.state
                        else {
                            continue;
                        };
                        let transition = match reply_rx.try_recv() {
                            Ok(ThreadShutdownAck::Complete) => {
                                Some((HostShutdownActorState::Settled, None))
                            }
                            Ok(ThreadShutdownAck::Retry) => {
                                Some((HostShutdownActorState::NeedsDispatch, None))
                            }
                            Ok(ThreadShutdownAck::Failed(error)) => {
                                Some((HostShutdownActorState::Settled, Some(error)))
                            }
                            Err(mpsc::TryRecvError::Empty) => None,
                            Err(mpsc::TryRecvError::Disconnected) => Some((
                                HostShutdownActorState::Settled,
                                Some(RuntimeHostError::ResponseChannelClosed {
                                    owner: "runtime thread",
                                }),
                            )),
                        };
                        if let Some((state, error)) = transition {
                            shutdown_actor.state = state;
                            if actor_error.is_none() {
                                actor_error = error;
                            }
                        }
                    }
                    if shutdown_actors
                        .iter()
                        .any(|actor| !matches!(actor.state, HostShutdownActorState::Settled))
                    {
                        tokio::time::sleep(SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL).await;
                    }
                }
                for (thread_id, actor) in actors.drain() {
                    if let Err(error) = actor.join.await {
                        if actor_error.is_none() {
                            actor_error = Some(RuntimeHostError::ThreadActorPanicked {
                                thread_id,
                                message: error.to_string(),
                            });
                        }
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
    capability_change_tx: tokio_mpsc::Sender<()>,
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
    fn notify_interaction_capability_changed(&self) {
        let _ = self.capability_change_tx.try_send(());
    }

    fn detach(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request: surface::DetachRequest,
    ) -> surface::DetachResult {
        let request_id = request.request_id.clone();
        let attachment_id = client.attachment_id().clone();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .command_tx
            .try_send(ThreadCommand::SurfaceDetach {
                client,
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            return surface::DetachResult::StaleAttachment {
                request_id,
                attachment_id,
            };
        }
        reply_rx
            .recv()
            .unwrap_or(surface::DetachResult::StaleAttachment {
                request_id,
                attachment_id,
            })
    }

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

    fn admit_reserved_with_output(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        writer: Box<dyn HostedOperationWriter>,
    ) -> Result<surface::MutationReply<surface::AdmissionOutput>, surface::SurfaceClientCommandError>
    {
        self.dispatch(|reply| ThreadCommand::SurfaceAdmitReservedWithOutput {
            client,
            request_id,
            operation_id,
            admission_lease_id,
            writer,
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

    fn resume_operation(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        expected_last_generation: surface::SurfaceGenerationId,
        resume_source: surface::ResumeSourceWitness,
    ) -> Result<
        surface::MutationReply<surface::ResumeOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfaceResumeOperation {
            client,
            request_id,
            operation_id,
            expected_last_generation,
            resume_source,
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

    fn update_settings(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        expected_thread_revision: surface::SettingsRevision,
        patch: surface::NonEmptyVec<surface::RuntimeSettingsPatch>,
    ) -> Result<
        surface::MutationReply<surface::SettingsMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfaceUpdateSettings {
            client,
            request_id,
            expected_thread_revision,
            patch,
            reply,
        })
    }

    fn pinned_context_mutation(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::PinnedContextAction,
    ) -> Result<
        surface::MutationReply<surface::PinnedContextMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfacePinnedContextMutation {
            client,
            request_id,
            action,
            reply,
        })
    }

    fn respond_interaction_by_id(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(|reply| ThreadCommand::SurfaceRespondInteractionById {
            client,
            request_id,
            interaction_id,
            answer,
            reply,
        })
    }

    fn respond_interaction_by_id_with_policy(
        &self,
        client: surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
        policy: surface::BrokerInteractionAnswerPolicy,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.dispatch(
            |reply| ThreadCommand::SurfaceRespondInteractionByIdWithPolicy {
                client,
                request_id,
                interaction_id,
                answer,
                policy,
                reply,
            },
        )
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
    thread: &mut RuntimeThread,
    config: &RunConfig,
    title: &str,
    host_incarnation: surface::HostIncarnation,
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    capability_change_tx: tokio_mpsc::Sender<()>,
    surface_owner: PreparedSurfaceOwner,
    surface_hub_config: surface::SurfaceHubConfig,
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
            coordinator
                .recover_unavailable_interactions(&operation_id, &materialization)
                .map_err(|error| RuntimeHostError::ThreadStartFailed {
                    message: format!(
                        "failed to reconcile typed interaction availability: {error:?}"
                    ),
                })?;
            loop {
                let before = coordinator.state().snapshot().cursor.clone();
                let action = coordinator
                    .recover_operation(&operation_id, &materialization)
                    .map_err(|error| RuntimeHostError::ThreadStartFailed {
                        message: format!("failed to reconcile typed operation: {error:?}"),
                    })?;
                if matches!(
                    action,
                    surface::RecoveryAction::ExposeRecoveryRequired
                        | surface::RecoveryAction::ExposeRetryFinalization
                        | surface::RecoveryAction::ExposeRetryProjection
                        | surface::RecoveryAction::NoOp
                ) {
                    break;
                }
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
    hydrate_session_pinned_context_from_surface(
        thread,
        coordinator
            .state()
            .snapshot()
            .pinned_context
            .entries
            .as_slice(),
    );
    let authority = surface::SurfaceAttachAuthority::new(
        host_incarnation,
        coordinator.state().snapshot().thread.thread_id.clone(),
        surface::SurfaceAttachmentRole::Tui,
        surface::NonEmptySet::try_new(BTreeSet::from([
            surface::SurfaceCapability::ReadSnapshot,
            surface::SurfaceCapability::SubmitOperation,
            surface::SurfaceCapability::ControlBoundOperation,
            surface::SurfaceCapability::ManageThreadSettings,
            surface::SurfaceCapability::ManagePinnedContext,
            surface::SurfaceCapability::RespondGrantedInteraction,
            surface::SurfaceCapability::RepairThread,
        ]))
        .expect("production TUI grant is non-empty"),
        surface::NonEmptySet::try_new(BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]))
            .expect("production TUI required grant is non-empty"),
        BTreeSet::from([
            surface::SurfaceInteractionKind::ToolApproval,
            surface::SurfaceInteractionKind::PermissionRequest,
            surface::SurfaceInteractionKind::UserInput,
            surface::SurfaceInteractionKind::McpElicitation,
        ]),
    );
    let hub = surface::SurfaceHub::from_authority(
        coordinator.state().snapshot().clone(),
        authority,
        surface_hub_config,
    )
    .map_err(|error| RuntimeHostError::ThreadStartFailed {
        message: format!("failed to create typed runtime surface: {error:?}"),
    })?
    .with_dispatcher(Arc::new(ThreadSurfaceDispatcher {
        command_tx,
        capability_change_tx,
    }));
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
            interactions: HashMap::new(),
            operation_origin_attachments: HashMap::new(),
            pending_detaches: HashMap::new(),
            pending_capability_losses: HashMap::new(),
            pending_terminalization: None,
            pending_admission_commits: HashMap::new(),
            pending_admission_repairs: HashMap::new(),
            pending_admission_terminals: HashMap::new(),
        },
    ))
}

fn hydrate_session_pinned_context_from_surface(
    thread: &mut RuntimeThread,
    entries: &[surface::SurfacePinnedContextEntry],
) {
    let missing = entries
        .iter()
        .filter(|entry| matches!(entry.kind, surface::SurfacePinnedContextKind::User))
        .filter(|entry| {
            !thread
                .session()
                .conversation()
                .messages
                .iter()
                .any(|message| {
                    matches!(
                        message,
                        Message::User { content, pinned: true } if content == entry.content.as_str()
                    )
                })
        })
        .map(|entry| entry.content.as_str().to_string())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }
    let session = thread.session_mut();
    for content in missing {
        session.add_pinned_context(content);
    }
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
    let workspace_roots = config
        .runtime_workspace_roots
        .clone()
        .unwrap_or_else(|| vec![cwd.as_path().to_path_buf()])
        .into_iter()
        .map(surface::CanonicalPath::try_new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| RuntimeHostError::ThreadStartFailed {
            message: format!("invalid surface workspace root: {error:?}"),
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
    let reasoning_effort = match config.reasoning_effort {
        orca_core::config::ReasoningEffort::High => surface::SurfaceReasoningEffort::High,
        orca_core::config::ReasoningEffort::Max => surface::SurfaceReasoningEffort::Max,
    };
    let settings = surface::SurfaceRuntimeSettings {
        model: surface::NonEmptyText::try_new(
            config
                .model
                .as_history_value()
                .unwrap_or_else(|| "runtime-default".to_string()),
        )
        .expect("runtime model is non-empty"),
        reasoning_effort,
        approval_mode,
        cwd: cwd.clone(),
        workspace_roots: workspace_roots.clone(),
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
            workspace_roots,
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

fn apply_runtime_settings_patch(
    config: &mut RunConfig,
    settings: &mut surface::SurfaceRuntimeSettings,
    patch: &surface::RuntimeSettingsPatch,
) -> Result<(), surface::SurfaceClientCommandError> {
    match patch {
        surface::RuntimeSettingsPatch::SetModel { model } => {
            config.model =
                orca_core::model::ModelSelection::from_unchecked(Some(model.as_str().to_string()));
            settings.model = model.clone();
        }
        surface::RuntimeSettingsPatch::SetReasoning { effort } => {
            config.reasoning_effort = match effort {
                surface::SurfaceReasoningEffort::Low | surface::SurfaceReasoningEffort::Medium => {
                    return Err(surface::SurfaceClientCommandError::Unauthorized);
                }
                surface::SurfaceReasoningEffort::High => orca_core::config::ReasoningEffort::High,
                surface::SurfaceReasoningEffort::Max => orca_core::config::ReasoningEffort::Max,
            };
            settings.reasoning_effort = *effort;
        }
        surface::RuntimeSettingsPatch::SetApprovalMode { mode } => {
            config.approval_mode = match mode {
                surface::SurfaceApprovalMode::Suggest => ApprovalMode::Suggest,
                surface::SurfaceApprovalMode::AutoEdit => ApprovalMode::AutoEdit,
                surface::SurfaceApprovalMode::FullAuto => ApprovalMode::FullAuto,
                surface::SurfaceApprovalMode::Plan => ApprovalMode::Plan,
            };
            settings.approval_mode = *mode;
        }
        _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
    }
    Ok(())
}

fn hydrate_run_config_from_surface_settings(
    config: &mut RunConfig,
    settings: &surface::SurfaceRuntimeSettings,
) -> Result<(), surface::SurfaceClientCommandError> {
    let mut restored = settings.clone();
    apply_runtime_settings_patch(
        config,
        &mut restored,
        &surface::RuntimeSettingsPatch::SetModel {
            model: settings.model.clone(),
        },
    )?;
    apply_runtime_settings_patch(
        config,
        &mut restored,
        &surface::RuntimeSettingsPatch::SetReasoning {
            effort: settings.reasoning_effort,
        },
    )?;
    apply_runtime_settings_patch(
        config,
        &mut restored,
        &surface::RuntimeSettingsPatch::SetApprovalMode {
            mode: settings.approval_mode,
        },
    )?;
    Ok(())
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
        bindings_digest: surface_sha256(
            &serde_json::to_vec(request).expect("surface input request is serializable"),
        ),
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
    interactions: HashMap<surface::SurfaceInteractionId, ResidentSurfaceInteraction>,
    operation_origin_attachments:
        HashMap<surface::SurfaceOperationId, surface::SurfaceAttachmentId>,
    pending_detaches: HashMap<surface::SurfaceAttachmentId, PendingSurfaceDetach>,
    pending_capability_losses: HashMap<surface::SurfaceAttachmentId, PendingSurfaceCapabilityLoss>,
    pending_terminalization: Option<PreparedSurfaceTerminalization>,
    pending_admission_commits: HashMap<surface::SurfaceOperationId, PendingSurfaceAdmissionCommit>,
    pending_admission_repairs: HashMap<surface::SurfaceOperationId, PendingSurfaceAdmissionRepair>,
    pending_admission_terminals:
        HashMap<surface::SurfaceOperationId, PendingSurfaceAdmissionTerminal>,
}

struct ResidentSurfaceInteraction {
    record: surface::BrokerInteractionRequestRecord,
    route: surface::BrokerInteractionResponseRoute,
    revision: surface::InteractionRevision,
    waiter: Option<ResidentInteractionWaiter>,
    private_response: Option<ResidentPrivateInteractionResponse>,
    winning_receipt: Option<surface::SurfaceInteractionResolutionReceipt>,
    resolution_ack: Option<surface::MutationCommitAck>,
    projected_cursor: Option<surface::SurfaceCursor>,
    cancelled: Option<surface::InteractionCancelReason>,
}

struct ResidentPrivateInteractionResponse {
    record: surface::BrokerInteractionResponseRecord,
    answer: surface::SurfaceClientInteractionAnswer,
    pending_batch: Option<surface::SurfaceCommitBatch>,
    retry_at: Option<tokio::time::Instant>,
}

#[derive(Clone)]
struct PendingSurfaceAdmissionRepair {
    fence: surface::SurfaceOperationFence,
    batch: surface::SurfaceCommitBatch,
    original_request_id: surface::SurfaceRequestId,
    finalize_intent_id: surface::SurfaceFinalizeIntentId,
    terminal_commit_id: surface::SurfaceCommitId,
    terminal: surface::OperationTerminal,
    retry_at: tokio::time::Instant,
}

#[derive(Clone)]
struct PendingSurfaceAdmissionCommit {
    fence: surface::SurfaceOperationFence,
    batch: surface::SurfaceCommitBatch,
    message: &'static str,
    retry_at: tokio::time::Instant,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PendingSurfaceTransitionRetry {
    AdmissionCommit(surface::SurfaceOperationId),
    AdmissionRepair(surface::SurfaceOperationId),
    AdmissionTerminal(surface::SurfaceOperationId),
    PreparedTerminalization(surface::SurfaceOperationId),
    PrivateResponse(surface::SurfaceInteractionId),
    Detach(surface::SurfaceAttachmentId),
    CapabilityLoss(surface::SurfaceAttachmentId),
}

#[derive(Clone)]
struct PendingSurfaceDetach {
    client: surface::RuntimeSurfaceClientHandle,
    transition: PreparedSurfaceAttachmentTransition,
    receipt: surface::DetachRevocationReceipt,
    retry_at: tokio::time::Instant,
}

#[derive(Clone)]
struct PendingSurfaceCapabilityLoss {
    transition: PreparedSurfaceAttachmentTransition,
    retry_at: tokio::time::Instant,
}

#[derive(Clone)]
struct PreparedSurfaceAttachmentTransition {
    fence: surface::SurfaceOperationFence,
    batch: surface::SurfaceCommitBatch,
    commit_id: surface::SurfaceCommitId,
    affected_route_epochs: Vec<(surface::SurfaceInteractionId, surface::ResponseRouteEpoch)>,
    interactions: Vec<PreparedSurfaceDetachInteraction>,
}

#[derive(Clone)]
struct PreparedSurfaceTerminalization {
    fence: surface::SurfaceOperationFence,
    cause: surface::TerminalizationCause,
    batch: surface::SurfaceCommitBatch,
    interaction_ids: Vec<surface::SurfaceInteractionId>,
    retry_at: tokio::time::Instant,
}

#[derive(Clone)]
struct PreparedSurfaceDetachInteraction {
    interaction_id: surface::SurfaceInteractionId,
    revision: surface::InteractionRevision,
    route: surface::BrokerInteractionResponseRoute,
    cancelled: bool,
}

struct ExactInteractionSelectorBinding {
    expected_revision: surface::InteractionRevision,
    response_token: surface::SurfaceResponseToken,
    route_epoch: surface::ResponseRouteEpoch,
    grant_token: surface::SurfaceResponseGrantToken,
    operation_fence: surface::SurfaceOperationFence,
}

enum ResidentInteractionWaiter {
    ToolApproval {
        approval_id: String,
        waiter: SyncSender<io::Result<orca_core::approval_types::ApprovalResolution>>,
    },
    Permission(SyncSender<io::Result<crate::runtime_permission::RuntimePermissionResponse>>),
    UserInput(SyncSender<io::Result<Option<String>>>),
    McpElicitation(SyncSender<Result<orca_mcp::McpElicitationResponse, String>>),
}

fn random_token_bytes() -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes
}

fn keyed_interaction_response_digest(
    token: &surface::SurfaceResponseToken,
    answer: &surface::SurfaceClientInteractionAnswer,
) -> surface::OpaqueToken {
    let mut hasher = Sha256::new();
    hasher.update(token.key_bytes());
    hasher.update(serde_json::to_vec(answer).expect("interaction answer serializes"));
    surface::OpaqueToken::new(hasher.finalize().into())
}

fn surface_sha256(bytes: &[u8]) -> surface::Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    surface::Sha256Digest::new(hasher.finalize().into())
}

fn surface_tool_action(
    action: orca_core::approval_types::ActionKind,
) -> surface::SurfaceToolAction {
    match action {
        orca_core::approval_types::ActionKind::Read => surface::SurfaceToolAction::Read,
        orca_core::approval_types::ActionKind::Write => surface::SurfaceToolAction::Write,
        orca_core::approval_types::ActionKind::Network => surface::SurfaceToolAction::Network,
        orca_core::approval_types::ActionKind::Agent => surface::SurfaceToolAction::Agent,
        orca_core::approval_types::ActionKind::Shell => surface::SurfaceToolAction::Shell,
    }
}

fn surface_permission_paths(
    paths: Option<Vec<std::path::PathBuf>>,
) -> io::Result<Option<Vec<surface::SurfacePermissionPathLabel>>> {
    paths
        .map(|paths| {
            paths
                .into_iter()
                .map(|path| {
                    path.to_str()
                        .map(|path| {
                            surface::SurfacePermissionPathLabel(surface::DisplayText::new(path))
                        })
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "permission path is not lossless UTF-8",
                            )
                        })
                })
                .collect()
        })
        .transpose()
}

fn surface_permission_profile_from_runtime(
    profile: crate::protocol::RequestPermissionProfile,
) -> io::Result<surface::SurfacePermissionProfile> {
    let profile = profile.normalize_file_system_entries();
    let file_system = profile
        .file_system
        .map(|permissions| {
            Ok::<surface::SurfaceFileSystemPermissionProfile, io::Error>(
                surface::SurfaceFileSystemPermissionProfile {
                    read: surface_permission_paths(permissions.read)?,
                    write: surface_permission_paths(permissions.write)?,
                },
            )
        })
        .transpose()?;
    let network = profile.network.map(|permissions| {
        let mut domains = permissions.domains.into_iter().collect::<Vec<_>>();
        domains.sort_by(|left, right| left.0.cmp(&right.0));
        surface::SurfacePermissionNetworkProfile {
            enabled: permissions.enabled,
            domains: domains
                .into_iter()
                .map(|(domain, access)| {
                    (
                        surface::SurfacePermissionDomainPattern(surface::DisplayText::new(domain)),
                        match access {
                            orca_core::config::PermissionProfileNetworkAccess::Allow => {
                                surface::SurfaceAllowDeny::Allow
                            }
                            orca_core::config::PermissionProfileNetworkAccess::Deny => {
                                surface::SurfaceAllowDeny::Deny
                            }
                        },
                    )
                })
                .collect(),
        }
    });
    let shell = profile
        .shell
        .map(|permissions| surface::SurfaceShellPermissionProfile {
            unsandboxed: permissions.unsandboxed,
        });
    Ok(surface::SurfacePermissionProfile {
        file_system,
        network,
        shell,
    })
}

fn runtime_permission_profile_from_surface(
    profile: &surface::SurfacePermissionProfile,
) -> crate::protocol::RequestPermissionProfile {
    let file_system = profile.file_system.as_ref().map(|permissions| {
        crate::protocol::RequestFileSystemPermissions {
            read: permissions.read.as_ref().map(|paths| {
                paths
                    .iter()
                    .map(|path| std::path::PathBuf::from(path.0.as_str()))
                    .collect()
            }),
            write: permissions.write.as_ref().map(|paths| {
                paths
                    .iter()
                    .map(|path| std::path::PathBuf::from(path.0.as_str()))
                    .collect()
            }),
            entries: None,
        }
    });
    let network =
        profile
            .network
            .as_ref()
            .map(|permissions| crate::protocol::RequestNetworkPermissions {
                enabled: permissions.enabled,
                domains: permissions
                    .domains
                    .iter()
                    .map(|(domain, access)| {
                        (
                            domain.0.as_str().to_string(),
                            match access {
                                surface::SurfaceAllowDeny::Allow => {
                                    orca_core::config::PermissionProfileNetworkAccess::Allow
                                }
                                surface::SurfaceAllowDeny::Deny => {
                                    orca_core::config::PermissionProfileNetworkAccess::Deny
                                }
                            },
                        )
                    })
                    .collect(),
            });
    let shell =
        profile
            .shell
            .as_ref()
            .map(|permissions| crate::protocol::RequestShellPermissions {
                unsandboxed: permissions.unsandboxed,
            });
    crate::protocol::RequestPermissionProfile {
        file_system,
        network,
        shell,
    }
}

fn permission_path_subset(
    candidate: Option<&Vec<surface::SurfacePermissionPathLabel>>,
    requested: Option<&Vec<surface::SurfacePermissionPathLabel>>,
) -> bool {
    candidate.is_none_or(|candidate| {
        let requested = requested
            .into_iter()
            .flatten()
            .map(|path| path.0.as_str())
            .collect::<BTreeSet<_>>();
        candidate
            .iter()
            .all(|path| requested.contains(path.0.as_str()))
    })
}

fn surface_permission_profile_is_subset(
    candidate: &surface::SurfacePermissionProfile,
    requested: &surface::SurfacePermissionProfile,
) -> bool {
    let file_system_subset = candidate.file_system.as_ref().is_none_or(|candidate| {
        requested.file_system.as_ref().is_some_and(|requested| {
            permission_path_subset(candidate.read.as_ref(), requested.read.as_ref())
                && permission_path_subset(candidate.write.as_ref(), requested.write.as_ref())
        })
    });
    let network_subset = candidate.network.as_ref().is_none_or(|candidate| {
        requested.network.as_ref().is_some_and(|requested| {
            let enabled_subset = match candidate.enabled {
                None | Some(false) => true,
                Some(true) => requested.enabled == Some(true),
            };
            let requested_domains = requested
                .domains
                .iter()
                .map(|(domain, access)| (domain.0.as_str(), *access))
                .collect::<HashMap<_, _>>();
            enabled_subset
                && candidate.domains.iter().all(|(domain, access)| {
                    requested_domains.get(domain.0.as_str()) == Some(access)
                })
        })
    });
    let shell_subset = candidate.shell.as_ref().is_none_or(|candidate| {
        !candidate.unsandboxed
            || requested
                .shell
                .as_ref()
                .is_some_and(|requested| requested.unsandboxed)
    });
    file_system_subset && network_subset && shell_subset
}

const PRIVATE_INTERACTION_ANSWER_DEPTH_LIMIT: usize = 64;
const PRIVATE_INTERACTION_ANSWER_NODE_LIMIT: usize = 16_384;

struct BoundedAnswerWriter {
    written: u64,
}

impl io::Write for BoundedAnswerWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "answer is too large"))?;
        if next > surface::SURFACE_COMMIT_BATCH_BYTE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "answer is too large",
            ));
        }
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn surface_data_within_private_limits(value: &surface::SurfaceDataValue) -> bool {
    let mut stack = vec![(value, 1_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        if depth > PRIVATE_INTERACTION_ANSWER_DEPTH_LIMIT {
            return false;
        }
        nodes = nodes.saturating_add(1);
        if nodes > PRIVATE_INTERACTION_ANSWER_NODE_LIMIT {
            return false;
        }
        match value {
            surface::SurfaceDataValue::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            surface::SurfaceDataValue::Object(properties) => {
                stack.extend(
                    properties
                        .iter()
                        .map(|property| (property.value.as_ref(), depth + 1)),
                );
            }
            surface::SurfaceDataValue::Null
            | surface::SurfaceDataValue::Boolean(_)
            | surface::SurfaceDataValue::Integer(_)
            | surface::SurfaceDataValue::Unsigned(_)
            | surface::SurfaceDataValue::Number(_)
            | surface::SurfaceDataValue::String(_) => {}
        }
    }
    true
}

fn interaction_answer_within_private_limits(
    answer: &surface::SurfaceClientInteractionAnswer,
) -> bool {
    if let surface::SurfaceClientInteractionAnswer::McpElicitation {
        decision: surface::SurfaceMcpElicitationDecision::Accept { content },
    } = answer
        && !surface_data_within_private_limits(content)
    {
        return false;
    }
    serde_json::to_writer(&mut BoundedAnswerWriter { written: 0 }, answer).is_ok()
}

fn interaction_answer_kind(
    answer: &surface::SurfaceClientInteractionAnswer,
) -> surface::SurfaceInteractionKind {
    match answer {
        surface::SurfaceClientInteractionAnswer::ToolApproval { .. } => {
            surface::SurfaceInteractionKind::ToolApproval
        }
        surface::SurfaceClientInteractionAnswer::PermissionRequest { .. } => {
            surface::SurfaceInteractionKind::PermissionRequest
        }
        surface::SurfaceClientInteractionAnswer::UserInput { .. } => {
            surface::SurfaceInteractionKind::UserInput
        }
        surface::SurfaceClientInteractionAnswer::McpElicitation { .. } => {
            surface::SurfaceInteractionKind::McpElicitation
        }
        surface::SurfaceClientInteractionAnswer::BackgroundApproval { .. } => {
            surface::SurfaceInteractionKind::BackgroundApproval
        }
    }
}

fn interaction_answer_authority(
    request: &surface::SurfaceInteractionRequest,
    answer: &surface::SurfaceClientInteractionAnswer,
) -> surface::ApplicableAuthorityFingerprint {
    match (request, answer) {
        (
            surface::SurfaceInteractionRequest::ToolApproval { authority, .. },
            surface::SurfaceClientInteractionAnswer::ToolApproval {
                decision: surface::SurfaceAllowDeny::Allow,
            },
        )
        | (
            surface::SurfaceInteractionRequest::PermissionRequest { authority, .. },
            surface::SurfaceClientInteractionAnswer::PermissionRequest {
                decision: surface::SurfacePermissionClientDecision::Allow { .. },
            },
        )
        | (
            surface::SurfaceInteractionRequest::BackgroundApproval { authority, .. },
            surface::SurfaceClientInteractionAnswer::BackgroundApproval {
                decision: surface::SurfaceAllowDeny::Allow,
            },
        ) => surface::ApplicableAuthorityFingerprint::persisted(authority.clone()),
        _ => surface::ApplicableAuthorityFingerprint::not_applicable(),
    }
}

fn interaction_answer_authority_matches(
    request: &surface::SurfaceInteractionRequest,
    answer: &surface::SurfaceClientInteractionAnswer,
    actual: &surface::ApplicableAuthorityFingerprint,
) -> bool {
    interaction_answer_authority(request, answer).authority() == actual.authority()
}

fn interaction_safe_projection(
    answer: &surface::SurfaceClientInteractionAnswer,
) -> surface::SurfaceInteractionSafeProjection {
    match answer {
        surface::SurfaceClientInteractionAnswer::ToolApproval { decision } => {
            surface::SurfaceInteractionSafeProjection::ToolApproval {
                allowed: *decision == surface::SurfaceAllowDeny::Allow,
            }
        }
        surface::SurfaceClientInteractionAnswer::PermissionRequest { decision } => {
            let (decision, scope, strict_auto_review) = match decision {
                surface::SurfacePermissionClientDecision::Allow {
                    scope,
                    strict_auto_review,
                    ..
                } => (
                    surface::SurfaceAllowDeny::Allow,
                    *scope,
                    *strict_auto_review,
                ),
                surface::SurfacePermissionClientDecision::Deny {
                    scope,
                    strict_auto_review,
                    ..
                } => (surface::SurfaceAllowDeny::Deny, *scope, *strict_auto_review),
            };
            surface::SurfaceInteractionSafeProjection::PermissionRequest {
                decision,
                scope,
                strict_auto_review,
            }
        }
        surface::SurfaceClientInteractionAnswer::UserInput { decision } => {
            surface::SurfaceInteractionSafeProjection::UserInput {
                answered: matches!(decision, surface::SurfaceUserInputDecision::Answer(_)),
            }
        }
        surface::SurfaceClientInteractionAnswer::McpElicitation { decision } => {
            surface::SurfaceInteractionSafeProjection::McpElicitation {
                accepted: matches!(
                    decision,
                    surface::SurfaceMcpElicitationDecision::Accept { .. }
                ),
            }
        }
        surface::SurfaceClientInteractionAnswer::BackgroundApproval { decision } => {
            surface::SurfaceInteractionSafeProjection::BackgroundApproval {
                allowed: *decision == surface::SurfaceAllowDeny::Allow,
            }
        }
    }
}

fn surface_data_from_json(value: &Value) -> Result<surface::SurfaceDataValue, String> {
    match value {
        Value::Null => Ok(surface::SurfaceDataValue::Null),
        Value::Bool(value) => Ok(surface::SurfaceDataValue::Boolean(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_u64() {
                Ok(surface::SurfaceDataValue::Unsigned(value))
            } else if let Some(value) = value.as_i64() {
                surface::NegativeI64::try_new(value)
                    .map(surface::SurfaceDataValue::Integer)
                    .map_err(|error| format!("invalid negative integer: {error:?}"))
            } else {
                surface::FiniteF64::try_new(
                    value
                        .as_f64()
                        .ok_or_else(|| "JSON number is not representable as f64".to_string())?,
                )
                .map(surface::SurfaceDataValue::Number)
                .map_err(|error| format!("non-finite JSON number: {error:?}"))
            }
        }
        Value::String(value) => Ok(surface::SurfaceDataValue::String(
            surface::DisplayText::new(value.clone()),
        )),
        Value::Array(values) => values
            .iter()
            .map(surface_data_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(surface::SurfaceDataValue::Array),
        Value::Object(properties) => properties
            .iter()
            .map(|(name, value)| {
                Ok(surface::SurfaceDataProperty {
                    name: surface::DisplayText::new(name.clone()),
                    value: Box::new(surface_data_from_json(value)?),
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .map(surface::SurfaceDataValue::Object),
    }
}

fn json_from_surface_data(value: &surface::SurfaceDataValue) -> Value {
    match value {
        surface::SurfaceDataValue::Null => Value::Null,
        surface::SurfaceDataValue::Boolean(value) => Value::Bool(*value),
        surface::SurfaceDataValue::Integer(value) => Value::Number(value.get().into()),
        surface::SurfaceDataValue::Unsigned(value) => Value::Number((*value).into()),
        surface::SurfaceDataValue::Number(value) => serde_json::Number::from_f64(value.get())
            .map(Value::Number)
            .expect("surface number is finite"),
        surface::SurfaceDataValue::String(value) => Value::String(value.as_str().to_string()),
        surface::SurfaceDataValue::Array(values) => {
            Value::Array(values.iter().map(json_from_surface_data).collect())
        }
        surface::SurfaceDataValue::Object(properties) => Value::Object(
            properties
                .iter()
                .map(|property| {
                    (
                        property.name.as_str().to_string(),
                        json_from_surface_data(&property.value),
                    )
                })
                .collect(),
        ),
    }
}

fn interaction_route_attachments(
    route: &surface::BrokerInteractionResponseRoute,
) -> Vec<surface::SurfaceAttachmentId> {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => Vec::new(),
        surface::BrokerInteractionResponseRoute::Exclusive { attachment_id, .. } => {
            vec![attachment_id.clone()]
        }
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { grants, .. } => grants
            .as_slice()
            .iter()
            .map(|(attachment_id, _)| attachment_id.clone())
            .collect(),
    }
}

fn interaction_route_admits(
    route: &surface::BrokerInteractionResponseRoute,
    attachment_id: &surface::SurfaceAttachmentId,
) -> bool {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => false,
        surface::BrokerInteractionResponseRoute::Exclusive {
            attachment_id: expected,
            ..
        } => expected == attachment_id,
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { grants, .. } => grants
            .as_slice()
            .iter()
            .any(|(expected, _)| expected == attachment_id),
    }
}

fn interaction_route_admits_exact(
    route: &surface::BrokerInteractionResponseRoute,
    attachment_id: &surface::SurfaceAttachmentId,
    route_epoch: surface::ResponseRouteEpoch,
    grant_token: &surface::SurfaceResponseGrantToken,
) -> bool {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => false,
        surface::BrokerInteractionResponseRoute::Exclusive {
            epoch,
            attachment_id: expected_attachment,
            grant_token: expected_grant,
        } => {
            *epoch == route_epoch
                && expected_attachment == attachment_id
                && expected_grant == grant_token
        }
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins {
            epoch, grants, ..
        } => {
            *epoch == route_epoch
                && grants
                    .as_slice()
                    .iter()
                    .any(|(expected_attachment, expected_grant)| {
                        expected_attachment == attachment_id && expected_grant == grant_token
                    })
        }
    }
}

fn interaction_route_epoch(
    route: &surface::BrokerInteractionResponseRoute,
) -> surface::ResponseRouteEpoch {
    match route {
        surface::BrokerInteractionResponseRoute::Unassigned { epoch }
        | surface::BrokerInteractionResponseRoute::Exclusive { epoch, .. }
        | surface::BrokerInteractionResponseRoute::SharedFirstCommitWins { epoch, .. } => *epoch,
    }
}

fn exact_interaction_selectors(
    interaction: &ResidentSurfaceInteraction,
) -> Vec<(surface::SurfaceAttachmentId, surface::InteractionSelector)> {
    let grants = match &interaction.route {
        surface::BrokerInteractionResponseRoute::Unassigned { .. } => Vec::new(),
        surface::BrokerInteractionResponseRoute::Exclusive {
            epoch,
            attachment_id,
            grant_token,
        } => vec![(attachment_id.clone(), *epoch, grant_token.clone())],
        surface::BrokerInteractionResponseRoute::SharedFirstCommitWins {
            epoch, grants, ..
        } => grants
            .as_slice()
            .iter()
            .map(|(attachment_id, grant_token)| {
                (attachment_id.clone(), *epoch, grant_token.clone())
            })
            .collect(),
    };
    grants
        .into_iter()
        .map(
            |(attachment_id, response_route_epoch, response_grant_token)| {
                (
                    attachment_id,
                    surface::InteractionSelector::Exact {
                        interaction_id: interaction.record.interaction_id.clone(),
                        expected_revision: interaction.revision,
                        kind: interaction.record.kind,
                        response_token: interaction.record.response_token.clone(),
                        response_route_epoch,
                        response_grant_token,
                        operation_fence: interaction.record.fence.clone(),
                    },
                )
            },
        )
        .collect()
}

#[cfg(test)]
fn exact_interaction_selector_for_test(
    state: &ResidentSurfaceState,
    operation_id: &surface::SurfaceOperationId,
) -> Option<surface::InteractionSelector> {
    state
        .interactions
        .values()
        .find(|interaction| &interaction.record.fence.operation_id == operation_id)
        .and_then(|interaction| exact_interaction_selectors(interaction).into_iter().next())
        .map(|(_, selector)| selector)
}

#[cfg(test)]
fn pending_capability_loss_for_test(
    state: &ResidentSurfaceState,
) -> Option<PendingCapabilityLossTestProbe> {
    state
        .pending_capability_losses
        .iter()
        .min_by_key(|(attachment_id, _)| (*attachment_id).clone())
        .map(|(attachment_id, pending)| PendingCapabilityLossTestProbe {
            attachment_id: attachment_id.clone(),
            commit_id: pending.transition.commit_id.clone(),
            cursor_after: pending.transition.batch.cursor_after.clone(),
            batch_digest: pending.transition.batch.batch_digest.clone(),
        })
}

#[cfg(test)]
fn pending_terminalization_for_test(
    state: &ResidentSurfaceState,
) -> Option<PendingTerminalizationTestProbe> {
    state.pending_terminalization.as_ref().map(|pending| {
        let surface::CommitClass::Recorded { commit_id, .. } = &pending.batch.commit_class else {
            unreachable!("recorded runtime surface used ephemeral commit class")
        };
        PendingTerminalizationTestProbe {
            commit_id: commit_id.clone(),
            cursor_before: pending.batch.cursor_before.clone(),
            cursor_after: pending.batch.cursor_after.clone(),
            batch_digest: pending.batch.batch_digest.clone(),
        }
    })
}

#[derive(Clone)]
struct PendingSurfaceTerminalCommit {
    batch: surface::SurfaceCommitBatch,
    value: surface::OperationTerminalAtCursor,
    failure: surface::WaitOperationTerminalResult,
    legacy_completion: Option<OperationCompletion>,
    legacy_terminal: Option<OperationTerminal>,
}

#[derive(Clone)]
struct PendingSurfaceAdmissionTerminal {
    pending: PendingSurfaceTerminalCommit,
    retry_at: tokio::time::Instant,
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
    surface_execution_failure: Option<surface::GenerationExecutionFailureClass>,
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
            let admitted = resident.hub.admits_client(client);
            let capability_granted = client.grant().capabilities.as_set().contains(&capability);
            admitted && capability_granted
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

    fn commit_surface_generation_batch_with_retry(
        &mut self,
        fence: surface::SurfaceOperationFence,
        batch: &surface::SurfaceCommitBatch,
    ) -> io::Result<()> {
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self
                .resident_surface
                .coordinator
                .commit_generation_batch(fence.clone(), batch)
            {
                Ok(_) => return Ok(()),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "failed to commit provider semantic batch: {error:?}"
                    )));
                }
            }
        }
        Err(io::Error::other(
            "provider semantic batch did not commit after bounded retries",
        ))
    }

    fn commit_surface_actor_batch_with_retry(
        &mut self,
        batch: &surface::SurfaceCommitBatch,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        for attempt in 0..SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS {
            match self.resident_surface.coordinator.commit_actor_batch(batch) {
                Ok(_) => return Ok(()),
                Err(surface::SurfaceCommitError::Ledger(error))
                    if attempt + 1 < SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS
                        && matches!(
                            error,
                            surface::SurfaceLedgerError::AppendFailed
                                | surface::SurfaceLedgerError::PartialAppend
                                | surface::SurfaceLedgerError::CheckpointFailed
                        ) => {}
                Err(_) => return Err(surface::SurfaceClientCommandError::RuntimeUnavailable),
            }
        }
        Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
    }

    fn commit_surface_provider_step(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        identity: &orca_core::thread_item_projection::ModelResponseIdentity,
        step: &ProviderStep,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider step generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        let (channel, item_id, text) = match step {
            ProviderStep::MessageDelta(text) => (
                surface::AssistantChannel::Message,
                identity.item_ids.conversation_item_id.clone(),
                text,
            ),
            ProviderStep::ReasoningDelta(text) => (
                surface::AssistantChannel::Reasoning,
                identity.item_ids.reasoning_item_id.clone(),
                text,
            ),
            _ => return Ok(()),
        };
        if text.is_empty() {
            return Ok(());
        }

        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        if generation.logical_turn_id != identity.turn_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provider step turn identity differs from active generation",
            ));
        }

        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut events = Vec::with_capacity(2);
        let (stream_id, offset) = if let Some(stream) = snapshot
            .assistant_streams
            .iter()
            .find(|stream| stream.item_id == item_id && stream.channel == channel)
        {
            if stream.fence != fence
                || stream.turn_id != identity.turn_id
                || stream.state != surface::SurfaceAssistantStreamState::Open
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider step targets a closed or foreign assistant stream",
                ));
            }
            (stream.stream_id.clone(), stream.next_offset)
        } else {
            let raw_id = item_id
                .as_str()
                .strip_prefix("item_")
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "provider step item identity is not UUIDv7-backed",
                    )
                })?;
            let stream_id =
                surface::SurfaceStreamId::try_from_bytes(*raw_id.as_bytes()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "provider step item identity is not UUIDv7-backed",
                    )
                })?;
            events.push((
                scope.clone(),
                surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamOpened {
                    stream: surface::SurfaceAssistantStream {
                        stream_id: stream_id.clone(),
                        fence: fence.clone(),
                        turn_id: identity.turn_id.clone(),
                        item_id,
                        channel,
                        next_offset: surface::ByteOffset::new(0),
                        text: surface::DisplayText::new(""),
                        state: surface::SurfaceAssistantStreamState::Open,
                    },
                }),
            ));
            (stream_id, surface::ByteOffset::new(0))
        };
        events.push((
            scope,
            surface::SurfaceEvent::Assistant(surface::AssistantPatch::Delta {
                stream_id,
                offset,
                text: surface::DisplayText::new(text.clone()),
            }),
        ));
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.commit_surface_generation_batch_with_retry(fence, &batch)
    }

    fn commit_surface_provider_response(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        response: &crate::model_response::RuntimeModelResponse,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider response generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }

        let completed = response.completed();
        let response_uuid = completed
            .identity
            .item_ids
            .conversation_item_id
            .as_str()
            .strip_prefix("item_")
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider response identity is not UUIDv7-backed",
                )
            })?;
        let response_id =
            surface::UuidV7::try_from_bytes(*response_uuid.as_bytes()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider response identity is not UUIDv7-backed",
                )
            })?;

        let mut message_item = None;
        let mut reasoning_item = None;
        let mut plan_item = None;
        for item in completed.completed_items() {
            match item {
                CompletedModelItem::AgentMessage { id, text } => {
                    message_item = Some(surface::SurfaceAssistantMessageItem {
                        id,
                        turn_id: completed.identity.turn_id.clone(),
                        text: surface::DisplayText::new(text),
                        pinned: false,
                    });
                }
                CompletedModelItem::Reasoning {
                    id,
                    summary,
                    content,
                } => {
                    let (summary, content) = if content.is_empty() && !summary.is_empty() {
                        (String::new(), summary)
                    } else {
                        (summary, content)
                    };
                    reasoning_item = Some(surface::SurfaceAssistantReasoningItem {
                        id,
                        turn_id: completed.identity.turn_id.clone(),
                        summary: surface::DisplayText::new(summary),
                        content: surface::DisplayText::new(content),
                        pinned: false,
                    });
                }
                CompletedModelItem::Plan { id, text } => {
                    plan_item = Some(surface::SurfaceAssistantPlanItem {
                        id,
                        turn_id: completed.identity.turn_id.clone(),
                        text: surface::DisplayText::new(text),
                        pinned: false,
                    });
                }
            }
        }

        let mut requests_by_id = HashMap::new();
        for step in &response.response.steps {
            if let ProviderStep::ToolCall(request) = step
                && requests_by_id
                    .insert(request.id.clone(), request.clone())
                    .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider response repeats a tool call id",
                ));
            }
        }
        if requests_by_id.len() != completed.tool_calls.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provider response tool metadata is incomplete",
            ));
        }

        let mut raw_tool_calls = Vec::with_capacity(completed.tool_calls.len());
        let mut tool_requests = Vec::with_capacity(completed.tool_calls.len());
        for raw_call in &completed.tool_calls {
            let request = requests_by_id.get(&raw_call.id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider response lacks the executable tool request",
                )
            })?;
            let raw_arguments = request.raw_arguments.clone().unwrap_or_default();
            if request.name.as_str() != raw_call.function_name
                || raw_arguments != raw_call.arguments
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "provider response tool identity differs from executable request",
                ));
            }
            let tool_call_id = surface::SurfaceToolCallId::try_new(raw_call.id.clone())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
            let name = surface::NonEmptyText::try_new(raw_call.function_name.clone())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool name"))?;
            let arguments_digest = surface_sha256(raw_call.arguments.as_bytes());
            raw_tool_calls.push(surface::SurfaceRawToolCall {
                id: tool_call_id.clone(),
                name: name.clone(),
                raw_arguments: surface::DisplayText::new(raw_call.arguments.clone()),
                arguments_digest: arguments_digest.clone(),
            });
            tool_requests.push(surface::SurfaceToolRequest {
                tool_call_id,
                source_response_id: Some(response_id.clone()),
                turn_id: completed.identity.turn_id.clone(),
                name,
                action: surface_tool_action(request.action),
                target: request.target.clone().map(surface::DisplayText::new),
                raw_arguments: surface::DisplayText::new(raw_call.arguments.clone()),
                arguments_digest,
            });
        }

        let completed_response = surface::SurfaceCompletedModelResponse {
            response_id,
            turn_id: completed.identity.turn_id.clone(),
            message_item,
            reasoning_item,
            plan_item,
            tool_calls: raw_tool_calls,
        };
        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let mut events = snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                if stream.fence != fence
                    || stream.turn_id != completed_response.turn_id
                    || stream.state != surface::SurfaceAssistantStreamState::Open
                {
                    return false;
                }
                let completed_text = match stream.channel {
                    surface::AssistantChannel::Message => completed_response
                        .message_item
                        .as_ref()
                        .filter(|item| item.id == stream.item_id)
                        .map(|item| &item.text),
                    surface::AssistantChannel::Reasoning => completed_response
                        .reasoning_item
                        .as_ref()
                        .filter(|item| item.id == stream.item_id)
                        .map(|item| &item.content),
                    surface::AssistantChannel::Plan => completed_response
                        .plan_item
                        .as_ref()
                        .filter(|item| item.id == stream.item_id)
                        .map(|item| &item.text),
                };
                completed_text != Some(&stream.text)
            })
            .map(|stream| {
                (
                    scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: surface::AssistantDiscardReason::ProviderFailed,
                    }),
                )
            })
            .collect::<Vec<_>>();
        events.push((
            scope.clone(),
            surface::SurfaceEvent::Assistant(surface::AssistantPatch::ResponseCompleted {
                response: completed_response,
            }),
        ));
        events.extend(tool_requests.into_iter().map(|request| {
            (
                scope.clone(),
                surface::SurfaceEvent::Tool(surface::ToolPatch::Requested { request }),
            )
        }));
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.commit_surface_generation_batch_with_retry(fence, &batch)
    }

    fn commit_surface_tool_results(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        results: &[orca_core::tool_types::ToolResult],
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "tool result generation fence is stale",
            ));
        }
        if results.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tool completion batch is empty",
            ));
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut seen = BTreeSet::new();
        let mut events = Vec::with_capacity(results.len() * 2);
        for result in results {
            let tool_call_id = surface::SurfaceToolCallId::try_new(result.id.clone())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
            if !seen.insert(tool_call_id.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tool completion batch repeats a tool call id",
                ));
            }
            let tool = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == tool_call_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "tool completion lacks a committed provider tool identity",
                    )
                })?;
            if tool.request.turn_id != generation.logical_turn_id
                || tool.request.name.as_str() != result.name.as_str()
                || tool.result.is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "tool completion differs from the active committed provider tool",
                ));
            }

            let kind = match result.kind {
                orca_core::tool_types::ToolResultKind::Success
                | orca_core::tool_types::ToolResultKind::Empty
                | orca_core::tool_types::ToolResultKind::NoMatches
                | orca_core::tool_types::ToolResultKind::Truncated => {
                    surface::SurfaceToolResultKind::Success
                }
                orca_core::tool_types::ToolResultKind::PermissionDenied => {
                    surface::SurfaceToolResultKind::Denied
                }
                orca_core::tool_types::ToolResultKind::InvalidInput => {
                    surface::SurfaceToolResultKind::InvalidArguments
                }
                orca_core::tool_types::ToolResultKind::RuntimeError => {
                    surface::SurfaceToolResultKind::Failed
                }
                orca_core::tool_types::ToolResultKind::Cancelled => {
                    surface::SurfaceToolResultKind::Cancelled
                }
                orca_core::tool_types::ToolResultKind::Indeterminate => {
                    surface::SurfaceToolResultKind::ExternalEffectAmbiguous
                }
            };
            let source = match result.source {
                orca_core::tool_types::ToolTerminalSource::Observed => {
                    surface::ToolTerminalSource::Observed
                }
                orca_core::tool_types::ToolTerminalSource::CompatibilityRepair => {
                    surface::ToolTerminalSource::CompatibilityRepair
                }
            };
            let invocation_started = match result.started {
                orca_core::tool_types::ToolInvocationStarted::Yes => {
                    surface::ToolInvocationStarted::Yes
                }
                orca_core::tool_types::ToolInvocationStarted::No => {
                    surface::ToolInvocationStarted::No
                }
                orca_core::tool_types::ToolInvocationStarted::Unknown => {
                    surface::ToolInvocationStarted::Unknown
                }
            };
            let terminal = surface::SurfaceToolTerminal {
                kind,
                source,
                invocation_started,
            };
            let output = result.output.clone().map(surface::DisplayText::new);
            let error = result.error.clone().map(surface::DisplayText::new);
            let content = output
                .clone()
                .or_else(|| error.clone())
                .unwrap_or_else(|| surface::DisplayText::new("(no output)"));
            let (output, error) = if output.is_none() && error.is_none() {
                if matches!(terminal.kind, surface::SurfaceToolResultKind::Success) {
                    (Some(content.clone()), None)
                } else {
                    (None, Some(content.clone()))
                }
            } else {
                (output, error)
            };
            let completed = surface::SurfaceToolResult {
                tool_call_id: tool_call_id.clone(),
                name: tool.request.name.clone(),
                terminal: terminal.clone(),
                output,
                error,
                exit_code: if matches!(tool.request.action, surface::SurfaceToolAction::Shell) {
                    result.exit_code
                } else {
                    None
                },
                truncated: result.truncated,
                file_change: None,
            };
            events.push((
                scope.clone(),
                surface::SurfaceEvent::Tool(surface::ToolPatch::Completed { result: completed }),
            ));
            events.push((
                scope.clone(),
                surface::SurfaceEvent::Item(surface::ItemPatch::Added {
                    item: surface::SurfaceItem::ToolResultMessage {
                        id: surface::SurfaceItemId::new(),
                        turn_id: tool.request.turn_id.clone(),
                        tool_call_id,
                        content,
                        terminal,
                        pinned: false,
                    },
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.commit_surface_generation_batch_with_retry(fence, &batch)
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

    fn committed_surface_resume_mutation(
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        generation: surface::SurfaceOperationFence,
        resume_batch: &surface::SurfaceCommitBatch,
        started_batch: &surface::SurfaceCommitBatch,
    ) -> surface::MutationReply<surface::ResumeOperationOutput> {
        let reserved_event = &resume_batch.events.as_slice()[0];
        let resume_event = &resume_batch.events.as_slice()[1];
        let started_event = &started_batch.events.as_slice()[0];
        let receipt =
            |role, event: &surface::SurfaceEventEnvelope, batch: &surface::SurfaceCommitBatch| {
                surface::ResumeTransitionReceipt {
                    role,
                    event_id: event.event_id.clone(),
                    cursor: batch.cursor_after.clone(),
                    commit_class: batch.commit_class.clone(),
                }
            };
        let resume_starting = receipt(
            surface::ResumeTransitionRole::ResumeStarting,
            resume_event,
            resume_batch,
        );
        let generation_reserved = receipt(
            surface::ResumeTransitionRole::GenerationReserved,
            reserved_event,
            resume_batch,
        );
        let generation_started = receipt(
            surface::ResumeTransitionRole::GenerationStarted,
            started_event,
            started_batch,
        );
        let acknowledgement = |receipt: &surface::ResumeTransitionReceipt| {
            surface::MutationCommitAck::ThreadLocalCursor {
                cursor: receipt.cursor.clone(),
                family: surface::SurfaceFactFamily::Operation,
                event_id: receipt.event_id.clone(),
                commit_class: receipt.commit_class.clone(),
            }
        };
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Operation {
                    thread_id: generation.thread_id.clone(),
                    operation_id: operation_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    acknowledgement(&resume_starting),
                    acknowledgement(&generation_reserved),
                    acknowledgement(&generation_started),
                ])
                .expect("resume commit has three acknowledgements"),
            },
            value: surface::ResumeOperationOutput {
                operation_id,
                generation,
                resume_starting,
                generation_reserved,
                generation_started,
                waiter: surface::OperationWaiterHandle::new(),
            },
        }
    }

    fn committed_settings_mutation<T>(
        &self,
        request_id: surface::SurfaceRequestId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::RuntimeSettings {
                    host_incarnation: self
                        .resident_surface
                        .hub
                        .authority()
                        .host_incarnation()
                        .clone(),
                    thread_id: Some(batch.cursor_after.thread_id.clone()),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Settings,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("settings commit has one acknowledgement"),
            },
            value,
        }
    }

    fn committed_pinned_context_mutation<T>(
        &self,
        request_id: surface::SurfaceRequestId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Thread {
                    thread_id: batch.cursor_after.thread_id.clone(),
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::PinnedContext,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("pinned context commit has one acknowledgement"),
            },
            value,
        }
    }

    fn committed_interaction_mutation<T>(
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        batch: &surface::SurfaceCommitBatch,
        value: T,
    ) -> surface::MutationReply<T> {
        let event = &batch.events.as_slice()[0];
        surface::MutationReply::Committed {
            mutation: surface::CommittedMutation {
                request_id,
                target: surface::MutationTarget::Interaction {
                    thread_id: batch.cursor_after.thread_id.clone(),
                    interaction_id,
                },
                disposition: surface::MutationDisposition::Accepted,
                acknowledgements: surface::NonEmptyVec::try_new(vec![
                    surface::MutationCommitAck::ThreadLocalCursor {
                        cursor: batch.cursor_after.clone(),
                        family: surface::SurfaceFactFamily::Interaction,
                        event_id: event.event_id.clone(),
                        commit_class: batch.commit_class.clone(),
                    },
                ])
                .expect("interaction commit has one acknowledgement"),
            },
            value,
        }
    }

    fn surface_operation_record<'a>(
        snapshot: &'a surface::SurfaceSnapshot,
        operation_id: &surface::SurfaceOperationId,
    ) -> Option<&'a surface::OperationRecord> {
        snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == *operation_id)
    }

    fn surface_tool_for_runtime_request(
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        request: &orca_core::tool_types::ToolRequest,
    ) -> io::Result<surface::SurfaceToolRequest> {
        let tool_call_id = surface::SurfaceToolCallId::try_new(request.id.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty tool call id"))?;
        let operation = Self::surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == tool_call_id)
            .map(|tool| tool.request.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tool interaction lacks a committed provider tool identity",
                )
            })?;
        let raw_arguments = request.raw_arguments.clone().unwrap_or_default();
        if tool.source_response_id.is_none()
            || tool.turn_id != generation.logical_turn_id
            || tool.name.as_str() != request.name.as_str()
            || tool.action != surface_tool_action(request.action)
            || tool.target.as_ref().map(surface::DisplayText::as_str) != request.target.as_deref()
            || tool.raw_arguments.as_str() != raw_arguments
            || tool.arguments_digest != surface_sha256(raw_arguments.as_bytes())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "tool interaction differs from the committed provider request",
            ));
        }
        Ok(tool)
    }

    fn surface_authority_for_tool(
        snapshot: &surface::SurfaceSnapshot,
        fence: &surface::SurfaceOperationFence,
        tool: &surface::SurfaceToolRequest,
    ) -> io::Result<surface::AuthorityFingerprint> {
        let operation = Self::surface_operation_record(snapshot, &fence.operation_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface operation missing"))?;
        let generation = operation
            .generations
            .iter()
            .find(|generation| generation.fence == *fence)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "surface generation missing"))?;
        let surface::Replayability::Replayable {
            request_digest: Some(request_digest),
            cwd,
            workspace_roots,
            policy_epoch,
            tool_schema_digest,
            ..
        } = &generation.replayability
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effect-bearing interactions require a replayable generation authority",
            ));
        };
        if *policy_epoch != operation.intent.policy_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effect-bearing interaction policy epoch is stale",
            ));
        }
        let executable_generation = surface_sha256(
            &serde_json::to_vec(tool)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
        let workspace_roots_digest = surface_sha256(
            &serde_json::to_vec(workspace_roots)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
        Ok(surface::AuthorityFingerprint::new(
            fence.operation_id.clone(),
            request_digest.clone(),
            tool_schema_digest.clone(),
            cwd.clone(),
            workspace_roots_digest,
            *policy_epoch,
            executable_generation,
            tool.arguments_digest.clone(),
            generation.capability_fingerprint.clone(),
        ))
    }

    fn commit_surface_effect_interaction_request(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        kind: surface::SurfaceInteractionKind,
        request: surface::SurfaceInteractionRequest,
    ) -> io::Result<
        Option<(
            surface::SurfaceInteractionId,
            surface::BrokerInteractionRequestRecord,
            surface::BrokerInteractionResponseRoute,
            surface::InteractionRevision,
        )>,
    > {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effect interaction generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        let preferred = self
            .resident_surface
            .operation_origin_attachments
            .get(&fence.operation_id);
        let attachment_id = self
            .resident_surface
            .hub
            .select_interaction_attachment_for(kind, preferred);
        let unavailable = attachment_id.is_none();
        let interaction_id =
            surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let revision = surface::InteractionRevision::try_new(1).expect("one is valid");
        let route_epoch = surface::ResponseRouteEpoch::try_new(1).expect("one is valid");
        let record = surface::BrokerInteractionRequestRecord {
            thread_id: fence.thread_id.clone(),
            interaction_id: interaction_id.clone(),
            fence: fence.clone(),
            kind,
            request: request.clone(),
            response_token: surface::SurfaceResponseToken::new(random_token_bytes()),
            answer_policy: surface::BrokerInteractionAnswerPolicy::NativeStrict,
            recovery_disposition: surface::InteractionUnavailableDisposition::FailOperation,
        };
        let route = match attachment_id.as_ref() {
            Some(attachment_id) => surface::BrokerInteractionResponseRoute::Exclusive {
                epoch: route_epoch,
                attachment_id: attachment_id.clone(),
                grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
            },
            None => surface::BrokerInteractionResponseRoute::Unassigned { epoch: route_epoch },
        };
        let public_route = match attachment_id {
            Some(attachment_id) => surface::SurfaceInteractionRoute::Exclusive {
                epoch: route_epoch,
                attachment_id,
            },
            None => surface::SurfaceInteractionRoute::Unassigned { epoch: route_epoch },
        };
        let view = surface::SurfaceInteractionView {
            interaction_id: interaction_id.clone(),
            revision,
            fence: fence.clone(),
            kind,
            request,
            route: public_route,
            lifecycle: surface::SurfaceInteractionLifecycle::Requested,
            recovery_disposition: record.recovery_disposition.clone(),
        };
        let mut events = vec![(
            surface::SurfaceScope::Generation {
                fence: fence.clone(),
            },
            surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                interaction: view,
            }),
        )];
        if unavailable {
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                    interaction_id: interaction_id.clone(),
                    expected_revision: revision,
                    next_revision: surface::InteractionRevision::try_new(revision.get() + 1)
                        .expect("interaction revision did not exhaust"),
                    reason: surface::InteractionCancelReason::CapabilityUnavailable,
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence, &batch)
            .map_err(|error| {
                io::Error::other(format!("failed to commit effect interaction: {error:?}"))
            })?;
        if unavailable {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            return Ok(None);
        }
        Ok(Some((interaction_id, record, route, revision)))
    }

    fn request_surface_tool_approval(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        approval: orca_core::approval_types::ApprovalRequest,
        request: orca_core::tool_types::ToolRequest,
        reply: SyncSender<io::Result<orca_core::approval_types::ApprovalResolution>>,
    ) {
        let result = (|| -> io::Result<()> {
            let snapshot = self.resident_surface.coordinator.state().snapshot();
            let tool = Self::surface_tool_for_runtime_request(&snapshot, &fence, &request)?;
            let authority = Self::surface_authority_for_tool(&snapshot, &fence, &tool)?;
            let interaction_request = surface::SurfaceInteractionRequest::ToolApproval {
                tool,
                description: surface::DisplayText::new(approval.description.clone()),
                preview: approval.preview.clone().map(surface::DisplayText::new),
                authority,
            };
            let Some((interaction_id, record, route, revision)) = self
                .commit_surface_effect_interaction_request(
                    active,
                    fence,
                    surface::SurfaceInteractionKind::ToolApproval,
                    interaction_request,
                )?
            else {
                let _ = reply.send(Ok(orca_core::approval_types::ApprovalResolution {
                    id: approval.id,
                    decision: orca_core::approval_types::ApprovalDecision::Deny,
                    reason: "no runtime surface can answer tool approval".to_string(),
                }));
                return Ok(());
            };
            self.resident_surface.interactions.insert(
                interaction_id,
                ResidentSurfaceInteraction {
                    record,
                    route,
                    revision,
                    waiter: Some(ResidentInteractionWaiter::ToolApproval {
                        approval_id: approval.id,
                        waiter: reply.clone(),
                    }),
                    private_response: None,
                    winning_receipt: None,
                    resolution_ack: None,
                    projected_cursor: None,
                    cancelled: None,
                },
            );
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    fn request_surface_permission(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: crate::runtime_permission::RuntimePermissionRequest,
        reply: SyncSender<io::Result<crate::runtime_permission::RuntimePermissionResponse>>,
    ) {
        let result = (|| -> io::Result<()> {
            let snapshot = self.resident_surface.coordinator.state().snapshot();
            let tool_call_id =
                surface::SurfaceToolCallId::try_new(request.id.clone()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "empty permission request id")
                })?;
            let tool_request = snapshot
                .tools
                .iter()
                .find(|tool| tool.request.tool_call_id == tool_call_id)
                .map(|tool| tool.request.clone())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "permission interaction lacks a committed provider tool identity",
                    )
                })?;
            let operation = Self::surface_operation_record(&snapshot, &fence.operation_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "operation missing"))?;
            let generation = operation
                .generations
                .iter()
                .find(|generation| generation.fence == fence)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "generation missing"))?;
            if tool_request.source_response_id.is_none()
                || tool_request.turn_id != generation.logical_turn_id
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "permission request is not bound to the current provider tool",
                ));
            }
            let authority = Self::surface_authority_for_tool(&snapshot, &fence, &tool_request)?;
            let permissions = surface_permission_profile_from_runtime(request.permissions.clone())?;
            let interaction_request = surface::SurfaceInteractionRequest::PermissionRequest {
                tool_call_id: tool_request.tool_call_id,
                reason: request.reason.clone().map(surface::DisplayText::new),
                permissions,
                authority,
            };
            let Some((interaction_id, record, route, revision)) = self
                .commit_surface_effect_interaction_request(
                    active,
                    fence,
                    surface::SurfaceInteractionKind::PermissionRequest,
                    interaction_request,
                )?
            else {
                let _ = reply.send(Ok(crate::runtime_permission::RuntimePermissionResponse {
                    decision: crate::protocol::PermissionResponseDecision::Deny,
                    scope: crate::protocol::PermissionGrantScope::Turn,
                    permissions: request.permissions,
                    strict_auto_review: false,
                }));
                return Ok(());
            };
            self.resident_surface.interactions.insert(
                interaction_id,
                ResidentSurfaceInteraction {
                    record,
                    route,
                    revision,
                    waiter: Some(ResidentInteractionWaiter::Permission(reply.clone())),
                    private_response: None,
                    winning_receipt: None,
                    resolution_ack: None,
                    projected_cursor: None,
                    cancelled: None,
                },
            );
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    fn request_surface_user_input(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: crate::lifecycle::RuntimeUserInputRequest,
        reply: SyncSender<io::Result<Option<String>>>,
    ) {
        let result = self.request_surface_user_input_inner(active, fence, &request, reply.clone());
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    fn request_surface_user_input_inner(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: &crate::lifecycle::RuntimeUserInputRequest,
        reply: SyncSender<io::Result<Option<String>>>,
    ) -> io::Result<()> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "user-input request generation fence is stale",
            ));
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "runtime generation is terminalizing",
            ));
        }
        surface::NonEmptyText::try_new(request.id.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "empty user-input id"))?;
        let question = surface::NonEmptyText::try_new(request.question.clone()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "empty user-input question")
        })?;
        let preferred = self
            .resident_surface
            .operation_origin_attachments
            .get(&fence.operation_id);
        let attachment_id = self.resident_surface.hub.select_interaction_attachment_for(
            surface::SurfaceInteractionKind::UserInput,
            preferred,
        );
        let unavailable = attachment_id.is_none();
        let interaction_id =
            surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let revision = surface::InteractionRevision::try_new(1).expect("one is a valid revision");
        let route_epoch = surface::ResponseRouteEpoch::try_new(1).expect("one is a valid revision");
        let response_token = surface::SurfaceResponseToken::new(random_token_bytes());
        let response_grant_token = surface::SurfaceResponseGrantToken::new(random_token_bytes());
        let interaction_request = surface::SurfaceInteractionRequest::UserInput {
            question,
            suggestions: request
                .choices
                .iter()
                .cloned()
                .map(surface::DisplayText::new)
                .collect(),
        };
        let record = surface::BrokerInteractionRequestRecord {
            thread_id: fence.thread_id.clone(),
            interaction_id: interaction_id.clone(),
            fence: fence.clone(),
            kind: surface::SurfaceInteractionKind::UserInput,
            request: interaction_request.clone(),
            response_token,
            answer_policy: surface::BrokerInteractionAnswerPolicy::NativeStrict,
            recovery_disposition: surface::InteractionUnavailableDisposition::FailOperation,
        };
        let route = match attachment_id.as_ref() {
            Some(attachment_id) => surface::BrokerInteractionResponseRoute::Exclusive {
                epoch: route_epoch,
                attachment_id: attachment_id.clone(),
                grant_token: response_grant_token,
            },
            None => surface::BrokerInteractionResponseRoute::Unassigned { epoch: route_epoch },
        };
        let public_route = match attachment_id {
            Some(attachment_id) => surface::SurfaceInteractionRoute::Exclusive {
                epoch: route_epoch,
                attachment_id,
            },
            None => surface::SurfaceInteractionRoute::Unassigned { epoch: route_epoch },
        };
        let view = surface::SurfaceInteractionView {
            interaction_id: interaction_id.clone(),
            revision,
            fence: fence.clone(),
            kind: record.kind,
            request: interaction_request,
            route: public_route,
            lifecycle: surface::SurfaceInteractionLifecycle::Requested,
            recovery_disposition: record.recovery_disposition.clone(),
        };
        let mut events = vec![(
            surface::SurfaceScope::Generation {
                fence: fence.clone(),
            },
            surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                interaction: view,
            }),
        )];
        if unavailable {
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                    interaction_id: interaction_id.clone(),
                    expected_revision: revision,
                    next_revision: surface::InteractionRevision::try_new(revision.get() + 1)
                        .expect("interaction revision did not exhaust"),
                    reason: surface::InteractionCancelReason::CapabilityUnavailable,
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &batch)
            .map_err(|error| {
                io::Error::other(format!("failed to commit user-input request: {error:?}"))
            })?;
        if unavailable {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            let _ = reply.send(Ok(None));
            return Ok(());
        }
        self.resident_surface.interactions.insert(
            interaction_id.clone(),
            ResidentSurfaceInteraction {
                record,
                route,
                revision,
                waiter: Some(ResidentInteractionWaiter::UserInput(reply)),
                private_response: None,
                winning_receipt: None,
                resolution_ack: None,
                projected_cursor: None,
                cancelled: None,
            },
        );
        Ok(())
    }

    fn request_surface_mcp_elicitation(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: orca_mcp::McpElicitationRequest,
        reply: SyncSender<Result<orca_mcp::McpElicitationResponse, String>>,
    ) {
        let result =
            self.request_surface_mcp_elicitation_inner(active, fence, &request, reply.clone());
        if let Err(error) = result {
            let _ = reply.send(Err(error));
        }
    }

    fn request_surface_mcp_elicitation_inner(
        &mut self,
        active: &mut ActiveOperation,
        fence: surface::SurfaceOperationFence,
        request: &orca_mcp::McpElicitationRequest,
        reply: SyncSender<Result<orca_mcp::McpElicitationResponse, String>>,
    ) -> Result<(), String> {
        if active.surface_operation.as_ref() != Some(&fence) {
            return Err("MCP elicitation generation fence is stale".to_string());
        }
        if Self::surface_interaction_admission_closed(active) {
            return Err("runtime generation is terminalizing".to_string());
        }
        let opaque_request_id = surface::NonEmptyText::try_new(request.id.clone())
            .map_err(|_| "empty MCP elicitation id".to_string())?;
        let server_name = surface::NonEmptyText::try_new(request.server_name.clone())
            .map_err(|_| "empty MCP server name".to_string())?;
        let requested_schema = request
            .requested_schema
            .as_ref()
            .map(surface_data_from_json)
            .transpose()?;
        let mcp_request = match request.mode {
            orca_mcp::McpElicitationMode::Form => surface::SurfaceMcpElicitationRequest::Form {
                requested_schema,
                supported_schema: None,
            },
            orca_mcp::McpElicitationMode::Url => surface::SurfaceMcpElicitationRequest::Url {
                raw_url: request.url.clone().map(surface::DisplayText::new),
                requested_schema,
            },
        };
        let preferred = self
            .resident_surface
            .operation_origin_attachments
            .get(&fence.operation_id);
        let attachment_id = self.resident_surface.hub.select_interaction_attachment_for(
            surface::SurfaceInteractionKind::McpElicitation,
            preferred,
        );
        let unavailable = attachment_id.is_none();
        let interaction_id =
            surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let revision = surface::InteractionRevision::try_new(1).expect("one is a valid revision");
        let route_epoch =
            surface::ResponseRouteEpoch::try_new(1).expect("one is a valid route epoch");
        let interaction_request = surface::SurfaceInteractionRequest::McpElicitation {
            server_name,
            server_request_id: opaque_request_id.clone(),
            message: surface::DisplayText::new(request.message.clone()),
            request: mcp_request,
        };
        let record = surface::BrokerInteractionRequestRecord {
            thread_id: fence.thread_id.clone(),
            interaction_id: interaction_id.clone(),
            fence: fence.clone(),
            kind: surface::SurfaceInteractionKind::McpElicitation,
            request: interaction_request.clone(),
            response_token: surface::SurfaceResponseToken::new(random_token_bytes()),
            answer_policy: surface::BrokerInteractionAnswerPolicy::NativeStrict,
            recovery_disposition: surface::InteractionUnavailableDisposition::FailOperation,
        };
        let route = match attachment_id.as_ref() {
            Some(attachment_id) => surface::BrokerInteractionResponseRoute::Exclusive {
                epoch: route_epoch,
                attachment_id: attachment_id.clone(),
                grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
            },
            None => surface::BrokerInteractionResponseRoute::Unassigned { epoch: route_epoch },
        };
        let public_route = match attachment_id {
            Some(attachment_id) => surface::SurfaceInteractionRoute::Exclusive {
                epoch: route_epoch,
                attachment_id,
            },
            None => surface::SurfaceInteractionRoute::Unassigned { epoch: route_epoch },
        };
        let view = surface::SurfaceInteractionView {
            interaction_id: interaction_id.clone(),
            revision,
            fence: fence.clone(),
            kind: record.kind,
            request: interaction_request,
            route: public_route,
            lifecycle: surface::SurfaceInteractionLifecycle::Requested,
            recovery_disposition: record.recovery_disposition.clone(),
        };
        let mut events = vec![(
            surface::SurfaceScope::Generation {
                fence: fence.clone(),
            },
            surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                interaction: view,
            }),
        )];
        if unavailable {
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                    interaction_id: interaction_id.clone(),
                    expected_revision: revision,
                    next_revision: surface::InteractionRevision::try_new(revision.get() + 1)
                        .expect("interaction revision did not exhaust"),
                    reason: surface::InteractionCancelReason::CapabilityUnavailable,
                }),
            ));
        }
        let batch = self.surface_event_batch_with_commit_id(events, None);
        self.resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &batch)
            .map_err(|error| format!("failed to commit MCP elicitation request: {error:?}"))?;
        if unavailable {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
            let _ = reply.send(Ok(orca_mcp::McpElicitationResponse::Decline));
            return Ok(());
        }
        self.resident_surface.interactions.insert(
            interaction_id.clone(),
            ResidentSurfaceInteraction {
                record,
                route,
                revision,
                waiter: Some(ResidentInteractionWaiter::McpElicitation(reply)),
                private_response: None,
                winning_receipt: None,
                resolution_ack: None,
                projected_cursor: None,
                cancelled: None,
            },
        );
        Ok(())
    }

    fn respond_surface_interaction_by_id(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        self.respond_surface_interaction_by_id_with_policy(
            client,
            request_id,
            interaction_id,
            answer,
            surface::BrokerInteractionAnswerPolicy::NativeStrict,
        )
    }

    fn respond_surface_interaction_by_id_with_policy(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        interaction_id: surface::SurfaceInteractionId,
        answer: surface::SurfaceClientInteractionAnswer,
        policy: surface::BrokerInteractionAnswerPolicy,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        let interaction = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        if interaction.cancelled.is_some() || interaction.winning_receipt.is_some() {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let expected_kind = interaction.record.kind;
        if !self
            .resident_surface
            .hub
            .admits_interaction_client(client, expected_kind)
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let selector = exact_interaction_selectors(interaction)
            .into_iter()
            .find_map(|(attachment_id, selector)| {
                (attachment_id == *client.attachment_id()).then_some(selector)
            })
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let response_id = interaction
            .private_response
            .as_ref()
            .map(|winner| winner.record.receipt.response_id.clone())
            .unwrap_or_else(|| {
                surface::SurfaceResponseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7")
            });
        let authority = interaction_answer_authority(&interaction.record.request, &answer);
        let response =
            surface::BoundInteractionResponse::new(response_id, answer, policy, authority);
        self.respond_surface_interaction(client, request_id, selector, response)
    }

    fn respond_surface_interaction(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        selector: surface::InteractionSelector,
        response: surface::BoundInteractionResponse,
    ) -> Result<
        surface::MutationReply<surface::RespondInteractionOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.resident_surface.pending_detaches.is_empty()
            || !self.resident_surface.pending_capability_losses.is_empty()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let (interaction_id, expected_kind, exact) = match selector {
            surface::InteractionSelector::OpaqueRequestId { .. } => {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
            surface::InteractionSelector::Exact {
                interaction_id,
                expected_revision,
                kind,
                response_token,
                response_route_epoch,
                response_grant_token,
                operation_fence,
            } => (
                interaction_id,
                kind,
                ExactInteractionSelectorBinding {
                    expected_revision,
                    response_token,
                    route_epoch: response_route_epoch,
                    grant_token: response_grant_token,
                    operation_fence,
                },
            ),
        };
        let interaction = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        {
            if interaction.revision != exact.expected_revision {
                return Ok(Self::stale_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::StaleRevision,
                    "interaction revision is stale",
                ));
            }
            if interaction.record.fence != exact.operation_fence {
                return Ok(Self::stale_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::StaleFence,
                    "interaction operation fence is stale",
                ));
            }
            if interaction.record.response_token != exact.response_token {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::WrongResponseToken,
                    "interaction response token does not match",
                ));
            }
            if interaction_route_epoch(&interaction.route) != exact.route_epoch {
                return Ok(Self::stale_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::StaleResponseRoute,
                    "interaction response route is stale",
                ));
            }
            if !interaction_route_admits_exact(
                &interaction.route,
                client.attachment_id(),
                exact.route_epoch,
                &exact.grant_token,
            ) {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::WrongAttachment,
                    "attachment does not hold the exact private response grant",
                ));
            }
        }
        if interaction.cancelled.is_some() {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::IllegalState,
                "interaction is already terminal",
            ));
        }
        if interaction.record.kind != expected_kind
            || interaction_answer_kind(response.answer()) != interaction.record.kind
        {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::WrongInteractionKind,
                "interaction request and answer kinds do not match",
            ));
        }
        if interaction.record.answer_policy != *response.policy() {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::InvalidInput,
                "interaction answer policy does not match the persisted request",
            ));
        }
        if !interaction_answer_authority_matches(
            &interaction.record.request,
            response.answer(),
            response.authority(),
        ) {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::WrongAuthorityFingerprint,
                "interaction response authority does not match the persisted request",
            ));
        }
        if let (
            surface::SurfaceInteractionRequest::PermissionRequest {
                permissions: requested,
                ..
            },
            surface::SurfaceClientInteractionAnswer::PermissionRequest {
                decision:
                    surface::SurfacePermissionClientDecision::Allow {
                        scope, permissions, ..
                    },
            },
        ) = (&interaction.record.request, response.answer())
        {
            if *scope == surface::PermissionGrantScope::Session {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::InvalidInput,
                    "session permission grants require runtime settings ownership",
                ));
            }
            if !surface_permission_profile_is_subset(permissions, requested) {
                return Ok(Self::uncommitted_interaction_response(
                    request_id,
                    interaction,
                    surface::SurfaceMutationErrorCode::InvalidInput,
                    "permission response exceeds the persisted requested profile",
                ));
            }
        }
        if !self
            .resident_surface
            .hub
            .admits_interaction_client(client, expected_kind)
            || !interaction_route_admits(&interaction.route, client.attachment_id())
        {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::WrongAttachment,
                "attachment does not hold the current interaction response grant",
            ));
        }
        if !interaction_answer_within_private_limits(response.answer()) {
            return Ok(Self::uncommitted_interaction_response(
                request_id,
                interaction,
                surface::SurfaceMutationErrorCode::InvalidInput,
                "interaction answer exceeds private retention limits",
            ));
        }
        if let Some(winning_receipt) = interaction.winning_receipt.clone() {
            let acknowledgement = interaction
                .resolution_ack
                .clone()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![acknowledgement])
                        .expect("interaction replay has one acknowledgement"),
                },
                value: surface::RespondInteractionOutput {
                    interaction_id,
                    attempted_response_id: response.response_id().clone(),
                    disposition: surface::RespondInteractionDisposition::AlreadyResolved {
                        winning_receipt,
                    },
                    projected_cursor: interaction.projected_cursor.clone(),
                },
            });
        }
        let expected_revision = interaction.revision;
        let next_revision = surface::InteractionRevision::try_new(expected_revision.get() + 1)
            .expect("interaction revision did not exhaust");
        let fence = interaction.record.fence.clone();
        let attempted_digest = keyed_interaction_response_digest(
            &interaction.record.response_token,
            response.answer(),
        );
        let (receipt, winner_answer, attempted_private_winner) =
            match interaction.private_response.as_ref() {
                Some(winner) => (
                    winner.record.receipt.clone(),
                    winner.answer.clone(),
                    winner.record.receipt.response_id == *response.response_id()
                        && winner.record.keyed_response_digest == attempted_digest,
                ),
                None => {
                    let receipt = surface::SurfaceInteractionResolutionReceipt {
                        response_id: response.response_id().clone(),
                        receipt_id: surface::SurfaceResponseReceiptId::try_from_bytes(
                            *uuid::Uuid::now_v7().as_bytes(),
                        )
                        .expect("generated UUID is v7"),
                        kind: expected_kind,
                        safe_projection: interaction_safe_projection(response.answer()),
                    };
                    let private_response = ResidentPrivateInteractionResponse {
                        record: surface::BrokerInteractionResponseRecord {
                            receipt: receipt.clone(),
                            payload: surface::BrokerResponsePayload::LiveOnly {
                                incarnation: self
                                    .resident_surface
                                    .coordinator
                                    .state()
                                    .snapshot()
                                    .cursor
                                    .incarnation
                                    .clone(),
                            },
                            keyed_response_digest: attempted_digest,
                        },
                        answer: response.answer().clone(),
                        pending_batch: None,
                        retry_at: None,
                    };
                    self.resident_surface
                        .interactions
                        .get_mut(&interaction_id)
                        .expect("validated interaction remains resident")
                        .private_response = Some(private_response);
                    (receipt, response.answer().clone(), true)
                }
            };
        let batch = if let Some(batch) = self
            .resident_surface
            .interactions
            .get(&interaction_id)
            .and_then(|interaction| interaction.private_response.as_ref())
            .and_then(|private| private.pending_batch.clone())
        {
            batch
        } else {
            let batch = self.surface_event_batch_with_commit_id(
                vec![(
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Resolved {
                        interaction_id: interaction_id.clone(),
                        expected_revision,
                        next_revision,
                        receipt: receipt.clone(),
                    }),
                )],
                None,
            );
            self.resident_surface
                .interactions
                .get_mut(&interaction_id)
                .and_then(|interaction| interaction.private_response.as_mut())
                .expect("private winner exists before public resolution")
                .pending_batch = Some(batch.clone());
            batch
        };
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence, &batch)
        {
            eprintln!("orca: typed interaction resolution commit failed: {error:?}");
            self.resident_surface
                .interactions
                .get_mut(&interaction_id)
                .and_then(|interaction| interaction.private_response.as_mut())
                .expect("failed private resolution remains resident")
                .retry_at =
                Some(tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL);
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.apply_surface_interaction_resolution(&interaction_id, &winner_answer);
        let output = surface::RespondInteractionOutput {
            interaction_id: interaction_id.clone(),
            attempted_response_id: response.response_id().clone(),
            disposition: if attempted_private_winner {
                surface::RespondInteractionDisposition::Resolved { receipt }
            } else {
                surface::RespondInteractionDisposition::AlreadyResolved {
                    winning_receipt: receipt,
                }
            },
            projected_cursor: Some(batch.cursor_after.clone()),
        };
        Ok(Self::committed_interaction_mutation(
            request_id,
            interaction_id,
            &batch,
            output,
        ))
    }

    fn apply_surface_interaction_resolution(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
        winner_answer: &surface::SurfaceClientInteractionAnswer,
    ) {
        let waiter = self
            .resident_surface
            .interactions
            .remove(interaction_id)
            .expect("committed interaction remains resident")
            .waiter;
        if let Some(waiter) = waiter {
            match (waiter, winner_answer) {
                (
                    ResidentInteractionWaiter::ToolApproval {
                        approval_id,
                        waiter,
                    },
                    surface::SurfaceClientInteractionAnswer::ToolApproval { decision },
                ) => {
                    let (decision, reason) = match decision {
                        surface::SurfaceAllowDeny::Allow => (
                            orca_core::approval_types::ApprovalDecision::Allow,
                            "approved through runtime surface",
                        ),
                        surface::SurfaceAllowDeny::Deny => (
                            orca_core::approval_types::ApprovalDecision::Deny,
                            "denied through runtime surface",
                        ),
                    };
                    let _ = waiter.send(Ok(orca_core::approval_types::ApprovalResolution {
                        id: approval_id,
                        decision,
                        reason: reason.to_string(),
                    }));
                }
                (
                    ResidentInteractionWaiter::Permission(waiter),
                    surface::SurfaceClientInteractionAnswer::PermissionRequest { decision },
                ) => {
                    let (decision, scope, permissions, strict_auto_review) = match decision {
                        surface::SurfacePermissionClientDecision::Allow {
                            scope,
                            permissions,
                            strict_auto_review,
                        } => (
                            crate::protocol::PermissionResponseDecision::Allow,
                            scope,
                            permissions,
                            strict_auto_review,
                        ),
                        surface::SurfacePermissionClientDecision::Deny {
                            scope,
                            permissions,
                            strict_auto_review,
                        } => (
                            crate::protocol::PermissionResponseDecision::Deny,
                            scope,
                            permissions,
                            strict_auto_review,
                        ),
                    };
                    let scope = match scope {
                        surface::PermissionGrantScope::Turn => {
                            crate::protocol::PermissionGrantScope::Turn
                        }
                        surface::PermissionGrantScope::Session => {
                            crate::protocol::PermissionGrantScope::Session
                        }
                    };
                    let _ = waiter.send(Ok(crate::runtime_permission::RuntimePermissionResponse {
                        decision,
                        scope,
                        permissions: runtime_permission_profile_from_surface(permissions),
                        strict_auto_review: *strict_auto_review,
                    }));
                }
                (
                    ResidentInteractionWaiter::UserInput(waiter),
                    surface::SurfaceClientInteractionAnswer::UserInput { decision },
                ) => {
                    let answer = match decision {
                        surface::SurfaceUserInputDecision::Answer(answer) => {
                            Some(answer.as_str().to_string())
                        }
                        surface::SurfaceUserInputDecision::Cancel => None,
                    };
                    let _ = waiter.send(Ok(answer));
                }
                (
                    ResidentInteractionWaiter::McpElicitation(waiter),
                    surface::SurfaceClientInteractionAnswer::McpElicitation { decision },
                ) => {
                    let response = match decision {
                        surface::SurfaceMcpElicitationDecision::Accept { content } => {
                            orca_mcp::McpElicitationResponse::Accept {
                                content: json_from_surface_data(content),
                            }
                        }
                        surface::SurfaceMcpElicitationDecision::Decline => {
                            orca_mcp::McpElicitationResponse::Decline
                        }
                    };
                    let _ = waiter.send(Ok(response));
                }
                _ => unreachable!("waiter and answer kind were validated before commit"),
            }
        }
    }

    fn uncommitted_interaction_response(
        request_id: surface::SurfaceRequestId,
        interaction: &ResidentSurfaceInteraction,
        code: surface::SurfaceMutationErrorCode,
        message: &'static str,
    ) -> surface::MutationReply<surface::RespondInteractionOutput> {
        surface::MutationReply::Uncommitted {
            mutation: surface::UncommittedMutation::Invalid {
                request_id,
                target: Some(surface::MutationTarget::Interaction {
                    thread_id: interaction.record.thread_id.clone(),
                    interaction_id: interaction.record.interaction_id.clone(),
                }),
                error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                    code,
                    message: surface::DisplayText::new(message),
                    winning_request_id: None,
                    current_revision: Some(surface::SurfaceMutationRevision::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction.record.interaction_id.clone(),
                        revision: interaction.revision,
                        route_epoch: interaction_route_epoch(&interaction.route),
                    }),
                }),
            },
        }
    }

    fn stale_interaction_response(
        request_id: surface::SurfaceRequestId,
        interaction: &ResidentSurfaceInteraction,
        code: surface::SurfaceMutationErrorCode,
        message: &'static str,
    ) -> surface::MutationReply<surface::RespondInteractionOutput> {
        surface::MutationReply::Uncommitted {
            mutation: surface::UncommittedMutation::Stale {
                request_id,
                target: Some(surface::MutationTarget::Interaction {
                    thread_id: interaction.record.thread_id.clone(),
                    interaction_id: interaction.record.interaction_id.clone(),
                }),
                error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                    code,
                    message: surface::DisplayText::new(message),
                    winning_request_id: None,
                    current_revision: Some(surface::SurfaceMutationRevision::Interaction {
                        thread_id: interaction.record.thread_id.clone(),
                        interaction_id: interaction.record.interaction_id.clone(),
                        revision: interaction.revision,
                        route_epoch: interaction_route_epoch(&interaction.route),
                    }),
                }),
            },
        }
    }

    fn prepare_surface_terminalization(
        &self,
        fence: &surface::SurfaceOperationFence,
        request_id: surface::SurfaceRequestId,
        cause: surface::TerminalizationCause,
    ) -> Result<PreparedSurfaceTerminalization, surface::SurfaceClientCommandError> {
        if self
            .resident_surface
            .interactions
            .values()
            .any(|interaction| {
                &interaction.record.fence == fence && interaction.private_response.is_some()
            })
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let reason = match cause {
            surface::TerminalizationCause::HostShutdown => {
                surface::InteractionCancelReason::HostShutdown
            }
            surface::TerminalizationCause::ThreadClose => {
                surface::InteractionCancelReason::ThreadClose
            }
            surface::TerminalizationCause::UserCancel => {
                surface::InteractionCancelReason::OperationCancelled {
                    reason: surface::CancelReason::User,
                }
            }
            surface::TerminalizationCause::GoalPause => {
                surface::InteractionCancelReason::OperationCancelled {
                    reason: surface::CancelReason::GoalPause,
                }
            }
        };
        let mut interactions = self
            .resident_surface
            .interactions
            .iter()
            .filter(|(_, interaction)| {
                &interaction.record.fence == fence
                    && interaction.winning_receipt.is_none()
                    && interaction.cancelled.is_none()
                    && interaction.private_response.is_none()
            })
            .map(|(interaction_id, interaction)| (interaction_id.clone(), interaction.revision))
            .collect::<Vec<_>>();
        interactions.sort_by_key(|(interaction_id, _)| interaction_id.clone());
        let mut events = vec![(
            surface::SurfaceScope::Operation {
                operation_id: fence.operation_id.clone(),
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::ControlIntentCommitted {
                operation_id: fence.operation_id.clone(),
                request_id,
                intent: surface::PendingControlIntent::Terminalize {
                    operation_id: fence.operation_id.clone(),
                    cause,
                },
            }),
        )];
        for (interaction_id, expected_revision) in &interactions {
            let next_revision =
                surface::InteractionRevision::try_new(expected_revision.get().saturating_add(1))
                    .expect("interaction revision did not exhaust");
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                    interaction_id: interaction_id.clone(),
                    expected_revision: *expected_revision,
                    next_revision,
                    reason: reason.clone(),
                }),
            ));
        }
        Ok(PreparedSurfaceTerminalization {
            fence: fence.clone(),
            cause,
            batch: self.surface_event_batch_with_commit_id(events, None),
            interaction_ids: interactions
                .into_iter()
                .map(|(interaction_id, _)| interaction_id)
                .collect(),
            retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
        })
    }

    fn apply_surface_interaction_cancellations(
        &mut self,
        interaction_ids: &[surface::SurfaceInteractionId],
    ) {
        for interaction_id in interaction_ids {
            let waiter = self
                .resident_surface
                .interactions
                .remove(interaction_id)
                .expect("committed interaction remains resident")
                .waiter;
            if let Some(waiter) = waiter {
                match waiter {
                    ResidentInteractionWaiter::ToolApproval { waiter, .. } => {
                        let _ = waiter.send(Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "tool approval was cancelled before resolution",
                        )));
                    }
                    ResidentInteractionWaiter::Permission(waiter) => {
                        let _ = waiter.send(Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "permission request was cancelled before resolution",
                        )));
                    }
                    ResidentInteractionWaiter::UserInput(waiter) => {
                        let _ = waiter.send(Ok(None));
                    }
                    ResidentInteractionWaiter::McpElicitation(waiter) => {
                        let _ = waiter.send(Ok(orca_mcp::McpElicitationResponse::Decline));
                    }
                }
            }
        }
    }

    fn prepare_surface_attachment_transition(
        &self,
        attachment_id: &surface::SurfaceAttachmentId,
    ) -> Result<Option<PreparedSurfaceAttachmentTransition>, ()> {
        let mut affected = self
            .resident_surface
            .interactions
            .iter()
            .filter(|(_, interaction)| {
                interaction.winning_receipt.is_none()
                    && interaction.cancelled.is_none()
                    && interaction.private_response.is_none()
                    && interaction_route_admits(&interaction.route, attachment_id)
            })
            .map(|(interaction_id, interaction)| {
                (
                    interaction_id.clone(),
                    interaction.record.kind,
                    interaction.record.fence.clone(),
                    interaction.revision,
                    interaction_route_epoch(&interaction.route),
                )
            })
            .collect::<Vec<_>>();
        affected.sort_by_key(|(interaction_id, ..)| interaction_id.clone());
        let Some((_, _, fence, _, _)) = affected.first() else {
            return Ok(None);
        };
        let fence = fence.clone();
        if affected
            .iter()
            .any(|(_, _, candidate, _, _)| candidate != &fence)
        {
            return Err(());
        }
        let mut events = Vec::new();
        let mut interactions = Vec::new();
        let mut affected_route_epochs = Vec::new();
        for (interaction_id, kind, _, expected_revision, current_epoch) in affected {
            let route_revision = surface::InteractionRevision::try_new(expected_revision.get() + 1)
                .expect("interaction revision did not exhaust");
            let next_epoch = surface::ResponseRouteEpoch::try_new(current_epoch.get() + 1)
                .expect("route epoch did not exhaust");
            let fallback = self
                .resident_surface
                .hub
                .select_interaction_attachment_excluding(kind, None, Some(attachment_id));
            let (private_route, public_route, cancelled) = match fallback {
                Some(fallback) => (
                    surface::BrokerInteractionResponseRoute::Exclusive {
                        epoch: next_epoch,
                        attachment_id: fallback.clone(),
                        grant_token: surface::SurfaceResponseGrantToken::new(random_token_bytes()),
                    },
                    surface::SurfaceInteractionRoute::Exclusive {
                        epoch: next_epoch,
                        attachment_id: fallback,
                    },
                    false,
                ),
                None => (
                    surface::BrokerInteractionResponseRoute::Unassigned { epoch: next_epoch },
                    surface::SurfaceInteractionRoute::Unassigned { epoch: next_epoch },
                    true,
                ),
            };
            events.push((
                surface::SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                surface::SurfaceEvent::Interaction(surface::InteractionPatch::RouteChanged {
                    interaction_id: interaction_id.clone(),
                    expected_revision,
                    next_revision: route_revision,
                    route: public_route,
                }),
            ));
            let revision = if cancelled {
                let cancelled_revision =
                    surface::InteractionRevision::try_new(route_revision.get() + 1)
                        .expect("interaction revision did not exhaust");
                events.push((
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                        interaction_id: interaction_id.clone(),
                        expected_revision: route_revision,
                        next_revision: cancelled_revision,
                        reason: surface::InteractionCancelReason::CapabilityUnavailable,
                    }),
                ));
                cancelled_revision
            } else {
                route_revision
            };
            affected_route_epochs.push((interaction_id.clone(), next_epoch));
            interactions.push(PreparedSurfaceDetachInteraction {
                interaction_id,
                revision,
                route: private_route,
                cancelled,
            });
        }
        let commit_id = surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
            .expect("generated UUID is v7");
        let batch = self.surface_event_batch_with_commit_id(events, Some(commit_id.clone()));
        Ok(Some(PreparedSurfaceAttachmentTransition {
            fence,
            batch,
            commit_id,
            affected_route_epochs,
            interactions,
        }))
    }

    fn apply_surface_attachment_transition(
        &mut self,
        active: Option<&mut ActiveOperation>,
        transition: &PreparedSurfaceAttachmentTransition,
    ) {
        let mut cancelled_waiters = Vec::new();
        for prepared in &transition.interactions {
            if prepared.cancelled {
                if let Some(waiter) = self
                    .resident_surface
                    .interactions
                    .remove(&prepared.interaction_id)
                    .expect("committed interaction remains resident")
                    .waiter
                {
                    cancelled_waiters.push(waiter);
                }
            } else {
                let interaction = self
                    .resident_surface
                    .interactions
                    .get_mut(&prepared.interaction_id)
                    .expect("committed interaction remains resident");
                interaction.revision = prepared.revision;
                interaction.route = prepared.route.clone();
            }
        }
        if !cancelled_waiters.is_empty()
            && let Some(active) = active
        {
            active.surface_execution_failure =
                Some(surface::GenerationExecutionFailureClass::ClientCapabilityUnavailable);
        }
        for waiter in cancelled_waiters {
            match waiter {
                ResidentInteractionWaiter::ToolApproval { waiter, .. } => {
                    let _ = waiter.send(Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "tool approval capability became unavailable",
                    )));
                }
                ResidentInteractionWaiter::Permission(waiter) => {
                    let _ = waiter.send(Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "permission capability became unavailable",
                    )));
                }
                ResidentInteractionWaiter::UserInput(waiter) => {
                    let _ = waiter.send(Ok(None));
                }
                ResidentInteractionWaiter::McpElicitation(waiter) => {
                    let _ = waiter.send(Ok(orca_mcp::McpElicitationResponse::Decline));
                }
            }
        }
    }

    fn next_invalid_surface_interaction_attachment(&self) -> Option<surface::SurfaceAttachmentId> {
        self.resident_surface
            .interactions
            .iter()
            .filter(|(_, interaction)| {
                interaction.winning_receipt.is_none()
                    && interaction.cancelled.is_none()
                    && interaction.private_response.is_none()
            })
            .flat_map(|(interaction_id, interaction)| {
                interaction_route_attachments(&interaction.route)
                    .into_iter()
                    .filter(|attachment_id| {
                        !self
                            .resident_surface
                            .hub
                            .admits_interaction_attachment(attachment_id, interaction.record.kind)
                    })
                    .map(|attachment_id| (interaction_id.clone(), attachment_id))
            })
            .min()
            .map(|(_, attachment_id)| attachment_id)
    }

    fn reconcile_surface_interaction_capabilities(
        &mut self,
        mut active: Option<&mut ActiveOperation>,
    ) {
        if !self.resident_surface.pending_detaches.is_empty()
            || !self.resident_surface.pending_capability_losses.is_empty()
        {
            return;
        }
        while let Some(attachment_id) = self.next_invalid_surface_interaction_attachment() {
            let transition = match self.prepare_surface_attachment_transition(&attachment_id) {
                Ok(Some(transition)) => transition,
                Ok(None) | Err(()) => return,
            };
            if self
                .resident_surface
                .coordinator
                .commit_generation_batch(transition.fence.clone(), &transition.batch)
                .is_err()
            {
                eprintln!("orca: typed interaction capability-loss commit failed");
                self.resident_surface.pending_capability_losses.insert(
                    attachment_id,
                    PendingSurfaceCapabilityLoss {
                        transition,
                        retry_at: tokio::time::Instant::now()
                            + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                    },
                );
                return;
            }
            self.apply_surface_attachment_transition(active.as_deref_mut(), &transition);
        }
    }

    fn next_surface_transition_retry_at(&self) -> Option<tokio::time::Instant> {
        self.resident_surface.0.as_ref().and_then(|resident| {
            resident
                .pending_admission_commits
                .values()
                .map(|pending| pending.retry_at)
                .chain(
                    resident
                        .pending_admission_repairs
                        .values()
                        .map(|pending| pending.retry_at),
                )
                .chain(
                    resident
                        .pending_admission_terminals
                        .values()
                        .map(|pending| pending.retry_at),
                )
                .chain(
                    resident
                        .pending_terminalization
                        .iter()
                        .map(|pending| pending.retry_at),
                )
                .chain(resident.interactions.values().filter_map(|interaction| {
                    interaction
                        .private_response
                        .as_ref()
                        .and_then(|private| private.retry_at)
                }))
                .chain(
                    resident
                        .pending_detaches
                        .values()
                        .map(|pending| pending.retry_at),
                )
                .chain(
                    resident
                        .pending_capability_losses
                        .values()
                        .map(|pending| pending.retry_at),
                )
                .min()
        })
    }

    fn has_pending_surface_transition_retry(&self) -> bool {
        self.next_surface_transition_retry_at().is_some()
    }

    fn retry_private_surface_interaction(
        &mut self,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let (fence, batch, winner_answer) = {
            let interaction = self
                .resident_surface
                .interactions
                .get(interaction_id)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            let private = interaction
                .private_response
                .as_ref()
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            (
                interaction.record.fence.clone(),
                private
                    .pending_batch
                    .clone()
                    .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
                private.answer.clone(),
            )
        };
        if self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence, &batch)
            .is_err()
        {
            if let Some(private) = self
                .resident_surface
                .interactions
                .get_mut(interaction_id)
                .and_then(|interaction| interaction.private_response.as_mut())
            {
                private.retry_at =
                    Some(tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL);
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.apply_surface_interaction_resolution(interaction_id, &winner_answer);
        Ok(())
    }

    fn drain_private_surface_interactions(
        &mut self,
        fence: &surface::SurfaceOperationFence,
    ) -> Result<(), surface::SurfaceClientCommandError> {
        let mut pending = self
            .resident_surface
            .interactions
            .iter()
            .filter_map(|(interaction_id, interaction)| {
                (&interaction.record.fence == fence)
                    .then_some(interaction.private_response.as_ref())
                    .flatten()
                    .and_then(|private| {
                        private
                            .pending_batch
                            .as_ref()
                            .map(|batch| (batch.cursor_before.next_seq, interaction_id.clone()))
                    })
            })
            .collect::<Vec<_>>();
        pending.sort();
        for (_, interaction_id) in pending {
            self.retry_private_surface_interaction(&interaction_id)?;
        }
        if self
            .resident_surface
            .interactions
            .values()
            .any(|interaction| {
                &interaction.record.fence == fence && interaction.private_response.is_some()
            })
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        Ok(())
    }

    fn retry_surface_admission_repair(&mut self, operation_id: &surface::SurfaceOperationId) {
        let Some(pending) = self
            .resident_surface
            .pending_admission_repairs
            .get(operation_id)
            .cloned()
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_live_generation_stop_disposition_batch(
                pending.fence.clone(),
                operation_id.clone(),
                pending.finalize_intent_id.clone(),
                &pending.batch,
            )
            .is_err()
        {
            if let Some(retained) = self
                .resident_surface
                .pending_admission_repairs
                .get_mut(operation_id)
            {
                retained.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            }
            return;
        }
        self.resident_surface
            .pending_admission_repairs
            .remove(operation_id);
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: operation_id.clone(),
                    finalize_intent_id: pending.finalize_intent_id.clone(),
                    terminal: pending.terminal.clone(),
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
            Some(pending.terminal_commit_id.clone()),
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal: pending.terminal,
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if let Err(error) = self.resident_surface.coordinator.commit_finalizer_batch(
            operation_id.clone(),
            pending.finalize_intent_id.clone(),
            &terminal_batch,
        ) {
            eprintln!("orca: typed surface admission repair terminal retry failed: {error:?}");
            let finalize_intent_id = pending.finalize_intent_id.clone();
            let terminal_commit_id = pending.terminal_commit_id.clone();
            let repair = surface::RetryFinalizationToken::new(
                pending.original_request_id,
                pending.fence.thread_id.clone(),
                operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                pending.fence.thread_owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            self.resident_surface.pending_admission_terminals.insert(
                operation_id.clone(),
                PendingSurfaceAdmissionTerminal {
                    pending: PendingSurfaceTerminalCommit {
                        batch: terminal_batch,
                        value,
                        failure: surface::WaitOperationTerminalResult::TerminalCommitFailure {
                            operation_id: operation_id.clone(),
                            finalize_intent_id,
                            commit_id: terminal_commit_id,
                            repair,
                        },
                        legacy_completion: None,
                        legacy_terminal: None,
                    },
                    retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                },
            );
            return;
        }
        self.cache_surface_terminal(value);
        if self.resident_surface.pending_terminal_commits.is_empty() {
            self.surface_terminal_blocked = None;
        }
    }

    fn retry_surface_admission_commit(&mut self, operation_id: &surface::SurfaceOperationId) {
        let Some(pending) = self
            .resident_surface
            .pending_admission_commits
            .get(operation_id)
            .cloned()
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_actor_batch(&pending.batch)
            .is_err()
        {
            if let Some(retained) = self
                .resident_surface
                .pending_admission_commits
                .get_mut(operation_id)
            {
                retained.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            }
            return;
        }
        self.resident_surface
            .pending_admission_commits
            .remove(operation_id);
        if let Err(error) = self.repair_surface_admission_failure(&pending.fence, pending.message) {
            self.surface_terminal_blocked = Some(format!(
                "typed surface admission repair failed for {:?}: {error:?}",
                pending.fence.operation_id
            ));
        }
    }

    fn retry_surface_admission_terminal(&mut self, operation_id: &surface::SurfaceOperationId) {
        let Some(pending) = self
            .resident_surface
            .pending_admission_terminals
            .get(operation_id)
            .cloned()
        else {
            return;
        };
        if self
            .resident_surface
            .coordinator
            .commit_finalizer_batch(
                operation_id.clone(),
                match &pending.pending.failure {
                    surface::WaitOperationTerminalResult::TerminalCommitFailure {
                        finalize_intent_id,
                        ..
                    } => finalize_intent_id.clone(),
                    _ => return,
                },
                &pending.pending.batch,
            )
            .is_err()
        {
            if let Some(retained) = self
                .resident_surface
                .pending_admission_terminals
                .get_mut(operation_id)
            {
                retained.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            }
            return;
        }
        self.resident_surface
            .pending_admission_terminals
            .remove(operation_id);
        self.cache_surface_terminal(pending.pending.value);
        if self.resident_surface.pending_terminal_commits.is_empty() {
            self.surface_terminal_blocked = None;
        }
    }

    fn retry_pending_surface_transition(&mut self, mut active: Option<&mut ActiveOperation>) {
        let Some((_, retry)) =
            self.resident_surface
                .pending_admission_repairs
                .values()
                .map(|pending| {
                    (
                        pending.retry_at,
                        PendingSurfaceTransitionRetry::AdmissionRepair(
                            pending.fence.operation_id.clone(),
                        ),
                    )
                })
                .chain(self.resident_surface.pending_admission_commits.iter().map(
                    |(operation_id, pending)| {
                        (
                            pending.retry_at,
                            PendingSurfaceTransitionRetry::AdmissionCommit(operation_id.clone()),
                        )
                    },
                ))
                .chain(
                    self.resident_surface
                        .pending_terminalization
                        .iter()
                        .map(|pending| {
                            (
                                pending.retry_at,
                                PendingSurfaceTransitionRetry::PreparedTerminalization(
                                    pending.fence.operation_id.clone(),
                                ),
                            )
                        }),
                )
                .chain(
                    self.resident_surface
                        .pending_admission_terminals
                        .iter()
                        .map(|(operation_id, pending)| {
                            (
                                pending.retry_at,
                                PendingSurfaceTransitionRetry::AdmissionTerminal(
                                    operation_id.clone(),
                                ),
                            )
                        }),
                )
                .chain(self.resident_surface.interactions.iter().filter_map(
                    |(interaction_id, interaction)| {
                        interaction
                            .private_response
                            .as_ref()
                            .and_then(|private| private.retry_at)
                            .map(|retry_at| {
                                (
                                    retry_at,
                                    PendingSurfaceTransitionRetry::PrivateResponse(
                                        interaction_id.clone(),
                                    ),
                                )
                            })
                    },
                ))
                .chain(self.resident_surface.pending_detaches.iter().map(
                    |(attachment_id, pending)| {
                        (
                            pending.retry_at,
                            PendingSurfaceTransitionRetry::Detach(attachment_id.clone()),
                        )
                    },
                ))
                .chain(self.resident_surface.pending_capability_losses.iter().map(
                    |(attachment_id, pending)| {
                        (
                            pending.retry_at,
                            PendingSurfaceTransitionRetry::CapabilityLoss(attachment_id.clone()),
                        )
                    },
                ))
                .min()
        else {
            return;
        };
        if let PendingSurfaceTransitionRetry::AdmissionCommit(operation_id) = retry {
            self.retry_surface_admission_commit(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::AdmissionRepair(operation_id) = retry {
            self.retry_surface_admission_repair(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::AdmissionTerminal(operation_id) = retry {
            self.retry_surface_admission_terminal(&operation_id);
            return;
        }
        if let PendingSurfaceTransitionRetry::PreparedTerminalization(operation_id) = retry {
            let pending = self
                .resident_surface
                .pending_terminalization
                .clone()
                .expect("selected terminalization remains pending");
            debug_assert_eq!(pending.fence.operation_id, operation_id);
            if self
                .resident_surface
                .coordinator
                .commit_actor_generation_terminalization_batch(
                    pending.fence.clone(),
                    &pending.batch,
                )
                .is_err()
            {
                if let Some(retained) = self.resident_surface.pending_terminalization.as_mut() {
                    retained.retry_at =
                        tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                }
                return;
            }
            self.resident_surface.pending_terminalization = None;
            self.apply_surface_interaction_cancellations(&pending.interaction_ids);
            if let Some(active) = active.as_deref_mut()
                && active.surface_operation.as_ref() == Some(&pending.fence)
            {
                active.surface_terminalization = Some(pending.cause);
                active.generation.cancel.cancel();
            }
            return;
        }
        if let PendingSurfaceTransitionRetry::PrivateResponse(interaction_id) = retry {
            if self
                .retry_private_surface_interaction(&interaction_id)
                .is_err()
            {
                return;
            }
            self.reconcile_surface_interaction_capabilities(active);
            return;
        }
        if let PendingSurfaceTransitionRetry::Detach(attachment_id) = retry {
            let pending = self
                .resident_surface
                .pending_detaches
                .get(&attachment_id)
                .expect("selected detach remains pending")
                .clone();
            if self
                .resident_surface
                .coordinator
                .commit_generation_batch(
                    pending.transition.fence.clone(),
                    &pending.transition.batch,
                )
                .is_err()
            {
                if let Some(retained) = self
                    .resident_surface
                    .pending_detaches
                    .get_mut(&attachment_id)
                {
                    retained.retry_at =
                        tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
                }
                return;
            }
            self.resident_surface
                .pending_detaches
                .remove(&attachment_id);
            self.resident_surface
                .pending_capability_losses
                .remove(&attachment_id);
            self.apply_surface_attachment_transition(active.as_deref_mut(), &pending.transition);
            let _ = self
                .resident_surface
                .hub
                .finalize_detach_local(&pending.client, pending.receipt);
            self.reconcile_surface_interaction_capabilities(active);
            return;
        }
        let PendingSurfaceTransitionRetry::CapabilityLoss(attachment_id) = retry else {
            unreachable!("private response and detach retries returned above")
        };
        let transition = self
            .resident_surface
            .pending_capability_losses
            .get(&attachment_id)
            .expect("selected capability loss remains pending")
            .transition
            .clone();
        if self
            .resident_surface
            .coordinator
            .commit_generation_batch(transition.fence.clone(), &transition.batch)
            .is_err()
        {
            if let Some(pending) = self
                .resident_surface
                .pending_capability_losses
                .get_mut(&attachment_id)
            {
                pending.retry_at =
                    tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            }
            return;
        }
        self.resident_surface
            .pending_capability_losses
            .remove(&attachment_id);
        self.apply_surface_attachment_transition(active.as_deref_mut(), &transition);
        self.reconcile_surface_interaction_capabilities(active);
    }

    fn detach_surface_attachment(
        &mut self,
        mut active: Option<&mut ActiveOperation>,
        client: &surface::RuntimeSurfaceClientHandle,
        request: surface::DetachRequest,
    ) -> surface::DetachResult {
        let attachment_id = client.attachment_id().clone();
        if !self.resident_surface.pending_capability_losses.is_empty()
            && !self
                .resident_surface
                .pending_capability_losses
                .contains_key(&attachment_id)
        {
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        if !self.resident_surface.pending_detaches.is_empty()
            && !self
                .resident_surface
                .pending_detaches
                .get(&attachment_id)
                .is_some_and(|pending| pending.receipt.request_id == request.request_id)
        {
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        let detached = self
            .resident_surface
            .hub
            .prepare_detach_local(client, request.clone());
        let mut receipt = match detached {
            surface::DetachResult::Detached { receipt } => receipt,
            other => return other,
        };
        let pending = match self
            .resident_surface
            .pending_detaches
            .get(&attachment_id)
            .cloned()
        {
            Some(pending) if pending.receipt.request_id == request.request_id => pending,
            Some(_) => {
                return surface::DetachResult::StaleAttachment {
                    request_id: request.request_id,
                    attachment_id,
                };
            }
            None => {
                let retained_capability_loss = self
                    .resident_surface
                    .pending_capability_losses
                    .get(&attachment_id)
                    .map(|pending| pending.transition.clone());
                let transition = match retained_capability_loss.map_or_else(
                    || self.prepare_surface_attachment_transition(&attachment_id),
                    |transition| Ok(Some(transition)),
                ) {
                    Ok(Some(transition)) => transition,
                    Ok(None) => {
                        return self
                            .resident_surface
                            .hub
                            .finalize_detach_local(client, receipt);
                    }
                    Err(()) => {
                        return surface::DetachResult::StaleAttachment {
                            request_id: request.request_id,
                            attachment_id,
                        };
                    }
                };
                receipt.affected_route_epochs = transition.affected_route_epochs.clone();
                receipt.route_commit_id = Some(transition.commit_id.clone());
                receipt.route_cursor = Some(transition.batch.cursor_after.clone());
                PendingSurfaceDetach {
                    client: client.clone(),
                    transition,
                    receipt,
                    retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                }
            }
        };
        if self
            .resident_surface
            .coordinator
            .commit_generation_batch(pending.transition.fence.clone(), &pending.transition.batch)
            .is_err()
        {
            let retry_at = tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL;
            self.resident_surface
                .pending_capability_losses
                .remove(&attachment_id);
            let mut pending = pending;
            pending.retry_at = retry_at;
            self.resident_surface
                .pending_detaches
                .insert(attachment_id.clone(), pending);
            return surface::DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id,
            };
        }
        self.resident_surface
            .pending_detaches
            .remove(&attachment_id);
        self.resident_surface
            .pending_capability_losses
            .remove(&attachment_id);
        self.apply_surface_attachment_transition(active.as_deref_mut(), &pending.transition);
        let result = self
            .resident_surface
            .hub
            .finalize_detach_local(client, pending.receipt);
        self.reconcile_surface_interaction_capabilities(active);
        result
    }

    fn reserve_surface_operation(
        &mut self,
        request_id: surface::SurfaceRequestId,
        intent: surface::OperationRequestIntent,
        origin_attachment: surface::SurfaceAttachmentId,
        origin_connection: Option<surface::SurfaceConnectionId>,
    ) -> Result<
        surface::MutationReply<surface::ReservedOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.resident_surface.pending_terminal_commits.is_empty()
            || !self.resident_surface.pending_admission_commits.is_empty()
            || !self.resident_surface.pending_admission_repairs.is_empty()
            || !self.resident_surface.pending_admission_terminals.is_empty()
            || self.surface_terminal_blocked.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        if !matches!(
            intent.correlation,
            surface::OperationIngressCorrelation::TuiUser
                | surface::OperationIngressCorrelation::AcpPrompt { .. }
                | surface::OperationIngressCorrelation::JsonlThreadTurn { .. }
                | surface::OperationIngressCorrelation::JsonlStatelessSubmit { .. }
        ) || intent.kind != surface::OperationKind::UserTurn
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if let surface::OperationIngressCorrelation::JsonlThreadTurn { legacy_turn_id, .. } =
            &intent.correlation
        {
            if TurnId::parse(legacy_turn_id.0.as_str()).is_err() {
                return Err(surface::SurfaceClientCommandError::Unauthorized);
            }
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
        let settings = &snapshot.settings;
        let (expected_settings_revision, expected_policy_epoch) = match &intent.settings_preparation
        {
            surface::OperationSettingsPreparation::UseCurrent {
                expected_settings_revision,
                expected_policy_epoch,
            }
            | surface::OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
                expected_settings_revision,
                expected_policy_epoch,
                ..
            } => (*expected_settings_revision, *expected_policy_epoch),
        };
        let stale_settings_message = if expected_settings_revision != settings.thread_revision {
            Some("thread settings revision is stale")
        } else if expected_policy_epoch != settings.effective.policy_epoch {
            Some("thread settings policy epoch is stale")
        } else {
            None
        };
        if let Some(message) = stale_settings_message {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::RuntimeSettings {
                        host_incarnation: self
                            .resident_surface
                            .hub
                            .authority()
                            .host_incarnation()
                            .clone(),
                        thread_id: Some(snapshot.thread.thread_id.clone()),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new(message),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Settings {
                            host_incarnation: self
                                .resident_surface
                                .hub
                                .authority()
                                .host_incarnation()
                                .clone(),
                            thread_id: Some(snapshot.thread.thread_id.clone()),
                            revision: settings.thread_revision,
                        }),
                    }),
                },
            });
        }
        if matches!(
            &intent.settings_preparation,
            surface::OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested { .. }
        ) {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid {
                    request_id,
                    target: Some(surface::MutationTarget::RuntimeSettings {
                        host_incarnation: self
                            .resident_surface
                            .hub
                            .authority()
                            .host_incarnation()
                            .clone(),
                        thread_id: Some(snapshot.thread.thread_id.clone()),
                    }),
                    error: surface::InvalidMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::IllegalState,
                        message: surface::DisplayText::new(
                            "thread settings overrides are not supported by typed admission",
                        ),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Settings {
                            host_incarnation: self
                                .resident_surface
                                .hub
                                .authority()
                                .host_incarnation()
                                .clone(),
                            thread_id: Some(snapshot.thread.thread_id.clone()),
                            revision: settings.thread_revision,
                        }),
                    }),
                },
            });
        }
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
        let jsonl_connection_id = origin_connection.or_else(|| {
            surface::SurfaceConnectionId::try_from_bytes(*origin_attachment.as_bytes()).ok()
        });
        let origin = match &intent.correlation {
            surface::OperationIngressCorrelation::TuiUser => surface::OperationOrigin::TuiUser,
            surface::OperationIngressCorrelation::AcpPrompt {
                session_id,
                inbound_seq,
                rpc_request_id,
            } => surface::OperationOrigin::AcpPrompt {
                connection_id: surface::SurfaceConnectionId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated ACP connection id is valid"),
                session_id: session_id.clone(),
                inbound_seq: *inbound_seq,
                rpc_request_id: rpc_request_id.clone(),
            },
            surface::OperationIngressCorrelation::JsonlThreadTurn {
                rpc_id_digest,
                legacy_turn_id,
            } => surface::OperationOrigin::JsonlThreadTurn {
                connection_id: jsonl_connection_id
                    .clone()
                    .expect("JSONL connection identity is bound"),
                rpc_id_digest: rpc_id_digest.clone(),
                legacy_turn_id: legacy_turn_id.clone(),
            },
            surface::OperationIngressCorrelation::JsonlStatelessSubmit { rpc_id_digest } => {
                surface::OperationOrigin::JsonlStatelessSubmit {
                    connection_id: jsonl_connection_id.expect("JSONL connection identity is bound"),
                    rpc_id_digest: rpc_id_digest.clone(),
                }
            }
            _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
        };
        let replayability = surface::Replayability::Replayable {
            capsule_digest: surface_sha256(
                &serde_json::to_vec(&input_request).expect("surface input is serializable"),
            ),
            request: Some(input_request.clone()),
            request_digest: Some(surface_sha256(
                &serde_json::to_vec(&input_request).expect("surface input is serializable"),
            )),
            cwd: settings.effective.cwd.clone(),
            workspace_roots: settings.effective.workspace_roots.clone(),
            settings_revision: settings.thread_revision,
            policy_epoch: settings.effective.policy_epoch,
            tool_schema_digest: surface_sha256(
                &serde_json::to_vec(&snapshot.tools).expect("surface tools are serializable"),
            ),
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
                capability_fingerprint: surface_sha256(
                    &serde_json::to_vec(&snapshot.tools).expect("surface tools are serializable"),
                ),
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
        self.resident_surface
            .operation_origin_attachments
            .insert(operation_id.clone(), origin_attachment);
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

    fn update_surface_settings(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        expected_thread_revision: surface::SettingsRevision,
        patches: surface::NonEmptyVec<surface::RuntimeSettingsPatch>,
    ) -> Result<
        surface::MutationReply<surface::SettingsMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.admits_surface_client(client, surface::SurfaceCapability::ManageThreadSettings) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.active.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let current = &snapshot.settings;
        if expected_thread_revision != current.thread_revision {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::RuntimeSettings {
                        host_incarnation: self
                            .resident_surface
                            .hub
                            .authority()
                            .host_incarnation()
                            .clone(),
                        thread_id: Some(snapshot.thread.thread_id.clone()),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("thread settings revision is stale"),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::Settings {
                            host_incarnation: self
                                .resident_surface
                                .hub
                                .authority()
                                .host_incarnation()
                                .clone(),
                            thread_id: Some(snapshot.thread.thread_id.clone()),
                            revision: current.thread_revision,
                        }),
                    }),
                },
            });
        }
        let mut next_settings = current.clone();
        let mut next_config = self.config.clone();
        for patch in patches.as_slice() {
            apply_runtime_settings_patch(&mut next_config, &mut next_settings.effective, patch)?;
        }
        let next_revision = surface::SettingsRevision::try_new(
            current
                .thread_revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        next_settings.thread_revision = next_revision;
        next_settings.pending = None;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::Settings(surface::SettingsPatch::Committed {
                    previous_revision: current.thread_revision,
                    snapshot: next_settings.clone(),
                }),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)?;
        if let Some(state) = self.state.as_mut() {
            if patches
                .as_slice()
                .iter()
                .any(|patch| matches!(patch, surface::RuntimeSettingsPatch::SetModel { .. }))
            {
                state
                    .thread
                    .session_mut()
                    .set_model(next_config.model.as_history_value().as_deref());
            }
        }
        self.config = next_config;
        Ok(self.committed_settings_mutation(
            request_id,
            &batch,
            surface::SettingsMutationOutput {
                settings: next_settings,
                cursor: batch.cursor_after.clone(),
            },
        ))
    }

    fn pinned_context_mutation(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        action: surface::PinnedContextAction,
    ) -> Result<
        surface::MutationReply<surface::PinnedContextMutationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.admits_surface_client(client, surface::SurfaceCapability::ManagePinnedContext) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if self.active.is_some() {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let surface::PinnedContextAction::Add {
            expected_revision,
            entry,
            memory_receipt: _memory_receipt,
        } = action
        else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        let current = &snapshot.pinned_context;
        if expected_revision != current.revision {
            return Ok(surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale {
                    request_id,
                    target: Some(surface::MutationTarget::Thread {
                        thread_id: snapshot.thread.thread_id.clone(),
                    }),
                    error: surface::StaleMutationError::new(surface::SurfaceMutationError {
                        code: surface::SurfaceMutationErrorCode::StaleRevision,
                        message: surface::DisplayText::new("pinned context revision is stale"),
                        winning_request_id: None,
                        current_revision: Some(surface::SurfaceMutationRevision::PinnedContext {
                            thread_id: snapshot.thread.thread_id.clone(),
                            revision: current.revision,
                        }),
                    }),
                },
            });
        }
        if current.entries.iter().any(|current| current.id == entry.id) {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let next_revision = surface::PinnedContextRevision::try_new(
            current
                .revision
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        )
        .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let batch = self.surface_event_batch_with_commit_id(
            vec![(
                surface::SurfaceScope::Thread,
                surface::SurfaceEvent::PinnedContext(surface::PinnedContextPatch::Added {
                    previous_revision: current.revision,
                    next_revision,
                    entry: entry.clone(),
                }),
            )],
            None,
        );
        self.commit_surface_actor_batch_with_retry(&batch)?;
        if let Some(state) = self.state.as_mut() {
            state
                .thread
                .session_mut()
                .add_pinned_context(entry.content.as_str().to_string());
        }
        let next_snapshot = self
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .pinned_context
            .clone();
        Ok(self.committed_pinned_context_mutation(
            request_id,
            &batch,
            surface::PinnedContextMutationOutput {
                snapshot: next_snapshot,
                cursor: batch.cursor_after.clone(),
            },
        ))
    }

    fn admit_surface_operation(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
    ) -> Result<surface::MutationReply<surface::AdmissionOutput>, surface::SurfaceClientCommandError>
    {
        self.admit_surface_operation_with_output(
            client,
            request_id,
            operation_id,
            admission_lease_id,
            None,
        )
    }

    fn admit_surface_operation_with_output(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        admission_lease_id: surface::SurfaceAdmissionLeaseId,
        output_writer: Option<Box<dyn HostedOperationWriter>>,
    ) -> Result<surface::MutationReply<surface::AdmissionOutput>, surface::SurfaceClientCommandError>
    {
        if self
            .resident_surface
            .operation_origin_attachments
            .get(&operation_id)
            != Some(client.attachment_id())
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        if !self.resident_surface.pending_terminal_commits.is_empty()
            || !self.resident_surface.pending_admission_commits.is_empty()
            || !self.resident_surface.pending_admission_repairs.is_empty()
            || !self.resident_surface.pending_admission_terminals.is_empty()
            || self.surface_terminal_blocked.is_some()
        {
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
        if !matches!(
            operation.intent.origin,
            surface::OperationOrigin::TuiUser
                | surface::OperationOrigin::AcpPrompt { .. }
                | surface::OperationOrigin::JsonlThreadTurn { .. }
                | surface::OperationOrigin::JsonlStatelessSubmit { .. }
        ) || operation.intent.kind != surface::OperationKind::UserTurn
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
        let logical_turn_id = match &operation.intent.origin {
            surface::OperationOrigin::JsonlThreadTurn { legacy_turn_id, .. } => {
                TurnId::parse(legacy_turn_id.0.as_str())
                    .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?
            }
            _ => TurnId::new(),
        };
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
        match self
            .resident_surface
            .coordinator
            .commit_actor_batch(&admitted_batch)
        {
            Ok(_) => {}
            Err(surface::SurfaceCommitError::Ledger(error)) => {
                eprintln!("orca: typed surface admission commit failed: {error:?}");
                self.resident_surface.pending_admission_commits.insert(
                    operation_id.clone(),
                    PendingSurfaceAdmissionCommit {
                        fence: fence.clone(),
                        batch: admitted_batch,
                        message: "typed surface admission commit failed",
                        retry_at: tokio::time::Instant::now()
                            + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                    },
                );
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
            Err(error) => {
                eprintln!("orca: typed surface admission commit failed: {error:?}");
                if let Err(repair_error) = self.repair_surface_admission_failure(
                    &fence,
                    "typed surface admission commit failed",
                ) {
                    self.surface_terminal_blocked = Some(format!(
                        "typed surface admission repair failed for {:?}: {repair_error:?}",
                        fence.operation_id
                    ));
                }
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        }

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
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &started_batch)
        {
            eprintln!("orca: typed surface start commit failed: {error:?}");
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface generation start commit failed",
            ) {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

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
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &resolved_batch)
        {
            eprintln!("orca: typed surface input resolution commit failed: {error:?}");
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface input resolution commit failed",
            ) {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

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
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &loop_started_batch)
        {
            eprintln!("orca: typed surface agent-loop start commit failed: {error:?}");
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface agent-loop start commit failed",
            ) {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let interaction_command_tx = self.handle.command_tx.clone();
        let interaction_fence = fence.clone();
        let mut hosted_request = HostedTurnRequest::new(resolved_input.canonical_text.as_str())
            .with_generation_handlers(move |_, _| {
                HostedGenerationHandlers::default()
                    .with_provider_response_ingress(Arc::new(
                        RuntimeSurfaceProviderResponseIngress {
                            command_tx: interaction_command_tx.clone(),
                            fence: interaction_fence.clone(),
                        },
                    ))
                    .with_approval_handler(Arc::new(RuntimeSurfaceApprovalHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_permission_handler(Arc::new(RuntimeSurfacePermissionHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_user_input_handler(Arc::new(RuntimeSurfaceUserInputHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_mcp_elicitation_handler(Arc::new(RuntimeSurfaceMcpElicitationHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
            });
        hosted_request.turn_id = logical_turn_id;
        hosted_request.task_id = Some(legacy_task_id);
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(hosted_request),
            writer: output_writer
                .unwrap_or_else(|| Box::new(PassthroughHostedOperationWriter::new(io::sink()))),
            config: None,
            reply: start_tx,
        });
        let start_result = match start_rx.recv() {
            Ok(result) => result,
            Err(_) => {
                if let Err(repair_error) = self.repair_surface_admission_failure(
                    &fence,
                    "typed surface runtime start reply was dropped",
                ) {
                    self.surface_terminal_blocked = Some(format!(
                        "typed surface admission repair failed for {:?}: {repair_error:?}",
                        fence.operation_id
                    ));
                }
                return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
            }
        };
        if let Err(error) = start_result {
            eprintln!("orca: typed surface runtime start failed: {error}");
            if let Err(repair_error) =
                self.repair_surface_admission_failure(&fence, "typed surface runtime start failed")
            {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let Some(active) = self.active.as_mut() else {
            if let Err(repair_error) = self.repair_surface_admission_failure(
                &fence,
                "typed surface runtime active generation was missing",
            ) {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface admission repair failed for {:?}: {repair_error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
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

    fn repair_surface_admission_failure(
        &mut self,
        fence: &surface::SurfaceOperationFence,
        message: &'static str,
    ) -> Result<surface::OperationTerminalAtCursor, surface::SurfaceClientCommandError> {
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == fence.operation_id)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let original_request_id = operation.request_id.clone();
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let diagnostic = surface::SafeDiagnosticText::try_new(message)
            .expect("admission failure diagnostic is bounded");
        let stop_reason = match operation
            .generations
            .last()
            .map(|generation| generation.phase)
        {
            Some(surface::GenerationPhase::Reserved) => surface::GenerationStopReason::NotStarted {
                reason: surface::NotStartedReason::StartCommitFailure {
                    message: diagnostic.clone(),
                },
            },
            _ => surface::GenerationStopReason::ExecutionFailed {
                class: surface::GenerationExecutionFailureClass::RuntimeInvariant,
                message: diagnostic.clone(),
            },
        };
        let stream_discard_reason = surface::AssistantDiscardReason::ProviderFailed;
        let generation_scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut events = snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == *fence && stream.state == surface::SurfaceAssistantStreamState::Open
            })
            .map(|stream| {
                (
                    generation_scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: stream_discard_reason,
                    }),
                )
            })
            .collect::<Vec<_>>();
        events.push((
            generation_scope,
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
        ));
        events.push((
            surface::SurfaceScope::Operation {
                operation_id: fence.operation_id.clone(),
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::FinalizationStarted {
                operation_id: fence.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::GenerationStop(
                    stop_reason.clone(),
                ),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        ));
        let stop_and_finalization_batch = self.surface_event_batch_with_commit_id(events, None);
        let terminal = match &stop_reason {
            surface::GenerationStopReason::NotStarted { .. } => {
                surface::OperationTerminal::Failed {
                    class: surface::FailureClass::Persistence,
                    message: diagnostic.clone(),
                }
            }
            _ => surface::OperationTerminal::Failed {
                class: surface::FailureClass::RuntimeInvariant,
                message: diagnostic.clone(),
            },
        };
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_live_generation_stop_disposition_batch(
                fence.clone(),
                fence.operation_id.clone(),
                finalize_intent_id.clone(),
                &stop_and_finalization_batch,
            )
        {
            eprintln!("orca: typed surface admission repair failed: {error:?}");
            self.resident_surface.pending_admission_repairs.insert(
                fence.operation_id.clone(),
                PendingSurfaceAdmissionRepair {
                    fence: fence.clone(),
                    batch: stop_and_finalization_batch,
                    original_request_id,
                    finalize_intent_id,
                    terminal_commit_id,
                    terminal,
                    retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                },
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let usage = surface::UsageTotals {
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            estimated_cost_usd_micros: 0,
        };
        let terminal_batch = self.surface_operation_batch_with_commit_id(
            &fence.operation_id,
            vec![surface::OperationPatch::Terminal {
                record: surface::OperationTerminalRecord {
                    operation_id: fence.operation_id.clone(),
                    finalize_intent_id: finalize_intent_id.clone(),
                    terminal: terminal.clone(),
                    usage: usage.clone(),
                    source_diagnostic_digest: None,
                    settlement_receipts: Vec::new(),
                    committed_at: surface::UnixMillis::new(0),
                },
            }],
            Some(terminal_commit_id.clone()),
        );
        let value = surface::OperationTerminalAtCursor {
            operation_id: fence.operation_id.clone(),
            terminal,
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        if let Err(error) = self.resident_surface.coordinator.commit_finalizer_batch(
            fence.operation_id.clone(),
            finalize_intent_id.clone(),
            &terminal_batch,
        ) {
            eprintln!("orca: typed surface admission repair terminal failed: {error:?}");
            let repair = surface::RetryFinalizationToken::new(
                original_request_id,
                snapshot.thread.thread_id.clone(),
                fence.operation_id.clone(),
                finalize_intent_id.clone(),
                terminal_commit_id.clone(),
                snapshot.thread.owner_epoch,
                terminal_batch.batch_digest.clone(),
            );
            self.resident_surface.pending_admission_terminals.insert(
                fence.operation_id.clone(),
                PendingSurfaceAdmissionTerminal {
                    pending: PendingSurfaceTerminalCommit {
                        batch: terminal_batch,
                        value,
                        failure: surface::WaitOperationTerminalResult::TerminalCommitFailure {
                            operation_id: fence.operation_id.clone(),
                            finalize_intent_id,
                            commit_id: terminal_commit_id,
                            repair,
                        },
                        legacy_completion: None,
                        legacy_terminal: None,
                    },
                    retry_at: tokio::time::Instant::now() + SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL,
                },
            );
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        self.cache_surface_terminal(value.clone());
        Ok(value)
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
        let (stop_reason, terminal) = if let Some(class) = active.surface_execution_failure {
            let message = surface::SafeDiagnosticText::try_new(
                "required client capability became unavailable",
            )
            .expect("fixed diagnostic is bounded");
            (
                surface::GenerationStopReason::ExecutionFailed {
                    class,
                    message: message.clone(),
                },
                surface::OperationTerminal::Failed {
                    class: surface::FailureClass::ClientCapabilityUnavailable,
                    message,
                },
            )
        } else {
            match outcome {
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
                        message: surface::SafeDiagnosticText::try_new(message.clone())
                            .unwrap_or_else(|_| {
                                surface::SafeDiagnosticText::try_new("generation panicked").unwrap()
                            }),
                    },
                    surface::OperationTerminal::Panicked {
                        message: surface::SafeDiagnosticText::try_new(message.clone())
                            .unwrap_or_else(|_| {
                                surface::SafeDiagnosticText::try_new("generation panicked").unwrap()
                            }),
                    },
                ),
                _ => {
                    let message =
                        surface::SafeDiagnosticText::try_new("foreground operation failed")
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
        let stream_discard_reason = match &stop_reason {
            surface::GenerationStopReason::Cancelled { .. } => {
                surface::AssistantDiscardReason::GenerationCancelled
            }
            surface::GenerationStopReason::InterruptedResumable => {
                surface::AssistantDiscardReason::GenerationInterrupted
            }
            surface::GenerationStopReason::RuntimeRestart
            | surface::GenerationStopReason::NotStarted {
                reason: surface::NotStartedReason::RuntimeRestart,
            } => surface::AssistantDiscardReason::RuntimeRestart,
            surface::GenerationStopReason::ProjectionFailure { .. } => {
                surface::AssistantDiscardReason::ProjectionRepair
            }
            _ => surface::AssistantDiscardReason::ProviderFailed,
        };
        let generation_scope = surface::SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut stop_and_finalization_events = snapshot
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == fence && stream.state == surface::SurfaceAssistantStreamState::Open
            })
            .map(|stream| {
                (
                    generation_scope.clone(),
                    surface::SurfaceEvent::Assistant(surface::AssistantPatch::StreamDiscarded {
                        stream_id: stream.stream_id.clone(),
                        reason: stream_discard_reason,
                    }),
                )
            })
            .collect::<Vec<_>>();
        stop_and_finalization_events.push((
            generation_scope,
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
        ));
        stop_and_finalization_events.push((
            surface::SurfaceScope::Operation {
                operation_id: operation_id.clone(),
            },
            surface::SurfaceEvent::Operation(surface::OperationPatch::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::GenerationStop(stop_reason),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        ));
        let stop_and_finalization_batch =
            self.surface_event_batch_with_commit_id(stop_and_finalization_events, None);
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

    fn bind_surface_operation_controller(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        operation_id: &surface::SurfaceOperationId,
    ) -> bool {
        match self
            .resident_surface
            .operation_origin_attachments
            .get(operation_id)
        {
            Some(bound) => bound == client.attachment_id(),
            None => {
                let snapshot = self.resident_surface.coordinator.state().snapshot();
                let visible = snapshot
                    .foreground_operation
                    .iter()
                    .chain(snapshot.queued_operations.iter())
                    .chain(snapshot.operation_history.iter())
                    .any(|operation| &operation.operation_id == operation_id);
                if visible {
                    self.resident_surface
                        .operation_origin_attachments
                        .insert(operation_id.clone(), client.attachment_id().clone());
                }
                visible
            }
        }
    }

    fn resume_surface_operation(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
        expected_last_generation: surface::SurfaceGenerationId,
        resume_source: surface::ResumeSourceWitness,
    ) -> Result<
        surface::MutationReply<surface::ResumeOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.bind_surface_operation_controller(client, &operation_id)
            || !self.resident_surface.pending_terminal_commits.is_empty()
            || !self.resident_surface.pending_admission_commits.is_empty()
            || !self.resident_surface.pending_admission_repairs.is_empty()
            || !self.resident_surface.pending_admission_terminals.is_empty()
            || self.surface_terminal_blocked.is_some()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot().clone();
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if !matches!(
            operation.phase,
            surface::OperationPhase::Suspended {
                cause: surface::SuspensionCause::Interrupted { .. }
                    | surface::SuspensionCause::RecoveryRequired { .. }
                    | surface::SuspensionCause::ProviderSuspended { .. }
            }
        ) || operation.pending_control.is_some()
            || operation.finalization.is_some()
            || operation.terminal.is_some()
            || operation.intent.kind != surface::OperationKind::UserTurn
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let previous = operation
            .generations
            .last()
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if previous.fence.generation_id != expected_last_generation
            || previous.phase != surface::GenerationPhase::Stopped
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let (input_request, request_digest) =
            match (&operation.intent.initial_replayability, &resume_source) {
                (
                    surface::Replayability::Replayable {
                        request: Some(input),
                        request_digest: Some(request_digest),
                        ..
                    },
                    surface::ResumeSourceWitness::DurableReplay {
                        replayability_digest,
                    },
                ) if replayability_digest
                    == &surface::canonical_replayability_digest(
                        &operation.intent.initial_replayability,
                    ) =>
                {
                    (input.clone(), request_digest.clone())
                }
                (
                    surface::Replayability::NonReplayable {
                        live_capsule: surface::LiveOperationCapsule::Available { incarnation },
                        ..
                    },
                    surface::ResumeSourceWitness::LiveCapsule {
                        incarnation: witness,
                    },
                ) if incarnation == witness && witness == &snapshot.cursor.incarnation => {
                    return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
                }
                _ => return Err(surface::SurfaceClientCommandError::Unauthorized),
            };
        let resolved_input = resolve_surface_input(&input_request)
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let generation_id = surface::SurfaceGenerationId::new(
            previous
                .fence
                .generation_id
                .get()
                .checked_add(1)
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?,
        );
        let fence = surface::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id,
        };
        let resume_turn_id = TurnId::new();
        let generation = surface::GenerationRecord {
            fence: fence.clone(),
            logical_turn_id: resume_turn_id.clone(),
            input: previous.input.clone(),
            predecessor: Some(previous.fence.clone()),
            attempt: surface::GenerationAttempt::RecoveryReplacement,
            goal_identity: None,
            replayability: operation.intent.initial_replayability.clone(),
            required_capabilities: operation.intent.required_capabilities.clone(),
            capability_fingerprint: operation.intent.capability_fingerprint.clone(),
            phase: surface::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let resume_batch = self.surface_operation_batch(
            &operation_id,
            vec![
                surface::OperationPatch::GenerationReserved {
                    generation: generation.clone(),
                },
                surface::OperationPatch::ControlIntentCommitted {
                    operation_id: operation_id.clone(),
                    request_id: operation.request_id.clone(),
                    intent: surface::PendingControlIntent::ResumeStarting {
                        generation_fence: fence.clone(),
                    },
                },
            ],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&resume_batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;

        let started_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let started_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::GenerationStarted {
                fence: fence.clone(),
                witness: surface::GenerationStartedWitness {
                    started_commit_id: started_commit_id.clone(),
                    settings_revision: operation.intent.settings_revision,
                    policy_epoch: operation.intent.policy_epoch,
                    durable_replayability_digest: surface::canonical_replayability_digest(
                        &operation.intent.initial_replayability,
                    ),
                    capability_fingerprint: operation.intent.capability_fingerprint.clone(),
                },
            }],
            Some(started_commit_id),
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &started_batch)
        {
            eprintln!("orca: typed surface resume Started commit failed: {error:?}");
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        let legacy_task_id = format!("typed-resume-{}", uuid::Uuid::now_v7());
        let loop_started_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::AgentLoopTurnStarted {
                turn: surface::SurfaceAgentLoopTurn {
                    turn_id: resume_turn_id.clone(),
                    fence: fence.clone(),
                    ordinal: 0,
                    task_id: surface::SurfaceTaskId::try_new(legacy_task_id.clone())
                        .expect("generated task id is non-empty"),
                    task_status: surface::SurfaceTaskRunningStatus::Running,
                },
            }],
        );
        if let Err(error) = self
            .resident_surface
            .coordinator
            .commit_generation_batch(fence.clone(), &loop_started_batch)
        {
            eprintln!("orca: typed surface resume loop commit failed: {error:?}");
            if let Err(error) =
                self.repair_surface_admission_failure(&fence, "typed surface resume loop failed")
            {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface resume repair failed for {:?}: {error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }

        debug_assert_eq!(
            request_digest,
            match &operation.intent.initial_replayability {
                surface::Replayability::Replayable {
                    request_digest: Some(request_digest),
                    ..
                } => request_digest.clone(),
                _ => unreachable!("resume replayability was checked"),
            }
        );
        let interaction_command_tx = self.handle.command_tx.clone();
        let interaction_fence = fence.clone();
        let mut hosted_request = HostedTurnRequest::new(resolved_input.canonical_text.as_str())
            .with_generation_handlers(move |_, _| {
                HostedGenerationHandlers::default()
                    .with_provider_response_ingress(Arc::new(
                        RuntimeSurfaceProviderResponseIngress {
                            command_tx: interaction_command_tx.clone(),
                            fence: interaction_fence.clone(),
                        },
                    ))
                    .with_approval_handler(Arc::new(RuntimeSurfaceApprovalHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_permission_handler(Arc::new(RuntimeSurfacePermissionHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_user_input_handler(Arc::new(RuntimeSurfaceUserInputHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
                    .with_mcp_elicitation_handler(Arc::new(RuntimeSurfaceMcpElicitationHandler {
                        command_tx: interaction_command_tx.clone(),
                        fence: interaction_fence.clone(),
                    }))
            });
        hosted_request.turn_id = resume_turn_id;
        hosted_request.task_id = Some(legacy_task_id);
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        self.handle_idle_command(ThreadCommand::StartTurn {
            request: Box::new(hosted_request),
            writer: Box::new(PassthroughHostedOperationWriter::new(io::sink())),
            config: None,
            reply: start_tx,
        });
        let start_result = start_rx
            .recv()
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if start_result.is_err() {
            if let Err(error) =
                self.repair_surface_admission_failure(&fence, "typed surface resume start failed")
            {
                self.surface_terminal_blocked = Some(format!(
                    "typed surface resume repair failed for {:?}: {error:?}",
                    fence.operation_id
                ));
            }
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        let active = self
            .active
            .as_mut()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        active.surface_operation = Some(fence.clone());
        Ok(Self::committed_surface_resume_mutation(
            request_id,
            operation_id,
            fence,
            &resume_batch,
            &started_batch,
        ))
    }

    fn cancel_surface_before_admission(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self
            .resident_surface
            .operation_origin_attachments
            .get(&operation_id)
            != Some(client.attachment_id())
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
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

    fn cancel_surface_idle(
        &mut self,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if !self.bind_surface_operation_controller(client, &operation_id) {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let snapshot = self.resident_surface.coordinator.state().snapshot();
        if snapshot
            .queued_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id)
        {
            return self.cancel_surface_before_admission(client, request_id, operation_id);
        }
        if let Some(terminal) = self.resident_surface.terminals.get(&operation_id).cloned() {
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Operation {
                        thread_id: terminal.cursor.thread_id.clone(),
                        operation_id: operation_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::OperationTerminalAck {
                            thread_id: terminal.cursor.thread_id.clone(),
                            thread_owner_epoch: snapshot.thread.owner_epoch,
                            operation_id: operation_id.clone(),
                            value: terminal.clone(),
                        },
                    ])
                    .expect("terminal replay has one acknowledgement"),
                },
                value: surface::CancelOperationOutput::AlreadyTerminal { terminal },
            });
        }
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        if let Some(finalization) = operation.finalization.as_ref() {
            return Ok(surface::MutationReply::Committed {
                mutation: surface::CommittedMutation {
                    request_id,
                    target: surface::MutationTarget::Operation {
                        thread_id: snapshot.thread.thread_id.clone(),
                        operation_id: operation_id.clone(),
                    },
                    disposition: surface::MutationDisposition::AlreadyApplied,
                    acknowledgements: surface::NonEmptyVec::try_new(vec![
                        surface::MutationCommitAck::ThreadLocalCursor {
                            cursor: finalization.started_at.cursor.clone(),
                            family: surface::SurfaceFactFamily::Operation,
                            event_id: finalization.started_at.event_id.clone(),
                            commit_class: finalization.started_at.commit_class.clone(),
                        },
                    ])
                    .expect("finalization replay has one acknowledgement"),
                },
                value: surface::CancelOperationOutput::FinalizationPending {
                    operation_id,
                    finalize_intent_id: finalization.finalize_intent_id.clone(),
                    finalization_cursor: finalization.started_at.clone(),
                    waiter: surface::OperationWaiterHandle::new(),
                },
            });
        }
        let surface::OperationPhase::Suspended { .. } = operation.phase else {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        };
        let finalize_intent_id =
            surface::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let terminal_commit_id =
            surface::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7");
        let suspended_cause = surface::SuspendedFinalizationCause::Terminalization(
            surface::TerminalizationCause::UserCancel,
        );
        let control_batch = self.surface_operation_batch(
            &operation_id,
            vec![surface::OperationPatch::ControlIntentCommitted {
                operation_id: operation_id.clone(),
                request_id: operation.request_id.clone(),
                intent: surface::PendingControlIntent::Terminalize {
                    operation_id: operation_id.clone(),
                    cause: surface::TerminalizationCause::UserCancel,
                },
            }],
        );
        self.resident_surface
            .coordinator
            .commit_actor_batch(&control_batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let finalization_batch = self.surface_operation_batch_with_commit_id(
            &operation_id,
            vec![surface::OperationPatch::FinalizationStarted {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: terminal_commit_id.clone(),
                selected_cause: surface::OperationFinalizationCause::Suspended(
                    suspended_cause.clone(),
                ),
                suspended_cause: Some(suspended_cause),
                expected_settlements: Vec::new(),
            }],
            None,
        );
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(
                operation_id.clone(),
                finalize_intent_id.clone(),
                &finalization_batch,
            )
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let terminal = surface::OperationTerminal::Cancelled {
            reason: surface::CancelReason::User,
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
            Some(terminal_commit_id),
        );
        self.resident_surface
            .coordinator
            .commit_finalizer_batch(operation_id.clone(), finalize_intent_id, &terminal_batch)
            .map_err(|_| surface::SurfaceClientCommandError::RuntimeUnavailable)?;
        let terminal_at_cursor = surface::OperationTerminalAtCursor {
            operation_id: operation_id.clone(),
            terminal,
            cursor: terminal_batch.cursor_after.clone(),
            commit_class: terminal_batch.commit_class.clone(),
            batch_digest: terminal_batch.batch_digest.clone(),
        };
        self.cache_surface_terminal(terminal_at_cursor);
        Ok(Self::committed_surface_mutation(
            request_id,
            operation_id.clone(),
            &control_batch,
            surface::CancelOperationOutput::Accepted {
                operation_id,
                accepted_cursor: control_batch.cursor_after.clone(),
                waiter: surface::OperationWaiterHandle::new(),
            },
        ))
    }

    fn cancel_surface_running(
        &mut self,
        active: &mut ActiveOperation,
        client: &surface::RuntimeSurfaceClientHandle,
        request_id: surface::SurfaceRequestId,
        operation_id: surface::SurfaceOperationId,
    ) -> Result<
        surface::MutationReply<surface::CancelOperationOutput>,
        surface::SurfaceClientCommandError,
    > {
        if self
            .resident_surface
            .operation_origin_attachments
            .get(&operation_id)
            != Some(client.attachment_id())
        {
            return Err(surface::SurfaceClientCommandError::Unauthorized);
        }
        let fence = active
            .surface_operation
            .as_ref()
            .filter(|fence| fence.operation_id == operation_id)
            .cloned()
            .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
        let batch = self.commit_surface_terminalization_batch(
            active,
            &fence,
            surface::TerminalizationCause::UserCancel,
        )?;
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
        let Some(fence) = active.surface_operation.clone() else {
            return Ok(());
        };
        self.commit_surface_terminalization_batch(active, &fence, cause)
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("failed to commit typed shutdown intent: {error:?}"),
            })?;
        Ok(())
    }

    fn commit_surface_terminalization_batch(
        &mut self,
        active: &mut ActiveOperation,
        fence: &surface::SurfaceOperationFence,
        cause: surface::TerminalizationCause,
    ) -> Result<surface::SurfaceCommitBatch, surface::SurfaceClientCommandError> {
        if active.surface_terminalization.is_some()
            || !self.resident_surface.pending_detaches.is_empty()
            || !self.resident_surface.pending_capability_losses.is_empty()
        {
            return Err(surface::SurfaceClientCommandError::RuntimeUnavailable);
        }
        active.surface_terminalization = Some(cause);
        let prepared = (|| {
            self.drain_private_surface_interactions(fence)?;
            let original_request_id = self
                .resident_surface
                .coordinator
                .state()
                .snapshot()
                .foreground_operation
                .as_ref()
                .map(|operation| operation.request_id.clone())
                .ok_or(surface::SurfaceClientCommandError::RuntimeUnavailable)?;
            self.prepare_surface_terminalization(fence, original_request_id, cause)
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                active.surface_terminalization = None;
                return Err(error);
            }
        };
        match self
            .resident_surface
            .coordinator
            .commit_actor_generation_terminalization_batch(fence.clone(), &prepared.batch)
        {
            Ok(_) => {
                self.apply_surface_interaction_cancellations(&prepared.interaction_ids);
                Ok(prepared.batch)
            }
            Err(
                surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::CheckpointFailed)
                | surface::SurfaceCommitError::Ledger(surface::SurfaceLedgerError::PartialAppend),
            ) => {
                self.resident_surface.pending_terminalization = Some(prepared);
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            }
            Err(_) => {
                active.surface_terminalization = None;
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
            }
        }
    }

    fn surface_interaction_admission_closed(active: &ActiveOperation) -> bool {
        active.surface_terminalization.is_some()
            || active.surface_execution_failure.is_some()
            || active.generation.cancel.is_cancelled()
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

    async fn run(
        mut self,
        mut command_rx: tokio_mpsc::Receiver<ThreadCommand>,
        mut capability_change_rx: tokio_mpsc::Receiver<()>,
    ) {
        loop {
            let Some(mut active) = self.active.take() else {
                let surface_retry_at = self.next_surface_transition_retry_at();
                tokio::select! {
                    biased;
                    _ = wait_for_surface_transition_retry(surface_retry_at) => {
                        self.retry_pending_surface_transition(None);
                    }
                    wake = capability_change_rx.recv(), if !capability_change_rx.is_closed() => {
                        if wake.is_some() {
                            self.reconcile_surface_interaction_capabilities(None);
                        }
                    }
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            if self.surface_terminal_blocked.is_some() {
                                std::future::pending::<()>().await;
                            }
                            self.shutdown_background_tasks().await;
                            break;
                        };
                        if let ThreadCommand::ShutdownThread { reply, reason } = command {
                            while self.has_pending_surface_transition_retry() {
                                self.retry_pending_surface_transition(None);
                                if self.has_pending_surface_transition_retry() {
                                    tokio::time::sleep(SURFACE_CAPABILITY_LOSS_RETRY_INTERVAL).await;
                                }
                            }
                            if let Some(message) = self.surface_terminal_blocked.as_ref() {
                                let error = RuntimeHostError::ThreadStartFailed {
                                    message: message.clone(),
                                };
                                if reason == surface::SurfaceShutdownReason::HostShutdown {
                                    self.shutdown_background_tasks().await;
                                    if let Some(reply) = reply {
                                        let _ = reply.send(ThreadShutdownAck::Failed(error));
                                    }
                                    break;
                                }
                                if let Some(reply) = reply {
                                    let _ = reply.send(ThreadShutdownAck::Failed(error));
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
                                if reason == surface::SurfaceShutdownReason::HostShutdown {
                                    self.shutdown_background_tasks().await;
                                    if let Some(reply) = reply {
                                        let _ = reply.send(ThreadShutdownAck::Failed(error));
                                    }
                                    break;
                                }
                                if let Some(reply) = reply {
                                    let _ = reply.send(ThreadShutdownAck::Failed(error));
                                }
                                continue;
                            }
                            self.shutdown_background_tasks().await;
                            if let Some(reply) = reply {
                                let _ = reply.send(ThreadShutdownAck::Complete);
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

            let surface_retry_at = self.next_surface_transition_retry_at();
            tokio::select! {
                biased;
                _ = wait_for_surface_transition_retry(surface_retry_at) => {
                    self.retry_pending_surface_transition(Some(&mut active));
                    self.active = Some(active);
                }
                wake = capability_change_rx.recv(), if !capability_change_rx.is_closed() => {
                    if wake.is_some() {
                        self.reconcile_surface_interaction_capabilities(Some(&mut active));
                    }
                    self.active = Some(active);
                }
                command = command_rx.recv() => {
                    match command {
                        Some(ThreadCommand::ShutdownThread { reply, reason }) => {
                            let terminalization = match reason {
                                surface::SurfaceShutdownReason::HostShutdown => {
                                    surface::TerminalizationCause::HostShutdown
                                }
                                surface::SurfaceShutdownReason::ThreadClose => {
                                    surface::TerminalizationCause::ThreadClose
                                }
                            };
                            if active.surface_terminalization.is_some() {
                                if let Some(reply) = reply.as_ref() {
                                    let _ = reply.send(ThreadShutdownAck::Retry);
                                }
                                self.active = Some(active);
                                continue;
                            }
                            if let Err(error) = self
                                .commit_surface_terminalization(&mut active, terminalization)
                            {
                                if self.resident_surface.pending_terminalization.is_some() {
                                    if let Some(reply) = reply.as_ref() {
                                        let _ = reply.send(ThreadShutdownAck::Retry);
                                    }
                                    self.active = Some(active);
                                    continue;
                                }
                                if reason == surface::SurfaceShutdownReason::HostShutdown {
                                    command_rx.close();
                                    active.generation.cancel.cancel();
                                    Self::drain_closed_thread_commands(&mut command_rx);
                                    self.resident_surface.0.take();
                                    let _ = (&mut active.generation.join).await;
                                    self.shutdown_background_tasks().await;
                                    if let Some(reply) = reply {
                                        let _ = reply.send(ThreadShutdownAck::Failed(error));
                                    }
                                    break;
                                }
                                if let Some(reply) = reply.as_ref() {
                                    let _ = reply.send(ThreadShutdownAck::Failed(error));
                                }
                                self.active = Some(active);
                                continue;
                            }
                            command_rx.close();
                            let pause_result = Self::pause_active_goal(
                                &mut active,
                                "goal run paused during runtime shutdown",
                            );
                            active.generation.cancel.cancel();
                            Self::drain_closed_thread_commands(&mut command_rx);
                            let result = (&mut active.generation.join).await;
                            let finish_result = self.finish_generation(active, result, false);
                            self.shutdown_background_tasks().await;
                            if let Err(error) = finish_result {
                                if let Some(reply) = reply.as_ref() {
                                    let _ = reply.send(ThreadShutdownAck::Failed(error));
                                }
                                if reason == surface::SurfaceShutdownReason::HostShutdown {
                                    break;
                                }
                                std::future::pending::<()>().await;
                            }
                            if let Some(reply) = reply.as_ref() {
                                let ack = match pause_result {
                                    Ok(()) => ThreadShutdownAck::Complete,
                                    Err(error) => ThreadShutdownAck::Failed(error),
                                };
                                let _ = reply.send(ack);
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

    fn drain_closed_thread_commands(command_rx: &mut tokio_mpsc::Receiver<ThreadCommand>) {
        while let Ok(command) = command_rx.try_recv() {
            match command {
                ThreadCommand::SurfaceDetach {
                    client,
                    request,
                    reply,
                } => {
                    let _ = reply.send(surface::DetachResult::StaleAttachment {
                        request_id: request.request_id,
                        attachment_id: client.attachment_id().clone(),
                    });
                }
                ThreadCommand::SurfaceReserveOperation { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceAdmitReserved { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceAdmitReservedWithOutput { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceCancelOperation { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceResumeOperation { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceWaitOperationTerminal { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceUpdateSettings { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfacePinnedContextMutation { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceCommitProviderResponse { reply, .. } => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "runtime thread is shutting down",
                    )));
                }
                ThreadCommand::SurfaceCommitProviderStep { reply, .. } => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "runtime thread is shutting down",
                    )));
                }
                ThreadCommand::SurfaceCommitToolResults { reply, .. } => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "runtime thread is shutting down",
                    )));
                }
                ThreadCommand::SurfaceRequestToolApproval { reply, .. } => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "runtime thread is shutting down",
                    )));
                }
                ThreadCommand::SurfaceRequestPermission { reply, .. } => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "runtime thread is shutting down",
                    )));
                }
                ThreadCommand::SurfaceRequestUserInput { reply, .. } => {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "runtime thread is shutting down",
                    )));
                }
                ThreadCommand::SurfaceRequestMcpElicitation { reply, .. } => {
                    let _ = reply.send(Err("runtime thread is shutting down".to_string()));
                }
                #[cfg(test)]
                ThreadCommand::SurfaceRespondInteraction { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceRespondInteractionById { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceRespondInteractionByIdWithPolicy { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                ThreadCommand::SurfaceRetryFinalization { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                #[cfg(test)]
                ThreadCommand::SurfaceSuspendOperationForTest { reply, .. } => {
                    let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
                }
                #[cfg(test)]
                ThreadCommand::SurfaceActorTestProbe { reply, .. } => drop(reply),
                ThreadCommand::StartTurn { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::LaunchWorkflow { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::InterruptOperation { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::PauseGoalRun { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::ResumeOperation { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::SteerOperation { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::AdmitGeneration { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::ReadState { reply } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::ReadSnapshot { reply } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::GoalRuntime { reply } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::MutateIdle { reply, .. } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::BacktrackLastUser { reply } => {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                }
                ThreadCommand::ShutdownThread { reply, .. } => {
                    if let Some(reply) = reply {
                        let _ = reply.send(ThreadShutdownAck::Failed(
                            RuntimeHostError::ThreadUnavailable,
                        ));
                    }
                }
            }
        }
    }

    fn handle_idle_command(&mut self, command: ThreadCommand) {
        match command {
            ThreadCommand::SurfaceDetach {
                client,
                request,
                reply,
            } => {
                let result = self.detach_surface_attachment(None, &client, request);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceReserveOperation {
                client,
                request_id,
                intent,
                reply,
            } => {
                let result = if self
                    .admits_surface_client(&client, surface::SurfaceCapability::SubmitOperation)
                {
                    self.reserve_surface_operation(
                        request_id,
                        intent,
                        client.attachment_id().clone(),
                        client.connection_id().cloned(),
                    )
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
                    self.admit_surface_operation(
                        &client,
                        request_id,
                        operation_id,
                        admission_lease_id,
                    )
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceAdmitReservedWithOutput {
                client,
                request_id,
                operation_id,
                admission_lease_id,
                writer,
                reply,
            } => {
                let result = if self
                    .admits_surface_client(&client, surface::SurfaceCapability::SubmitOperation)
                {
                    self.admit_surface_operation_with_output(
                        &client,
                        request_id,
                        operation_id,
                        admission_lease_id,
                        Some(writer),
                    )
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
                    self.cancel_surface_idle(&client, request_id, operation_id)
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceResumeOperation {
                client,
                request_id,
                operation_id,
                expected_last_generation,
                resume_source,
                reply,
            } => {
                let result = if self.admits_surface_client(
                    &client,
                    surface::SurfaceCapability::ControlBoundOperation,
                ) {
                    self.resume_surface_operation(
                        &client,
                        request_id,
                        operation_id,
                        expected_last_generation,
                        resume_source,
                    )
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
            ThreadCommand::SurfaceUpdateSettings {
                client,
                request_id,
                expected_thread_revision,
                patch,
                reply,
            } => {
                let result = self.update_surface_settings(
                    &client,
                    request_id,
                    expected_thread_revision,
                    patch,
                );
                let _ = reply.send(result);
            }
            ThreadCommand::SurfacePinnedContextMutation {
                client,
                request_id,
                action,
                reply,
            } => {
                let result = self.pinned_context_mutation(&client, request_id, action);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceCommitProviderResponse { reply, .. } => {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                )));
            }
            ThreadCommand::SurfaceCommitProviderStep { reply, .. } => {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                )));
            }
            ThreadCommand::SurfaceCommitToolResults { reply, .. } => {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                )));
            }
            ThreadCommand::SurfaceRequestToolApproval { reply, .. } => {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                )));
            }
            ThreadCommand::SurfaceRequestPermission { reply, .. } => {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                )));
            }
            ThreadCommand::SurfaceRequestUserInput { reply, .. } => {
                let _ = reply.send(Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                )));
            }
            ThreadCommand::SurfaceRequestMcpElicitation { reply, .. } => {
                let _ = reply.send(Err("runtime generation is not active".to_string()));
            }
            #[cfg(test)]
            ThreadCommand::SurfaceRespondInteraction {
                client,
                request_id,
                selector,
                response,
                reply,
            } => {
                let result =
                    self.respond_surface_interaction(&client, request_id, selector, response);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceRespondInteractionById {
                client,
                request_id,
                interaction_id,
                answer,
                reply,
            } => {
                let result = self.respond_surface_interaction_by_id(
                    &client,
                    request_id,
                    interaction_id,
                    answer,
                );
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceRespondInteractionByIdWithPolicy {
                client,
                request_id,
                interaction_id,
                answer,
                policy,
                reply,
            } => {
                let result = self.respond_surface_interaction_by_id_with_policy(
                    &client,
                    request_id,
                    interaction_id,
                    answer,
                    policy,
                );
                let _ = reply.send(result);
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
            ThreadCommand::SurfaceSuspendOperationForTest { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
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
                    exact_interaction_selector: exact_interaction_selector_for_test(
                        &self.resident_surface,
                        &operation_id,
                    ),
                    secret_bearing_interaction_count: self.resident_surface.interactions.len(),
                    pending_capability_loss: pending_capability_loss_for_test(
                        &self.resident_surface,
                    ),
                    pending_terminalization: pending_terminalization_for_test(
                        &self.resident_surface,
                    ),
                    interaction_admission_closed: false,
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
                    surface_execution_failure: None,
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
            ThreadCommand::SurfaceDetach {
                client,
                request,
                reply,
            } => {
                let result = self.detach_surface_attachment(Some(active), &client, request);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceReserveOperation { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            }
            ThreadCommand::SurfaceAdmitReserved { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            }
            ThreadCommand::SurfaceAdmitReservedWithOutput { reply, .. } => {
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
                    self.cancel_surface_running(active, &client, request_id, operation_id)
                } else {
                    Err(surface::SurfaceClientCommandError::Unauthorized)
                };
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceResumeOperation { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
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
            ThreadCommand::SurfaceUpdateSettings { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            }
            ThreadCommand::SurfacePinnedContextMutation { reply, .. } => {
                let _ = reply.send(Err(surface::SurfaceClientCommandError::RuntimeUnavailable));
            }
            ThreadCommand::SurfaceCommitProviderResponse {
                fence,
                response,
                reply,
            } => {
                let result = self.commit_surface_provider_response(active, fence, &response);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceCommitProviderStep {
                fence,
                identity,
                step,
                reply,
            } => {
                let result = self.commit_surface_provider_step(active, fence, &identity, &step);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceCommitToolResults {
                fence,
                results,
                reply,
            } => {
                let result = self.commit_surface_tool_results(active, fence, &results);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceRequestToolApproval {
                fence,
                approval,
                request,
                reply,
            } => {
                self.request_surface_tool_approval(active, fence, approval, request, reply);
            }
            ThreadCommand::SurfaceRequestPermission {
                fence,
                request,
                reply,
            } => {
                self.request_surface_permission(active, fence, request, reply);
            }
            ThreadCommand::SurfaceRequestUserInput {
                fence,
                request,
                reply,
            } => {
                if Self::surface_interaction_admission_closed(active) {
                    let _ = reply.send(Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "runtime generation is terminalizing",
                    )));
                } else {
                    self.request_surface_user_input(active, fence, request, reply);
                }
            }
            ThreadCommand::SurfaceRequestMcpElicitation {
                fence,
                request,
                reply,
            } => {
                if Self::surface_interaction_admission_closed(active) {
                    let _ = reply.send(Err("runtime generation is terminalizing".to_string()));
                } else {
                    self.request_surface_mcp_elicitation(active, fence, request, reply);
                }
            }
            #[cfg(test)]
            ThreadCommand::SurfaceRespondInteraction {
                client,
                request_id,
                selector,
                response,
                reply,
            } => {
                let result =
                    self.respond_surface_interaction(&client, request_id, selector, response);
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceRespondInteractionById {
                client,
                request_id,
                interaction_id,
                answer,
                reply,
            } => {
                let result = self.respond_surface_interaction_by_id(
                    &client,
                    request_id,
                    interaction_id,
                    answer,
                );
                let _ = reply.send(result);
            }
            ThreadCommand::SurfaceRespondInteractionByIdWithPolicy {
                client,
                request_id,
                interaction_id,
                answer,
                policy,
                reply,
            } => {
                let result = self.respond_surface_interaction_by_id_with_policy(
                    &client,
                    request_id,
                    interaction_id,
                    answer,
                    policy,
                );
                let _ = reply.send(result);
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
            ThreadCommand::SurfaceSuspendOperationForTest {
                operation_id,
                reply,
            } => {
                let result = (|| {
                    let fence = active
                        .surface_operation
                        .as_ref()
                        .filter(|fence| fence.operation_id == operation_id)
                        .cloned()
                        .ok_or(surface::SurfaceClientCommandError::Unauthorized)?;
                    let batch = self.surface_operation_batch(
                        &operation_id,
                        vec![
                            surface::OperationPatch::GenerationStopped {
                                fence: fence.clone(),
                                reason: surface::GenerationStopReason::InterruptedResumable,
                                usage_delta: surface::UsageTotals {
                                    input_tokens: 0,
                                    output_tokens: 0,
                                    cache_tokens: 0,
                                    estimated_cost_usd_micros: 0,
                                },
                            },
                            surface::OperationPatch::Suspended {
                                operation_id: operation_id.clone(),
                                cause: surface::SuspensionCause::Interrupted {
                                    generation_id: fence.generation_id,
                                },
                            },
                        ],
                    );
                    self.resident_surface
                        .coordinator
                        .commit_live_generation_suspend_batch(fence, &batch)
                        .map_err(|error| {
                            eprintln!("orca: test suspension commit failed: {error:?}");
                            surface::SurfaceClientCommandError::RuntimeUnavailable
                        })?;
                    active.surface_operation = None;
                    active.generation.cancel.cancel();
                    Ok(())
                })();
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
                    exact_interaction_selector: exact_interaction_selector_for_test(
                        &self.resident_surface,
                        &operation_id,
                    ),
                    secret_bearing_interaction_count: self.resident_surface.interactions.len(),
                    pending_capability_loss: pending_capability_loss_for_test(
                        &self.resident_surface,
                    ),
                    pending_terminalization: pending_terminalization_for_test(
                        &self.resident_surface,
                    ),
                    interaction_admission_closed: Self::surface_interaction_admission_closed(
                        active,
                    ),
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

async fn wait_for_surface_transition_retry(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender};
    use std::time::Instant;

    use crate::model_response::RuntimeModelResponse;

    const TERMINAL_FAILURE_CHILD_ENV: &str = "ORCA_RUNTIME_HOST_TERMINAL_FAILURE_CHILD";
    const PREPARED_TERMINAL_CHILD_ENV: &str = "ORCA_RUNTIME_HOST_PREPARED_TERMINAL_CHILD";
    const PREPARED_TERMINALIZATION_RESTART_CHILD_ENV: &str =
        "ORCA_RUNTIME_HOST_PREPARED_TERMINALIZATION_RESTART_CHILD";
    const EMPTY_THREAD_OWNER_RECOVERY_CHILD_ENV: &str =
        "ORCA_RUNTIME_HOST_EMPTY_THREAD_OWNER_RECOVERY_CHILD";
    const SUSPENDED_OPERATION_RECOVERY_CHILD_ENV: &str =
        "ORCA_RUNTIME_HOST_SUSPENDED_OPERATION_RECOVERY_CHILD";
    const RESERVATION_TERMINAL_FAILURE_CHILD_ENV: &str =
        "ORCA_RUNTIME_HOST_RESERVATION_TERMINAL_FAILURE_CHILD";
    const SURFACE_TEST_TIMEOUT: Duration = Duration::from_secs(5);

    struct GatedSuccessExecutor {
        entered: SyncSender<()>,
        release: Mutex<Receiver<()>>,
    }

    struct SuspendThenResumeExecutor {
        calls: AtomicUsize,
        entered: SyncSender<usize>,
    }

    struct CancelAwareShutdownExecutor {
        entered: SyncSender<()>,
        cancel_observed: SyncSender<()>,
        completed: SyncSender<()>,
    }

    struct QueuedInteractionShutdownExecutor {
        entered: SyncSender<()>,
        release_interaction: Mutex<Receiver<()>>,
        interaction_result: SyncSender<io::Result<Option<String>>>,
        completed: SyncSender<()>,
    }

    struct RoundRobinShutdownExecutor {
        entered: SyncSender<String>,
        completed: SyncSender<String>,
    }

    struct DispatchFairShutdownExecutor {
        entered: SyncSender<String>,
        shutdown_observed: SyncSender<String>,
        completed: SyncSender<String>,
        blocked_release: Mutex<Receiver<()>>,
    }

    struct ExactSelectorUserInputExecutor {
        answer_tx: SyncSender<Option<String>>,
    }

    struct SequentialUserInputExecutor {
        first_answer_tx: SyncSender<Option<String>>,
        second_answer_tx: SyncSender<Option<String>>,
        continue_second: Mutex<Receiver<()>>,
    }

    struct ImmediateSecondUserInputExecutor {
        first_answer_tx: SyncSender<Option<String>>,
        second_result_tx: SyncSender<io::Result<Option<String>>>,
    }

    struct SlowSubscriberUserInputExecutor {
        entered: SyncSender<()>,
        release: Mutex<Receiver<()>>,
        answer_tx: SyncSender<Option<String>>,
    }

    struct ProviderResponseCheckpointRetryExecutor;

    struct OutputWriterExecutor;

    struct SharedOutputWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedOutputWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct SettingsConfigExecutor {
        observed_model: SyncSender<String>,
    }

    struct SettingsConfigSnapshotExecutor {
        observed: SyncSender<(String, ApprovalMode, orca_core::config::ReasoningEffort)>,
    }

    #[derive(Clone, Copy)]
    enum ExactSelectorCorruption {
        Revision,
        ResponseToken,
        RouteEpoch,
        OperationFence,
        GrantToken,
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

    impl ThreadOperationExecutor for ProviderResponseCheckpointRetryExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let turn_request = request.thread_turn_request(generation);
            let ingress = turn_request
                .provider_response_ingress()
                .expect("typed generation installs provider response ingress");
            ingress.commit_response(&RuntimeModelResponse::new(
                ProviderResponse {
                    steps: Vec::new(),
                    assistant_content: Some("checkpoint retry succeeded".to_string()),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                },
                request.turn_id().clone(),
            ))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for OutputWriterExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            writer.write_all(b"typed output\n")?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for SettingsConfigExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.observed_model
                .send(generation.config().model.display_name().to_string())
                .map_err(|_| io::Error::other("settings config observer closed"))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for SettingsConfigSnapshotExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.observed
                .send((
                    generation.config().model.display_name().to_string(),
                    generation.config().approval_mode,
                    generation.config().reasoning_effort,
                ))
                .map_err(|_| io::Error::other("settings config snapshot observer closed"))?;
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
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

    impl ThreadOperationExecutor for SuspendThenResumeExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.send(call).expect("report executor entry");
            if call == 0 {
                while !cancel.is_cancelled() {
                    std::thread::yield_now();
                }
                thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
                return Ok(RunStatus::Cancelled.into());
            }
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for CancelAwareShutdownExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.entered.send(()).expect("report executor entry");
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.cancel_observed
                .send(())
                .expect("report observed cancellation");
            thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
            self.completed.send(()).expect("report worker completion");
            Ok(RunStatus::Cancelled.into())
        }
    }

    impl ThreadOperationExecutor for QueuedInteractionShutdownExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.entered.send(()).expect("report executor entry");
            self.release_interaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("release queued interaction");
            let result = generation
                .user_input_handler()
                .expect("runtime installs typed user-input broker")
                .request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                    id: "queued-during-host-shutdown".to_string(),
                    question: "Reject interaction behind shutdown?".to_string(),
                    choices: Vec::new(),
                });
            self.interaction_result
                .send(result)
                .expect("report rejected interaction");
            thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
            self.completed.send(()).expect("report worker completion");
            Ok(RunStatus::Cancelled.into())
        }
    }

    impl ThreadOperationExecutor for RoundRobinShutdownExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let label = request.prompt.clone();
            self.entered
                .send(label.clone())
                .expect("report executor entry");
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
            self.completed
                .send(label)
                .expect("report worker completion");
            Ok(RunStatus::Cancelled.into())
        }
    }

    impl ThreadOperationExecutor for DispatchFairShutdownExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            request: &HostedTurnRequest,
            _generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let label = request.prompt.clone();
            self.entered
                .send(label.clone())
                .expect("report executor entry");
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.shutdown_observed
                .send(label.clone())
                .expect("report shutdown cancellation");
            if label == "blocked-actor" {
                self.blocked_release
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv()
                    .expect("release blocked shutdown actor");
            }
            thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
            self.completed
                .send(label)
                .expect("report worker completion");
            Ok(RunStatus::Cancelled.into())
        }
    }

    impl ThreadOperationExecutor for ExactSelectorUserInputExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let answer = generation
                .user_input_handler()
                .expect("runtime installs typed user-input broker")
                .request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                    id: "exact-selector-input".to_string(),
                    question: "Accept exact selector?".to_string(),
                    choices: Vec::new(),
                })?;
            self.answer_tx.send(answer).expect("report typed answer");
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for SequentialUserInputExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let handler = generation
                .user_input_handler()
                .expect("runtime installs typed user-input broker");
            let first = handler.request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                id: "private-input-1".to_string(),
                question: "First private answer?".to_string(),
                choices: Vec::new(),
            })?;
            self.first_answer_tx
                .send(first)
                .expect("report first typed answer");
            self.continue_second
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("release second typed interaction");
            let second =
                handler.request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                    id: "private-input-2".to_string(),
                    question: "Second private answer?".to_string(),
                    choices: Vec::new(),
                })?;
            self.second_answer_tx
                .send(second)
                .expect("report second typed answer");
            thread.lifecycle_mut().finish_task(RunStatus::Success);
            Ok(RunStatus::Success.into())
        }
    }

    impl ThreadOperationExecutor for ImmediateSecondUserInputExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            let handler = generation
                .user_input_handler()
                .expect("runtime installs typed user-input broker");
            let first = handler.request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                id: "cancel-private-input-1".to_string(),
                question: "First answer before cancellation?".to_string(),
                choices: Vec::new(),
            })?;
            self.first_answer_tx
                .send(first)
                .expect("report first typed answer");
            let second = handler.request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                id: "cancel-private-input-2".to_string(),
                question: "Second answer after cancellation starts?".to_string(),
                choices: Vec::new(),
            });
            self.second_result_tx
                .send(second)
                .expect("report second typed interaction result");
            thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
            Ok(RunStatus::Cancelled.into())
        }
    }

    impl ThreadOperationExecutor for SlowSubscriberUserInputExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            _events: &mut EventFactory,
            _writer: &mut (dyn io::Write + Send),
            _cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            self.entered.send(()).expect("report executor entry");
            self.release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("release user-input request");
            let answer = generation
                .user_input_handler()
                .expect("runtime installs typed user-input broker")
                .request_user_input(&crate::lifecycle::RuntimeUserInputRequest {
                    id: "slow-subscriber-input".to_string(),
                    question: "Reroute slow responder?".to_string(),
                    choices: Vec::new(),
                })?;
            self.answer_tx.send(answer).expect("report typed answer");
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
                surface::SurfaceCapability::ManageThreadSettings,
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

    fn fresh_surface_interaction_attachment(
        handle: &surface::RuntimeSurfaceHandle,
    ) -> surface::FreshSurfaceAttachment {
        match handle.attach_fresh(surface::FreshAttachRequest {
            request_id: surface_request_id(),
            role: surface::SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
                surface::SurfaceCapability::ControlBoundOperation,
                surface::SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: BTreeSet::from([surface::SurfaceInteractionKind::UserInput]),
        }) {
            surface::AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh interaction attachment failed"),
        }
    }

    fn collect_requested_surface_interaction(
        receiver: &mut surface::SurfaceSubscriptionReceiver,
    ) -> surface::SurfaceInteractionView {
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        loop {
            while let Some(item) = receiver.try_recv() {
                if let surface::SurfaceSubscriptionItem::Batch { batch } = item {
                    for event in batch.events.as_slice() {
                        if let surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Requested { interaction },
                        ) = &event.event
                        {
                            return interaction.clone();
                        }
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "typed interaction request was not published"
            );
            std::thread::yield_now();
        }
    }

    fn collect_surface_interaction_route(
        receiver: &mut surface::SurfaceSubscriptionReceiver,
        interaction_id: &surface::SurfaceInteractionId,
    ) -> surface::SurfaceInteractionRoute {
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        loop {
            while let Some(item) = receiver.try_recv() {
                if let surface::SurfaceSubscriptionItem::Batch { batch } = item {
                    for event in batch.events.as_slice() {
                        if let surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::RouteChanged {
                                interaction_id: candidate,
                                route,
                                ..
                            },
                        ) = &event.event
                            && candidate == interaction_id
                        {
                            return route.clone();
                        }
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "typed interaction route change was not published"
            );
            std::thread::yield_now();
        }
    }

    fn corrupt_exact_selector(
        mut selector: surface::InteractionSelector,
        corruption: ExactSelectorCorruption,
    ) -> surface::InteractionSelector {
        let surface::InteractionSelector::Exact {
            expected_revision,
            response_token,
            response_route_epoch,
            response_grant_token,
            operation_fence,
            ..
        } = &mut selector
        else {
            panic!("actor probe returned an opaque selector")
        };
        match corruption {
            ExactSelectorCorruption::Revision => {
                *expected_revision =
                    surface::InteractionRevision::try_new(expected_revision.get() + 1)
                        .expect("test interaction revision did not exhaust");
            }
            ExactSelectorCorruption::ResponseToken => {
                let mut bytes = *response_token.key_bytes();
                bytes[0] ^= 0xff;
                *response_token = surface::SurfaceResponseToken::new(bytes);
            }
            ExactSelectorCorruption::RouteEpoch => {
                *response_route_epoch =
                    surface::ResponseRouteEpoch::try_new(response_route_epoch.get() + 1)
                        .expect("test route epoch did not exhaust");
            }
            ExactSelectorCorruption::OperationFence => {
                operation_fence.generation_id =
                    surface::SurfaceGenerationId::new(operation_fence.generation_id.get() + 1);
            }
            ExactSelectorCorruption::GrantToken => {
                let mut bytes = *response_grant_token.key_bytes();
                bytes[0] ^= 0xff;
                *response_grant_token = surface::SurfaceResponseGrantToken::new(bytes);
            }
        }
        selector
    }

    fn assert_exact_selector_rejection_is_non_consuming(
        corruption: ExactSelectorCorruption,
        expected_stale: bool,
        expected_code: surface::SurfaceMutationErrorCode,
    ) {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "validate exact interaction selector",
            )
            .expect("start recorded runtime thread");
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let _subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "request exact interaction",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        let exact_selector = loop {
            let probe = thread
                .surface_actor_probe_for_test(operation_id.clone())
                .expect("probe resident interaction");
            if let Some(selector) = probe.exact_interaction_selector {
                break selector;
            }
            assert!(
                Instant::now() < deadline,
                "typed interaction did not become resident"
            );
            std::thread::yield_now();
        };
        let response = surface::BoundInteractionResponse::new(
            surface::SurfaceResponseId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            surface::SurfaceClientInteractionAnswer::UserInput {
                decision: surface::SurfaceUserInputDecision::Answer(surface::DisplayText::new(
                    "accepted",
                )),
            },
            surface::BrokerInteractionAnswerPolicy::NativeStrict,
            surface::ApplicableAuthorityFingerprint::not_applicable(),
        );
        let rejected = thread
            .respond_surface_interaction_for_test(
                attachment.client.clone(),
                surface_request_id(),
                corrupt_exact_selector(exact_selector.clone(), corruption),
                response.clone(),
            )
            .expect("actor returns typed selector rejection");
        let (actual_stale, actual_code) = match rejected {
            surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Invalid { error, .. },
            } => (false, error.error().code),
            surface::MutationReply::Uncommitted {
                mutation: surface::UncommittedMutation::Stale { error, .. },
            } => (true, error.error().code),
            _ => panic!("invalid exact selector consumed or deferred the interaction"),
        };
        assert_eq!(actual_stale, expected_stale);
        assert_eq!(actual_code, expected_code);
        assert!(
            answer_rx.try_recv().is_err(),
            "invalid exact selector woke the native waiter"
        );

        let _ = committed_surface_value(
            thread
                .respond_surface_interaction_for_test(
                    attachment.client.clone(),
                    surface_request_id(),
                    exact_selector,
                    response,
                )
                .expect("valid exact selector remains usable"),
        );
        assert_eq!(
            answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("accepted".to_string())
        );
        let _ = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait exact-selector operation terminal");
        host.shutdown().expect("shutdown runtime host");
    }

    #[test]
    fn failed_typed_admission_is_terminal_and_allows_next_turn_without_restart() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(GatedSuccessExecutor {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "repair failed typed admission",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load runtime transcript")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let first = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "first turn"),
                )
                .expect("reserve first typed operation"),
        );
        surface::JsonlSurfaceCommitLedger::inject_generation_append_failure_once(
            transcript_path.clone(),
        );
        surface::JsonlSurfaceCommitLedger::inject_admission_repair_append_failure_once(
            transcript_path.clone(),
        );
        assert!(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    first.operation_id.clone(),
                    first.lease.lease_id,
                )
                .is_err(),
            "admission should report the injected durable failure"
        );
        let failed = attachment
            .client
            .wait_operation_terminal(surface_request_id(), first.operation_id.clone())
            .expect("wait repaired admission terminal");
        assert!(matches!(
            failed,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(
                    value.terminal,
                    surface::OperationTerminal::Failed {
                        class: surface::FailureClass::Persistence,
                        ..
                    }
                )
        ));

        let second = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "second turn"),
                )
                .expect("reserve second typed operation after repair"),
        );
        let admitted = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    second.operation_id.clone(),
                    second.lease.lease_id,
                )
                .expect("admit second typed operation after repair"),
        );
        assert!(matches!(
            admitted,
            surface::AdmissionOutput::Admitted { .. }
        ));
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("second turn did not reach executor");
        release_tx.send(()).expect("release second turn");
        let second_terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), second.operation_id)
            .expect("wait second turn terminal");
        assert!(matches!(
            second_terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        host.shutdown().expect("shutdown repaired runtime host");
    }

    #[test]
    fn suspended_surface_operation_resumes_same_identity_after_durable_barriers() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let host = RuntimeHost::start_with_executor(Arc::new(SuspendThenResumeExecutor {
            calls: AtomicUsize::new(0),
            entered: entered_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "resume suspended typed operation",
            )
            .expect("start recorded runtime thread");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "resume the same operation",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let admitted = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let surface::AdmissionOutput::Admitted {
            first_generation, ..
        } = admitted
        else {
            panic!("operation was queued instead of admitted");
        };
        assert_eq!(entered_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), 0);
        thread
            .suspend_surface_operation_for_test(operation_id.clone())
            .expect("suspend typed operation");

        let suspended_snapshot = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
        )
        .baseline
        .snapshot;
        let replayability_digest = surface::canonical_replayability_digest(
            &suspended_snapshot
                .foreground_operation
                .as_ref()
                .filter(|operation| operation.operation_id == operation_id)
                .expect("suspended operation remains visible")
                .intent
                .initial_replayability,
        );
        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        let resumed = loop {
            match attachment.client.resume_operation(
                surface_request_id(),
                operation_id.clone(),
                first_generation.generation_id,
                surface::ResumeSourceWitness::DurableReplay {
                    replayability_digest: replayability_digest.clone(),
                },
            ) {
                Ok(reply) => break committed_surface_value(reply),
                Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
                    if Instant::now() < deadline =>
                {
                    std::thread::yield_now();
                }
                Err(error) => {
                    let failed = fresh_surface_attachment_with_capabilities(
                        &surface,
                        BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
                    )
                    .baseline
                    .snapshot;
                    let failed = failed
                        .foreground_operation
                        .as_ref()
                        .filter(|operation| operation.operation_id == operation_id);
                    panic!(
                        "resume did not become admissible: {error:?}; runtime={:?}; phase={:?}; pending={:?}; generations={}",
                        thread.state(),
                        failed.map(|operation| &operation.phase),
                        failed.and_then(|operation| operation.pending_control.as_ref()),
                        failed.map_or(0, |operation| operation.generations.len())
                    );
                }
            }
        };
        assert_eq!(resumed.operation_id, operation_id);
        assert_eq!(resumed.generation.generation_id.get(), 1);
        assert_eq!(
            resumed.resume_starting.role,
            surface::ResumeTransitionRole::ResumeStarting
        );
        assert_eq!(
            resumed.generation_reserved.role,
            surface::ResumeTransitionRole::GenerationReserved
        );
        assert_eq!(
            resumed.generation_started.role,
            surface::ResumeTransitionRole::GenerationStarted
        );
        assert_eq!(entered_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), 1);
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id.clone())
            .expect("wait resumed operation terminal");
        let snapshot = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
        )
        .baseline
        .snapshot;
        host.shutdown().expect("shutdown runtime host");

        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if value.operation_id == operation_id
                    && matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        let operation = snapshot
            .operation_history
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("resumed operation reached history");
        assert_eq!(operation.generations.len(), 2);
        assert!(matches!(
            operation.generations[1].attempt,
            surface::GenerationAttempt::RecoveryReplacement
        ));
    }

    type SuspendedOperationFixture = (
        String,
        surface::SurfaceOperationId,
        surface::SurfaceGenerationId,
        surface::Sha256Digest,
    );

    fn suspended_operation_fixture_path() -> PathBuf {
        PathBuf::from(std::env::var_os("ORCA_HOME").expect("ORCA_HOME for recovery child"))
            .join("suspended-operation-recovery.json")
    }

    fn run_suspended_operation_seed_child() -> ! {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(SuspendThenResumeExecutor {
            calls: AtomicUsize::new(0),
            entered: entered_tx,
        }))
        .expect("start suspended-operation seed host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "seed suspended operation",
            )
            .expect("start suspended-operation seed thread");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "recover this operation after restart",
                    ),
                )
                .expect("reserve suspended-operation fixture"),
        );
        let operation_id = reserved.operation_id.clone();
        let admitted = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit suspended-operation fixture"),
        );
        let surface::AdmissionOutput::Admitted {
            first_generation, ..
        } = admitted
        else {
            panic!("fixture operation was queued");
        };
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("fixture executor did not start");
        thread
            .suspend_surface_operation_for_test(operation_id.clone())
            .expect("durably suspend fixture operation");
        let snapshot = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
        )
        .baseline
        .snapshot;
        let operation = snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == operation_id)
            .expect("fixture operation remains suspended");
        let fixture: SuspendedOperationFixture = (
            thread.thread_id().to_string(),
            operation_id,
            first_generation.generation_id,
            surface::canonical_replayability_digest(&operation.intent.initial_replayability),
        );
        std::fs::write(
            suspended_operation_fixture_path(),
            serde_json::to_vec(&fixture).unwrap(),
        )
        .expect("write suspended operation fixture");
        std::mem::forget(host);
        std::process::exit(0)
    }

    fn run_suspended_operation_recovery_child(resume: bool) -> ! {
        let (thread_id, operation_id, generation_id, replayability_digest):
            SuspendedOperationFixture = serde_json::from_slice(
            &std::fs::read(suspended_operation_fixture_path())
                .expect("read suspended operation fixture"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let executor = SuspendThenResumeExecutor {
            calls: AtomicUsize::new(1),
            entered: entered_tx,
        };
        let host = RuntimeHost::start_with_executor(Arc::new(executor))
            .expect("start suspended-operation recovery host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover suspended operation",
            )
            .expect("resume suspended-operation thread");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let operation = attachment
            .baseline
            .snapshot
            .foreground_operation
            .as_ref()
            .filter(|operation| operation.operation_id == operation_id)
            .expect("cold-recovered operation remains visible");
        assert!(matches!(
            operation.phase,
            surface::OperationPhase::Suspended { .. }
        ));

        if resume {
            let resumed = committed_surface_value(
                attachment
                    .client
                    .resume_operation(
                        surface_request_id(),
                        operation_id.clone(),
                        generation_id,
                        surface::ResumeSourceWitness::DurableReplay {
                            replayability_digest,
                        },
                    )
                    .expect("resume cold-recovered operation"),
            );
            assert_eq!(resumed.operation_id, operation_id);
            assert_eq!(entered_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), 1);
            let terminal = attachment
                .client
                .wait_operation_terminal(surface_request_id(), operation_id)
                .expect("wait resumed cold-recovered operation");
            assert!(matches!(
                terminal,
                surface::WaitOperationTerminalResult::Terminal { value }
                    if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
            ));
        } else {
            let cancelled = committed_surface_value(
                attachment
                    .client
                    .cancel_operation(surface_request_id(), operation_id.clone())
                    .expect("cancel cold-recovered operation"),
            );
            assert!(matches!(
                cancelled,
                surface::CancelOperationOutput::Accepted { .. }
            ));
            let terminal = attachment
                .client
                .wait_operation_terminal(surface_request_id(), operation_id)
                .expect("wait cancelled cold-recovered operation");
            assert!(matches!(
                terminal,
                surface::WaitOperationTerminalResult::Terminal { value }
                    if matches!(
                        value.terminal,
                        surface::OperationTerminal::Cancelled {
                            reason: surface::CancelReason::User,
                        }
                    )
            ));
            assert!(entered_rx.try_recv().is_err());
        }
        host.shutdown().expect("shutdown recovery host");
        std::process::exit(0)
    }

    #[test]
    fn cold_restart_exposes_exact_resume_and_cancel_for_suspended_operation() {
        if let Some(phase) = std::env::var_os(SUSPENDED_OPERATION_RECOVERY_CHILD_ENV) {
            match phase.to_string_lossy().as_ref() {
                "seed" => run_suspended_operation_seed_child(),
                "resume" => run_suspended_operation_recovery_child(true),
                "cancel" => run_suspended_operation_recovery_child(false),
                phase => panic!("unknown suspended-operation recovery phase: {phase}"),
            }
        }

        let home = tempfile::tempdir().unwrap();
        for recovery in ["resume", "cancel"] {
            for phase in ["seed", recovery] {
                let status = Command::new(std::env::current_exe().unwrap())
                    .arg("--exact")
                    .arg(
                        "runtime_host::tests::cold_restart_exposes_exact_resume_and_cancel_for_suspended_operation",
                    )
                    .arg("--nocapture")
                    .arg("--test-threads=1")
                    .env(SUSPENDED_OPERATION_RECOVERY_CHILD_ENV, phase)
                    .env("ORCA_HOME", home.path())
                    .status()
                    .expect("start suspended-operation recovery child");
                assert!(
                    status.success(),
                    "suspended-operation recovery child failed during {phase}"
                );
            }
        }
    }

    #[test]
    fn typed_admission_with_output_routes_writer_through_runtime_operation() {
        let cwd = tempfile::tempdir().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let host = RuntimeHost::start_with_executor(Arc::new(OutputWriterExecutor))
            .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "typed output writer",
            )
            .expect("start recorded runtime thread");
        let attachment = fresh_surface_attachment(&thread.surface());
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "writer turn"),
                )
                .expect("reserve typed operation"),
        );
        let writer = PassthroughHostedOperationWriter::new(SharedOutputWriter(Arc::clone(&output)));
        let admitted = committed_surface_value(
            attachment
                .client
                .admit_reserved_with_output(
                    surface_request_id(),
                    reserved.operation_id.clone(),
                    reserved.lease.lease_id,
                    writer,
                )
                .expect("admit typed operation with output"),
        );
        assert!(matches!(
            admitted,
            surface::AdmissionOutput::Admitted { .. }
        ));
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), reserved.operation_id)
            .expect("wait typed output operation");
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        assert_eq!(
            output.lock().unwrap().as_slice(),
            b"typed output\n",
            "the supplied writer must be owned by the admitted runtime operation"
        );
        host.shutdown().expect("shutdown typed output host");
    }

    #[test]
    fn admitted_batch_checkpoint_failure_retries_exact_batch_before_terminal_repair() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, _entered_rx) = mpsc::sync_channel(1);
        let (_release_tx, release_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(GatedSuccessExecutor {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry admitted batch",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load runtime transcript")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&attachment.baseline.snapshot, "checkpoint retry"),
                )
                .expect("reserve typed operation"),
        );
        surface::JsonlSurfaceCommitLedger::inject_admission_checkpoint_failures(transcript_path, 2);
        assert!(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    reserved.operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .is_err(),
            "checkpoint failure must return a recoverable admission error"
        );

        let wait_client = attachment.client.clone();
        let wait_operation_id = reserved.operation_id.clone();
        let (wait_tx, wait_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = wait_tx
                .send(wait_client.wait_operation_terminal(surface_request_id(), wait_operation_id));
        });
        let terminal = wait_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("admission repair did not settle after checkpoint retries")
            .expect("terminal wait failed");
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Failed {
                    class: surface::FailureClass::Persistence,
                    ..
                })
        ));
        host.shutdown().expect("shutdown retry runtime host");
    }

    #[test]
    fn provider_response_checkpoint_failure_retries_exact_semantic_batch() {
        let cwd = tempfile::tempdir().unwrap();
        let host =
            RuntimeHost::start_with_executor(Arc::new(ProviderResponseCheckpointRetryExecutor))
                .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry provider response semantic batch",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load runtime transcript")
            .path;
        surface::JsonlSurfaceCommitLedger::inject_provider_response_checkpoint_failures(
            transcript_path,
            2,
        );
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "provider response checkpoint retry",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait semantic retry terminal");
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        host.shutdown()
            .expect("shutdown semantic retry runtime host");
    }

    #[test]
    fn typed_settings_update_commits_cas_and_survives_restart_before_next_turn() {
        let cwd = tempfile::tempdir().unwrap();
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(SettingsConfigExecutor {
            observed_model: observed_tx,
        }))
        .expect("start settings runtime host");
        let config = surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record);
        let thread = host
            .start_thread(config.clone(), "typed runtime settings")
            .expect("start settings runtime thread");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let previous_revision = attachment.baseline.snapshot.settings.thread_revision;
        let updated = committed_surface_value(
            attachment
                .client
                .update_settings(
                    surface_request_id(),
                    previous_revision,
                    surface::NonEmptyVec::try_new(vec![surface::RuntimeSettingsPatch::SetModel {
                        model: surface::NonEmptyText::try_new("deepseek-v4-pro").unwrap(),
                    }])
                    .unwrap(),
                )
                .expect("commit runtime model settings"),
        );
        assert_eq!(updated.settings.effective.model.as_str(), "deepseek-v4-pro");
        assert_eq!(
            updated.settings.thread_revision.get(),
            previous_revision.get() + 1
        );
        let stale = attachment
            .client
            .update_settings(
                surface_request_id(),
                previous_revision,
                surface::NonEmptyVec::try_new(vec![
                    surface::RuntimeSettingsPatch::SetApprovalMode {
                        mode: surface::SurfaceApprovalMode::Plan,
                    },
                ])
                .unwrap(),
            )
            .expect("stale settings response");
        assert!(matches!(stale, surface::MutationReply::Uncommitted { .. }));
        let current = fresh_surface_attachment(&surface);
        assert_eq!(
            current.baseline.snapshot.settings.effective.model.as_str(),
            "deepseek-v4-pro"
        );
        assert_eq!(
            current.baseline.snapshot.settings.effective.approval_mode,
            surface::SurfaceApprovalMode::Suggest
        );
        let reserved = committed_surface_value(
            current
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&current.baseline.snapshot, "settings turn"),
                )
                .expect("reserve settings turn"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            current
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit settings turn"),
        );
        let terminal = current
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait settings turn");
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        assert_eq!(
            observed_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            "deepseek-v4-pro"
        );
        let thread_id = thread.thread_id().to_string();
        thread.shutdown().expect("shutdown settings thread");
        host.shutdown().expect("shutdown settings host");

        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let resumed_host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start resumed settings host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config, "resume typed runtime settings")
            .expect("resume settings thread");
        let resumed = fresh_surface_attachment(&resumed_thread.surface());
        assert_eq!(
            resumed.baseline.snapshot.settings.effective.model.as_str(),
            "deepseek-v4-pro"
        );
        resumed_thread
            .shutdown()
            .expect("shutdown resumed settings thread");
        resumed_host
            .shutdown()
            .expect("shutdown resumed settings host");
    }

    #[test]
    fn typed_settings_batch_is_atomic_retried_and_recovered_into_next_turn() {
        let cwd = tempfile::tempdir().unwrap();
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(SettingsConfigSnapshotExecutor {
            observed: observed_tx,
        }))
        .expect("start settings batch runtime host");
        let config = surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record);
        let thread = host
            .start_thread(config.clone(), "typed runtime settings batch")
            .expect("start settings batch runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load settings batch transcript")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::ManageThreadSettings,
            ]),
        );
        let previous_revision = attachment.baseline.snapshot.settings.thread_revision;
        surface::JsonlSurfaceCommitLedger::inject_settings_checkpoint_failures(transcript_path, 2);
        let updated = committed_surface_value(
            attachment
                .client
                .update_settings(
                    surface_request_id(),
                    previous_revision,
                    surface::NonEmptyVec::try_new(vec![
                        surface::RuntimeSettingsPatch::SetModel {
                            model: surface::NonEmptyText::try_new("deepseek-v4-pro").unwrap(),
                        },
                        surface::RuntimeSettingsPatch::SetReasoning {
                            effort: surface::SurfaceReasoningEffort::High,
                        },
                        surface::RuntimeSettingsPatch::SetApprovalMode {
                            mode: surface::SurfaceApprovalMode::Plan,
                        },
                    ])
                    .unwrap(),
                )
                .expect("settings batch commits after exact retries"),
        );
        assert_eq!(
            updated.settings.thread_revision.get(),
            previous_revision.get() + 1
        );
        assert_eq!(updated.settings.effective.model.as_str(), "deepseek-v4-pro");
        assert_eq!(
            updated.settings.effective.reasoning_effort,
            surface::SurfaceReasoningEffort::High
        );
        assert_eq!(
            updated.settings.effective.approval_mode,
            surface::SurfaceApprovalMode::Plan
        );

        let weak = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::ControlBoundOperation,
            ]),
        );
        assert!(matches!(
            weak.client.update_settings(
                surface_request_id(),
                updated.settings.thread_revision,
                surface::NonEmptyVec::try_new(vec![surface::RuntimeSettingsPatch::SetModel {
                    model: surface::NonEmptyText::try_new("unauthorized-model").unwrap(),
                },])
                .unwrap(),
            ),
            Err(surface::SurfaceClientCommandError::Unauthorized)
        ));

        let current = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
                surface::SurfaceCapability::ManageThreadSettings,
            ]),
        );
        let reserved = committed_surface_value(
            current
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(&current.baseline.snapshot, "settings batch turn"),
                )
                .expect("reserve settings batch turn"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            current
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit settings batch turn"),
        );
        let terminal = current
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait settings batch turn");
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        assert_eq!(
            observed_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            (
                "deepseek-v4-pro".to_string(),
                ApprovalMode::Plan,
                orca_core::config::ReasoningEffort::High,
            )
        );

        let thread_id = thread.thread_id().to_string();
        thread.shutdown().expect("shutdown settings batch thread");
        host.shutdown().expect("shutdown settings batch host");
        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let (resumed_observed_tx, resumed_observed_rx) = mpsc::sync_channel(1);
        let resumed_host =
            RuntimeHost::start_with_executor(Arc::new(SettingsConfigSnapshotExecutor {
                observed: resumed_observed_tx,
            }))
            .expect("start resumed settings batch host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config, "resume typed runtime settings batch")
            .expect("resume settings batch thread");
        let resumed = fresh_surface_attachment_with_capabilities(
            &resumed_thread.surface(),
            BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
            ]),
        );
        assert_eq!(
            resumed.baseline.snapshot.settings.effective.model.as_str(),
            "deepseek-v4-pro"
        );
        assert_eq!(
            resumed.baseline.snapshot.settings.effective.approval_mode,
            surface::SurfaceApprovalMode::Plan
        );
        let resumed_reserved = committed_surface_value(
            resumed
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &resumed.baseline.snapshot,
                        "post-restart settings batch turn",
                    ),
                )
                .expect("reserve post-restart settings batch turn"),
        );
        let resumed_operation_id = resumed_reserved.operation_id.clone();
        let _ = committed_surface_value(
            resumed
                .client
                .admit_reserved(
                    surface_request_id(),
                    resumed_operation_id.clone(),
                    resumed_reserved.lease.lease_id,
                )
                .expect("admit post-restart settings batch turn"),
        );
        let resumed_terminal = resumed
            .client
            .wait_operation_terminal(surface_request_id(), resumed_operation_id)
            .expect("wait post-restart settings batch turn");
        assert!(matches!(
            resumed_terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(value.terminal, surface::OperationTerminal::Succeeded { .. })
        ));
        assert_eq!(
            resumed_observed_rx
                .recv_timeout(SURFACE_TEST_TIMEOUT)
                .unwrap(),
            (
                "deepseek-v4-pro".to_string(),
                ApprovalMode::Plan,
                orca_core::config::ReasoningEffort::High,
            )
        );
        resumed_thread
            .shutdown()
            .expect("shutdown resumed settings batch thread");
        resumed_host
            .shutdown()
            .expect("shutdown resumed settings batch host");
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
                Ok(Err(_))
            ),
            "host shutdown did not report the reservation terminal failure barrier",
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
                Ok(Err(_))
            ),
            "host shutdown did not report the terminal commit barrier",
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
    fn exact_selector_stale_revision_is_non_consuming() {
        assert_exact_selector_rejection_is_non_consuming(
            ExactSelectorCorruption::Revision,
            true,
            surface::SurfaceMutationErrorCode::StaleRevision,
        );
    }

    #[test]
    fn exact_selector_wrong_response_token_is_non_consuming() {
        assert_exact_selector_rejection_is_non_consuming(
            ExactSelectorCorruption::ResponseToken,
            false,
            surface::SurfaceMutationErrorCode::WrongResponseToken,
        );
    }

    #[test]
    fn exact_selector_stale_route_epoch_is_non_consuming() {
        assert_exact_selector_rejection_is_non_consuming(
            ExactSelectorCorruption::RouteEpoch,
            true,
            surface::SurfaceMutationErrorCode::StaleResponseRoute,
        );
    }

    #[test]
    fn exact_selector_wrong_operation_fence_is_non_consuming() {
        assert_exact_selector_rejection_is_non_consuming(
            ExactSelectorCorruption::OperationFence,
            true,
            surface::SurfaceMutationErrorCode::StaleFence,
        );
    }

    #[test]
    fn exact_selector_wrong_grant_token_is_non_consuming() {
        assert_exact_selector_rejection_is_non_consuming(
            ExactSelectorCorruption::GrantToken,
            false,
            surface::SurfaceMutationErrorCode::WrongAttachment,
        );
    }

    #[test]
    fn terminal_interactions_scrub_private_resident_state_before_waiter_wake() {
        let cwd = tempfile::tempdir().unwrap();
        let (first_answer_tx, first_answer_rx) = mpsc::sync_channel(1);
        let (second_answer_tx, second_answer_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(SequentialUserInputExecutor {
            first_answer_tx,
            second_answer_tx,
            continue_second: Mutex::new(continue_rx),
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "scrub terminal interaction secrets",
            )
            .expect("start recorded runtime thread");
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "request sequential private interactions",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );

        let first = collect_requested_surface_interaction(&mut subscription);
        let _ = committed_surface_value(
            attachment
                .client
                .respond_interaction_by_id(
                    surface_request_id(),
                    first.interaction_id,
                    surface::SurfaceClientInteractionAnswer::UserInput {
                        decision: surface::SurfaceUserInputDecision::Answer(
                            surface::DisplayText::new("first secret"),
                        ),
                    },
                )
                .expect("resolve first typed interaction"),
        );
        assert_eq!(
            first_answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("first secret".to_string())
        );
        let after_first = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe after first interaction")
            .secret_bearing_interaction_count;

        continue_tx.send(()).expect("release second interaction");
        let second = collect_requested_surface_interaction(&mut subscription);
        let _ = committed_surface_value(
            attachment
                .client
                .respond_interaction_by_id(
                    surface_request_id(),
                    second.interaction_id,
                    surface::SurfaceClientInteractionAnswer::UserInput {
                        decision: surface::SurfaceUserInputDecision::Answer(
                            surface::DisplayText::new("second secret"),
                        ),
                    },
                )
                .expect("resolve second typed interaction"),
        );
        assert_eq!(
            second_answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("second secret".to_string())
        );
        let after_second = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe after second interaction")
            .secret_bearing_interaction_count;
        let _ = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait terminal operation");
        host.shutdown().expect("shutdown runtime host");
        assert_eq!(
            after_first, 0,
            "first terminal answer remained resident after waiter wake"
        );
        assert_eq!(
            after_second, 0,
            "sequential terminal answers accumulated resident secrets"
        );
    }

    #[test]
    fn failed_private_winner_append_retries_before_capability_reroute() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry retained private winner before reroute",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.private-winner-backup");
        let surface = thread.surface();
        let origin = fresh_surface_interaction_attachment(&surface);
        let fallback = fresh_surface_interaction_attachment(&surface);
        let mut origin_subscription = surface
            .claim_subscription(&origin.subscription)
            .expect("claim origin subscription");
        let mut fallback_subscription = surface
            .claim_subscription(&fallback.subscription)
            .expect("claim fallback subscription");
        let reserved = committed_surface_value(
            origin
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &origin.baseline.snapshot,
                        "retain first private winner",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            origin
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut origin_subscription);

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        let failed = origin.client.respond_interaction_by_id(
            surface_request_id(),
            interaction.interaction_id.clone(),
            surface::SurfaceClientInteractionAnswer::UserInput {
                decision: surface::SurfaceUserInputDecision::Answer(surface::DisplayText::new(
                    "winner",
                )),
            },
        );
        assert!(matches!(
            failed,
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(answer_rx.try_recv().is_err());
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");

        drop(origin_subscription);
        let fallback_response = fallback.client.respond_interaction_by_id(
            surface_request_id(),
            interaction.interaction_id.clone(),
            surface::SurfaceClientInteractionAnswer::UserInput {
                decision: surface::SurfaceUserInputDecision::Answer(surface::DisplayText::new(
                    "fallback",
                )),
            },
        );
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut answer = None;
        let mut resolved_count = 0;
        let mut route_count = 0;
        let mut cancelled_count = 0;
        while Instant::now() < deadline && answer.is_none() {
            while let Some(item) = fallback_subscription.try_recv() {
                let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                    continue;
                };
                for event in batch.events.as_slice() {
                    match &event.event {
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Resolved { interaction_id, .. },
                        ) if interaction_id == &interaction.interaction_id => resolved_count += 1,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::RouteChanged { interaction_id, .. },
                        ) if interaction_id == &interaction.interaction_id => route_count += 1,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Cancelled { interaction_id, .. },
                        ) if interaction_id == &interaction.interaction_id => cancelled_count += 1,
                        _ => {}
                    }
                }
            }
            answer = answer_rx.try_recv().ok();
            std::thread::yield_now();
        }
        let secret_count_before_cleanup = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe retained private winner")
            .secret_bearing_interaction_count;
        if answer.is_none() {
            let _ = fallback
                .client
                .cancel_operation(surface_request_id(), operation_id.clone());
        }
        let terminal = fallback
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait terminal operation");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(answer, Some(Some("winner".to_string())));
        assert!(matches!(
            fallback_response,
            Err(surface::SurfaceClientCommandError::Unauthorized)
        ));
        assert_eq!(resolved_count, 1);
        assert_eq!(route_count, 0);
        assert_eq!(cancelled_count, 0);
        assert_eq!(secret_count_before_cleanup, 0);
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { .. }
        ));
    }

    #[test]
    fn cancel_running_drains_failed_private_winner_before_cancel_intent() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(2);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "drain retained private winner before cancellation",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.private-winner-cancel-backup");
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "retain winner before cancellation",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut subscription);

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        let failed = attachment.client.respond_interaction_by_id(
            surface_request_id(),
            interaction.interaction_id.clone(),
            surface::SurfaceClientInteractionAnswer::UserInput {
                decision: surface::SurfaceUserInputDecision::Answer(surface::DisplayText::new(
                    "winner",
                )),
            },
        );
        assert!(matches!(
            failed,
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(answer_rx.try_recv().is_err());
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");

        let cancel = attachment
            .client
            .cancel_operation(surface_request_id(), operation_id.clone())
            .expect("cancel running operation after retained winner");
        assert!(matches!(
            cancel,
            surface::MutationReply::Committed {
                value: surface::CancelOperationOutput::Accepted { .. },
                ..
            }
        ));
        assert_eq!(
            answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("winner".to_string())
        );
        assert!(answer_rx.try_recv().is_err(), "winner waiter woke twice");

        let deadline = Instant::now() + Duration::from_millis(750);
        let mut resolved_count = 0;
        let mut cancelled_count = 0;
        while Instant::now() < deadline && resolved_count == 0 {
            while let Some(item) = subscription.try_recv() {
                let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                    continue;
                };
                for event in batch.events.as_slice() {
                    match &event.event {
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Resolved { interaction_id, .. },
                        ) if interaction_id == &interaction.interaction_id => resolved_count += 1,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Cancelled { interaction_id, .. },
                        ) if interaction_id == &interaction.interaction_id => cancelled_count += 1,
                        _ => {}
                    }
                }
            }
            std::thread::yield_now();
        }
        let secret_count = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe cancelled operation")
            .secret_bearing_interaction_count;
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait cancelled operation terminal");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(resolved_count, 1);
        assert_eq!(cancelled_count, 0);
        assert_eq!(secret_count, 0);
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { .. }
        ));
    }

    #[test]
    fn cancel_running_commits_control_intent_and_interaction_cancel_atomically() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "atomically cancel pending interaction",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.atomic-cancel-backup");
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "atomically cancel this interaction",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut subscription);

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        let failed_cancel = attachment
            .client
            .cancel_operation(surface_request_id(), operation_id.clone());
        assert!(matches!(
            failed_cancel,
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(answer_rx.try_recv().is_err());
        assert_eq!(
            thread
                .surface_actor_probe_for_test(operation_id.clone())
                .expect("probe retained interaction")
                .secret_bearing_interaction_count,
            1
        );
        assert!(subscription.try_recv().is_none());
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");

        let cancel = attachment
            .client
            .cancel_operation(surface_request_id(), operation_id.clone())
            .expect("retry running cancellation");
        assert!(matches!(
            cancel,
            surface::MutationReply::Committed {
                value: surface::CancelOperationOutput::Accepted { .. },
                ..
            }
        ));
        assert_eq!(answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), None);
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut control_count = 0;
        let mut cancelled_count = 0;
        let mut atomic_batch = false;
        while Instant::now() < deadline && (control_count == 0 || cancelled_count == 0) {
            while let Some(item) = subscription.try_recv() {
                let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                    continue;
                };
                let has_control = batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::ControlIntentCommitted {
                                operation_id: candidate,
                                intent: surface::PendingControlIntent::Terminalize { .. },
                                ..
                            }
                        ) if candidate == &operation_id
                    )
                });
                let has_cancelled = batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Cancelled { interaction_id, .. }
                        ) if interaction_id == &interaction.interaction_id
                    )
                });
                control_count += usize::from(has_control);
                cancelled_count += usize::from(has_cancelled);
                atomic_batch |= has_control && has_cancelled;
            }
            std::thread::yield_now();
        }
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait cancelled operation terminal");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(control_count, 1);
        assert_eq!(cancelled_count, 1);
        assert!(
            atomic_batch,
            "cancel intent and interaction settlement split"
        );
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { .. }
        ));
    }

    #[test]
    fn cancel_checkpoint_failure_retries_exact_prepared_terminalization() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry exact prepared cancellation",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "cancel through prepared checkpoint failure",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut subscription);

        surface::JsonlSurfaceCommitLedger::inject_terminal_checkpoint_failure_once(transcript_path);
        assert!(matches!(
            attachment
                .client
                .cancel_operation(surface_request_id(), operation_id.clone()),
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(answer_rx.try_recv().is_err());
        let retained = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe retained terminalization");
        assert!(retained.interaction_admission_closed);
        assert_eq!(retained.secret_bearing_interaction_count, 1);
        let retained = retained
            .pending_terminalization
            .expect("checkpoint failure discarded prepared terminalization");

        let deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        let mut control_count = 0;
        let mut cancelled_count = 0;
        let mut exact_batch = false;
        while Instant::now() < deadline && (control_count == 0 || cancelled_count == 0) {
            while let Some(item) = subscription.try_recv() {
                let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                    continue;
                };
                let has_control = batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        surface::SurfaceEvent::Operation(
                            surface::OperationPatch::ControlIntentCommitted {
                                operation_id: candidate,
                                intent: surface::PendingControlIntent::Terminalize {
                                    cause: surface::TerminalizationCause::UserCancel,
                                    ..
                                },
                                ..
                            }
                        ) if candidate == &operation_id
                    )
                });
                let has_cancelled = batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Cancelled { interaction_id, .. }
                        ) if interaction_id == &interaction.interaction_id
                    )
                });
                if has_control || has_cancelled {
                    let surface::CommitClass::Recorded { commit_id, .. } = &batch.commit_class
                    else {
                        unreachable!("recorded runtime surface used ephemeral commit class")
                    };
                    exact_batch |= has_control
                        && has_cancelled
                        && commit_id == &retained.commit_id
                        && batch.cursor_after == retained.cursor_after
                        && batch.batch_digest == retained.batch_digest;
                }
                control_count += usize::from(has_control);
                cancelled_count += usize::from(has_cancelled);
            }
            std::thread::yield_now();
        }
        assert_eq!(answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), None);
        assert!(
            answer_rx.try_recv().is_err(),
            "terminalization waiter woke twice"
        );
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait cancelled operation terminal");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(control_count, 1);
        assert_eq!(cancelled_count, 1);
        assert!(exact_batch, "retry replaced the retained prepared batch");
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { .. }
        ));
    }

    type PreparedTerminalizationRestartFixture = (
        String,
        surface::SurfaceOperationId,
        surface::SurfaceInteractionId,
        surface::SurfaceCommitId,
        surface::SurfaceCursor,
        surface::SurfaceCursor,
        surface::Sha256Digest,
    );

    fn prepared_terminalization_restart_fixture_path() -> PathBuf {
        PathBuf::from(
            std::env::var_os("ORCA_HOME").expect("prepared terminalization restart ORCA_HOME"),
        )
        .join("runtime-host-prepared-terminalization-restart.json")
    }

    #[test]
    fn prepared_combined_terminalization_recovers_with_exact_authority_after_restart() {
        if let Some(phase) = std::env::var_os(PREPARED_TERMINALIZATION_RESTART_CHILD_ENV) {
            match phase.to_string_lossy().as_ref() {
                "failure" => run_prepared_terminalization_restart_failure_child(),
                "recovery" => run_prepared_terminalization_restart_recovery_child(),
                phase => panic!("unknown prepared terminalization restart phase: {phase}"),
            }
        }

        let home = tempfile::tempdir().unwrap();
        for phase in ["failure", "recovery"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "runtime_host::tests::prepared_combined_terminalization_recovers_with_exact_authority_after_restart",
                )
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(PREPARED_TERMINALIZATION_RESTART_CHILD_ENV, phase)
                .env("ORCA_HOME", home.path())
                .status()
                .expect("start prepared terminalization restart child");
            assert!(
                status.success(),
                "prepared terminalization restart child failed during {phase}"
            );
        }
    }

    fn run_prepared_terminalization_restart_failure_child() -> ! {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, _answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "leave prepared terminalization for restart",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "restart prepared cancellation",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut subscription);

        surface::JsonlSurfaceCommitLedger::inject_terminal_checkpoint_failure_once(transcript_path);
        if !matches!(
            attachment
                .client
                .cancel_operation(surface_request_id(), operation_id.clone()),
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        ) {
            std::process::exit(101);
        }
        let pending = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe prepared terminalization")
            .pending_terminalization
            .expect("checkpoint failure retained prepared terminalization");
        std::fs::write(
            prepared_terminalization_restart_fixture_path(),
            serde_json::to_vec(&(
                thread.thread_id().to_string(),
                operation_id,
                interaction.interaction_id,
                pending.commit_id,
                pending.cursor_before,
                pending.cursor_after,
                pending.batch_digest,
            ))
            .unwrap(),
        )
        .expect("write prepared terminalization restart fixture");
        std::process::exit(0)
    }

    fn run_prepared_terminalization_restart_recovery_child() -> ! {
        let (
            thread_id,
            operation_id,
            interaction_id,
            commit_id,
            cursor_before,
            cursor_after,
            batch_digest,
        ): PreparedTerminalizationRestartFixture = serde_json::from_slice(
            &std::fs::read(prepared_terminalization_restart_fixture_path())
                .expect("read prepared terminalization restart fixture"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor))
            .expect("start recovery runtime host");
        let thread = host
            .start_thread(
                surface_test_config(
                    cwd.path().to_path_buf(),
                    HistoryMode::Resume(thread_id.clone()),
                ),
                "recover prepared terminalization",
            )
            .expect("resume prepared terminalization");
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
        let interaction = attachment
            .baseline
            .snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .expect("recovered interaction remains visible");
        assert!(matches!(operation.phase, surface::OperationPhase::Terminal));
        assert!(matches!(
            interaction.lifecycle,
            surface::SurfaceInteractionLifecycle::Cancelled {
                reason: surface::InteractionCancelReason::OperationCancelled {
                    reason: surface::CancelReason::User,
                },
            }
        ));

        let transcript_path = SessionStore::new()
            .load_session(&thread_id)
            .expect("load recovered runtime thread")
            .path;
        let ledger = surface::JsonlSurfaceCommitLedger::new(transcript_path, cursor_before.clone());
        let recovered = ledger
            .recover_batches()
            .expect("read recovered surface batches");
        assert!(recovered.prepared.is_none());
        let exact = recovered
            .committed
            .iter()
            .filter(|batch| {
                matches!(
                    &batch.commit_class,
                    surface::CommitClass::Recorded { commit_id: candidate, .. }
                        if candidate == &commit_id
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].cursor_before, cursor_before);
        assert_eq!(exact[0].cursor_after, cursor_after);
        assert_eq!(exact[0].batch_digest, batch_digest);
        let terminalization_count = recovered
            .committed
            .iter()
            .flat_map(|batch| batch.events.as_slice())
            .filter(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::ControlIntentCommitted {
                            operation_id: candidate,
                            intent: surface::PendingControlIntent::Terminalize { .. },
                            ..
                        }
                    ) if candidate == &operation_id
                )
            })
            .count();
        assert_eq!(terminalization_count, 1);
        host.shutdown().expect("shutdown recovered runtime host");
        std::process::exit(0)
    }

    #[test]
    fn unavailable_responder_is_one_atomic_batch_and_append_failure_is_invisible() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "atomically settle unavailable responder",
            )
            .expect("start recorded runtime thread");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim observer subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "request unavailable input atomically",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait failed operation terminal");
        assert_eq!(answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), None);
        let mut interaction_id = None;
        let mut requested_count = 0;
        let mut cancelled_count = 0;
        let mut atomic_batch = false;
        while let Some(item) = subscription.try_recv() {
            let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            let requested = batch
                .events
                .as_slice()
                .iter()
                .find_map(|event| match &event.event {
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Requested {
                        interaction,
                    }) => Some(interaction.interaction_id.clone()),
                    _ => None,
                });
            let cancelled = batch
                .events
                .as_slice()
                .iter()
                .find_map(|event| match &event.event {
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                        interaction_id,
                        reason: surface::InteractionCancelReason::CapabilityUnavailable,
                        ..
                    }) => Some(interaction_id.clone()),
                    _ => None,
                });
            requested_count += usize::from(requested.is_some());
            cancelled_count += usize::from(cancelled.is_some());
            if requested.is_some() {
                interaction_id = requested.clone();
            }
            atomic_batch |= requested.is_some() && requested == cancelled;
        }
        let snapshot = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
        )
        .baseline
        .snapshot;
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(requested_count, 1);
        assert_eq!(cancelled_count, 1);
        assert!(
            atomic_batch,
            "unavailable responder settlement split batches"
        );
        assert!(snapshot.interactions.iter().any(|interaction| {
            Some(&interaction.interaction_id) == interaction_id.as_ref()
                && matches!(
                    interaction.lifecycle,
                    surface::SurfaceInteractionLifecycle::Cancelled {
                        reason: surface::InteractionCancelReason::CapabilityUnavailable,
                    }
                )
        }));
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { value }
                if matches!(
                    value.terminal,
                    surface::OperationTerminal::Failed {
                        class: surface::FailureClass::ClientCapabilityUnavailable,
                        ..
                    }
                )
        ));

        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start failure runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "hide failed unavailable responder batch",
            )
            .expect("start failure runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim failure observer subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "fail unavailable interaction append",
                    ),
                )
                .expect("reserve failure operation"),
        );
        let operation_id = reserved.operation_id.clone();
        surface::JsonlSurfaceCommitLedger::inject_interaction_request_append_failure_once(
            transcript_path,
        );
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit failure operation"),
        );
        let _ = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait append-failed operation terminal");
        assert!(answer_rx.try_recv().is_err());
        let interaction_event_count = std::iter::from_fn(|| subscription.try_recv())
            .filter_map(|item| match item {
                surface::SurfaceSubscriptionItem::Batch { batch } => Some(batch),
                surface::SurfaceSubscriptionItem::Gap { .. }
                | surface::SurfaceSubscriptionItem::Sealed { .. } => None,
            })
            .flat_map(|batch| batch.events.as_slice().to_vec())
            .filter(|event| matches!(event.event, surface::SurfaceEvent::Interaction(_)))
            .count();
        let snapshot = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
        )
        .baseline
        .snapshot;
        host.shutdown().expect("shutdown failure runtime host");
        assert_eq!(interaction_event_count, 0);
        assert!(snapshot.interactions.is_empty());
    }

    #[test]
    fn accepted_cancel_rejects_interaction_requested_after_private_winner_wakes() {
        let cwd = tempfile::tempdir().unwrap();
        let (first_answer_tx, first_answer_rx) = mpsc::sync_channel(1);
        let (second_result_tx, second_result_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ImmediateSecondUserInputExecutor {
            first_answer_tx,
            second_result_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "reject interaction after cancellation begins",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.cancel-admission-backup");
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "cancel after the first winner",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let first = collect_requested_surface_interaction(&mut subscription);

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        assert!(matches!(
            attachment.client.respond_interaction_by_id(
                surface_request_id(),
                first.interaction_id.clone(),
                surface::SurfaceClientInteractionAnswer::UserInput {
                    decision: surface::SurfaceUserInputDecision::Answer(surface::DisplayText::new(
                        "winner"
                    ),),
                },
            ),
            Err(surface::SurfaceClientCommandError::RuntimeUnavailable)
        ));
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");

        let cancel = attachment
            .client
            .cancel_operation(surface_request_id(), operation_id.clone())
            .expect("cancel after retained winner");
        assert!(matches!(
            cancel,
            surface::MutationReply::Committed {
                value: surface::CancelOperationOutput::Accepted { .. },
                ..
            }
        ));
        assert_eq!(
            first_answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("winner".to_string())
        );

        let deadline = Instant::now() + Duration::from_millis(750);
        let mut second_requested = None;
        let mut first_resolved_count = 0;
        let mut second_result = None;
        while Instant::now() < deadline && second_result.is_none() {
            while let Some(item) = subscription.try_recv() {
                let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                    continue;
                };
                for event in batch.events.as_slice() {
                    match &event.event {
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Resolved { interaction_id, .. },
                        ) if interaction_id == &first.interaction_id => first_resolved_count += 1,
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Requested { interaction },
                        ) if interaction.interaction_id != first.interaction_id => {
                            second_requested = Some(interaction.interaction_id.clone())
                        }
                        _ => {}
                    }
                }
            }
            second_result = second_result_rx.try_recv().ok();
            if second_result.is_none() {
                std::thread::yield_now();
            }
        }
        if let Some(interaction_id) = second_requested.as_ref() {
            let _ = attachment.client.respond_interaction_by_id(
                surface_request_id(),
                interaction_id.clone(),
                surface::SurfaceClientInteractionAnswer::UserInput {
                    decision: surface::SurfaceUserInputDecision::Cancel,
                },
            );
            second_result =
                second_result.or_else(|| second_result_rx.recv_timeout(SURFACE_TEST_TIMEOUT).ok());
        }
        let terminal = attachment
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait cancelled operation terminal");
        host.shutdown().expect("shutdown runtime host");

        assert_eq!(first_resolved_count, 1);
        assert!(
            second_requested.is_none(),
            "second interaction was admitted"
        );
        assert!(matches!(second_result, Some(Err(_))));
        assert!(matches!(
            terminal,
            surface::WaitOperationTerminalResult::Terminal { .. }
        ));
    }

    #[test]
    fn thread_close_append_failure_restores_gate_and_retries_without_joining_waiter() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry thread close after atomic append failure",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.thread-close-atomic-backup");
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "close with a pending interaction",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut subscription);
        let completion = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe active operation")
            .legacy_completion
            .expect("typed operation retains legacy completion");

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        let started = Instant::now();
        assert!(thread.shutdown().is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "failed close waited for the blocked generation"
        );
        assert!(answer_rx.try_recv().is_err());
        assert!(completion.try_terminal().is_none());
        assert_eq!(
            thread
                .surface_actor_probe_for_test(operation_id.clone())
                .expect("probe restored active operation")
                .secret_bearing_interaction_count,
            1
        );
        assert!(subscription.try_recv().is_none());
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");

        thread.shutdown().expect("retry thread close");
        assert_eq!(answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), None);
        assert!(completion.wait_timeout(SURFACE_TEST_TIMEOUT).is_some());
        let mut atomic_batch = false;
        while let Some(item) = subscription.try_recv() {
            let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            let has_close = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::ControlIntentCommitted {
                            intent: surface::PendingControlIntent::Terminalize {
                                cause: surface::TerminalizationCause::ThreadClose,
                                ..
                            },
                            ..
                        }
                    )
                )
            });
            let has_cancelled = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Interaction(
                        surface::InteractionPatch::Cancelled { interaction_id, .. }
                    ) if interaction_id == &interaction.interaction_id
                )
            });
            atomic_batch |= has_close && has_cancelled;
        }
        host.shutdown().expect("shutdown runtime host");
        assert!(atomic_batch);
    }

    #[test]
    fn host_shutdown_acknowledges_after_prepared_terminalization_retry() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "ack host shutdown after prepared retry",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let attachment = fresh_surface_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim interaction subscription");
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "shutdown with a pending interaction",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut subscription);
        let completion = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe active operation")
            .legacy_completion
            .expect("typed operation retains legacy completion");

        surface::JsonlSurfaceCommitLedger::inject_terminal_checkpoint_failure_once(transcript_path);
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        let shutdown = shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("host shutdown hung after one prepared checkpoint failure");
        assert!(shutdown.is_ok(), "host shutdown returned {shutdown:?}");
        assert_eq!(answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(), None);
        assert!(answer_rx.try_recv().is_err(), "shutdown waiter woke twice");
        assert!(completion.wait_timeout(SURFACE_TEST_TIMEOUT).is_some());

        let mut control_count = 0;
        let mut cancelled_count = 0;
        let mut atomic_batch = false;
        while let Some(item) = subscription.try_recv() {
            let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            let has_control = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::ControlIntentCommitted {
                            operation_id: candidate,
                            intent: surface::PendingControlIntent::Terminalize {
                                cause: surface::TerminalizationCause::HostShutdown,
                                ..
                            },
                            ..
                        }
                    ) if candidate == &operation_id
                )
            });
            let has_cancelled = batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Interaction(
                        surface::InteractionPatch::Cancelled {
                            interaction_id,
                            reason: surface::InteractionCancelReason::HostShutdown,
                            ..
                        }
                    ) if interaction_id == &interaction.interaction_id
                )
            });
            control_count += usize::from(has_control);
            cancelled_count += usize::from(has_cancelled);
            atomic_batch |= has_control && has_cancelled;
        }
        assert_eq!(control_count, 1);
        assert_eq!(cancelled_count, 1);
        assert!(atomic_batch);
    }

    #[test]
    fn host_shutdown_retries_prepared_actor_without_blocking_later_actor_cleanup() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let (completed_tx, completed_rx) = mpsc::sync_channel(2);
        let host = RuntimeHost::start_with_executor(Arc::new(RoundRobinShutdownExecutor {
            entered: entered_tx,
            completed: completed_tx,
        }))
        .expect("start runtime host");
        let mut threads = vec![
            host.start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "first shutdown actor",
            )
            .expect("start first runtime thread"),
            host.start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "second shutdown actor",
            )
            .expect("start second runtime thread"),
        ];
        threads.sort_by_key(|thread| thread.thread_id().to_string());
        let retrying = threads.remove(0);
        let later = threads.remove(0);

        for (thread, prompt) in [(&retrying, "retrying-actor"), (&later, "later-actor")] {
            let attachment = fresh_surface_attachment(&thread.surface());
            let reserved = committed_surface_value(
                attachment
                    .client
                    .reserve_operation(
                        surface_request_id(),
                        surface_user_turn_intent(&attachment.baseline.snapshot, prompt),
                    )
                    .expect("reserve typed operation"),
            );
            let _ = committed_surface_value(
                attachment
                    .client
                    .admit_reserved(
                        surface_request_id(),
                        reserved.operation_id,
                        reserved.lease.lease_id,
                    )
                    .expect("admit typed operation"),
            );
        }
        let entered = [
            entered_rx
                .recv_timeout(SURFACE_TEST_TIMEOUT)
                .expect("first executor did not start"),
            entered_rx
                .recv_timeout(SURFACE_TEST_TIMEOUT)
                .expect("second executor did not start"),
        ];
        assert!(entered.contains(&"retrying-actor".to_string()));
        assert!(entered.contains(&"later-actor".to_string()));

        let transcript_path = SessionStore::new()
            .load_session(retrying.thread_id())
            .expect("load retrying runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.shutdown-fairness-backup");
        surface::JsonlSurfaceCommitLedger::inject_terminal_checkpoint_failure_once(
            transcript_path.clone(),
        );
        let (initial_shutdown_tx, initial_shutdown_rx) = mpsc::sync_channel(1);
        send_thread_shutdown(
            &retrying.command_tx,
            ThreadCommand::ShutdownThread {
                reply: Some(initial_shutdown_tx),
                reason: surface::SurfaceShutdownReason::HostShutdown,
            },
        )
        .expect("request initial retrying shutdown");
        assert!(matches!(
            initial_shutdown_rx
                .recv_timeout(SURFACE_TEST_TIMEOUT)
                .expect("retrying actor did not acknowledge prepared failure"),
            ThreadShutdownAck::Retry
        ));
        std::fs::rename(&transcript_path, &backup_path).expect("hide retrying ledger");
        std::fs::create_dir(&transcript_path).expect("replace retrying ledger with directory");

        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        let later_completed_before_recovery = completed_rx.recv_timeout(Duration::from_millis(500));
        let later_closed_deadline = Instant::now() + Duration::from_millis(500);
        while later_completed_before_recovery.is_ok()
            && !later.command_tx.is_closed()
            && Instant::now() < later_closed_deadline
        {
            std::thread::yield_now();
        }
        let later_closed_before_recovery = later.command_tx.is_closed();

        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore retrying ledger");
        let shutdown = shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("host shutdown did not finish after retry recovery");
        assert!(shutdown.is_ok(), "host shutdown returned {shutdown:?}");
        assert_eq!(
            later_completed_before_recovery.expect("later actor was starved by retrying actor"),
            "later-actor"
        );
        assert!(
            later_closed_before_recovery,
            "later actor was not joined before retrying actor recovered"
        );
    }

    #[test]
    fn host_shutdown_dispatches_to_later_actor_before_blocked_actor_ack() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(2);
        let (shutdown_observed_tx, shutdown_observed_rx) = mpsc::sync_channel(2);
        let (completed_tx, completed_rx) = mpsc::sync_channel(2);
        let (blocked_release_tx, blocked_release_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(DispatchFairShutdownExecutor {
            entered: entered_tx,
            shutdown_observed: shutdown_observed_tx,
            completed: completed_tx,
            blocked_release: Mutex::new(blocked_release_rx),
        }))
        .expect("start runtime host");
        let mut threads = vec![
            host.start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "first shutdown actor",
            )
            .expect("start first runtime thread"),
            host.start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "second shutdown actor",
            )
            .expect("start second runtime thread"),
        ];
        threads.sort_by_key(|thread| thread.thread_id().to_string());
        let blocked = threads.remove(0);
        let later = threads.remove(0);

        for (thread, prompt) in [(&blocked, "blocked-actor"), (&later, "later-actor")] {
            let attachment = fresh_surface_attachment(&thread.surface());
            let reserved = committed_surface_value(
                attachment
                    .client
                    .reserve_operation(
                        surface_request_id(),
                        surface_user_turn_intent(&attachment.baseline.snapshot, prompt),
                    )
                    .expect("reserve typed operation"),
            );
            let _ = committed_surface_value(
                attachment
                    .client
                    .admit_reserved(
                        surface_request_id(),
                        reserved.operation_id,
                        reserved.lease.lease_id,
                    )
                    .expect("admit typed operation"),
            );
        }
        let entered = [
            entered_rx
                .recv_timeout(SURFACE_TEST_TIMEOUT)
                .expect("first executor did not start"),
            entered_rx
                .recv_timeout(SURFACE_TEST_TIMEOUT)
                .expect("second executor did not start"),
        ];
        assert!(entered.contains(&"blocked-actor".to_string()));
        assert!(entered.contains(&"later-actor".to_string()));

        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        let first_shutdown_observed = shutdown_observed_rx.recv_timeout(SURFACE_TEST_TIMEOUT);
        let second_shutdown_observed =
            shutdown_observed_rx.recv_timeout(Duration::from_millis(500));
        let later_completed = completed_rx.recv_timeout(Duration::from_millis(500));
        let later_closed_deadline = Instant::now() + Duration::from_millis(500);
        while !later.command_tx.is_closed() && Instant::now() < later_closed_deadline {
            std::thread::yield_now();
        }
        let later_closed = later.command_tx.is_closed();
        let host_waited_for_blocked_actor = shutdown_rx.try_recv().is_err();

        blocked_release_tx
            .send(())
            .expect("release blocked shutdown actor");
        let shutdown = shutdown_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("host shutdown did not finish after releasing blocked actor");

        let shutdown_observed = [
            first_shutdown_observed.expect("no actor received shutdown"),
            second_shutdown_observed
                .expect("later actor was starved before blocked actor acknowledged"),
        ];
        assert!(shutdown_observed.contains(&"blocked-actor".to_string()));
        assert!(shutdown_observed.contains(&"later-actor".to_string()));
        assert_eq!(
            later_completed
                .expect("later actor did not complete before blocked actor acknowledged"),
            "later-actor"
        );
        assert!(
            later_closed,
            "later actor command receiver stayed open before blocked actor acknowledged"
        );
        assert!(
            host_waited_for_blocked_actor,
            "host shutdown returned before blocked actor acknowledged"
        );
        assert!(shutdown.is_ok(), "host shutdown returned {shutdown:?}");
    }

    #[test]
    fn host_shutdown_preprepare_failure_cancels_and_joins_generation_before_returning_error() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (cancel_observed_tx, cancel_observed_rx) = mpsc::sync_channel(1);
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(CancelAwareShutdownExecutor {
            entered: entered_tx,
            cancel_observed: cancel_observed_tx,
            completed: completed_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "fail host shutdown before prepare",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.host-shutdown-backup");
        let surface = thread.surface();
        let attachment = fresh_surface_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim operation subscription");
        let initial_cursor = attachment.baseline.cursor.clone();
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "cancel generation after failed host shutdown prepare",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("executor did not start");
        let completion = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe active operation")
            .legacy_completion
            .expect("typed operation retains legacy completion");

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        let shutdown = shutdown_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("host shutdown hung after pre-prepare failure");
        assert!(shutdown.is_err());
        cancel_observed_rx
            .try_recv()
            .expect("shutdown returned before the generation observed cancellation");
        completed_rx
            .try_recv()
            .expect("shutdown returned before the blocking generation completed");
        assert!(completion.try_terminal().is_none());
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");

        while let Some(item) = subscription.try_recv() {
            let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            assert!(!batch.events.as_slice().iter().any(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal { record })
                        if record.operation_id == operation_id
                )
            }));
        }
        let recovered = surface::JsonlSurfaceCommitLedger::new(transcript_path, initial_cursor)
            .recover_batches()
            .expect("recover surface batches after failed shutdown");
        assert!(!recovered
            .committed
            .iter()
            .flat_map(|batch| batch.events.as_slice())
            .any(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Operation(surface::OperationPatch::Terminal { record })
                        if record.operation_id == operation_id
                )
            }));
    }

    #[test]
    fn host_shutdown_preprepare_failure_rejects_buffered_interaction_before_join() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (interaction_result_tx, interaction_result_rx) = mpsc::sync_channel(1);
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(QueuedInteractionShutdownExecutor {
            entered: entered_tx,
            release_interaction: Mutex::new(release_rx),
            interaction_result: interaction_result_tx,
            completed: completed_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "reject buffered interaction during failed shutdown",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let backup_path = transcript_path.with_extension("jsonl.host-shutdown-buffer-backup");
        let attachment = fresh_surface_attachment(&thread.surface());
        let reserved = committed_surface_value(
            attachment
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &attachment.baseline.snapshot,
                        "queue interaction behind host shutdown",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            attachment
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("executor did not start");

        let (barrier_tx, barrier_rx) = mpsc::sync_channel(0);
        thread
            .command_tx
            .try_send(ThreadCommand::SurfaceActorTestProbe {
                operation_id,
                reply: barrier_tx,
            })
            .expect("enqueue actor barrier");
        let barrier_deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        while thread.command_tx.capacity() != THREAD_COMMAND_CAPACITY {
            assert!(
                Instant::now() < barrier_deadline,
                "actor did not enter barrier"
            );
            std::thread::yield_now();
        }

        std::fs::rename(&transcript_path, &backup_path).expect("hide surface ledger");
        std::fs::create_dir(&transcript_path).expect("replace ledger with directory");
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = shutdown_tx.send(host.shutdown());
        });
        let shutdown_queued_deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        while thread.command_tx.capacity() != THREAD_COMMAND_CAPACITY - 1 {
            assert!(
                Instant::now() < shutdown_queued_deadline,
                "host shutdown was not queued behind actor barrier"
            );
            std::thread::yield_now();
        }
        release_tx.send(()).expect("release second interaction");
        let interaction_queued_deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        while thread.command_tx.capacity() != THREAD_COMMAND_CAPACITY - 2 {
            assert!(
                Instant::now() < interaction_queued_deadline,
                "interaction was not queued behind host shutdown"
            );
            std::thread::yield_now();
        }
        let _ = barrier_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("release actor barrier");

        let shutdown = shutdown_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("host shutdown hung on buffered interaction");
        assert!(shutdown.is_err());
        let interaction_error = interaction_result_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("buffered interaction did not receive rejection")
            .expect_err("buffered interaction was accepted during shutdown");
        assert_eq!(interaction_error.kind(), io::ErrorKind::NotConnected);
        completed_rx
            .try_recv()
            .expect("shutdown returned before queued-interaction worker completed");
        std::fs::remove_dir(&transcript_path).expect("remove blocking ledger directory");
        std::fs::rename(&backup_path, &transcript_path).expect("restore surface ledger");
    }

    #[test]
    fn slow_exclusive_responder_rotates_route_before_fallback_can_wake_waiter() {
        let cwd = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor_and_surface_config(
            Arc::new(SlowSubscriberUserInputExecutor {
                entered: entered_tx,
                release: Mutex::new(release_rx),
                answer_tx,
            }),
            surface::SurfaceHubConfig {
                subscriber_event_limit: 7,
                ..surface::SurfaceHubConfig::default()
            },
        )
        .expect("start runtime host with bounded subscriber lane");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "reroute slow interaction responder",
            )
            .expect("start recorded runtime thread");
        let surface = thread.surface();
        let origin = fresh_surface_interaction_attachment(&surface);
        let mut origin_subscription = surface
            .claim_subscription(&origin.subscription)
            .expect("claim origin subscription");
        let reserved = committed_surface_value(
            origin
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &origin.baseline.snapshot,
                        "overflow origin interaction lane",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            origin
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        entered_rx
            .recv_timeout(SURFACE_TEST_TIMEOUT)
            .expect("executor did not reach request gate");

        let fallback = fresh_surface_interaction_attachment(&surface);
        let mut fallback_subscription = surface
            .claim_subscription(&fallback.subscription)
            .expect("claim fallback subscription");
        release_tx.send(()).expect("release user-input request");
        let interaction = collect_requested_surface_interaction(&mut fallback_subscription);
        assert!(
            matches!(
                interaction.route,
                surface::SurfaceInteractionRoute::Exclusive {
                    ref attachment_id,
                    ..
                } if attachment_id == &origin.attachment_id
            ),
            "origin retired before request: queued durable events={}, initial route={:?}",
            fallback
                .baseline
                .cursor
                .next_seq
                .get()
                .saturating_sub(origin.baseline.cursor.next_seq.get()),
            interaction.route
        );
        let route = collect_surface_interaction_route(
            &mut fallback_subscription,
            &interaction.interaction_id,
        );
        assert!(matches!(
            route,
            surface::SurfaceInteractionRoute::Exclusive {
                ref attachment_id,
                epoch,
            } if attachment_id == &fallback.attachment_id
                && epoch == surface::ResponseRouteEpoch::try_new(2).unwrap()
        ));
        assert!(answer_rx.try_recv().is_err());
        let _ = committed_surface_value(
            fallback
                .client
                .respond_interaction_by_id(
                    surface_request_id(),
                    interaction.interaction_id.clone(),
                    surface::SurfaceClientInteractionAnswer::UserInput {
                        decision: surface::SurfaceUserInputDecision::Answer(
                            surface::DisplayText::new("fallback"),
                        ),
                    },
                )
                .expect("fallback response after slow-lane reroute"),
        );
        assert_eq!(
            answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("fallback".to_string())
        );
        let saw_slow_gap = std::iter::from_fn(|| origin_subscription.try_recv()).any(|item| {
            matches!(
                item,
                surface::SurfaceSubscriptionItem::Gap {
                    required: surface::SnapshotRequired {
                        reason: surface::SnapshotRequiredReason::SlowSubscriber,
                        ..
                    },
                }
            )
        });
        assert!(
            saw_slow_gap,
            "origin lane did not retire as a slow subscriber"
        );
        let _ = fallback
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait terminal operation");
        host.shutdown().expect("shutdown runtime host");
    }

    #[test]
    fn capability_loss_append_failure_retries_under_sustained_command_traffic() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry capability-loss route commit",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let origin = fresh_surface_interaction_attachment(&surface);
        let fallback = fresh_surface_interaction_attachment(&surface);
        let mut origin_subscription = surface
            .claim_subscription(&origin.subscription)
            .expect("claim origin subscription");
        let mut fallback_subscription = surface
            .claim_subscription(&fallback.subscription)
            .expect("claim fallback subscription");
        let reserved = committed_surface_value(
            origin
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &origin.baseline.snapshot,
                        "retry capability loss internally",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            origin
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut origin_subscription);
        surface::JsonlSurfaceCommitLedger::inject_interaction_route_append_failure_once(
            transcript_path,
        );
        drop(origin_subscription);

        let pending = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe retained capability-loss transition")
            .pending_capability_loss;
        let traffic_deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < traffic_deadline {
            let (reply_tx, _reply_rx) = mpsc::sync_channel(1);
            match thread
                .command_tx
                .try_send(ThreadCommand::SurfaceActorTestProbe {
                    operation_id: operation_id.clone(),
                    reply: reply_tx,
                }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => panic!("runtime command mailbox closed"),
            }
        }
        let probe_deadline = Instant::now() + SURFACE_TEST_TIMEOUT;
        let after_traffic = loop {
            match thread.surface_actor_probe_for_test(operation_id.clone()) {
                Ok(probe) => break probe,
                Err(RuntimeHostError::MailboxFull { .. }) if Instant::now() < probe_deadline => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("probe after sustained command traffic failed: {error}"),
            }
        };
        assert!(
            after_traffic.pending_capability_loss.is_none(),
            "expired capability-loss retry was starved by command traffic"
        );
        let automatic_deadline = Instant::now() + Duration::from_millis(750);
        let mut automatic_route = None;
        let mut automatic_commit = None;
        while Instant::now() < automatic_deadline && automatic_route.is_none() {
            while let Some(item) = fallback_subscription.try_recv() {
                if let surface::SurfaceSubscriptionItem::Batch { batch } = item {
                    for event in batch.events.as_slice() {
                        if let surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::RouteChanged {
                                interaction_id,
                                route,
                                ..
                            },
                        ) = &event.event
                            && interaction_id == &interaction.interaction_id
                        {
                            automatic_route = Some(route.clone());
                            automatic_commit = Some((
                                batch.commit_class.clone(),
                                batch.cursor_after.clone(),
                                batch.batch_digest.clone(),
                            ));
                        }
                    }
                }
            }
            std::thread::yield_now();
        }
        if automatic_route.is_none() {
            let _ = surface.detach(
                &origin.client,
                surface::DetachRequest {
                    request_id: surface_request_id(),
                },
            );
            let _ = collect_surface_interaction_route(
                &mut fallback_subscription,
                &interaction.interaction_id,
            );
        }

        for _ in 0..32 {
            let churn = fresh_surface_interaction_attachment(&surface);
            let churn_subscription = surface
                .claim_subscription(&churn.subscription)
                .expect("claim churn subscription");
            drop(churn_subscription);
        }
        let _ = committed_surface_value(
            fallback
                .client
                .respond_interaction_by_id(
                    surface_request_id(),
                    interaction.interaction_id.clone(),
                    surface::SurfaceClientInteractionAnswer::UserInput {
                        decision: surface::SurfaceUserInputDecision::Answer(
                            surface::DisplayText::new("fallback"),
                        ),
                    },
                )
                .expect("respond after capability-loss retry"),
        );
        assert_eq!(
            answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("fallback".to_string())
        );
        let duplicate_route_commits = std::iter::from_fn(|| fallback_subscription.try_recv())
            .filter_map(|item| match item {
                surface::SurfaceSubscriptionItem::Batch { batch } => Some(batch),
                surface::SurfaceSubscriptionItem::Gap { .. }
                | surface::SurfaceSubscriptionItem::Sealed { .. } => None,
            })
            .map(|batch| {
                batch
                    .events
                    .as_slice()
                    .iter()
                    .filter(|event| {
                        matches!(
                            &event.event,
                            surface::SurfaceEvent::Interaction(
                                surface::InteractionPatch::RouteChanged {
                                    interaction_id,
                                    ..
                                }
                            ) if interaction_id == &interaction.interaction_id
                        )
                    })
                    .count()
            })
            .sum::<usize>();
        let _ = fallback
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait terminal operation");
        host.shutdown().expect("shutdown runtime host");

        assert!(
            matches!(
                automatic_route,
                Some(surface::SurfaceInteractionRoute::Exclusive {
                    ref attachment_id,
                    epoch,
                }) if attachment_id == &fallback.attachment_id
                    && epoch == surface::ResponseRouteEpoch::try_new(2).unwrap()
            ),
            "capability-loss retry required an external command"
        );
        let pending = pending.expect("failed capability-loss transition was not retained");
        assert_eq!(pending.attachment_id, origin.attachment_id);
        let (commit_class, cursor_after, batch_digest) =
            automatic_commit.expect("automatic route commit identity was not observed");
        assert!(matches!(
            commit_class,
            surface::CommitClass::Recorded { commit_id, .. } if commit_id == pending.commit_id
        ));
        assert_eq!(cursor_after, pending.cursor_after);
        assert_eq!(batch_digest, pending.batch_digest);
        assert_eq!(
            duplicate_route_commits, 0,
            "duplicate loss hints committed another route transition"
        );
    }

    #[test]
    fn capability_change_wake_is_bounded_outside_full_command_mailbox() {
        let (command_tx, mut command_rx) = tokio_mpsc::channel(1);
        let (capability_change_tx, mut capability_change_rx) = tokio_mpsc::channel(1);
        let dispatcher = ThreadSurfaceDispatcher {
            command_tx: command_tx.clone(),
            capability_change_tx,
        };
        let (reply_tx, _reply_rx) = mpsc::sync_channel(1);
        command_tx
            .blocking_send(ThreadCommand::SurfaceActorTestProbe {
                operation_id: surface::SurfaceOperationId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7"),
                reply: reply_tx,
            })
            .expect("fill runtime command mailbox");

        for _ in 0..1_024 {
            surface::RuntimeSurfaceCommandDispatcher::notify_interaction_capability_changed(
                &dispatcher,
            );
        }

        assert!(matches!(
            command_rx.try_recv(),
            Ok(ThreadCommand::SurfaceActorTestProbe { .. })
        ));
        assert!(command_rx.try_recv().is_err());
        assert_eq!(capability_change_rx.try_recv(), Ok(()));
        assert!(capability_change_rx.try_recv().is_err());
    }

    #[test]
    fn distinct_capability_loss_is_reconciled_after_retained_retry() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "reconcile queued capability losses",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let origin = fresh_surface_interaction_attachment(&surface);
        let fallback = fresh_surface_interaction_attachment(&surface);
        let observer = fresh_surface_attachment_with_capabilities(
            &surface,
            BTreeSet::from([surface::SurfaceCapability::ReadSnapshot]),
        );
        let mut origin_subscription = surface
            .claim_subscription(&origin.subscription)
            .expect("claim origin subscription");
        let fallback_subscription = surface
            .claim_subscription(&fallback.subscription)
            .expect("claim fallback subscription");
        let mut observer_subscription = surface
            .claim_subscription(&observer.subscription)
            .expect("claim observer subscription");
        let reserved = committed_surface_value(
            origin
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &origin.baseline.snapshot,
                        "reconcile both lost responders",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            origin
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut origin_subscription);
        surface::JsonlSurfaceCommitLedger::inject_interaction_route_append_failure_once(
            transcript_path,
        );
        drop(origin_subscription);

        let pending = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("probe retained origin capability-loss transition")
            .pending_capability_loss
            .expect("failed origin capability-loss transition was not retained");
        assert_eq!(pending.attachment_id, origin.attachment_id);
        drop(fallback_subscription);

        let automatic_deadline = Instant::now() + Duration::from_millis(750);
        let mut automatic_answer = None;
        let mut origin_retry_commit = None;
        let mut route_change_count = 0;
        let mut cancellation_count = 0;
        while Instant::now() < automatic_deadline && automatic_answer.is_none() {
            while let Some(item) = observer_subscription.try_recv() {
                let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                    continue;
                };
                for event in batch.events.as_slice() {
                    match &event.event {
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::RouteChanged {
                                interaction_id,
                                route,
                                ..
                            },
                        ) if interaction_id == &interaction.interaction_id => {
                            route_change_count += 1;
                            if matches!(
                                route,
                                surface::SurfaceInteractionRoute::Exclusive {
                                    attachment_id,
                                    epoch,
                                } if attachment_id == &fallback.attachment_id
                                    && *epoch == surface::ResponseRouteEpoch::try_new(2).unwrap()
                            ) {
                                origin_retry_commit = Some((
                                    batch.commit_class.clone(),
                                    batch.cursor_after.clone(),
                                    batch.batch_digest.clone(),
                                ));
                            }
                        }
                        surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::Cancelled {
                                interaction_id,
                                reason: surface::InteractionCancelReason::CapabilityUnavailable,
                                ..
                            },
                        ) if interaction_id == &interaction.interaction_id => {
                            cancellation_count += 1;
                        }
                        _ => {}
                    }
                }
            }
            automatic_answer = answer_rx.try_recv().ok();
            std::thread::yield_now();
        }
        let reconciled_without_external_hint = automatic_answer.is_some();
        if automatic_answer.is_none() {
            let _ = surface.detach(
                &fallback.client,
                surface::DetachRequest {
                    request_id: surface_request_id(),
                },
            );
            automatic_answer = Some(
                answer_rx
                    .recv_timeout(SURFACE_TEST_TIMEOUT)
                    .expect("cleanup capability-loss hint did not wake waiter"),
            );
        }
        while let Some(item) = observer_subscription.try_recv() {
            let surface::SurfaceSubscriptionItem::Batch { batch } = item else {
                continue;
            };
            for event in batch.events.as_slice() {
                match &event.event {
                    surface::SurfaceEvent::Interaction(
                        surface::InteractionPatch::RouteChanged {
                            interaction_id,
                            route,
                            ..
                        },
                    ) if interaction_id == &interaction.interaction_id => {
                        route_change_count += 1;
                        if matches!(
                            route,
                            surface::SurfaceInteractionRoute::Exclusive {
                                attachment_id,
                                epoch,
                            } if attachment_id == &fallback.attachment_id
                                && *epoch == surface::ResponseRouteEpoch::try_new(2).unwrap()
                        ) {
                            origin_retry_commit = Some((
                                batch.commit_class.clone(),
                                batch.cursor_after.clone(),
                                batch.batch_digest.clone(),
                            ));
                        }
                    }
                    surface::SurfaceEvent::Interaction(surface::InteractionPatch::Cancelled {
                        interaction_id,
                        reason: surface::InteractionCancelReason::CapabilityUnavailable,
                        ..
                    }) if interaction_id == &interaction.interaction_id => {
                        cancellation_count += 1;
                    }
                    _ => {}
                }
            }
        }
        for _ in 0..32 {
            let churn = fresh_surface_interaction_attachment(&surface);
            let churn_subscription = surface
                .claim_subscription(&churn.subscription)
                .expect("claim churn subscription");
            drop(churn_subscription);
        }
        let _ = thread
            .surface_actor_probe_for_test(operation_id.clone())
            .expect("barrier after duplicate capability-loss hints");
        let duplicate_events = std::iter::from_fn(|| observer_subscription.try_recv())
            .filter_map(|item| match item {
                surface::SurfaceSubscriptionItem::Batch { batch } => Some(batch),
                surface::SurfaceSubscriptionItem::Gap { .. }
                | surface::SurfaceSubscriptionItem::Sealed { .. } => None,
            })
            .flat_map(|batch| batch.events.as_slice().to_vec())
            .filter(|event| {
                matches!(
                    &event.event,
                    surface::SurfaceEvent::Interaction(
                        surface::InteractionPatch::RouteChanged { interaction_id, .. }
                            | surface::InteractionPatch::Cancelled { interaction_id, .. }
                    ) if interaction_id == &interaction.interaction_id
                )
            })
            .count();
        let _ = observer
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait terminal operation");
        host.shutdown().expect("shutdown runtime host");

        assert!(
            reconciled_without_external_hint,
            "fallback capability loss was dropped while origin retry was retained"
        );
        assert_eq!(automatic_answer, Some(None));
        let (commit_class, cursor_after, batch_digest) =
            origin_retry_commit.expect("origin retry route-to-fallback batch was not observed");
        assert!(matches!(
            commit_class,
            surface::CommitClass::Recorded { commit_id, .. } if commit_id == pending.commit_id
        ));
        assert_eq!(cursor_after, pending.cursor_after);
        assert_eq!(batch_digest, pending.batch_digest);
        assert_eq!(route_change_count, 2);
        assert_eq!(cancellation_count, 1);
        assert_eq!(duplicate_events, 0);
    }

    #[test]
    fn detach_append_failure_finalizes_without_client_retry() {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ExactSelectorUserInputExecutor {
            answer_tx,
        }))
        .expect("start runtime host");
        let thread = host
            .start_thread(
                surface_test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "detach append failure retries internally",
            )
            .expect("start recorded runtime thread");
        let transcript_path = SessionStore::new()
            .load_session(thread.thread_id())
            .expect("load recorded runtime thread")
            .path;
        let surface = thread.surface();
        let origin = fresh_surface_interaction_attachment(&surface);
        let fallback = fresh_surface_interaction_attachment(&surface);
        let mut origin_subscription = surface
            .claim_subscription(&origin.subscription)
            .expect("claim origin subscription");
        let mut fallback_subscription = surface
            .claim_subscription(&fallback.subscription)
            .expect("claim fallback subscription");
        let reserved = committed_surface_value(
            origin
                .client
                .reserve_operation(
                    surface_request_id(),
                    surface_user_turn_intent(
                        &origin.baseline.snapshot,
                        "detach after capability-loss append failure",
                    ),
                )
                .expect("reserve typed operation"),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_surface_value(
            origin
                .client
                .admit_reserved(
                    surface_request_id(),
                    operation_id.clone(),
                    reserved.lease.lease_id,
                )
                .expect("admit typed operation"),
        );
        let interaction = collect_requested_surface_interaction(&mut origin_subscription);
        surface::JsonlSurfaceCommitLedger::inject_interaction_route_append_failure_once(
            transcript_path,
        );
        let detach_request = surface::DetachRequest {
            request_id: surface_request_id(),
        };
        let initial = surface.detach(&origin.client, detach_request.clone());
        assert!(matches!(
            initial,
            surface::DetachResult::StaleAttachment { .. }
        ));
        let automatic_deadline = Instant::now() + Duration::from_millis(750);
        let mut automatic_route = None;
        while Instant::now() < automatic_deadline && automatic_route.is_none() {
            while let Some(item) = fallback_subscription.try_recv() {
                if let surface::SurfaceSubscriptionItem::Batch { batch } = item {
                    for event in batch.events.as_slice() {
                        if let surface::SurfaceEvent::Interaction(
                            surface::InteractionPatch::RouteChanged {
                                interaction_id: candidate,
                                route,
                                ..
                            },
                        ) = &event.event
                            && candidate == &interaction.interaction_id
                        {
                            automatic_route = Some(route.clone());
                        }
                    }
                }
            }
            std::thread::yield_now();
        }
        let finalized_without_client_retry = automatic_route.is_some();
        if automatic_route.is_none() {
            let _ = surface.detach(&origin.client, detach_request.clone());
            automatic_route = Some(collect_surface_interaction_route(
                &mut fallback_subscription,
                &interaction.interaction_id,
            ));
        }
        let already_detached = surface.detach(
            &origin.client,
            surface::DetachRequest {
                request_id: surface_request_id(),
            },
        );
        let _ = committed_surface_value(
            fallback
                .client
                .respond_interaction_by_id(
                    surface_request_id(),
                    interaction.interaction_id,
                    surface::SurfaceClientInteractionAnswer::UserInput {
                        decision: surface::SurfaceUserInputDecision::Answer(
                            surface::DisplayText::new("fallback"),
                        ),
                    },
                )
                .expect("fallback response after reconciled detach"),
        );
        assert_eq!(
            answer_rx.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap(),
            Some("fallback".to_string())
        );
        let _ = fallback
            .client
            .wait_operation_terminal(surface_request_id(), operation_id)
            .expect("wait terminal operation");
        host.shutdown().expect("shutdown runtime host");

        assert!(
            finalized_without_client_retry,
            "detach append failure required an explicit client retry"
        );
        assert!(matches!(
            automatic_route,
            Some(surface::SurfaceInteractionRoute::Exclusive {
                attachment_id,
                epoch,
            }) if attachment_id == fallback.attachment_id
                && epoch == surface::ResponseRouteEpoch::try_new(2).unwrap()
        ));
        assert!(matches!(
            already_detached,
            surface::DetachResult::AlreadyDetached { .. }
        ));
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
