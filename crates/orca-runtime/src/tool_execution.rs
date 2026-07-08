use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::ApprovalDecision;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::tool_types;
use orca_mcp::McpRegistry;

use crate::agent_child::ChildAgentExecutor;
use crate::agent_common;
use crate::cost::CostTracker;
use crate::extension::{
    ExtensionRegistry, RuntimeExtensionStores, ToolCallOutcome, ToolFinishInput, ToolStartInput,
};
use crate::hooks::{HookOutcome, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeApprovalDecision, RuntimeConfigApprovalHandler, RuntimePermissionRequestHandler,
    RuntimeTaskActor, RuntimeToolActorContext, RuntimeUserInputHandler, TurnPermissionOverlay,
};
use crate::memory::MemoryBlock;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::{
    ToolInvocation, apply_pre_tool_outcome, approval_request_for_invocation,
    prepare_tool_invocation, validate_tool_invocation,
};
use crate::tool_router::{RuntimeToolInvocationContext, RuntimeToolRouter};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

const DEFAULT_TOOL_MAX_TURNS: u32 = 128;

pub(crate) struct ToolExecutionContext<'a> {
    cwd: &'a Path,
    subagent_depth: u32,
    emit_deltas: bool,
    policy: &'a ApprovalPolicy,
    instructions: Option<&'a ProjectInstructions>,
    memory: Option<&'a MemoryBlock>,
    mcp_registry: Option<&'a McpRegistry>,
    hooks: Option<&'a HookRunner>,
    cost_tracker: Option<&'a mut CostTracker>,
    cancel: Option<&'a CancelToken>,
    task_registry: Option<&'a TaskRegistry>,
    background_workflows: Option<&'a mut Vec<BackgroundWorkflowRun>>,
    workflow_ipc: Option<&'a WorkflowIpcContext>,
    permission_overlay: Option<&'a mut TurnPermissionOverlay>,
    permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    extension_registry: Option<&'a ExtensionRegistry>,
    extension_stores: Option<RuntimeExtensionStores<'a>>,
}

pub(crate) struct ToolApprovalGateContext<'a, W: io::Write> {
    pub(crate) config: &'a RunConfig,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) tool_request: &'a tool_types::ToolRequest,
    pub(crate) invocation: &'a ToolInvocation,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
    pub(crate) emit_deltas: bool,
}

pub(crate) struct ToolExecutionActor {
    runtime: RuntimeToolActorContext,
}

pub(crate) fn policy_for_tool_execution(config: &RunConfig) -> ApprovalPolicy {
    ApprovalPolicy::new(config.approval_mode).with_permission_rules(config.permission_rules.clone())
}

impl<'a> ToolExecutionContext<'a> {
    pub(crate) fn new(
        cwd: &'a Path,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &'a ApprovalPolicy,
    ) -> Self {
        Self {
            cwd,
            subagent_depth,
            emit_deltas,
            policy,
            instructions: None,
            memory: None,
            mcp_registry: None,
            hooks: None,
            cost_tracker: None,
            cancel: None,
            task_registry: None,
            background_workflows: None,
            workflow_ipc: None,
            permission_overlay: None,
            permission_handler: None,
            user_input_handler: None,
            extension_registry: None,
            extension_stores: None,
        }
    }

    pub(crate) fn with_services(
        mut self,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
    ) -> Self {
        self.instructions = Some(instructions);
        self.memory = Some(memory);
        self.mcp_registry = Some(mcp_registry);
        self.hooks = Some(hooks);
        self
    }

    pub(crate) fn with_permission_overlay(
        mut self,
        permission_overlay: &'a mut TurnPermissionOverlay,
    ) -> Self {
        self.permission_overlay = Some(permission_overlay);
        self
    }

    pub(crate) fn with_permission_handler(
        mut self,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        self.permission_handler = permission_handler;
        self
    }

    pub(crate) fn with_user_input_handler(
        mut self,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    ) -> Self {
        self.user_input_handler = user_input_handler;
        self
    }

    pub(crate) fn with_extensions(
        mut self,
        extension_registry: &'a ExtensionRegistry,
        extension_stores: RuntimeExtensionStores<'a>,
    ) -> Self {
        self.extension_registry = Some(extension_registry);
        self.extension_stores = Some(extension_stores);
        self
    }

    pub(crate) fn with_runtime(
        mut self,
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
    ) -> Self {
        self.cost_tracker = Some(cost_tracker);
        self.cancel = Some(cancel);
        self.task_registry = Some(task_registry);
        self.background_workflows = Some(background_workflows);
        self.workflow_ipc = workflow_ipc;
        self
    }

    #[cfg(test)]
    pub(crate) fn cwd(&self) -> &'a Path {
        self.cwd
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
    pub(crate) fn policy(&self) -> &'a ApprovalPolicy {
        self.policy
    }

    #[cfg(test)]
    pub(crate) fn instructions(&self) -> &'a ProjectInstructions {
        self.instructions.expect("tool execution instructions")
    }

    #[cfg(test)]
    pub(crate) fn memory(&self) -> &'a MemoryBlock {
        self.memory.expect("tool execution memory")
    }

    #[cfg(test)]
    pub(crate) fn mcp_registry(&self) -> &'a McpRegistry {
        self.mcp_registry.expect("tool execution mcp registry")
    }

    #[cfg(test)]
    pub(crate) fn hooks(&self) -> &'a HookRunner {
        self.hooks.expect("tool execution hooks")
    }

    #[cfg(test)]
    pub(crate) fn cost_tracker(&self) -> &CostTracker {
        self.cost_tracker
            .as_deref()
            .expect("tool execution cost tracker")
    }

    #[cfg(test)]
    pub(crate) fn cancel(&self) -> &'a CancelToken {
        self.cancel.expect("tool execution cancel token")
    }

    #[cfg(test)]
    pub(crate) fn task_registry(&self) -> &'a TaskRegistry {
        self.task_registry.expect("tool execution task registry")
    }

    #[cfg(test)]
    pub(crate) fn background_workflow_count(&self) -> usize {
        self.background_workflows
            .as_deref()
            .expect("tool execution background workflows")
            .len()
    }

    #[cfg(test)]
    pub(crate) fn workflow_ipc(&self) -> Option<&'a WorkflowIpcContext> {
        self.workflow_ipc
    }
}

pub(crate) fn execute_tool_with_approval<W: io::Write>(
    config: &RunConfig,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    tool_request: &tool_types::ToolRequest,
    context: ToolExecutionContext<'_>,
    child_executor: ChildAgentExecutor<W>,
    workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
) -> io::Result<(RunStatus, tool_types::ToolResult)> {
    let mut actor = ToolExecutionActor::new(events.run_id().to_string(), DEFAULT_TOOL_MAX_TURNS);
    actor.execute(
        config,
        events,
        sink,
        tool_request,
        context,
        child_executor,
        workflow_child_executor,
    )
}

impl ToolExecutionActor {
    pub(crate) fn new(run_id: impl Into<String>, max_turns: u32) -> Self {
        Self {
            runtime: RuntimeToolActorContext::new(run_id, max_turns),
        }
    }

    #[cfg(test)]
    pub(crate) fn active_task(&self) -> Option<&crate::lifecycle::RuntimeTaskLifecycle> {
        self.runtime.active_task()
    }

    fn resolve_tool_approval(
        &mut self,
        policy: &ApprovalPolicy,
        approval: Option<orca_core::approval_types::ApprovalRequest>,
        request: &tool_types::ToolRequest,
    ) -> RuntimeApprovalDecision {
        self.runtime
            .resolve_tool_approval(policy, approval, request)
    }

    fn run_pre_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &tool_types::ToolRequest,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, tool_types::ToolResult> {
        self.runtime
            .run_pre_tool_hook_with_cancel(hooks, cwd, request, cancel)
    }

    fn run_post_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &tool_types::ToolRequest,
        result: &tool_types::ToolResult,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        self.runtime
            .run_post_tool_hook_with_cancel(hooks, cwd, request, result, cancel)
    }

    pub(crate) fn execute<W: io::Write>(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        tool_request: &tool_types::ToolRequest,
        context: ToolExecutionContext<'_>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    ) -> io::Result<(RunStatus, tool_types::ToolResult)> {
        let ToolExecutionContext {
            cwd,
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
            permission_overlay,
            permission_handler,
            user_input_handler,
            extension_registry,
            extension_stores,
        } = context;
        let instructions = instructions.expect("tool execution instructions");
        let memory = memory.expect("tool execution memory");
        let mcp_registry = mcp_registry.expect("tool execution mcp registry");
        let hooks = hooks.expect("tool execution hooks");
        let cost_tracker = cost_tracker.expect("tool execution cost tracker");
        let cancel = cancel.expect("tool execution cancel token");
        let task_registry = task_registry.expect("tool execution task registry");
        let background_workflows =
            background_workflows.expect("tool execution background workflows");
        let permission_overlay = permission_overlay.expect("tool execution permission overlay");
        let invocation =
            prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config);
        if let Err(error) = validate_tool_invocation(&invocation, mcp_registry, config) {
            if emit_deltas {
                emit_tool_call_requested(events, sink, tool_request)?;
            }
            let result = error.into_result();
            if emit_deltas {
                emit_tool_call_completed(events, sink, tool_request, &result)?;
            }
            return Ok((RunStatus::Failed, result));
        }

        if let Some(outcome) = self.handle_approval(ToolApprovalGateContext {
            config,
            events,
            sink,
            tool_request,
            invocation: &invocation,
            policy,
            permission_overlay,
            emit_deltas,
        })? {
            return Ok(outcome);
        }

        if emit_deltas {
            emit_tool_call_requested(events, sink, tool_request)?;
        }
        let cwd_display = cwd.display().to_string();
        let invocation = match self.apply_pre_tool_hook(
            config,
            events,
            sink,
            tool_request,
            invocation,
            hooks,
            &cwd_display,
            mcp_registry,
            emit_deltas,
            Some(cancel),
        )? {
            Ok(invocation) => invocation,
            Err(outcome) => return Ok(outcome),
        };
        let execution_request = &invocation.effective;
        if let (Some(registry), Some(extension_stores)) = (extension_registry, extension_stores) {
            registry.on_tool_start(ToolStartInput {
                thread_store: extension_stores.thread_store(),
                turn_store: extension_stores.turn_store(),
                tool_name: execution_request.name.as_str(),
                call_id: &execution_request.id,
            });
        }
        let result =
            match RuntimeToolRouter::new(&mut self.runtime).dispatch(RuntimeToolInvocationContext {
                config,
                cwd,
                events,
                sink,
                execution_request,
                subagent_depth,
                instructions,
                memory,
                mcp_registry,
                hooks,
                emit_deltas,
                cost_tracker,
                cancel,
                task_registry,
                background_workflows,
                workflow_ipc,
                permission_overlay,
                permission_handler,
                user_input_handler,
                extension_stores,
                child_executor,
                workflow_child_executor,
            }) {
                Ok(result) => result,
                Err(error) => {
                    if let (Some(registry), Some(extension_stores)) =
                        (extension_registry, extension_stores)
                    {
                        registry.on_tool_finish(ToolFinishInput {
                            thread_store: extension_stores.thread_store(),
                            turn_store: extension_stores.turn_store(),
                            tool_name: execution_request.name.as_str(),
                            call_id: &execution_request.id,
                            outcome: ToolCallOutcome::Aborted,
                        });
                    }
                    return Err(error);
                }
            };
        if let (Some(registry), Some(extension_stores)) = (extension_registry, extension_stores) {
            registry.on_tool_finish(ToolFinishInput {
                thread_store: extension_stores.thread_store(),
                turn_store: extension_stores.turn_store(),
                tool_name: execution_request.name.as_str(),
                call_id: &execution_request.id,
                outcome: tool_call_outcome_for_result(&result),
            });
        }
        self.finish_tool_result(
            events,
            sink,
            execution_request,
            &result,
            hooks,
            &cwd_display,
            emit_deltas,
            Some(cancel),
        )
    }

    pub(crate) fn handle_approval<W: io::Write>(
        &mut self,
        context: ToolApprovalGateContext<'_, W>,
    ) -> io::Result<Option<(RunStatus, tool_types::ToolResult)>> {
        let ToolApprovalGateContext {
            config,
            events,
            sink,
            tool_request,
            invocation,
            policy,
            permission_overlay,
            emit_deltas,
        } = context;

        if let Some(approval) = approval_request_for_invocation(invocation)
            && agent_common::requires_approval(approval.action)
        {
            let preapproved = permission_overlay.consume_preapproved_tool_call_id(&tool_request.id);
            let mut approval_decision = if preapproved {
                RuntimeApprovalDecision::Allowed(orca_core::approval_types::ApprovalResolution {
                    id: approval.id.clone(),
                    decision: ApprovalDecision::Allow,
                    reason: "approved background continuation".to_string(),
                })
            } else {
                self.resolve_tool_approval(policy, Some(approval.clone()), tool_request)
            };
            if !preapproved
                && permission_overlay.strict_auto_review()
                && matches!(approval_decision, RuntimeApprovalDecision::Allowed(_))
            {
                approval_decision = RuntimeApprovalDecision::Ask(approval.clone());
            }
            if emit_deltas {
                sink.emit(&events.approval_requested(&approval))?;
            }

            match approval_decision {
                RuntimeApprovalDecision::Allowed(resolution) => {
                    if emit_deltas {
                        sink.emit(&events.approval_resolved(&resolution))?;
                    }
                }
                RuntimeApprovalDecision::Ask(approval) => {
                    let handler = RuntimeConfigApprovalHandler::new(config);
                    let final_resolution = self.runtime.resolve_interactive_tool_approval(
                        &handler,
                        &approval,
                        tool_request,
                    )?;
                    if emit_deltas {
                        sink.emit(&events.approval_resolved(&final_resolution))?;
                    }
                    if final_resolution.decision == ApprovalDecision::Deny {
                        if emit_deltas {
                            emit_tool_call_requested(events, sink, tool_request)?;
                        }
                        let result =
                            tool_types::ToolResult::denied(tool_request, final_resolution.reason);
                        if emit_deltas {
                            emit_tool_call_completed(events, sink, tool_request, &result)?;
                        }
                        return Ok(Some((RunStatus::ApprovalRequired, result)));
                    }
                }
                RuntimeApprovalDecision::Denied { resolution, result } => {
                    if emit_deltas {
                        sink.emit(&events.approval_resolved(&resolution))?;
                        emit_tool_call_requested(events, sink, tool_request)?;
                    }
                    if emit_deltas {
                        emit_tool_call_completed(events, sink, tool_request, &result)?;
                    }
                    return Ok(Some((RunStatus::ApprovalRequired, result)));
                }
                RuntimeApprovalDecision::NotRequired => {}
            }
        }
        Ok(None)
    }

    fn apply_pre_tool_hook(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<impl io::Write>,
        tool_request: &tool_types::ToolRequest,
        invocation: ToolInvocation,
        hooks: &HookRunner,
        cwd_display: &str,
        mcp_registry: &McpRegistry,
        emit_deltas: bool,
        cancel: Option<&CancelToken>,
    ) -> io::Result<Result<ToolInvocation, (RunStatus, tool_types::ToolResult)>> {
        let pre_tool_outcome =
            match self.run_pre_tool_hook(hooks, cwd_display, tool_request, cancel) {
                Ok(outcome) => outcome,
                Err(result) => {
                    if emit_deltas {
                        emit_tool_call_completed(events, sink, tool_request, &result)?;
                    }
                    return Ok(Err((RunStatus::Failed, result)));
                }
            };
        match apply_pre_tool_outcome(invocation, &pre_tool_outcome, mcp_registry, config) {
            Ok(invocation) => Ok(Ok(invocation)),
            Err(error) => {
                let result = error.into_result();
                if emit_deltas {
                    emit_tool_call_completed(events, sink, tool_request, &result)?;
                }
                Ok(Err((RunStatus::Failed, result)))
            }
        }
    }

    fn finish_tool_result(
        &mut self,
        events: &mut EventFactory,
        sink: &mut EventSink<impl io::Write>,
        execution_request: &tool_types::ToolRequest,
        result: &tool_types::ToolResult,
        hooks: &HookRunner,
        cwd_display: &str,
        emit_deltas: bool,
        cancel: Option<&CancelToken>,
    ) -> io::Result<(RunStatus, tool_types::ToolResult)> {
        let is_failure = matches!(
            result.status,
            tool_types::ToolStatus::Failed | tool_types::ToolStatus::Denied
        );
        if emit_deltas {
            emit_tool_call_completed(events, sink, execution_request, result)?;
            if execution_request.name == tool_types::ToolName::UpdatePlan
                && result.status == tool_types::ToolStatus::Completed
            {
                match orca_tools::update_plan::parse_args(execution_request) {
                    Ok(update) => sink.emit(&events.plan_updated(&update))?,
                    Err(error) => {
                        sink.emit(&events.error(&format!("failed to render plan update: {error}")))?
                    }
                }
            }
            if let Some(warning) =
                self.run_post_tool_hook(hooks, cwd_display, execution_request, result, cancel)
            {
                sink.emit(&events.error(&warning))?;
            }
        }

        let status = if is_failure {
            RunStatus::Failed
        } else {
            RunStatus::Success
        };

        Ok((status, result.clone()))
    }
}

fn tool_call_outcome_for_result(result: &tool_types::ToolResult) -> ToolCallOutcome {
    match result.status {
        tool_types::ToolStatus::Completed => ToolCallOutcome::Completed,
        tool_types::ToolStatus::Failed => ToolCallOutcome::Failed {
            handler_executed: true,
        },
        tool_types::ToolStatus::NotImplemented => ToolCallOutcome::Failed {
            handler_executed: false,
        },
        tool_types::ToolStatus::Denied => ToolCallOutcome::Blocked,
    }
}

fn emit_tool_call_requested(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    request: &tool_types::ToolRequest,
) -> io::Result<()> {
    let event = RuntimeTaskActor::tool_call_requested_event_for(events, request);
    sink.emit(&event)
}

fn emit_tool_call_completed(
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    request: &tool_types::ToolRequest,
    result: &tool_types::ToolResult,
) -> io::Result<()> {
    let event = RuntimeTaskActor::tool_call_completed_event_for(events, request, result);
    sink.emit(&event)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use orca_core::approval_rules::{PermissionRule, PermissionRules};
    use orca_core::approval_types::{
        ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest, Decision,
    };
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::EventSink;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
    use orca_mcp::McpRegistry;

    use super::{
        ToolApprovalGateContext, ToolExecutionActor, ToolExecutionContext,
        policy_for_tool_execution,
    };
    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::extension::{
        ExtensionData, ExtensionRegistryBuilder, RuntimeExtensionStores, ToolFinishInput,
        ToolLifecycleContributor, ToolStartInput,
    };
    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::{
        RuntimeUserInputHandler, RuntimeUserInputRequest, TurnPermissionOverlay,
    };
    use crate::memory::MemoryBlock;
    use crate::tasks::TaskRegistry;

    fn config_with_permission_rules(permission_rules: PermissionRules) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: "test".to_string(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
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
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules,
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
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
        panic!("read_file tool execution must not execute child agents")
    }

    #[test]
    fn policy_for_tool_execution_preserves_config_permission_rules() {
        let config = config_with_permission_rules(PermissionRules {
            rules: vec![PermissionRule::new("bash", "rm *", Decision::Deny)],
        });
        let request = ApprovalRequest {
            id: "approval-tool-1".to_string(),
            action: ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("rm scratch.txt".to_string()),
            preview: None,
        };

        let policy = policy_for_tool_execution(&config);
        let resolution = policy.resolve_for_tool(&request, "bash", Some("rm scratch.txt"));

        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert!(resolution.reason.contains("permission deny rule"));
    }

    #[test]
    fn approval_gate_consumes_matching_preapproved_tool_call_once() {
        let config = config_with_permission_rules(PermissionRules::default());
        let mut events = EventFactory::new("preapproved-tool".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = ToolRequest {
            id: "shell-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let registry = McpRegistry::default();
        let invocation =
            crate::tool_invocation::prepare_tool_invocation(&request, 0, &registry, &config);
        let policy = policy_for_tool_execution(&config);
        let mut overlay = TurnPermissionOverlay::default();
        overlay.set_preapproved_tool_call_id(Some("shell-1".to_string()));
        let mut actor = ToolExecutionActor::new(events.run_id().to_string(), 128);

        let result = actor
            .handle_approval(ToolApprovalGateContext {
                config: &config,
                events: &mut events,
                sink: &mut sink,
                tool_request: &request,
                invocation: &invocation,
                policy: &policy,
                permission_overlay: &mut overlay,
                emit_deltas: true,
            })
            .expect("approval gate");

        assert!(result.is_none());
        assert!(!overlay.consume_preapproved_tool_call_id("shell-1"));
    }

    #[test]
    fn tool_execution_notifies_extension_lifecycle_for_completed_tool() {
        #[derive(Debug)]
        struct RecordingContributor {
            events: Arc<Mutex<Vec<String>>>,
        }

        impl ToolLifecycleContributor for RecordingContributor {
            fn on_tool_start(&self, input: ToolStartInput<'_>) {
                self.events.lock().unwrap().push(format!(
                    "start:{}:{}:{}",
                    input.thread_store.level_id(),
                    input.turn_store.level_id(),
                    input.call_id
                ));
            }

            fn on_tool_finish(&self, input: ToolFinishInput<'_>) {
                self.events.lock().unwrap().push(format!(
                    "finish:{}:{}:{}:{:?}",
                    input.thread_store.level_id(),
                    input.turn_store.level_id(),
                    input.call_id,
                    input.outcome
                ));
            }
        }

        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        let mut config = config_with_permission_rules(PermissionRules::default());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("extension-tool-execution".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = ToolRequest {
            id: "read-file".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("extension-tool-execution".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let lifecycle_events = Arc::new(Mutex::new(Vec::new()));
        let mut extension_builder = ExtensionRegistryBuilder::new();
        extension_builder.tool_lifecycle_contributor(Arc::new(RecordingContributor {
            events: Arc::clone(&lifecycle_events),
        }));
        let extension_registry = Arc::new(extension_builder.build());
        let thread_store = Arc::new(ExtensionData::new("thread-1"));
        let turn_store = Arc::new(ExtensionData::new("turn-1"));
        let context = ToolExecutionContext::new(cwd.path(), 0, true, &policy)
            .with_services(&instructions, &memory, &registry, &hooks)
            .with_runtime(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
            )
            .with_permission_overlay(&mut permission_overlay)
            .with_extensions(
                &extension_registry,
                RuntimeExtensionStores::new(&thread_store, &turn_store),
            );

        let mut actor = ToolExecutionActor::new(events.run_id().to_string(), 128);
        let (status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                context,
                unused_child_executor,
                unused_child_executor,
            )
            .expect("execute read_file");

        assert_eq!(status, RunStatus::Success);
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            lifecycle_events.lock().unwrap().as_slice(),
            [
                "start:thread-1:turn-1:read-file",
                "finish:thread-1:turn-1:read-file:Completed"
            ]
        );
    }

    #[test]
    fn tool_execution_routes_request_user_input_through_turn_interaction_handler() {
        struct AnswerHandler;

        impl RuntimeUserInputHandler for AnswerHandler {
            fn request_user_input(
                &self,
                request: &RuntimeUserInputRequest,
            ) -> io::Result<Option<String>> {
                assert_eq!(request.id, "ask");
                assert_eq!(request.question, "Continue?");
                assert_eq!(request.choices, vec!["yes".to_string(), "no".to_string()]);
                Ok(Some("yes".to_string()))
            }
        }

        let config = config_with_permission_rules(PermissionRules::default());
        let mut actor = ToolExecutionActor::new("tool-user-input-run", 128);
        let mut events = EventFactory::new("tool-user-input-run".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = std::env::current_dir().expect("cwd");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::new(Vec::new());
        let mut cost_tracker = CostTracker::new(None);
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new_for_cwd("tool-user-input-run".to_string(), &cwd);
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();
        let handler = AnswerHandler;
        let request = ToolRequest {
            id: "ask".to_string(),
            name: ToolName::RequestUserInput,
            action: ActionKind::Read,
            target: Some("Continue?".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "question": "Continue?",
                    "choices": ["yes", "no"]
                })
                .to_string(),
            ),
        };

        let (_status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                ToolExecutionContext::new(&cwd, 0, true, &policy_for_tool_execution(&config))
                    .with_services(&instructions, &memory, &mcp_registry, &hooks)
                    .with_runtime(
                        &mut cost_tracker,
                        &cancel,
                        &task_registry,
                        &mut background_workflows,
                        None,
                    )
                    .with_permission_overlay(&mut permission_overlay)
                    .with_user_input_handler(Some(&handler)),
                unused_child_executor,
                unused_child_executor,
            )
            .expect("tool execution");

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("yes"));
    }
}
