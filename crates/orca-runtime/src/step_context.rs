use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_mcp::McpRegistry;

use crate::extension::{ExtensionData, ExtensionRegistry};
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::RuntimePermissionRequestHandler;
use crate::memory::MemoryBlock;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;

#[derive(Clone, Copy)]
pub(crate) struct RuntimeStepContext<'a> {
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
    pub(crate) extension_registry: Option<&'a ExtensionRegistry>,
    pub(crate) thread_extensions: Option<&'a ExtensionData>,
    pub(crate) turn_extensions: Option<&'a ExtensionData>,
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
            extension_registry: None,
            thread_extensions: None,
            turn_extensions: None,
        }
    }

    pub(crate) fn with_extensions(
        mut self,
        extension_registry: &'a ExtensionRegistry,
        thread_extensions: &'a ExtensionData,
        turn_extensions: &'a ExtensionData,
    ) -> Self {
        self.extension_registry = Some(extension_registry);
        self.thread_extensions = Some(thread_extensions);
        self.turn_extensions = Some(turn_extensions);
        self
    }
}
