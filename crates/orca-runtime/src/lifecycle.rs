use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::event_schema::{EventEnvelope, EventFactory, RunStatus};
use orca_core::external_config::ExternalToolConfig;
use orca_core::hook_types::HookEvent;
use orca_core::model::{ModelRouteContext, ModelRouteDecision, ModelSelection};
use orca_core::provider_types::{ProviderResponse, ProviderStep, Usage};
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
use orca_core::tool_types::{ToolName, ToolOutputTruncation, ToolRequest, ToolResult, ToolStatus};
use orca_core::{
    cancel::CancelToken,
    config::{ProviderKind, RunConfig},
    conversation::Conversation,
};
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookOutcome, HookRunner, conversation_with_hook_context};
use crate::memory::{self, MemoryBlock};
use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};
use crate::session::{
    AgentConversationContext, bootstrap_agent_conversation_for_loop,
    record_assistant_response_for_agent, record_initial_history_for_agent,
};
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::policy_for_tool_execution;
use crate::tool_invocation::{
    AgentToolPolicyContext, ToolTurnOutcome, provider_config_for_agent_loop, run_tool_turns,
    tool_requests_from_provider_steps,
};
use crate::workflow::WorkflowDraftStore;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;
use crate::{agent_child::ChildAgentExecutor, instructions::ProjectInstructions};
use orca_core::event_sink::EventSink;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSessionLifecycle {
    run_id: String,
    active_task: Option<RuntimeTaskLifecycle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTaskLifecycle {
    id: String,
    kind: RuntimeTaskKind,
    status: RuntimeTaskStatus,
    current_turn: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskKind {
    Agent,
    Workflow,
    Subagent,
    Shell,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    ApprovalRequired,
    BudgetExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnLifecycle {
    number: u32,
}

pub struct RuntimeTurnRunner<'a> {
    lifecycle: &'a mut RuntimeSessionLifecycle,
}

pub struct RuntimeTaskActor<'a> {
    lifecycle: &'a mut RuntimeSessionLifecycle,
    max_turns: u32,
    turns_started: u32,
}

pub struct RuntimeToolActorContext {
    lifecycle: RuntimeSessionLifecycle,
    max_turns: u32,
    permission_overlay: TurnPermissionOverlay,
}

pub(crate) struct RuntimeSteerStep;
pub(crate) struct RuntimeConversationBootstrapStep;
pub(crate) struct RuntimeTurnSetupStep;
pub(crate) struct RuntimeTurnStartStep;
pub(crate) struct RuntimeModelRouteStep;
pub(crate) struct RuntimeProviderErrorStep {
    reactive_compacted: bool,
}
pub(crate) struct RuntimeProviderTurnResultStep;

pub(crate) struct RuntimeProviderTurnStep;
pub(crate) struct RuntimeProviderResponseStep;
pub(crate) struct RuntimeProviderResponseResultStep;

pub(crate) struct RuntimeProviderTurnOutput {
    pub(crate) response: Option<ProviderResponse>,
    pub(crate) terminal_error: Option<RuntimeTurnStartError>,
}

pub(crate) enum RuntimeProviderErrorOutcome {
    ContinueAfterCompaction,
    Failed(String),
    NoError,
}

pub(crate) enum RuntimeProviderResponseOutcome {
    Continue,
    Success {
        final_message: Option<String>,
    },
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) struct RuntimeTurnStartStepOutput {
    pub(crate) error: Option<RuntimeTurnStartError>,
}

pub(crate) enum RuntimeProviderErrorStepOutcome {
    ContinueAfterCompaction,
    Failed(RuntimeTurnStartError),
    NoError,
}

pub(crate) enum RuntimeProviderTurnResultOutcome {
    Response(ProviderResponse),
    Failed(RuntimeTurnStartError),
}

pub(crate) enum RuntimeProviderResponseResult {
    Continue,
    Return(AgentLoopResult),
}

#[derive(Clone, Debug)]
pub(crate) struct AgentLoopResult {
    pub(crate) status: RunStatus,
    pub(crate) final_message: Option<String>,
    pub(crate) error: Option<String>,
}

impl AgentLoopResult {
    pub(crate) fn success(final_message: Option<String>) -> Self {
        Self {
            status: RunStatus::Success,
            final_message,
            error: None,
        }
    }

    pub(crate) fn failure(status: RunStatus, error: impl Into<String>) -> Self {
        Self::terminal(status, Some(error.into()))
    }

    pub(crate) fn terminal(status: RunStatus, error: Option<String>) -> Self {
        Self {
            status,
            final_message: None,
            error,
        }
    }
}

pub(crate) struct RuntimeTurnSetup {
    pub(crate) context_config: context::ContextConfig,
    pub(crate) policy: ApprovalPolicy,
    pub(crate) provider_config: ProviderConfig,
}

pub(crate) struct RuntimePreparedConversation<'a> {
    conversation: RuntimePreparedConversationStorage<'a>,
    history_writer: Option<&'a mut SessionWriter>,
}

enum RuntimePreparedConversationStorage<'a> {
    Borrowed(&'a mut Conversation),
    Owned(Conversation),
}

pub(crate) struct RuntimeCompactionStep<'a, W: io::Write> {
    provider: ProviderKind,
    context_config: &'a context::ContextConfig,
    provider_config: &'a ProviderConfig,
    cwd: &'a Path,
    emit_deltas: bool,
    hooks: &'a HookRunner,
    events: &'a mut EventFactory,
    sink: &'a mut EventSink<W>,
    history_writer: Option<&'a mut SessionWriter>,
}

#[derive(Clone, Debug, Default)]
pub struct ThreadSteerHandle {
    pending: Arc<Mutex<Vec<String>>>,
}

pub(crate) struct AgentLoopContext<'a> {
    pub(crate) turn_config: RuntimeTurnConfig<'a>,
    pub(crate) turn_deps: Option<RuntimeTurnDeps<'a>>,
    pub(crate) turn_state: Option<RuntimeTurnState<'a>>,
    pub(crate) turn_execution: Option<RuntimeTurnExecution<'a>>,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimeTurnConfig<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) prompt: &'a str,
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
    pub(crate) subagent_type: &'a SubagentType,
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeTurnDeps<'a> {
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
}

pub(crate) struct RuntimeTurnState<'a> {
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
}

pub(crate) struct RuntimeTurnExecution<'a> {
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
}

#[derive(Clone)]
pub struct RuntimeModelTurn {
    pub decision: ModelRouteDecision,
    pub provider_config: ProviderConfig,
}

#[derive(Clone, Debug)]
pub struct RuntimeStartedTurn {
    turn: u32,
    task: Option<RuntimeTaskLifecycle>,
    pub event: EventEnvelope,
}

#[derive(Clone, Debug)]
pub struct RuntimeActorStartedTurn {
    turn: u32,
    task: Option<RuntimeTaskLifecycle>,
    event: Option<EventEnvelope>,
}

#[derive(Clone, Debug)]
pub struct RuntimeAdvancedTurn {
    turn: u32,
    task: Option<RuntimeTaskLifecycle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStartError {
    pub status: RunStatus,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum RuntimeApprovalDecision {
    NotRequired,
    Allowed(ApprovalResolution),
    Ask(ApprovalRequest),
    Denied {
        resolution: ApprovalResolution,
        result: ToolResult,
    },
}

pub trait RuntimeApprovalHandler {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution>;
}

pub struct RuntimeConfigApprovalHandler<'a> {
    config: &'a RunConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUserInputRequest {
    pub id: String,
    pub question: String,
    pub choices: Vec<String>,
}

pub trait RuntimeUserInputHandler {
    fn request_user_input(&self, request: &RuntimeUserInputRequest) -> io::Result<Option<String>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionRequest {
    pub id: String,
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePermissionResponse {
    pub decision: PermissionResponseDecision,
    pub scope: PermissionGrantScope,
    pub permissions: RequestPermissionProfile,
    pub strict_auto_review: bool,
}

pub trait RuntimePermissionRequestHandler {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse>;
}

struct AllowRequestedPermissions;

impl RuntimePermissionRequestHandler for AllowRequestedPermissions {
    fn request_permissions(
        &self,
        request: &RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        Ok(RuntimePermissionResponse {
            decision: PermissionResponseDecision::Allow,
            scope: PermissionGrantScope::Turn,
            permissions: request.permissions.clone(),
            strict_auto_review: false,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeUserInputRequestArgs {
    question: String,
    #[serde(default)]
    choices: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnPermissionOverlay {
    additional_working_directories: Vec<PathBuf>,
    strict_auto_review: bool,
}

impl TurnPermissionOverlay {
    pub fn additional_working_directories(&self) -> &[PathBuf] {
        &self.additional_working_directories
    }

    pub fn strict_auto_review(&self) -> bool {
        self.strict_auto_review
    }

    pub fn merge(&mut self, other: &Self) {
        for root in &other.additional_working_directories {
            if !self.additional_working_directories.contains(root) {
                self.additional_working_directories.push(root.clone());
            }
        }
        self.strict_auto_review |= other.strict_auto_review;
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimePermissionRequestArgs {
    #[serde(default)]
    reason: Option<String>,
    permissions: RequestPermissionProfile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSpecialToolDispatch {
    WorkflowDraft,
    WorkflowDraftAction,
    Workflow,
    Subagent,
    SubagentStatus,
    TaskList,
    TaskStop,
    RequestPermissions,
    WorkflowIpc,
    Normal,
}

impl<'a> RuntimeConfigApprovalHandler<'a> {
    pub fn new(config: &'a RunConfig) -> Self {
        Self { config }
    }
}

impl RuntimeApprovalHandler for RuntimeConfigApprovalHandler<'_> {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        crate::approval_resolution::resolve_interactive(self.config, approval, request)
    }
}

pub trait RuntimeWorkflowIpc {
    fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String>;
    fn read_messages(&self, channel: &str) -> Result<Value, String>;
    fn clear_messages(&self, channel: &str) -> Result<Value, String>;
    fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String>;
    fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String>;
    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String>;
    fn list_tasks(&self, name: &str) -> Result<Value, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSubagentStatusRecord {
    pub id: String,
    pub status: String,
    pub description: String,
    pub agent_type: Option<String>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub usage: Option<RuntimeUsageTotals>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuntimeUsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub estimated_cost_usd: f64,
}

pub trait RuntimeSubagentStatusLookup {
    fn subagent_status_record(&self, agent_id: &str) -> Option<RuntimeSubagentStatusRecord>;
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeWorkflowDraftRequest<'a> {
    pub workflows_enabled: bool,
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub max_concurrent_agents: usize,
}

impl RuntimeSessionLifecycle {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            active_task: None,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn start_task(&mut self, kind: RuntimeTaskKind) -> &RuntimeTaskLifecycle {
        let id = format!("{}:task-1", self.run_id);
        self.start_task_with_id(kind, id)
    }

    pub fn start_task_with_id(
        &mut self,
        kind: RuntimeTaskKind,
        id: impl Into<String>,
    ) -> &RuntimeTaskLifecycle {
        self.active_task = Some(RuntimeTaskLifecycle {
            id: id.into(),
            kind,
            status: RuntimeTaskStatus::Running,
            current_turn: 0,
        });
        self.active_task.as_ref().expect("task just started")
    }

    pub fn active_task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.active_task.as_ref()
    }

    pub fn next_turn(&mut self) -> RuntimeTurnLifecycle {
        let task = self
            .active_task
            .get_or_insert_with(|| RuntimeTaskLifecycle {
                id: format!("{}:task-1", self.run_id),
                kind: RuntimeTaskKind::Agent,
                status: RuntimeTaskStatus::Running,
                current_turn: 0,
            });
        task.current_turn = task.current_turn.saturating_add(1);
        RuntimeTurnLifecycle {
            number: task.current_turn,
        }
    }

    pub fn finish_task(&mut self, status: RunStatus) -> Option<&RuntimeTaskLifecycle> {
        let task = self.active_task.as_mut()?;
        task.status = RuntimeTaskStatus::from_run_status(status);
        Some(task)
    }
}

impl<'a> RuntimeTurnRunner<'a> {
    pub fn new(lifecycle: &'a mut RuntimeSessionLifecycle) -> Self {
        Self { lifecycle }
    }

    pub fn start_turn(
        &mut self,
        events: &mut EventFactory,
        prompt: Option<&str>,
    ) -> RuntimeStartedTurn {
        let advanced = self.advance_turn();
        let event = advanced
            .task
            .as_ref()
            .map(|task| {
                RuntimeTurnLifecycle {
                    number: advanced.turn,
                }
                .started_event(events, prompt, task)
            })
            .unwrap_or_else(|| events.turn_started(advanced.turn, prompt));
        RuntimeStartedTurn {
            turn: advanced.turn,
            task: advanced.task,
            event,
        }
    }

    pub fn advance_turn(&mut self) -> RuntimeAdvancedTurn {
        let turn = self.lifecycle.next_turn();
        let task = self.lifecycle.active_task().cloned();
        RuntimeAdvancedTurn {
            turn: turn.number(),
            task,
        }
    }
}

impl<'a> RuntimeTaskActor<'a> {
    pub fn new(lifecycle: &'a mut RuntimeSessionLifecycle, max_turns: u32) -> Self {
        let turns_started = lifecycle
            .active_task()
            .map(RuntimeTaskLifecycle::current_turn)
            .unwrap_or(0);
        Self {
            lifecycle,
            max_turns,
            turns_started,
        }
    }

    pub fn start_turn(
        &mut self,
        events: &mut EventFactory,
        prompt: Option<&str>,
        emit_event: bool,
    ) -> Result<RuntimeActorStartedTurn, RuntimeTurnStartError> {
        if self.turns_started >= self.max_turns {
            return Err(RuntimeTurnStartError {
                status: RunStatus::BudgetExhausted,
                message: "max turns exhausted".to_string(),
            });
        }
        self.turns_started = self.turns_started.saturating_add(1);
        let started = if emit_event {
            let started = RuntimeTurnRunner::new(self.lifecycle).start_turn(events, prompt);
            RuntimeActorStartedTurn {
                turn: started.turn,
                task: started.task,
                event: Some(started.event),
            }
        } else {
            let advanced = RuntimeTurnRunner::new(self.lifecycle).advance_turn();
            RuntimeActorStartedTurn {
                turn: advanced.turn,
                task: advanced.task,
                event: None,
            }
        };
        Ok(started)
    }

    pub fn active_task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.lifecycle.active_task()
    }

    pub fn route_model_turn(
        &mut self,
        model: &ModelSelection,
        subagent_type: &SubagentType,
        subagent_model: Option<&str>,
        provider_config: &ProviderConfig,
        cost_tracker: &mut CostTracker,
    ) -> RuntimeModelTurn {
        let decision = model.route(ModelRouteContext {
            subagent_type,
            subagent_model,
        });
        cost_tracker.set_model(Some(&decision.actual_model));
        let mut provider_config = provider_config.clone();
        provider_config.model = Some(decision.actual_model.clone());
        RuntimeModelTurn {
            decision,
            provider_config,
        }
    }

    pub fn run_pre_model_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
    ) -> Result<HookOutcome, RuntimeTurnStartError> {
        self.run_pre_model_hook_with_cancel(hooks, cwd, None)
    }

    pub fn run_pre_model_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, RuntimeTurnStartError> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        };
        let result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PreModelCall, context, cancel)
        } else {
            hooks.run(HookEvent::PreModelCall, context)
        };
        result.map_err(|error| {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                RuntimeTurnStartError {
                    status: RunStatus::Cancelled,
                    message: "turn cancelled".to_string(),
                }
            } else {
                RuntimeTurnStartError {
                    status: RunStatus::Failed,
                    message: format!("pre_model_call hook failed: {error}"),
                }
            }
        })
    }

    pub fn run_post_model_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        usage: Option<&Usage>,
    ) -> Option<String> {
        self.run_post_model_hook_with_cancel(hooks, cwd, usage, None)
    }

    pub fn run_post_model_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        usage: Option<&Usage>,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage,
        };
        let result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PostModelCall, context, cancel)
        } else {
            hooks.run(HookEvent::PostModelCall, context)
        };
        if cancel.is_some_and(CancelToken::is_cancelled) {
            None
        } else {
            result
                .err()
                .map(|error| format!("post_model_call hook failed: {error}"))
        }
    }

    pub fn call_streaming_provider(
        &mut self,
        kind: ProviderKind,
        conversation: &Conversation,
        provider_config: &ProviderConfig,
        cancel: &CancelToken,
        on_step: &mut dyn FnMut(&ProviderStep),
    ) -> ProviderResponse {
        orca_provider::call_streaming(kind, conversation, provider_config, cancel, on_step)
    }

    pub fn tool_call_requested_event(
        &mut self,
        events: &mut EventFactory,
        request: &ToolRequest,
    ) -> EventEnvelope {
        Self::tool_call_requested_event_for(events, request)
    }

    pub fn tool_call_completed_event(
        &mut self,
        events: &mut EventFactory,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> EventEnvelope {
        Self::tool_call_completed_event_for(events, request, result)
    }

    pub fn tool_call_requested_event_for(
        events: &mut EventFactory,
        request: &ToolRequest,
    ) -> EventEnvelope {
        let event = events.tool_call_requested(request);
        attach_shell_task_to_tool_event(event, request, RuntimeTaskStatus::Running)
    }

    pub fn tool_call_completed_event_for(
        events: &mut EventFactory,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> EventEnvelope {
        let status = match result.status {
            ToolStatus::Completed => RuntimeTaskStatus::Succeeded,
            ToolStatus::Failed | ToolStatus::NotImplemented => RuntimeTaskStatus::Failed,
            ToolStatus::Denied => RuntimeTaskStatus::ApprovalRequired,
        };
        let event = events.tool_call_completed(result);
        attach_shell_task_to_tool_event(event, request, status)
    }

    pub fn run_pre_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
    ) -> Result<HookOutcome, ToolResult> {
        self.run_pre_tool_hook_with_cancel(hooks, cwd, request, None)
    }

    pub fn run_pre_tool_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, ToolResult> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: Some(request),
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        };
        let result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PreToolUse, context, cancel)
        } else {
            hooks.run(HookEvent::PreToolUse, context)
        };
        result.map_err(|error| {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                ToolResult::failed(request, "tool cancelled", None)
            } else {
                ToolResult::failed(
                    request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                )
            }
        })
    }

    pub fn run_post_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> Option<String> {
        self.run_post_tool_hook_with_cancel(hooks, cwd, request, result, None)
    }

    pub fn run_post_tool_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: Some(request),
            tool_result: Some(result),
            before_messages: None,
            after_messages: None,
            usage: None,
        };
        let hook_result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PostToolUse, context, cancel)
        } else {
            hooks.run(HookEvent::PostToolUse, context)
        };
        if cancel.is_some_and(CancelToken::is_cancelled) {
            None
        } else {
            hook_result
                .err()
                .map(|error| format!("post_tool_use hook failed: {error}"))
        }
    }

    pub fn resolve_tool_approval(
        &mut self,
        policy: &ApprovalPolicy,
        approval: Option<ApprovalRequest>,
        request: &ToolRequest,
    ) -> RuntimeApprovalDecision {
        let Some(approval) = approval else {
            return RuntimeApprovalDecision::NotRequired;
        };
        let resolution =
            policy.resolve_for_tool(&approval, request.name.as_str(), request.target.as_deref());
        match resolution.decision {
            ApprovalDecision::Allow => RuntimeApprovalDecision::Allowed(resolution),
            ApprovalDecision::Ask => RuntimeApprovalDecision::Ask(approval),
            ApprovalDecision::Deny => {
                let result = ToolResult::denied(request, resolution.reason.clone());
                RuntimeApprovalDecision::Denied { resolution, result }
            }
        }
    }

    pub fn resolve_interactive_tool_approval(
        &mut self,
        handler: &dyn RuntimeApprovalHandler,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        handler.resolve_interactive(approval, request)
    }

    pub fn execute_normal_tool(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
    ) -> ToolResult {
        self.execute_normal_tool_with_cancel(
            request,
            cwd,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_cancel(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
    ) -> ToolResult {
        self.execute_normal_tool_with_roots_and_cancel(
            request,
            cwd,
            &[],
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_roots_and_cancel(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        additional_roots: &[PathBuf],
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
    ) -> ToolResult {
        if request.name == ToolName::Bash
            && let Some(task_registry) = task_registry
        {
            return execute_bash_with_shell_session(
                request,
                cwd,
                additional_roots,
                output_truncation,
                shell_timeout_secs,
                task_registry,
                cancel,
            );
        }
        orca_tools::execute_with_mcp_external_roots_policy_or_cancel(
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            || cancel.is_some_and(CancelToken::is_cancelled),
        )
    }

    pub fn execute_user_input_tool(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimeUserInputHandler,
    ) -> io::Result<ToolResult> {
        let args = parse_runtime_user_input_request(request)?;
        let input = RuntimeUserInputRequest {
            id: request.id.clone(),
            question: args.question,
            choices: args.choices,
        };
        Ok(match handler.request_user_input(&input)? {
            Some(answer) => ToolResult::completed(request, answer, false),
            None => ToolResult::failed(request, "user input request cancelled", None),
        })
    }

    pub fn record_usage(
        &mut self,
        usage: Usage,
        cost_tracker: &mut CostTracker,
        max_budget_usd: Option<f64>,
    ) -> Result<orca_core::cost_types::UsageTotals, RuntimeTurnStartError> {
        let totals = cost_tracker.add_usage(usage);
        if let Some(max_budget) = max_budget_usd
            && totals.estimated_cost_usd > max_budget
        {
            return Err(RuntimeTurnStartError {
                status: RunStatus::BudgetExhausted,
                message: format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd, max_budget
                ),
            });
        }
        Ok(totals)
    }
}

impl ThreadSteerHandle {
    pub fn push(&self, input: impl Into<String>) {
        self.pending
            .lock()
            .expect("thread steer handle lock")
            .push(input.into());
    }

    pub fn drain(&self) -> Vec<String> {
        self.pending
            .lock()
            .expect("thread steer handle lock")
            .drain(..)
            .collect()
    }
}

impl RuntimeSteerStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn apply(
        &mut self,
        steer_handle: Option<&ThreadSteerHandle>,
        conversation: &mut Conversation,
        mut history_writer: Option<&mut SessionWriter>,
    ) -> io::Result<usize> {
        let Some(steer_handle) = steer_handle else {
            return Ok(0);
        };

        let mut injected = 0;
        for input in steer_handle.drain() {
            conversation.add_user(input);
            injected += 1;
            if let Some(writer) = history_writer.as_deref_mut()
                && let Some(message) = conversation.messages.last()
            {
                writer.append_message(message)?;
            }
        }
        Ok(injected)
    }
}

impl RuntimeConversationBootstrapStep {
    pub(crate) fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare<'a>(
        &mut self,
        conversation_context: AgentConversationContext<'a>,
        cwd: &Path,
        prompt: &str,
        subagent_depth: u32,
        subagent_type: &SubagentType,
        instructions: &ProjectInstructions,
        approval_mode: orca_core::approval_types::ApprovalMode,
        memory: &MemoryBlock,
        emit_deltas: bool,
    ) -> io::Result<RuntimePreparedConversation<'a>> {
        let AgentConversationContext {
            resumed,
            history_writer,
            conversation,
        } = conversation_context;

        let mut prepared = RuntimePreparedConversation {
            conversation: match conversation {
                Some(conversation) => RuntimePreparedConversationStorage::Borrowed(conversation),
                None => RuntimePreparedConversationStorage::Owned(
                    bootstrap_agent_conversation_for_loop(
                        resumed,
                        cwd,
                        prompt,
                        subagent_depth,
                        subagent_type,
                        instructions,
                        approval_mode,
                        memory,
                    ),
                ),
            },
            history_writer,
        };

        let (conversation, history_writer) = prepared.parts_mut();
        record_initial_history_for_agent(
            conversation,
            history_writer,
            resumed.is_some(),
            emit_deltas,
        )?;

        Ok(prepared)
    }
}

impl RuntimePreparedConversation<'_> {
    pub(crate) fn conversation_mut(&mut self) -> &mut Conversation {
        match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        }
    }

    pub(crate) fn history_writer_mut(&mut self) -> Option<&mut SessionWriter> {
        self.history_writer.as_deref_mut()
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut Conversation, Option<&mut SessionWriter>) {
        let conversation = match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        };
        (conversation, self.history_writer.as_deref_mut())
    }

    pub(crate) fn parts_ref_mut(&mut self) -> (&Conversation, Option<&mut SessionWriter>) {
        let conversation = match &self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => &**conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        };
        (conversation, self.history_writer.as_deref_mut())
    }
}

impl RuntimeTurnSetupStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn prepare(
        &mut self,
        config: &RunConfig,
        subagent_depth: u32,
        subagent_type: &SubagentType,
        tool_policy: AgentToolPolicyContext<'_>,
        mcp_registry: &McpRegistry,
    ) -> RuntimeTurnSetup {
        let budget_model = config.model.as_option();
        let context_config = context::ContextConfig::for_model_with_runtime(
            budget_model.as_deref(),
            &config.model_runtime,
        );
        let policy = policy_for_tool_execution(config);
        let provider_config = provider_config_for_agent_loop(
            config,
            subagent_depth,
            subagent_type,
            tool_policy,
            mcp_registry,
        );

        RuntimeTurnSetup {
            context_config,
            policy,
            provider_config,
        }
    }
}

impl<'a> AgentLoopContext<'a> {
    pub fn new(
        cwd: &'a Path,
        prompt: &'a str,
        subagent_depth: u32,
        emit_deltas: bool,
        subagent_type: &'a SubagentType,
    ) -> Self {
        Self {
            turn_config: RuntimeTurnConfig::new(
                cwd,
                prompt,
                subagent_depth,
                emit_deltas,
                subagent_type,
            ),
            turn_deps: None,
            turn_state: None,
            turn_execution: None,
            steer_handle: None,
            permission_handler: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn turn_config(&self) -> RuntimeTurnConfig<'a> {
        self.turn_config
    }

    pub fn with_services(
        mut self,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
    ) -> Self {
        self.turn_deps = Some(RuntimeTurnDeps::new(
            instructions,
            memory,
            mcp_registry,
            hooks,
        ));
        self
    }

    #[cfg(test)]
    pub(crate) fn turn_deps(&self) -> RuntimeTurnDeps<'a> {
        self.turn_deps.expect("agent loop turn deps")
    }

    #[cfg(test)]
    pub fn instructions(&self) -> &'a ProjectInstructions {
        self.turn_deps().instructions()
    }

    #[cfg(test)]
    pub fn memory(&self) -> &'a MemoryBlock {
        self.turn_deps().memory()
    }

    #[cfg(test)]
    pub fn mcp_registry(&self) -> &'a McpRegistry {
        self.turn_deps().mcp_registry()
    }

    #[cfg(test)]
    pub fn hooks(&self) -> &'a HookRunner {
        self.turn_deps().hooks()
    }

    pub fn with_runtime(
        mut self,
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
    ) -> Self {
        self.turn_state = Some(RuntimeTurnState::new(cost_tracker, cancel, task_registry));
        self
    }

    #[cfg(test)]
    pub(crate) fn turn_state(&self) -> &RuntimeTurnState<'a> {
        self.turn_state.as_ref().expect("agent loop turn state")
    }

    #[cfg(test)]
    pub fn cost_tracker(&self) -> &CostTracker {
        self.turn_state().cost_tracker()
    }

    #[cfg(test)]
    pub fn cancel(&self) -> &'a CancelToken {
        self.turn_state().cancel()
    }

    #[cfg(test)]
    pub fn task_registry(&self) -> &'a TaskRegistry {
        self.turn_state().task_registry()
    }

    pub(crate) fn with_execution(
        mut self,
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    ) -> Self {
        self.turn_execution = Some(RuntimeTurnExecution::new(
            background_workflows,
            workflow_ipc,
            lifecycle,
        ));
        self
    }

    pub(crate) fn with_steer_handle(mut self, steer_handle: Option<&'a ThreadSteerHandle>) -> Self {
        self.steer_handle = steer_handle;
        self
    }

    pub(crate) fn with_permission_handler(
        mut self,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        self.permission_handler = permission_handler;
        self
    }

    #[cfg(test)]
    pub fn background_workflow_count(&self) -> usize {
        self.turn_execution().background_workflow_count()
    }

    #[cfg(test)]
    pub fn workflow_ipc(&self) -> Option<&'a WorkflowIpcContext> {
        self.turn_execution().workflow_ipc()
    }

    #[cfg(test)]
    pub fn lifecycle(&self) -> Option<&RuntimeSessionLifecycle> {
        self.turn_execution().lifecycle()
    }

    #[cfg(test)]
    pub(crate) fn turn_execution(&self) -> &RuntimeTurnExecution<'a> {
        self.turn_execution
            .as_ref()
            .expect("agent loop turn execution")
    }
}

impl<'a> RuntimeTurnConfig<'a> {
    pub(crate) fn new(
        cwd: &'a Path,
        prompt: &'a str,
        subagent_depth: u32,
        emit_deltas: bool,
        subagent_type: &'a SubagentType,
    ) -> Self {
        Self {
            cwd,
            prompt,
            subagent_depth,
            emit_deltas,
            subagent_type,
        }
    }

    #[cfg(test)]
    pub(crate) fn cwd(&self) -> &'a Path {
        self.cwd
    }

    #[cfg(test)]
    pub(crate) fn prompt(&self) -> &'a str {
        self.prompt
    }

    #[cfg(test)]
    pub(crate) fn subagent_depth(&self) -> u32 {
        self.subagent_depth
    }

    #[cfg(test)]
    pub(crate) fn emit_deltas(&self) -> bool {
        self.emit_deltas
    }

    #[cfg(test)]
    pub(crate) fn subagent_type(&self) -> &'a SubagentType {
        self.subagent_type
    }
}

impl<'a> RuntimeTurnDeps<'a> {
    pub(crate) fn new(
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
    ) -> Self {
        Self {
            instructions,
            memory,
            mcp_registry,
            hooks,
        }
    }

    #[cfg(test)]
    pub(crate) fn instructions(&self) -> &'a ProjectInstructions {
        self.instructions
    }

    #[cfg(test)]
    pub(crate) fn memory(&self) -> &'a MemoryBlock {
        self.memory
    }

    #[cfg(test)]
    pub(crate) fn mcp_registry(&self) -> &'a McpRegistry {
        self.mcp_registry
    }

    #[cfg(test)]
    pub(crate) fn hooks(&self) -> &'a HookRunner {
        self.hooks
    }
}

impl<'a> RuntimeTurnState<'a> {
    pub(crate) fn new(
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
    ) -> Self {
        Self {
            cost_tracker,
            cancel,
            task_registry,
        }
    }

    #[cfg(test)]
    pub(crate) fn cost_tracker(&self) -> &CostTracker {
        self.cost_tracker
    }

    #[cfg(test)]
    pub(crate) fn cancel(&self) -> &'a CancelToken {
        self.cancel
    }

    #[cfg(test)]
    pub(crate) fn task_registry(&self) -> &'a TaskRegistry {
        self.task_registry
    }
}

impl<'a> RuntimeTurnExecution<'a> {
    pub(crate) fn new(
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    ) -> Self {
        Self {
            background_workflows,
            workflow_ipc,
            lifecycle,
        }
    }

    #[cfg(test)]
    pub(crate) fn background_workflow_count(&self) -> usize {
        self.background_workflows.len()
    }

    #[cfg(test)]
    pub(crate) fn workflow_ipc(&self) -> Option<&'a WorkflowIpcContext> {
        self.workflow_ipc
    }

    #[cfg(test)]
    pub(crate) fn lifecycle(&self) -> Option<&RuntimeSessionLifecycle> {
        self.lifecycle.as_deref()
    }
}

impl RuntimeToolActorContext {
    pub fn new(run_id: impl Into<String>, max_turns: u32) -> Self {
        let mut lifecycle = RuntimeSessionLifecycle::new(run_id);
        lifecycle.start_task(RuntimeTaskKind::Agent);
        Self {
            lifecycle,
            max_turns,
            permission_overlay: TurnPermissionOverlay::default(),
        }
    }

    fn actor(&mut self) -> RuntimeTaskActor<'_> {
        RuntimeTaskActor::new(&mut self.lifecycle, self.max_turns)
    }

    pub fn active_task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.lifecycle.active_task()
    }

    pub fn classify_dispatch(&self, request: &ToolRequest) -> RuntimeSpecialToolDispatch {
        match request.name {
            ToolName::WorkflowDraft => RuntimeSpecialToolDispatch::WorkflowDraft,
            ToolName::WorkflowDraftAction => RuntimeSpecialToolDispatch::WorkflowDraftAction,
            ToolName::Workflow => RuntimeSpecialToolDispatch::Workflow,
            ToolName::Subagent => RuntimeSpecialToolDispatch::Subagent,
            ToolName::SubagentStatus => RuntimeSpecialToolDispatch::SubagentStatus,
            ToolName::TaskList => RuntimeSpecialToolDispatch::TaskList,
            ToolName::TaskStop => RuntimeSpecialToolDispatch::TaskStop,
            ToolName::RequestPermissions => RuntimeSpecialToolDispatch::RequestPermissions,
            ToolName::WorkflowSendMessage
            | ToolName::WorkflowReadMessages
            | ToolName::WorkflowClearMessages
            | ToolName::WorkflowCreateTaskList
            | ToolName::WorkflowClaimTask
            | ToolName::WorkflowCompleteTask
            | ToolName::WorkflowListTasks => RuntimeSpecialToolDispatch::WorkflowIpc,
            _ => RuntimeSpecialToolDispatch::Normal,
        }
    }

    pub fn granted_additional_working_directories(&self) -> Vec<PathBuf> {
        self.permission_overlay
            .additional_working_directories
            .clone()
    }

    pub fn permission_overlay(&self) -> &TurnPermissionOverlay {
        &self.permission_overlay
    }

    pub fn execute_request_permissions_tool(&mut self, request: &ToolRequest) -> ToolResult {
        self.execute_request_permissions_tool_with_handler(request, &AllowRequestedPermissions)
    }

    pub fn execute_request_permissions_tool_with_handler(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimePermissionRequestHandler,
    ) -> ToolResult {
        let args = match parse_runtime_permission_request(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let permission_request = RuntimePermissionRequest {
            id: request.id.clone(),
            reason: args.reason,
            permissions: args.permissions,
        };
        let response = match handler.request_permissions(&permission_request) {
            Ok(response) => response,
            Err(error) => return ToolResult::failed(request, error.to_string(), None),
        };
        if response.decision == PermissionResponseDecision::Deny {
            return ToolResult::denied(request, "permission request denied".to_string());
        }
        let write_roots = response
            .permissions
            .file_system
            .as_ref()
            .and_then(|file_system| file_system.write.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();

        for root in &write_roots {
            if !self
                .permission_overlay
                .additional_working_directories
                .contains(root)
            {
                self.permission_overlay
                    .additional_working_directories
                    .push(root.clone());
            }
        }
        self.permission_overlay.strict_auto_review |= response.strict_auto_review;

        let read_roots = response
            .permissions
            .file_system
            .as_ref()
            .and_then(|file_system| file_system.read.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let write_roots_json = write_roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let network_enabled = response
            .permissions
            .network
            .as_ref()
            .and_then(|network| network.enabled);
        let output = json!({
            "message": "Permissions granted for the current turn",
            "reason": permission_request.reason,
            "granted": {
                "fileSystem": {
                    "read": read_roots,
                    "write": write_roots_json,
                },
                "network": {
                    "enabled": network_enabled,
                },
            },
            "scope": response.scope,
            "persistent": response.scope == PermissionGrantScope::Session,
            "strictAutoReview": response.strict_auto_review,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn run_pre_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
    ) -> Result<HookOutcome, ToolResult> {
        self.actor().run_pre_tool_hook(hooks, cwd, request)
    }

    pub fn run_pre_tool_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, ToolResult> {
        self.actor()
            .run_pre_tool_hook_with_cancel(hooks, cwd, request, cancel)
    }

    pub fn run_post_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> Option<String> {
        self.actor().run_post_tool_hook(hooks, cwd, request, result)
    }

    pub fn run_post_tool_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        self.actor()
            .run_post_tool_hook_with_cancel(hooks, cwd, request, result, cancel)
    }

    pub fn resolve_tool_approval(
        &mut self,
        policy: &ApprovalPolicy,
        approval: Option<ApprovalRequest>,
        request: &ToolRequest,
    ) -> RuntimeApprovalDecision {
        self.actor()
            .resolve_tool_approval(policy, approval, request)
    }

    pub fn resolve_interactive_tool_approval(
        &mut self,
        handler: &dyn RuntimeApprovalHandler,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        self.actor()
            .resolve_interactive_tool_approval(handler, approval, request)
    }

    pub fn execute_normal_tool(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
    ) -> ToolResult {
        self.actor().execute_normal_tool(
            request,
            cwd,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_cancel(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
    ) -> ToolResult {
        self.actor().execute_normal_tool_with_cancel(
            request,
            cwd,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_roots_and_cancel(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        additional_roots: &[PathBuf],
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
    ) -> ToolResult {
        self.actor().execute_normal_tool_with_roots_and_cancel(
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
        )
    }

    pub fn execute_user_input_tool(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimeUserInputHandler,
    ) -> io::Result<ToolResult> {
        self.actor().execute_user_input_tool(request, handler)
    }

    pub fn execute_workflow_ipc_tool(
        &mut self,
        request: &ToolRequest,
        workflow_ipc: Option<&dyn RuntimeWorkflowIpc>,
    ) -> ToolResult {
        let Some(workflow_ipc) = workflow_ipc else {
            return ToolResult::failed(
                request,
                "workflow IPC tools are only available inside workflow child agents",
                None,
            );
        };
        let raw = request.raw_arguments.as_deref().unwrap_or("{}");
        let args: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::invalid_input(
                    request,
                    format!("arguments are not valid JSON: {error}"),
                );
            }
        };
        let result = match request.name {
            ToolName::WorkflowSendMessage => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                let message = args.get("message").cloned().unwrap_or(Value::Null);
                let from = args.get("from").and_then(Value::as_str);
                workflow_ipc.send_message(channel, from, message)
            }
            ToolName::WorkflowReadMessages => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                workflow_ipc.read_messages(channel)
            }
            ToolName::WorkflowClearMessages => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                workflow_ipc.clear_messages(channel)
            }
            ToolName::WorkflowCreateTaskList => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let items = match args.get("items").and_then(Value::as_array) {
                    Some(items) => items.clone(),
                    None => {
                        return ToolResult::invalid_input(
                            request,
                            "missing required array field: items",
                        );
                    }
                };
                workflow_ipc.create_task_list(name, items)
            }
            ToolName::WorkflowClaimTask => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let by = args.get("by").and_then(Value::as_str);
                workflow_ipc.claim_task(name, by)
            }
            ToolName::WorkflowCompleteTask => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let task_id = match required_string_arg(request, &args, "task_id") {
                    Ok(task_id) => task_id,
                    Err(result) => return result,
                };
                let result = args.get("result").cloned().unwrap_or(Value::Null);
                let by = args.get("by").and_then(Value::as_str);
                workflow_ipc.complete_task(name, task_id, result, by)
            }
            ToolName::WorkflowListTasks => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                workflow_ipc.list_tasks(name)
            }
            _ => unreachable!("workflow IPC tool dispatch guarded by caller"),
        };

        match result {
            Ok(value) => ToolResult::completed(request, value.to_string(), false),
            Err(error) => ToolResult::invalid_input(request, error),
        }
    }

    pub fn execute_subagent_status_tool(
        &mut self,
        request: &ToolRequest,
        lookup: &dyn RuntimeSubagentStatusLookup,
    ) -> ToolResult {
        let agent_id =
            extract_tool_string_field(request, "agent_id").or_else(|| request.target.clone());
        let Some(agent_id) = agent_id else {
            return ToolResult::invalid_input(request, "missing agent_id");
        };
        let Some(record) = lookup.subagent_status_record(&agent_id) else {
            return ToolResult::failed(request, format!("subagent '{agent_id}' not found"), None);
        };
        let (result_output, result_task) = record
            .output
            .as_deref()
            .map(unpack_async_subagent_result)
            .unwrap_or((None, None));
        let (error_output, error_task) = record
            .error
            .as_deref()
            .map(unpack_async_subagent_result)
            .unwrap_or((None, None));
        let output = json!({
            "agent_id": agent_id,
            "status": record.status,
            "description": record.description,
            "agent_type": record.agent_type,
            "created_at_ms": record.created_at_ms,
            "started_at_ms": record.started_at_ms,
            "completed_at_ms": record.completed_at_ms,
            "output": result_output,
            "error": error_output,
            "task": result_task.or(error_task),
            "usage": record.usage.map(runtime_usage_totals_json),
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_task_list_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let tasks = task_registry
            .list()
            .into_iter()
            .map(task_summary_json)
            .collect::<Vec<_>>();
        ToolResult::completed(request, json!({ "tasks": tasks }).to_string(), false)
    }

    pub fn execute_task_stop_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let args = match parse_tool_arguments(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let Some(task_id) = args
            .get("task_id")
            .and_then(Value::as_str)
            .or_else(|| args.get("shell_id").and_then(Value::as_str))
            .filter(|task_id| !task_id.trim().is_empty())
        else {
            return ToolResult::invalid_input(request, "missing required field: task_id");
        };
        let Some(record) = task_registry.get(task_id) else {
            return ToolResult::failed(request, format!("task '{task_id}' not found"), None);
        };
        if is_terminal_task_status(record.status) {
            return ToolResult::failed(
                request,
                format!(
                    "task is already {} and cannot be stopped",
                    task_status_label(record.status)
                ),
                None,
            );
        }
        if let Err(error) = task_registry.request_stop(task_id) {
            return ToolResult::failed(request, error, None);
        }
        let output = json!({
            "message": "Task stop requested",
            "task_id": record.id,
            "task_type": task_type_label(record.task_type),
            "command": record.command,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_workflow_draft_tool(
        &mut self,
        request: &ToolRequest,
        draft_request: RuntimeWorkflowDraftRequest<'_>,
    ) -> std::io::Result<ToolResult> {
        if !draft_request.workflows_enabled {
            return Ok(ToolResult::failed(request, "workflows are disabled", None));
        }
        let script = workflow_draft_script_arg(request)?;
        let session_dir = draft_request
            .cwd
            .join(".orca")
            .join("workflow-sessions")
            .join(draft_request.session_id);
        let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
        let draft = draft_store.create_from_script(
            draft_request.session_id,
            draft_request.cwd,
            &script,
            draft_request.max_concurrent_agents,
        )?;
        let output = serde_json::to_string(&draft)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        Ok(ToolResult::completed(request, output, false))
    }
}

fn parse_runtime_user_input_request(
    request: &ToolRequest,
) -> io::Result<RuntimeUserInputRequestArgs> {
    let raw = request.raw_arguments.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing request_user_input arguments JSON",
        )
    })?;
    let args: RuntimeUserInputRequestArgs = serde_json::from_str(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid request_user_input arguments JSON: {error}"),
        )
    })?;
    if args.question.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing required request_user_input argument: question",
        ));
    }
    Ok(args)
}

fn parse_runtime_permission_request(
    request: &ToolRequest,
) -> Result<RuntimePermissionRequestArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing request_permissions arguments JSON".to_string())?;
    let mut args: RuntimePermissionRequestArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid request_permissions arguments JSON: {error}"))?;
    args.permissions = args.permissions.normalize_file_system_entries();
    if args
        .reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err("missing required request_permissions argument: reason".to_string());
    }
    let file_system = args.permissions.file_system.as_ref();
    let has_file_system_request = file_system.is_some_and(|file_system| {
        file_system
            .read
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
            || file_system
                .write
                .as_ref()
                .is_some_and(|paths| !paths.is_empty())
    });
    let has_network_request = args
        .permissions
        .network
        .as_ref()
        .and_then(|network| network.enabled)
        .is_some();
    if !has_file_system_request && !has_network_request {
        return Err("request_permissions requires at least one permission request".to_string());
    }
    Ok(args)
}

fn required_string_arg<'a>(
    request: &ToolRequest,
    args: &'a Value,
    field: &str,
) -> Result<&'a str, ToolResult> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        ToolResult::invalid_input(request, format!("missing required string field: {field}"))
    })
}

fn parse_tool_arguments(request: &ToolRequest) -> Result<Value, String> {
    serde_json::from_str(request.raw_arguments.as_deref().unwrap_or("{}"))
        .map_err(|error| format!("arguments are not valid JSON: {error}"))
}

fn task_summary_json(task: BackgroundTaskSummary) -> Value {
    json!({
        "id": task.id,
        "subject": task.description,
        "status": task_status_label(task.status),
        "owner": Value::Null,
        "blockedBy": [],
        "task_type": task_type_label(task.task_type),
        "command": task.command,
    })
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

fn is_terminal_task_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Stopped | TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

fn execute_bash_with_shell_session(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    task_registry: &TaskRegistry,
    cancel: Option<&CancelToken>,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let mut manager = RuntimeShellSessionManager::new(task_registry.clone());
    let handle = match manager.spawn(ShellSessionCommand {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        additional_readable_directories: Vec::new(),
        additional_working_directories: additional_roots.to_vec(),
        denied_working_directories: Vec::new(),
        env: Default::default(),
        description: command.to_string(),
        terminal: ShellTerminalMode::pipe(),
        sandbox: ShellSandboxMode::default(),
    }) {
        Ok(handle) => handle,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to run shell command: {error}"),
                None,
            );
        }
    };
    let _ = manager.close_stdin(&handle.id);
    let output = match manager.wait_or_cancel(
        &handle.id,
        std::time::Duration::from_secs(shell_timeout_secs.max(1)),
        || {
            cancel.is_some_and(CancelToken::is_cancelled)
                || task_registry.is_cancelled(&handle.task_id)
        },
    ) {
        Ok(output) => output,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to wait for shell command: {error}"),
                None,
            );
        }
    };

    let stdout = output.stdout.trim_end().to_string();
    let stderr = output.stderr.trim_end().to_string();
    if cancel.is_some_and(CancelToken::is_cancelled) || task_registry.is_cancelled(&handle.task_id)
    {
        let message = if stderr.is_empty() && stdout.is_empty() {
            "shell command cancelled".to_string()
        } else if stderr.is_empty() {
            format!("shell command cancelled: {stdout}")
        } else if stdout.is_empty() {
            format!("shell command cancelled: {stderr}")
        } else {
            format!("shell command cancelled: {stdout}\n{stderr}")
        };
        let (message, truncated) =
            orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
        let mut result = ToolResult::failed(request, message, output.exit_code);
        result.truncated = truncated;
        return result;
    }
    if output.status == orca_core::task_types::TaskStatus::Completed {
        let (stdout, truncated) =
            orca_core::tool_types::truncate_output_with_policy(stdout, output_truncation);
        return ToolResult::completed(request, stdout, truncated);
    }

    let message = if stderr.is_empty() {
        stdout
    } else if stdout.is_empty() {
        stderr
    } else {
        format!("{stdout}\n{stderr}")
    };
    let (message, truncated) =
        orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
    let mut result = ToolResult::failed(request, message, output.exit_code);
    result.truncated = truncated;
    result
}

fn extract_tool_string_field(request: &ToolRequest, field: &str) -> Option<String> {
    let raw = request.raw_arguments.as_deref()?;
    let value = serde_json::from_str::<Value>(raw).ok()?;
    value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn runtime_usage_totals_json(usage: RuntimeUsageTotals) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_tokens": usage.cache_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "estimated_cost_usd": usage.estimated_cost_usd,
    })
}

fn unpack_async_subagent_result(raw: &str) -> (Option<Value>, Option<Value>) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return (Some(Value::String(raw.to_string())), None);
    };
    let Some(output) = value.get("output") else {
        return (Some(Value::String(raw.to_string())), None);
    };
    let task = value.get("task").cloned().filter(|task| !task.is_null());
    (Some(output.clone()), task)
}

fn workflow_draft_script_arg(request: &ToolRequest) -> std::io::Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WorkflowDraftInput {
        script: String,
    }

    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str::<WorkflowDraftInput>(raw_arguments)
        .map(|input| input.script)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

fn attach_shell_task_to_tool_event(
    event: EventEnvelope,
    request: &ToolRequest,
    status: RuntimeTaskStatus,
) -> EventEnvelope {
    if request.action != orca_core::approval_types::ActionKind::Shell {
        return event;
    }

    RuntimeTaskLifecycle::new_snapshot(shell_task_id(request), RuntimeTaskKind::Shell, status, 1)
        .attach_to_event(event)
}

fn shell_task_id(request: &ToolRequest) -> String {
    format!("shell-{}:task-1", request.id)
}

impl RuntimeStartedTurn {
    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.task.as_ref()
    }
}

impl RuntimeActorStartedTurn {
    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.task.as_ref()
    }

    pub fn event(&self) -> Option<&EventEnvelope> {
        self.event.as_ref()
    }

    pub fn into_event(self) -> Option<EventEnvelope> {
        self.event
    }
}

impl RuntimeAdvancedTurn {
    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.task.as_ref()
    }
}

impl RuntimeTurnStartStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn start<W: io::Write>(
        &mut self,
        actor: &mut RuntimeTaskActor<'_>,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        prompt: &str,
        emit_deltas: bool,
    ) -> io::Result<RuntimeTurnStartStepOutput> {
        let turn_prompt = if actor
            .active_task()
            .map(|task| task.current_turn())
            .unwrap_or(0)
            == 0
        {
            Some(prompt)
        } else {
            None
        };
        let started_turn = match actor.start_turn(events, turn_prompt, emit_deltas) {
            Ok(started_turn) => started_turn,
            Err(error) => {
                if emit_deltas {
                    sink.emit(&events.error(&error.message))?;
                }
                return Ok(RuntimeTurnStartStepOutput { error: Some(error) });
            }
        };
        if let Some(event) = started_turn.into_event() {
            sink.emit(&event)?;
        }
        Ok(RuntimeTurnStartStepOutput { error: None })
    }
}

impl RuntimeModelRouteStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn route<W: io::Write>(
        &mut self,
        actor: &mut RuntimeTaskActor<'_>,
        model: &ModelSelection,
        subagent_type: &SubagentType,
        provider_config: &ProviderConfig,
        cost_tracker: &mut CostTracker,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        emit_deltas: bool,
    ) -> io::Result<RuntimeModelTurn> {
        let routed_model =
            actor.route_model_turn(model, subagent_type, None, provider_config, cost_tracker);
        if emit_deltas {
            sink.emit(&events.model_routed(&routed_model.decision))?;
        }
        Ok(routed_model)
    }
}

impl RuntimeProviderErrorStep {
    pub(crate) fn new() -> Self {
        Self {
            reactive_compacted: false,
        }
    }

    pub(crate) fn handle<W: io::Write>(
        &mut self,
        response: &ProviderResponse,
        compaction: &mut RuntimeCompactionStep<'_, W>,
        conversation: &mut Conversation,
    ) -> io::Result<RuntimeProviderErrorStepOutcome> {
        match RuntimeProviderTurnStep::new().handle_provider_error(
            response,
            compaction,
            conversation,
            self.reactive_compacted,
        )? {
            RuntimeProviderErrorOutcome::ContinueAfterCompaction => {
                self.reactive_compacted = true;
                Ok(RuntimeProviderErrorStepOutcome::ContinueAfterCompaction)
            }
            RuntimeProviderErrorOutcome::Failed(message) => {
                self.reactive_compacted = false;
                Ok(RuntimeProviderErrorStepOutcome::Failed(
                    RuntimeTurnStartError {
                        status: RunStatus::Failed,
                        message,
                    },
                ))
            }
            RuntimeProviderErrorOutcome::NoError => {
                self.reactive_compacted = false;
                Ok(RuntimeProviderErrorStepOutcome::NoError)
            }
        }
    }

    #[cfg(test)]
    fn mark_reactive_compacted_for_test(&mut self) {
        self.reactive_compacted = true;
    }

    #[cfg(test)]
    fn reactive_compacted_for_test(&self) -> bool {
        self.reactive_compacted
    }
}

impl RuntimeProviderTurnResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold<W: io::Write>(
        &mut self,
        provider_turn: RuntimeProviderTurnOutput,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        emit_deltas: bool,
    ) -> io::Result<RuntimeProviderTurnResultOutcome> {
        match provider_response_or_terminal(provider_turn) {
            Ok(response) => Ok(RuntimeProviderTurnResultOutcome::Response(response)),
            Err(error) => {
                if emit_deltas && error.status != RunStatus::Cancelled {
                    sink.emit(&events.error(&error.message))?;
                }
                Ok(RuntimeProviderTurnResultOutcome::Failed(error))
            }
        }
    }
}

impl<'a, W: io::Write> RuntimeCompactionStep<'a, W> {
    pub(crate) fn new(
        provider: ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        cwd: &'a Path,
        emit_deltas: bool,
        hooks: &'a HookRunner,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        history_writer: Option<&'a mut SessionWriter>,
    ) -> Self {
        Self {
            provider,
            context_config,
            provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            history_writer,
        }
    }

    pub(crate) fn compact_if_needed(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<bool> {
        if !context::needs_compaction_wire(conversation, self.context_config, self.provider_config)
        {
            return Ok(false);
        }

        self.compact_with_budget_hooks(conversation)?;
        Ok(true)
    }

    pub(crate) fn compact_after_prompt_too_long(
        &mut self,
        conversation: &mut Conversation,
    ) -> io::Result<()> {
        self.compact_and_persist(conversation)?;
        Ok(())
    }

    fn compact_with_budget_hooks(&mut self, conversation: &mut Conversation) -> io::Result<()> {
        let before_messages = conversation.messages.len();
        match self.hooks.run(
            HookEvent::OnBudgetWarning,
            HookContext {
                cwd: &self.cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) if !outcome.injected_context.is_empty() => {
                *conversation = conversation_with_hook_context(conversation, &outcome);
            }
            Err(error) if self.emit_deltas => {
                self.sink.emit(
                    &self
                        .events
                        .error(&format!("on_budget_warning hook failed: {error}")),
                )?;
            }
            _ => {}
        }

        if self.emit_deltas {
            self.run_compaction_hook(HookEvent::PreCompact, before_messages, None)?;
        }

        let after_messages = self.compact_and_persist(conversation)?;

        if self.emit_deltas {
            self.run_compaction_hook(
                HookEvent::PostCompact,
                before_messages,
                Some(after_messages),
            )?;
        }

        Ok(())
    }

    fn compact_and_persist(&mut self, conversation: &mut Conversation) -> io::Result<usize> {
        let before_messages = conversation.messages.len();
        let compaction = context::compact_with_summary(
            self.provider,
            conversation,
            self.context_config,
            self.provider_config,
        );
        *conversation = compaction.conversation;
        let after_messages = conversation.messages.len();
        if self.emit_deltas
            && let Some(writer) = self.history_writer.as_deref_mut()
        {
            writer.append_compaction(before_messages, after_messages)?;
            if let context::CompactionKind::RemoteSummary(summary) = compaction.kind {
                writer.append_summary_state(
                    before_messages,
                    after_messages,
                    summary,
                    &conversation.summary,
                )?;
            }
        }
        Ok(after_messages)
    }

    fn run_compaction_hook(
        &mut self,
        event: HookEvent,
        before_messages: usize,
        after_messages: Option<usize>,
    ) -> io::Result<()> {
        if let Err(error) = self.hooks.run(
            event,
            HookContext {
                cwd: &self.cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: Some(before_messages),
                after_messages,
                usage: None,
            },
        ) {
            self.sink.emit(
                &self
                    .events
                    .error(&format!("{} hook failed: {error}", event.as_str())),
            )?;
        }
        Ok(())
    }
}

impl RuntimeProviderTurnStep {
    pub(crate) fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run<W: io::Write>(
        &mut self,
        actor: &mut RuntimeTaskActor<'_>,
        provider: ProviderKind,
        conversation: &Conversation,
        provider_config: &ProviderConfig,
        cwd: &str,
        emit_deltas: bool,
        hooks: &HookRunner,
        cancel: &CancelToken,
        cost_tracker: &mut CostTracker,
        max_budget_usd: Option<f64>,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        mut history_writer: Option<&mut SessionWriter>,
    ) -> io::Result<RuntimeProviderTurnOutput> {
        let pre_model_outcome = match actor.run_pre_model_hook_with_cancel(hooks, cwd, Some(cancel))
        {
            Ok(outcome) => outcome,
            Err(error) => return Ok(RuntimeProviderTurnOutput::terminal(error)),
        };
        if cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        let model_conversation = conversation_with_hook_context(conversation, &pre_model_outcome);
        let response = actor.call_streaming_provider(
            provider,
            &model_conversation,
            provider_config,
            cancel,
            &mut |step| emit_provider_delta(step, emit_deltas, events, sink),
        );
        if cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        if let Some(warning) =
            actor.run_post_model_hook_with_cancel(hooks, cwd, response.usage.as_ref(), Some(cancel))
            && emit_deltas
        {
            sink.emit(&events.error(&warning))?;
        }
        if cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            match actor.record_usage(usage, cost_tracker, max_budget_usd) {
                Ok(totals) => {
                    if emit_deltas {
                        sink.emit(&events.usage_updated(totals))?;
                        if let Some(writer) = history_writer.as_deref_mut() {
                            writer.append_usage(totals)?;
                        }
                    }
                }
                Err(error) => return Ok(RuntimeProviderTurnOutput::terminal(error)),
            }
        }

        Ok(RuntimeProviderTurnOutput::response(response))
    }

    pub(crate) fn handle_provider_error<W: io::Write>(
        &mut self,
        response: &ProviderResponse,
        compaction: &mut RuntimeCompactionStep<'_, W>,
        conversation: &mut Conversation,
        reactive_compacted: bool,
    ) -> io::Result<RuntimeProviderErrorOutcome> {
        let provider_error = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        });

        let Some(error) = provider_error else {
            return Ok(RuntimeProviderErrorOutcome::NoError);
        };

        if context::is_prompt_too_long_error(&error) && !reactive_compacted {
            compaction.compact_after_prompt_too_long(conversation)?;
            return Ok(RuntimeProviderErrorOutcome::ContinueAfterCompaction);
        }

        if compaction.emit_deltas {
            compaction.sink.emit(&compaction.events.error(&error))?;
        }
        Ok(RuntimeProviderErrorOutcome::Failed(error))
    }
}

impl RuntimeProviderResponseStep {
    pub(crate) fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle<W: io::Write>(
        &mut self,
        response: ProviderResponse,
        config: &RunConfig,
        cwd: &Path,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        conversation: &mut Conversation,
        mut history_writer: Option<&mut SessionWriter>,
        tool_policy: AgentToolPolicyContext<'_>,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &ApprovalPolicy,
        instructions: &ProjectInstructions,
        memory: &MemoryBlock,
        mcp_registry: &McpRegistry,
        hooks: &HookRunner,
        cost_tracker: &mut CostTracker,
        cancel: &CancelToken,
        task_registry: &TaskRegistry,
        background_workflows: &mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&WorkflowIpcContext>,
        permission_handler: Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeProviderResponseOutcome> {
        if response.tool_calls.is_empty() {
            let final_message = response.assistant_content.clone();
            record_assistant_response_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
                emit_deltas,
            )?;
            if emit_deltas && config.auto_memory {
                memory::extract_project_memory_after_final_response(
                    config,
                    cwd,
                    &conversation.messages,
                    events,
                    sink,
                )?;
            }
            return Ok(RuntimeProviderResponseOutcome::Success { final_message });
        }

        record_assistant_response_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
            emit_deltas,
        )?;

        let tool_requests = tool_requests_from_provider_steps(&response.steps);
        match run_tool_turns(
            config,
            cwd,
            events,
            sink,
            conversation,
            history_writer.as_deref_mut(),
            &tool_requests,
            tool_policy,
            subagent_depth,
            emit_deltas,
            policy,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cost_tracker,
            cancel,
            task_registry,
            background_workflows,
            workflow_ipc,
            permission_handler,
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        )? {
            ToolTurnOutcome::Continue => Ok(RuntimeProviderResponseOutcome::Continue),
            ToolTurnOutcome::Return { status, error } => {
                Ok(RuntimeProviderResponseOutcome::Return { status, error })
            }
        }
    }
}

impl RuntimeProviderResponseResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderResponseOutcome,
    ) -> RuntimeProviderResponseResult {
        match outcome {
            RuntimeProviderResponseOutcome::Continue => RuntimeProviderResponseResult::Continue,
            RuntimeProviderResponseOutcome::Success { final_message } => {
                RuntimeProviderResponseResult::Return(AgentLoopResult::success(final_message))
            }
            RuntimeProviderResponseOutcome::Return { status, error } => {
                RuntimeProviderResponseResult::Return(AgentLoopResult::terminal(status, error))
            }
        }
    }
}

fn cancelled_provider_turn<W: io::Write>(
    emit_deltas: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) -> io::Result<RuntimeProviderTurnOutput> {
    if emit_deltas {
        sink.emit(&events.error("turn cancelled"))?;
    }
    Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
        status: RunStatus::Cancelled,
        message: "turn cancelled".to_string(),
    }))
}

fn emit_provider_delta<W: io::Write>(
    step: &ProviderStep,
    emit_deltas: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) {
    if !emit_deltas {
        return;
    }
    match step {
        ProviderStep::ReasoningDelta(text) => {
            let _ = sink.emit(&events.assistant_reasoning_delta(text));
        }
        ProviderStep::MessageDelta(text) => {
            let _ = sink.emit(&events.assistant_message_delta(text));
        }
        ProviderStep::ReplayState(replay) => {
            let _ = sink.emit(&events.provider_replay_updated(replay));
        }
        _ => {}
    }
}

impl RuntimeProviderTurnOutput {
    fn response(response: ProviderResponse) -> Self {
        Self {
            response: Some(response),
            terminal_error: None,
        }
    }

    fn terminal(error: RuntimeTurnStartError) -> Self {
        Self {
            response: None,
            terminal_error: Some(error),
        }
    }
}

pub(crate) fn provider_response_or_terminal(
    provider_turn: RuntimeProviderTurnOutput,
) -> Result<ProviderResponse, RuntimeTurnStartError> {
    match provider_turn.response {
        Some(response) => Ok(response),
        None => Err(provider_turn
            .terminal_error
            .expect("provider turn terminal")),
    }
}

impl RuntimeTaskLifecycle {
    pub fn new_snapshot(
        id: impl Into<String>,
        kind: RuntimeTaskKind,
        status: RuntimeTaskStatus,
        current_turn: u32,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            status,
            current_turn,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> RuntimeTaskKind {
        self.kind
    }

    pub fn status(&self) -> RuntimeTaskStatus {
        self.status
    }

    pub fn current_turn(&self) -> u32 {
        self.current_turn
    }

    pub fn payload(&self) -> Value {
        json!({
            "task_id": self.id,
            "kind": self.kind,
            "status": self.status,
            "turn": self.current_turn
        })
    }

    pub fn with_status(&self, status: RuntimeTaskStatus) -> Self {
        let mut task = self.clone();
        task.status = status;
        task
    }

    pub fn attach_to_event(&self, mut event: EventEnvelope) -> EventEnvelope {
        event.payload["task"] = self.payload();
        event
    }
}

impl RuntimeTaskStatus {
    fn from_run_status(status: RunStatus) -> Self {
        match status {
            RunStatus::Success => Self::Succeeded,
            RunStatus::Failed | RunStatus::VerificationFailed => Self::Failed,
            RunStatus::Cancelled => Self::Cancelled,
            RunStatus::ApprovalRequired => Self::ApprovalRequired,
            RunStatus::BudgetExhausted => Self::BudgetExhausted,
        }
    }
}

impl RuntimeTurnLifecycle {
    pub fn number(&self) -> u32 {
        self.number
    }

    pub fn started_event(
        self,
        events: &mut EventFactory,
        prompt: Option<&str>,
        task: &RuntimeTaskLifecycle,
    ) -> EventEnvelope {
        let mut event = events.turn_started(self.number, prompt);
        event.payload["task"] = task.payload();
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::session::AgentConversationContext;
    use crate::tool_execution::policy_for_tool_execution;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            mcp_servers: Vec::<McpServerConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("final provider response must not execute child agents")
    }

    #[test]
    fn provider_turn_error_handler_emits_failure_event_for_non_compaction_errors() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error(
                "DeepSeek provider error: quota".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config =
            context::ContextConfig::for_model_with_runtime(Some("deepseek-chat"), &runtime);
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("provider-error-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = Path::new(".");
        let mut compaction = RuntimeCompactionStep::new(
            ProviderKind::DeepSeek,
            &context_config,
            &provider_config,
            cwd,
            true,
            &hooks,
            &mut events,
            &mut sink,
            None,
        );
        let mut conversation = Conversation::new();

        let outcome = RuntimeProviderTurnStep::new()
            .handle_provider_error(&response, &mut compaction, &mut conversation, false)
            .expect("provider error handling succeeds");

        match outcome {
            RuntimeProviderErrorOutcome::Failed(error) => {
                assert_eq!(error, "DeepSeek provider error: quota");
            }
            _ => panic!("expected non-compaction provider error to fail"),
        }
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains("DeepSeek provider error: quota"));
    }

    #[test]
    fn provider_error_step_returns_failure_and_resets_reactive_state() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error(
                "DeepSeek provider error: quota".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config =
            context::ContextConfig::for_model_with_runtime(Some("deepseek-chat"), &runtime);
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("provider-error-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = Path::new(".");
        let mut compaction = RuntimeCompactionStep::new(
            ProviderKind::DeepSeek,
            &context_config,
            &provider_config,
            cwd,
            true,
            &hooks,
            &mut events,
            &mut sink,
            None,
        );
        let mut conversation = Conversation::new();
        let mut step = RuntimeProviderErrorStep::new();
        step.mark_reactive_compacted_for_test();

        let outcome = step
            .handle(&response, &mut compaction, &mut conversation)
            .expect("provider error step succeeds");

        match outcome {
            RuntimeProviderErrorStepOutcome::Failed(error) => {
                assert_eq!(error.status, RunStatus::Failed);
                assert_eq!(error.message, "DeepSeek provider error: quota");
            }
            _ => panic!("expected provider error step failure"),
        }
        assert!(!step.reactive_compacted_for_test());
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains("DeepSeek provider error: quota"));
    }

    #[test]
    fn provider_response_step_records_final_assistant_message() {
        let config = config();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("done".to_string())],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let cwd = tempfile::tempdir().expect("cwd");
        let mut events = EventFactory::new("provider-response-final".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("provider-response-final".to_string());
        let mut background_workflows = Vec::new();
        let policy = policy_for_tool_execution(&config);

        let outcome = RuntimeProviderResponseStep::new()
            .handle(
                response,
                &config,
                cwd.path(),
                &mut events,
                &mut sink,
                &mut conversation,
                None,
                AgentToolPolicyContext::unrestricted(),
                0,
                true,
                &policy,
                &instructions,
                &memory,
                &mcp_registry,
                &hooks,
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
                None,
                unused_child_executor::<Vec<u8>>,
                unused_child_executor::<crate::workflow::runner::SharedEventBuffer>,
                unused_child_executor::<io::Sink>,
            )
            .expect("handle provider response");

        match outcome {
            RuntimeProviderResponseOutcome::Success { final_message } => {
                assert_eq!(final_message.as_deref(), Some("done"));
            }
            _ => panic!("final response should complete the agent loop"),
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Assistant { content, tool_calls, .. }
                if content.as_deref() == Some("done") && tool_calls.is_empty())
        );
    }

    #[test]
    fn provider_response_or_terminal_returns_terminal_error() {
        let output = RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            message: "turn cancelled".to_string(),
        });

        let error = match provider_response_or_terminal(output) {
            Ok(_) => panic!("expected terminal error"),
            Err(error) => error,
        };

        assert_eq!(error.status, RunStatus::Cancelled);
        assert_eq!(error.message, "turn cancelled");
    }

    #[test]
    fn provider_turn_result_step_suppresses_cancelled_error_event() {
        let output = RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            message: "turn cancelled".to_string(),
        });
        let mut events = EventFactory::new("provider-turn-result".to_string());
        let mut emitted = Vec::new();
        let mut sink = EventSink::new(&mut emitted, OutputFormat::Jsonl);

        let outcome = RuntimeProviderTurnResultStep::new()
            .fold(output, &mut events, &mut sink, true)
            .expect("fold provider turn result");

        match outcome {
            RuntimeProviderTurnResultOutcome::Failed(error) => {
                assert_eq!(error.status, RunStatus::Cancelled);
                assert_eq!(error.message, "turn cancelled");
            }
            _ => panic!("expected cancelled provider turn to fail"),
        }
        drop(sink);
        assert!(emitted.is_empty());
    }

    #[test]
    fn agent_loop_result_constructors_preserve_terminal_shape() {
        let success = AgentLoopResult::success(Some("done".to_string()));
        assert_eq!(success.status, RunStatus::Success);
        assert_eq!(success.final_message.as_deref(), Some("done"));
        assert_eq!(success.error, None);

        let failure = AgentLoopResult::failure(RunStatus::Failed, "provider failed");
        assert_eq!(failure.status, RunStatus::Failed);
        assert_eq!(failure.final_message, None);
        assert_eq!(failure.error.as_deref(), Some("provider failed"));

        let terminal = AgentLoopResult::terminal(RunStatus::Cancelled, None);
        assert_eq!(terminal.status, RunStatus::Cancelled);
        assert_eq!(terminal.final_message, None);
        assert_eq!(terminal.error, None);
    }

    #[test]
    fn provider_response_result_step_folds_success_return_and_continue() {
        let success = RuntimeProviderResponseResultStep::new().fold(
            RuntimeProviderResponseOutcome::Success {
                final_message: Some("done".to_string()),
            },
        );
        match success {
            RuntimeProviderResponseResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("done"));
                assert_eq!(result.error, None);
            }
            RuntimeProviderResponseResult::Continue => panic!("success should return loop result"),
        }

        let terminal =
            RuntimeProviderResponseResultStep::new().fold(RuntimeProviderResponseOutcome::Return {
                status: RunStatus::Cancelled,
                error: Some("cancelled".to_string()),
            });
        match terminal {
            RuntimeProviderResponseResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Cancelled);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("cancelled"));
            }
            RuntimeProviderResponseResult::Continue => panic!("terminal outcome should return"),
        }

        let continuing =
            RuntimeProviderResponseResultStep::new().fold(RuntimeProviderResponseOutcome::Continue);
        assert!(matches!(
            continuing,
            RuntimeProviderResponseResult::Continue
        ));
    }

    #[test]
    fn turn_start_step_emits_started_event() {
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-start-step".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut events = EventFactory::new("turn-start-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        let result = RuntimeTurnStartStep::new()
            .start(&mut actor, &mut events, &mut sink, "hello", true)
            .expect("start turn");

        assert!(result.error.is_none());
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"turn.started\""));
        assert!(output.contains("hello"));
    }

    #[test]
    fn model_route_step_returns_provider_config_and_emits_event() {
        let mut lifecycle = RuntimeSessionLifecycle::new("model-route-step".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut events = EventFactory::new("model-route-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let provider_config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let mut cost_tracker = CostTracker::new(None);
        let model = ModelSelection::parse(None).expect("model");
        let subagent_type = SubagentType::General;

        let result = RuntimeModelRouteStep::new()
            .route(
                &mut actor,
                &model,
                &subagent_type,
                &provider_config,
                &mut cost_tracker,
                &mut events,
                &mut sink,
                true,
            )
            .expect("route model");

        assert_eq!(result.provider_config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            result.provider_config.model.as_deref(),
            Some(orca_core::model::PRO_MODEL)
        );
        assert_eq!(result.decision.actual_model, orca_core::model::PRO_MODEL);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"model.routed\""));
        assert!(output.contains(orca_core::model::PRO_MODEL));
    }

    #[test]
    fn turn_setup_step_builds_runtime_context_policy_and_provider_config() {
        let mut config = config();
        config.api_key = Some("test-key".to_string());
        config.model =
            ModelSelection::parse(Some(orca_core::model::FLASH_MODEL.to_string())).expect("model");
        config.model_runtime = ModelRuntimeConfig {
            context_window: Some(128_000),
            auto_compact_token_limit: Some(96_000),
        };
        let mcp_registry = McpRegistry::default();

        let setup = RuntimeTurnSetupStep::new().prepare(
            &config,
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &mcp_registry,
        );

        assert_eq!(setup.context_config.max_tokens, 128_000);
        assert_eq!(setup.context_config.effective_limit(), 96_000);
        assert_eq!(setup.provider_config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            setup.provider_config.model.as_deref(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert!(setup.provider_config.mcp_registry.is_some());
    }

    #[test]
    fn conversation_bootstrap_step_builds_owned_conversation_when_missing() {
        let config = config();
        let cwd = tempfile::tempdir().expect("cwd");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let context = AgentConversationContext::new();

        let mut prepared = RuntimeConversationBootstrapStep::new()
            .prepare(
                context,
                cwd.path(),
                "inspect repo",
                0,
                &SubagentType::General,
                &instructions,
                config.approval_mode,
                &memory,
                true,
            )
            .expect("prepare conversation");
        let conversation = prepared.conversation_mut();

        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], Message::System { .. }),
            "owned bootstrap should seed a system prompt"
        );
        assert!(
            matches!(&conversation.messages[1], Message::User { content, .. } if content == "inspect repo"),
            "owned bootstrap should seed the user prompt"
        );
        assert!(prepared.history_writer_mut().is_none());
    }
}
