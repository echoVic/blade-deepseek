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
use crate::hooks::{HookOutcome, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeApprovalDecision, RuntimeConfigApprovalHandler, RuntimePermissionRequestHandler,
    RuntimeSpecialToolDispatch, RuntimeTaskActor, RuntimeToolActorContext,
    RuntimeWorkflowDraftRequest, TurnPermissionOverlay,
};
use crate::memory::MemoryBlock;
use crate::subagent_execution::execute_subagent_tool;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::{
    ToolInvocation, apply_pre_tool_outcome, approval_request_for_invocation,
    prepare_tool_invocation, validate_tool_invocation,
};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::{
    BackgroundWorkflowRun, execute_workflow_draft_action_tool, execute_workflow_tool,
};

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
}

pub(crate) struct ToolExecutionActor {
    runtime: RuntimeToolActorContext,
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

    fn classify_dispatch(&self, request: &tool_types::ToolRequest) -> RuntimeSpecialToolDispatch {
        self.runtime.classify_dispatch(request)
    }

    fn execute_workflow_draft_tool(
        &mut self,
        request: &tool_types::ToolRequest,
        draft: RuntimeWorkflowDraftRequest<'_>,
    ) -> io::Result<tool_types::ToolResult> {
        self.runtime.execute_workflow_draft_tool(request, draft)
    }

    fn execute_subagent_status_tool(
        &mut self,
        request: &tool_types::ToolRequest,
        task_registry: &TaskRegistry,
    ) -> tool_types::ToolResult {
        self.runtime
            .execute_subagent_status_tool(request, task_registry)
    }

    fn execute_task_list_tool(
        &mut self,
        request: &tool_types::ToolRequest,
        task_registry: &TaskRegistry,
    ) -> tool_types::ToolResult {
        self.runtime.execute_task_list_tool(request, task_registry)
    }

    fn execute_task_stop_tool(
        &mut self,
        request: &tool_types::ToolRequest,
        task_registry: &TaskRegistry,
    ) -> tool_types::ToolResult {
        self.runtime.execute_task_stop_tool(request, task_registry)
    }

    fn execute_workflow_ipc_tool(
        &mut self,
        request: &tool_types::ToolRequest,
        workflow_ipc: Option<&dyn crate::lifecycle::RuntimeWorkflowIpc>,
    ) -> tool_types::ToolResult {
        self.runtime
            .execute_workflow_ipc_tool(request, workflow_ipc)
    }

    fn execute_normal_tool(
        &mut self,
        request: &tool_types::ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[orca_core::external_config::ExternalToolConfig],
        truncation: orca_core::tool_types::ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        additional_roots: &[std::path::PathBuf],
        cancel: Option<&CancelToken>,
    ) -> tool_types::ToolResult {
        self.runtime.execute_normal_tool_with_roots_and_cancel(
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
        )
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

        if let Some(outcome) = self.handle_approval(
            config,
            events,
            sink,
            tool_request,
            &invocation,
            policy,
            permission_overlay.strict_auto_review(),
            emit_deltas,
        )? {
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
        let result = self.dispatch_tool(
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
            child_executor,
            workflow_child_executor,
        )?;
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

    pub(crate) fn handle_approval(
        &mut self,
        config: &RunConfig,
        events: &mut EventFactory,
        sink: &mut EventSink<impl io::Write>,
        tool_request: &tool_types::ToolRequest,
        invocation: &ToolInvocation,
        policy: &ApprovalPolicy,
        strict_auto_review: bool,
        emit_deltas: bool,
    ) -> io::Result<Option<(RunStatus, tool_types::ToolResult)>> {
        if let Some(approval) = approval_request_for_invocation(invocation)
            && agent_common::requires_approval(approval.action)
        {
            let mut approval_decision =
                self.resolve_tool_approval(policy, Some(approval.clone()), tool_request);
            if strict_auto_review
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_tool<W: io::Write>(
        &mut self,
        config: &RunConfig,
        cwd: &Path,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        execution_request: &tool_types::ToolRequest,
        subagent_depth: u32,
        instructions: &ProjectInstructions,
        memory: &MemoryBlock,
        mcp_registry: &McpRegistry,
        hooks: &HookRunner,
        emit_deltas: bool,
        cost_tracker: &mut CostTracker,
        cancel: &CancelToken,
        task_registry: &TaskRegistry,
        background_workflows: &mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&WorkflowIpcContext>,
        permission_overlay: &mut TurnPermissionOverlay,
        permission_handler: Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    ) -> io::Result<tool_types::ToolResult> {
        match self.classify_dispatch(execution_request) {
            RuntimeSpecialToolDispatch::WorkflowDraft => self.execute_workflow_draft_tool(
                execution_request,
                RuntimeWorkflowDraftRequest {
                    workflows_enabled: config.workflows.enabled,
                    cwd,
                    session_id: task_registry.session_id(),
                    max_concurrent_agents: config.workflows.max_concurrent_agents,
                },
            ),
            RuntimeSpecialToolDispatch::WorkflowDraftAction => execute_workflow_draft_action_tool(
                config,
                cwd,
                events,
                sink,
                execution_request,
                emit_deltas,
                task_registry,
                background_workflows,
                workflow_child_executor,
            ),
            RuntimeSpecialToolDispatch::Workflow => execute_workflow_tool(
                config,
                cwd,
                events,
                sink,
                execution_request,
                emit_deltas,
                task_registry,
                background_workflows,
                workflow_child_executor,
            ),
            RuntimeSpecialToolDispatch::Subagent => execute_subagent_tool(
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
                workflow_ipc,
                child_executor,
            ),
            RuntimeSpecialToolDispatch::SubagentStatus => {
                Ok(self.execute_subagent_status_tool(execution_request, task_registry))
            }
            RuntimeSpecialToolDispatch::TaskList => {
                Ok(self.execute_task_list_tool(execution_request, task_registry))
            }
            RuntimeSpecialToolDispatch::TaskStop => {
                Ok(self.execute_task_stop_tool(execution_request, task_registry))
            }
            RuntimeSpecialToolDispatch::RequestPermissions => {
                let result = if let Some(permission_handler) = permission_handler {
                    self.runtime.execute_request_permissions_tool_with_handler(
                        execution_request,
                        permission_handler,
                    )
                } else {
                    self.runtime
                        .execute_request_permissions_tool(execution_request)
                };
                permission_overlay.merge(self.runtime.permission_overlay());
                Ok(result)
            }
            RuntimeSpecialToolDispatch::WorkflowIpc => Ok(self.execute_workflow_ipc_tool(
                execution_request,
                workflow_ipc.map(|ipc| ipc as &dyn crate::lifecycle::RuntimeWorkflowIpc),
            )),
            RuntimeSpecialToolDispatch::Normal => {
                let additional_roots = config
                    .additional_working_directories
                    .iter()
                    .map(|directory| directory.path.clone())
                    .chain(
                        permission_overlay
                            .additional_working_directories()
                            .iter()
                            .cloned(),
                    )
                    .collect::<Vec<_>>();
                Ok(self.execute_normal_tool(
                    execution_request,
                    cwd,
                    mcp_registry,
                    &config.external_tools,
                    config.tools.output_truncation,
                    config.tools.shell_timeout_secs,
                    Some(task_registry),
                    &additional_roots,
                    Some(cancel),
                ))
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
                self.run_post_tool_hook(hooks, &cwd_display, execution_request, result, cancel)
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
