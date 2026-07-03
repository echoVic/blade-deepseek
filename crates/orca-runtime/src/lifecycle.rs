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
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult, ToolStatus};
use orca_core::{
    cancel::CancelToken,
    config::{ProviderKind, RunConfig},
    conversation::Conversation,
};
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::compaction::RuntimeCompactionStep;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookOutcome, HookRunner};
use crate::memory::MemoryBlock;
use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};
use crate::provider_turn::{
    RuntimeProviderCycleInput, RuntimeTurnProviderCycleResult, RuntimeTurnProviderCycleStep,
};
use crate::runtime_normal_tool::{
    RuntimeNormalToolInvocation, execute_runtime_normal_tool_invocation,
};
use crate::session::{
    AgentConversationContext, bootstrap_agent_conversation_for_loop,
    record_initial_history_for_agent,
};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::policy_for_tool_execution;
use crate::tool_invocation::{AgentToolPolicyContext, provider_config_for_agent_loop};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;
use crate::{agent_child::ChildAgentExecutor, instructions::ProjectInstructions};
use orca_core::event_sink::EventSink;

pub use crate::runtime_special::{RuntimeSpecialToolDispatch, RuntimeWorkflowDraftRequest};

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
    pub(crate) permission_overlay: TurnPermissionOverlay,
}

pub(crate) struct RuntimeSteerStep;
pub(crate) struct RuntimeConversationBootstrapStep;
pub(crate) struct RuntimeTurnSetupStep;
pub(crate) struct RuntimeTurnOpeningStep;
pub(crate) struct RuntimeTurnStartStep;
pub(crate) struct RuntimeTurnStartResultStep;
pub(crate) struct RuntimeModelRouteStep;
pub(crate) struct RuntimeTurnIterationStep {
    opening_step: RuntimeTurnOpeningStep,
    provider_cycle_step: RuntimeTurnProviderCycleStep,
}
pub(crate) struct RuntimeTurnLoopStep {
    iteration_step: RuntimeTurnIterationStep,
}

pub(crate) struct RuntimeTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
}

pub(crate) struct RuntimeTurnIterationInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
}

pub(crate) struct RuntimeTurnLoopExecutors<W: io::Write> {
    pub(crate) child_executor: ChildAgentExecutor<W>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput<'a, 'runtime, W> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor: &'a mut RuntimeTaskActor<'runtime>,
        provider: ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        cwd: &'a Path,
        emit_deltas: bool,
        hooks: &'a HookRunner,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
        prompt: &'a str,
        model: &'a ModelSelection,
        subagent_type: &'a SubagentType,
        cost_tracker: &'a mut CostTracker,
        steer_handle: Option<&'a ThreadSteerHandle>,
        cancel: &'a CancelToken,
        max_budget_usd: Option<f64>,
        config: &'a RunConfig,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        task_registry: &'a TaskRegistry,
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        Self {
            actor,
            provider,
            context_config,
            provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            prepared_conversation,
            prompt,
            model,
            subagent_type,
            cost_tracker,
            steer_handle,
            cancel,
            max_budget_usd,
            config,
            tool_policy,
            subagent_depth,
            policy,
            instructions,
            memory,
            mcp_registry,
            task_registry,
            background_workflows,
            workflow_ipc,
            permission_handler,
        }
    }

    pub(crate) fn iteration_input<'iter>(
        &'iter mut self,
    ) -> RuntimeTurnIterationInput<'iter, 'runtime, W> {
        RuntimeTurnIterationInput {
            actor: &mut *self.actor,
            provider: self.provider,
            context_config: self.context_config,
            provider_config: self.provider_config,
            cwd: self.cwd,
            emit_deltas: self.emit_deltas,
            hooks: self.hooks,
            events: &mut *self.events,
            sink: &mut *self.sink,
            prepared_conversation: &mut *self.prepared_conversation,
            prompt: self.prompt,
            model: self.model,
            subagent_type: self.subagent_type,
            cost_tracker: &mut *self.cost_tracker,
            steer_handle: self.steer_handle,
            cancel: self.cancel,
            max_budget_usd: self.max_budget_usd,
            config: self.config,
            tool_policy: self.tool_policy,
            subagent_depth: self.subagent_depth,
            policy: self.policy,
            instructions: self.instructions,
            memory: self.memory,
            mcp_registry: self.mcp_registry,
            task_registry: self.task_registry,
            background_workflows: &mut *self.background_workflows,
            workflow_ipc: self.workflow_ipc,
            permission_handler: self.permission_handler,
        }
    }
}

impl<W: io::Write> RuntimeTurnLoopExecutors<W> {
    pub(crate) fn new(
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> Self {
        Self {
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        }
    }
}

pub(crate) struct RuntimeTurnStartStepOutput {
    pub(crate) error: Option<RuntimeTurnStartError>,
}

pub(crate) enum RuntimeTurnStartResult {
    Continue,
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeTurnOpeningResult {
    Continue { provider_config: ProviderConfig },
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeTurnIterationResult {
    ContinueLoop,
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

pub(crate) struct AllowRequestedPermissions;

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
    network_domain_permissions:
        std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
    strict_auto_review: bool,
}

impl TurnPermissionOverlay {
    pub fn additional_working_directories(&self) -> &[PathBuf] {
        &self.additional_working_directories
    }

    pub fn network_domain_permissions(
        &self,
    ) -> &std::collections::HashMap<String, orca_core::config::PermissionProfileNetworkAccess> {
        &self.network_domain_permissions
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
        for (domain, access) in &other.network_domain_permissions {
            self.network_domain_permissions
                .insert(domain.clone(), *access);
        }
        self.strict_auto_review |= other.strict_auto_review;
    }

    pub(crate) fn merge_network_permissions(&mut self, permissions: &RequestPermissionProfile) {
        if let Some(network) = permissions.network.as_ref() {
            for (domain, access) in &network.domains {
                self.network_domain_permissions
                    .insert(domain.clone(), *access);
            }
        }
    }

    pub(crate) fn merge_strict_auto_review(&mut self, strict_auto_review: bool) {
        self.strict_auto_review |= strict_auto_review;
    }

    pub fn request_and_merge(
        &mut self,
        handler: &dyn RuntimePermissionRequestHandler,
        request: RuntimePermissionRequest,
    ) -> io::Result<RuntimePermissionResponse> {
        let response = handler.request_permissions(&request)?;
        if response.decision == PermissionResponseDecision::Allow {
            self.merge_permissions(&response.permissions);
            self.merge_strict_auto_review(response.strict_auto_review);
        }
        Ok(response)
    }

    pub(crate) fn merge_permissions(&mut self, permissions: &RequestPermissionProfile) {
        if let Some(file_system) = permissions.file_system.as_ref() {
            if let Some(write_roots) = file_system.write.as_ref() {
                for root in write_roots {
                    if !root.as_os_str().is_empty()
                        && !self.additional_working_directories.contains(root)
                    {
                        self.additional_working_directories.push(root.clone());
                    }
                }
            }
        }
        self.merge_network_permissions(permissions);
    }
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
            None,
            request,
            cwd,
            &[],
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_roots_and_cancel(
        &mut self,
        config: Option<&RunConfig>,
        request: &ToolRequest,
        cwd: &Path,
        additional_roots: &[PathBuf],
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
        permission_handler: Option<&dyn RuntimePermissionRequestHandler>,
    ) -> ToolResult {
        self.execute_normal_tool_invocation(RuntimeNormalToolInvocation {
            config,
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
            permission_handler,
        })
    }

    pub(crate) fn execute_normal_tool_invocation(
        &mut self,
        invocation: RuntimeNormalToolInvocation<'_>,
    ) -> ToolResult {
        execute_runtime_normal_tool_invocation(invocation, None)
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
    #[cfg(test)]
    pub(crate) fn conversation_mut(&mut self) -> &mut Conversation {
        match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        }
    }

    #[cfg(test)]
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

    pub fn granted_additional_working_directories(&self) -> Vec<PathBuf> {
        self.permission_overlay
            .additional_working_directories
            .clone()
    }

    pub fn permission_overlay(&self) -> &TurnPermissionOverlay {
        &self.permission_overlay
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
        self.execute_normal_tool_with_roots_and_cancel(
            None,
            request,
            cwd,
            &[],
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            None,
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
            None,
            request,
            cwd,
            &[],
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_roots_and_cancel(
        &mut self,
        config: Option<&RunConfig>,
        request: &ToolRequest,
        cwd: &Path,
        additional_roots: &[PathBuf],
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
        permission_handler: Option<&dyn RuntimePermissionRequestHandler>,
    ) -> ToolResult {
        self.execute_normal_tool_invocation(RuntimeNormalToolInvocation {
            config,
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
            permission_handler,
        })
    }

    pub(crate) fn execute_normal_tool_invocation(
        &mut self,
        invocation: RuntimeNormalToolInvocation<'_>,
    ) -> ToolResult {
        execute_runtime_normal_tool_invocation(invocation, Some(&mut self.permission_overlay))
    }

    pub fn execute_user_input_tool(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimeUserInputHandler,
    ) -> io::Result<ToolResult> {
        self.actor().execute_user_input_tool(request, handler)
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

impl RuntimeTurnStartResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(&self, output: RuntimeTurnStartStepOutput) -> RuntimeTurnStartResult {
        match output.error {
            Some(error) => RuntimeTurnStartResult::Return(AgentLoopResult::failure(
                error.status,
                error.message,
            )),
            None => RuntimeTurnStartResult::Continue,
        }
    }
}

impl RuntimeTurnOpeningStep {
    pub(crate) fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open<W: io::Write>(
        &mut self,
        actor: &mut RuntimeTaskActor<'_>,
        provider: ProviderKind,
        context_config: &context::ContextConfig,
        provider_config: &ProviderConfig,
        cwd: &Path,
        emit_deltas: bool,
        hooks: &HookRunner,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        conversation: &mut Conversation,
        mut history_writer: Option<&mut SessionWriter>,
        prompt: &str,
        model: &ModelSelection,
        subagent_type: &SubagentType,
        cost_tracker: &mut CostTracker,
        steer_handle: Option<&ThreadSteerHandle>,
    ) -> io::Result<RuntimeTurnOpeningResult> {
        RuntimeCompactionStep::new(
            provider,
            context_config,
            provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            history_writer.as_deref_mut(),
        )
        .compact_if_needed(conversation)?;

        match RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStep::new().start(
            actor,
            events,
            sink,
            prompt,
            emit_deltas,
        )?) {
            RuntimeTurnStartResult::Return(result) => {
                return Ok(RuntimeTurnOpeningResult::Return(result));
            }
            RuntimeTurnStartResult::Continue => {}
        }

        let turn_provider_config = RuntimeModelRouteStep::new()
            .route(
                actor,
                model,
                subagent_type,
                provider_config,
                cost_tracker,
                events,
                sink,
                emit_deltas,
            )?
            .provider_config;

        RuntimeSteerStep::new().apply(steer_handle, conversation, history_writer.as_deref_mut())?;

        Ok(RuntimeTurnOpeningResult::Continue {
            provider_config: turn_provider_config,
        })
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

impl RuntimeTurnIterationStep {
    pub(crate) fn new() -> Self {
        Self {
            opening_step: RuntimeTurnOpeningStep::new(),
            provider_cycle_step: RuntimeTurnProviderCycleStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        input: RuntimeTurnIterationInput<'_, '_, W>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeTurnIterationResult> {
        let turn_provider_config = {
            let (conversation, history_writer) = input.prepared_conversation.parts_mut();
            match self.opening_step.open(
                input.actor,
                input.provider,
                input.context_config,
                input.provider_config,
                input.cwd,
                input.emit_deltas,
                input.hooks,
                input.events,
                input.sink,
                conversation,
                history_writer,
                input.prompt,
                input.model,
                input.subagent_type,
                input.cost_tracker,
                input.steer_handle,
            )? {
                RuntimeTurnOpeningResult::Continue { provider_config } => provider_config,
                RuntimeTurnOpeningResult::Return(result) => {
                    return Ok(RuntimeTurnIterationResult::Return(result));
                }
            }
        };

        match self.provider_cycle_step.run(
            RuntimeProviderCycleInput {
                actor: input.actor,
                provider: input.provider,
                turn_provider_config: &turn_provider_config,
                cwd: input.cwd,
                context_config: input.context_config,
                base_provider_config: input.provider_config,
                emit_deltas: input.emit_deltas,
                hooks: input.hooks,
                cancel: input.cancel,
                cost_tracker: input.cost_tracker,
                max_budget_usd: input.max_budget_usd,
                events: input.events,
                sink: input.sink,
                conversation: input.prepared_conversation,
                config: input.config,
                tool_policy: input.tool_policy,
                subagent_depth: input.subagent_depth,
                policy: input.policy,
                instructions: input.instructions,
                memory: input.memory,
                mcp_registry: input.mcp_registry,
                task_registry: input.task_registry,
                background_workflows: input.background_workflows,
                workflow_ipc: input.workflow_ipc,
                permission_handler: input.permission_handler,
                steer_handle: input.steer_handle,
            },
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        )? {
            RuntimeTurnProviderCycleResult::ContinueLoop
            | RuntimeTurnProviderCycleResult::ContinueTurn => {
                Ok(RuntimeTurnIterationResult::ContinueLoop)
            }
            RuntimeTurnProviderCycleResult::Return(result) => {
                Ok(RuntimeTurnIterationResult::Return(result))
            }
        }
    }
}

impl RuntimeTurnLoopStep {
    pub(crate) fn new() -> Self {
        Self {
            iteration_step: RuntimeTurnIterationStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnLoopInput<'_, '_, W>,
        executors: RuntimeTurnLoopExecutors<W>,
    ) -> io::Result<AgentLoopResult> {
        loop {
            match self.iteration_step.run(
                input.iteration_input(),
                executors.child_executor,
                executors.workflow_child_executor,
                executors.batch_child_executor,
            )? {
                RuntimeTurnIterationResult::ContinueLoop => {
                    continue;
                }
                RuntimeTurnIterationResult::Return(result) => return Ok(result),
            }
        }
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
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    use crate::session::AgentConversationContext;

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
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
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
            external_tools: Vec::new(),
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
    fn turn_start_result_step_folds_error_and_continue() {
        let continuing =
            RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStepOutput { error: None });
        assert!(matches!(continuing, RuntimeTurnStartResult::Continue));

        let failed = RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStepOutput {
            error: Some(RuntimeTurnStartError {
                status: RunStatus::Failed,
                message: "max turns exceeded".to_string(),
            }),
        });
        match failed {
            RuntimeTurnStartResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("max turns exceeded"));
            }
            RuntimeTurnStartResult::Continue => {
                panic!("turn-start error should return loop result")
            }
        }
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
    fn turn_opening_step_compacts_starts_routes_and_steers() {
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-opening-step".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut events = EventFactory::new("turn-opening-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let provider_config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config =
            context::ContextConfig::for_model_with_runtime(Some("deepseek-chat"), &runtime);
        let hooks = HookRunner::default();
        let mut conversation = Conversation::new();
        let mut cost_tracker = CostTracker::new(None);
        let model = ModelSelection::parse(None).expect("model");
        let subagent_type = SubagentType::General;
        let cwd = Path::new(".");

        let result = RuntimeTurnOpeningStep::new()
            .open(
                &mut actor,
                ProviderKind::DeepSeek,
                &context_config,
                &provider_config,
                cwd,
                true,
                &hooks,
                &mut events,
                &mut sink,
                &mut conversation,
                None,
                "hello",
                &model,
                &subagent_type,
                &mut cost_tracker,
                None,
            )
            .expect("open turn");

        match result {
            RuntimeTurnOpeningResult::Continue { provider_config } => {
                assert_eq!(provider_config.api_key.as_deref(), Some("test-key"));
                assert_eq!(
                    provider_config.model.as_deref(),
                    Some(orca_core::model::PRO_MODEL)
                );
            }
            RuntimeTurnOpeningResult::Return(_) => panic!("opening should continue"),
        }
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"turn.started\""));
        assert!(output.contains("\"type\":\"model.routed\""));
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
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
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
