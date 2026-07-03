use std::path::{Path, PathBuf};

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolName, ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::lifecycle::{RuntimePermissionRequestHandler, TurnPermissionOverlay};
use crate::runtime_bash::execute_bash_with_shell_session;
use crate::tasks::TaskRegistry;

pub(crate) struct RuntimeNormalToolExecutionContext<'a> {
    pub(crate) config: Option<&'a RunConfig>,
    pub(crate) request: &'a ToolRequest,
    pub(crate) cwd: &'a Path,
    pub(crate) additional_roots: &'a [PathBuf],
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) external_tools: &'a [ExternalToolConfig],
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) shell_timeout_secs: u64,
    pub(crate) task_registry: Option<&'a TaskRegistry>,
    pub(crate) cancel: Option<&'a CancelToken>,
    pub(crate) permission_handler: Option<&'a dyn RuntimePermissionRequestHandler>,
    pub(crate) permission_overlay: Option<&'a mut TurnPermissionOverlay>,
}

pub(crate) struct RuntimeNormalToolExecutor;

impl RuntimeNormalToolExecutor {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn execute(&mut self, context: RuntimeNormalToolExecutionContext<'_>) -> ToolResult {
        let RuntimeNormalToolExecutionContext {
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
            permission_overlay,
        } = context;

        if request.name == ToolName::Bash
            && let Some(task_registry) = task_registry
            && let Some(permission_overlay) = permission_overlay
        {
            return execute_bash_with_shell_session(
                config,
                request,
                cwd,
                additional_roots,
                output_truncation,
                shell_timeout_secs,
                task_registry,
                cancel,
                permission_handler,
                permission_overlay,
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
}
