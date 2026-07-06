use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_mcp::McpRegistry;

use crate::extension::{ExtensionRegistry, RuntimeExtensionContext, RuntimeExtensionStores};
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
    pub(crate) extensions: Option<RuntimeExtensionContext<'a>>,
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
            extensions: None,
        }
    }

    pub(crate) fn with_extensions(
        mut self,
        extension_registry: &'a ExtensionRegistry,
        extension_stores: RuntimeExtensionStores<'a>,
    ) -> Self {
        self.extensions = Some(RuntimeExtensionContext::new(
            extension_registry,
            extension_stores,
        ));
        self
    }
}
