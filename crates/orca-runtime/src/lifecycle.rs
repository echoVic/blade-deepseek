use std::io;
use std::path::{Path, PathBuf};

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
use orca_provider::ProviderConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookOutcome, HookRunner};
use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
};
use crate::tasks::TaskRegistry;
use crate::workflow::WorkflowDraftStore;

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
