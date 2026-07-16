use orca_core::tool_types::{ToolName, ToolResult};

use crate::runtime_bash::{RuntimeBashInvocationContext, execute_bash_with_shell_session};
use crate::runtime_tool_call::{RuntimeNormalToolInvocation, RuntimeNormalToolWorkerContext};

pub(crate) fn execute_runtime_normal_tool(
    invocation: &RuntimeNormalToolInvocation,
    context: &mut RuntimeNormalToolWorkerContext<'_>,
) -> ToolResult {
    if invocation.request.name == ToolName::Bash
        && let Some(task_registry) = invocation.task_registry.as_ref()
    {
        return execute_bash_with_shell_session(RuntimeBashInvocationContext {
            config: invocation.config.as_ref(),
            request: &invocation.request,
            cwd: &invocation.cwd,
            additional_roots: &invocation.additional_roots,
            output_truncation: invocation.output_truncation,
            shell_timeout_secs: invocation.shell_timeout_secs,
            task_registry,
            cancel: Some(context.cancel),
            permission_handler: context.permission_handler,
            permission_overlay: context.permission_overlay,
            output_handler: context.output_handler.take(),
        });
    }

    orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
        &invocation.request,
        &invocation.cwd,
        &invocation.additional_roots,
        &invocation.mcp_registry,
        &invocation.external_tools,
        invocation.output_truncation,
        invocation.shell_timeout_secs,
        context.mcp_elicitation_handler,
        || context.cancel.is_cancelled(),
    )
}
