use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::tool_types;
use orca_mcp::McpRegistry;

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimePermissionRequestHandler, RuntimeToolActorContext, RuntimeWorkflowIpc,
    TurnPermissionOverlay,
};
use crate::memory::MemoryBlock;
use crate::runtime_special::{RuntimeSpecialToolDispatch, RuntimeWorkflowDraftRequest};
use crate::subagent_execution::execute_subagent_tool;
use crate::tasks::TaskRegistry;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::{
    BackgroundWorkflowRun, execute_workflow_draft_action_tool, execute_workflow_tool,
};

pub(crate) struct RuntimeToolInvocationContext<'a, W: io::Write> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) execution_request: &'a tool_types::ToolRequest,
    pub(crate) subagent_depth: u32,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) emit_deltas: bool,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    pub(crate) child_executor: ChildAgentExecutor<W>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
}

pub(crate) struct RuntimeToolRouter<'a> {
    runtime: &'a mut RuntimeToolActorContext,
}

impl<'a> RuntimeToolRouter<'a> {
    pub(crate) fn new(runtime: &'a mut RuntimeToolActorContext) -> Self {
        Self { runtime }
    }

    pub(crate) fn dispatch<W: io::Write>(
        &mut self,
        context: RuntimeToolInvocationContext<'_, W>,
    ) -> io::Result<tool_types::ToolResult> {
        let RuntimeToolInvocationContext {
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
        } = context;

        match self.runtime.classify_dispatch(execution_request) {
            RuntimeSpecialToolDispatch::WorkflowDraft => self.runtime.execute_workflow_draft_tool(
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
            RuntimeSpecialToolDispatch::SubagentStatus => Ok(self
                .runtime
                .execute_subagent_status_tool(execution_request, task_registry)),
            RuntimeSpecialToolDispatch::TaskList => Ok(self
                .runtime
                .execute_task_list_tool(execution_request, task_registry)),
            RuntimeSpecialToolDispatch::TaskStop => Ok(self
                .runtime
                .execute_task_stop_tool(execution_request, task_registry)),
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
            RuntimeSpecialToolDispatch::WorkflowIpc => Ok(self.runtime.execute_workflow_ipc_tool(
                execution_request,
                workflow_ipc.map(|ipc| ipc as &dyn RuntimeWorkflowIpc),
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
                Ok(self.runtime.execute_normal_tool_with_roots_and_cancel(
                    Some(config),
                    execution_request,
                    cwd,
                    &additional_roots,
                    mcp_registry,
                    &config.external_tools,
                    config.tools.output_truncation,
                    config.tools.shell_timeout_secs,
                    Some(task_registry),
                    Some(cancel),
                    permission_handler
                        .map(|handler| handler as &dyn RuntimePermissionRequestHandler),
                ))
            }
        }
    }
}
