use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::RunStatus;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::extension::RuntimeExtensionContext;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{RuntimePermissionRequestHandler, TurnPermissionOverlay};
use crate::memory::MemoryBlock;
use crate::session::{record_plan_state_for_agent, record_tool_result_for_agent};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;

#[derive(Clone, Copy)]
pub(crate) struct RuntimeStepSnapshot<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeStepContext<'a> {
    pub(crate) snapshot: RuntimeStepSnapshot<'a>,
    pub(crate) extensions: Option<RuntimeExtensionContext<'a>>,
}

#[derive(Default)]
pub(crate) struct RuntimeSamplingRequestState {
    pub(crate) permission_overlay: TurnPermissionOverlay,
    tool_cursor_index: usize,
}

pub(crate) struct RuntimeToolDispatchWindow<'a> {
    tool_requests: &'a [ToolRequest],
    end_index: usize,
}

pub(crate) enum RuntimeToolResultRecordOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

impl<'a> RuntimeToolDispatchWindow<'a> {
    pub(crate) fn tool_requests(&self) -> &'a [ToolRequest] {
        self.tool_requests
    }

    pub(crate) fn end_index(&self) -> usize {
        self.end_index
    }
}

impl RuntimeSamplingRequestState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn permission_overlay_mut(&mut self) -> &mut TurnPermissionOverlay {
        &mut self.permission_overlay
    }

    pub(crate) fn current_tool_request<'a>(
        &self,
        tool_requests: &'a [ToolRequest],
    ) -> Option<&'a ToolRequest> {
        tool_requests.get(self.tool_cursor_index)
    }

    #[cfg(test)]
    pub(crate) fn tool_cursor_position(&self) -> usize {
        self.tool_cursor_index
    }

    pub(crate) fn advance_tool_cursor_one(&mut self, tool_request_count: usize) {
        self.advance_tool_cursor_to(self.tool_cursor_index.saturating_add(1), tool_request_count);
    }

    pub(crate) fn advance_tool_cursor_to(&mut self, next_index: usize, tool_request_count: usize) {
        self.tool_cursor_index = next_index.min(tool_request_count);
    }

    pub(crate) fn tool_dispatch_window<'a, F>(
        &self,
        tool_requests: &'a [ToolRequest],
        collect_end: F,
    ) -> RuntimeToolDispatchWindow<'a>
    where
        F: FnOnce(&[ToolRequest], usize) -> usize,
    {
        let start_index = self.tool_cursor_index.min(tool_requests.len());
        let minimum_end_index = start_index.saturating_add(1).min(tool_requests.len());
        let end_index = collect_end(tool_requests, start_index)
            .min(tool_requests.len())
            .max(minimum_end_index);
        RuntimeToolDispatchWindow {
            tool_requests: &tool_requests[start_index..end_index],
            end_index,
        }
    }

    pub(crate) fn advance_tool_cursor_to_window_end(
        &mut self,
        window: &RuntimeToolDispatchWindow<'_>,
    ) {
        self.tool_cursor_index = window.end_index();
    }

    pub(crate) fn record_normal_tool_result(
        &self,
        conversation: &mut Conversation,
        mut history_writer: Option<&mut SessionWriter>,
        tool_request: &ToolRequest,
        result: &ToolResult,
        status: RunStatus,
        emit_deltas: bool,
    ) -> io::Result<RuntimeToolResultRecordOutcome> {
        record_plan_state_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            tool_request,
            result,
        );
        record_tool_result_for_agent(conversation, history_writer, result, emit_deltas)?;

        if status == RunStatus::ApprovalRequired {
            return Ok(RuntimeToolResultRecordOutcome::Return {
                status,
                error: result.error.clone(),
            });
        }
        if status == RunStatus::Failed && tool_request.name == ToolName::Subagent {
            return Ok(RuntimeToolResultRecordOutcome::Return {
                status: RunStatus::Failed,
                error: Some(result.error.clone().unwrap_or_default()),
            });
        }

        Ok(RuntimeToolResultRecordOutcome::Continue)
    }
}

impl<'a> RuntimeStepSnapshot<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: &'a RunConfig,
        cwd: &'a Path,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        Self {
            config,
            cwd,
            tool_policy,
            subagent_depth,
            emit_deltas,
            policy,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cancel,
            task_registry,
            workflow_ipc,
            permission_handler,
        }
    }
}

impl<'a> RuntimeStepContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: &'a RunConfig,
        cwd: &'a Path,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        Self {
            snapshot: RuntimeStepSnapshot::new(
                config,
                cwd,
                tool_policy,
                subagent_depth,
                emit_deltas,
                policy,
                instructions,
                memory,
                mcp_registry,
                hooks,
                cancel,
                task_registry,
                workflow_ipc,
                permission_handler,
            ),
            extensions: None,
        }
    }

    pub(crate) fn snapshot(&self) -> RuntimeStepSnapshot<'a> {
        self.snapshot
    }

    pub(crate) fn into_parts(
        self,
    ) -> (RuntimeStepSnapshot<'a>, Option<RuntimeExtensionContext<'a>>) {
        (self.snapshot, self.extensions)
    }
}
