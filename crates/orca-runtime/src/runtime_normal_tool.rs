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

pub(crate) struct RuntimeNormalToolFallbackContext<'a> {
    pub(crate) request: &'a ToolRequest,
    pub(crate) cwd: &'a Path,
    pub(crate) additional_roots: &'a [PathBuf],
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) external_tools: &'a [ExternalToolConfig],
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) shell_timeout_secs: u64,
    pub(crate) cancel: Option<&'a CancelToken>,
}

impl RuntimeNormalToolFallbackContext<'_> {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_some_and(CancelToken::is_cancelled)
    }
}

pub(crate) trait RuntimeNormalToolFallbackExecutor {
    fn execute(&self, context: RuntimeNormalToolFallbackContext<'_>) -> ToolResult;
}

struct DefaultRuntimeNormalToolFallbackExecutor;

impl RuntimeNormalToolFallbackExecutor for DefaultRuntimeNormalToolFallbackExecutor {
    fn execute(&self, context: RuntimeNormalToolFallbackContext<'_>) -> ToolResult {
        orca_tools::execute_with_mcp_external_roots_policy_or_cancel(
            context.request,
            context.cwd,
            context.additional_roots,
            context.mcp_registry,
            context.external_tools,
            context.output_truncation,
            context.shell_timeout_secs,
            || context.is_cancelled(),
        )
    }
}

static DEFAULT_NORMAL_TOOL_FALLBACK: DefaultRuntimeNormalToolFallbackExecutor =
    DefaultRuntimeNormalToolFallbackExecutor;

pub(crate) fn execute_runtime_normal_tool(
    context: RuntimeNormalToolExecutionContext<'_>,
) -> ToolResult {
    RuntimeNormalToolExecutor::new().execute(context)
}

pub(crate) struct RuntimeNormalToolExecutor<'a> {
    fallback: &'a dyn RuntimeNormalToolFallbackExecutor,
}

impl<'a> RuntimeNormalToolExecutor<'a> {
    pub(crate) fn new() -> Self {
        Self {
            fallback: &DEFAULT_NORMAL_TOOL_FALLBACK,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_fallback(fallback: &'a dyn RuntimeNormalToolFallbackExecutor) -> Self {
        Self { fallback }
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

        self.fallback.execute(RuntimeNormalToolFallbackContext {
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            cancel,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolOutputTruncation, ToolStatus};

    use super::*;

    #[derive(Default)]
    struct RecordingFallback {
        calls: RefCell<Vec<RecordedCall>>,
    }

    #[derive(Debug, PartialEq)]
    struct RecordedCall {
        request_id: String,
        cwd: String,
        additional_roots: Vec<String>,
        timeout: u64,
        truncation: ToolOutputTruncation,
        cancelled: bool,
    }

    impl RuntimeNormalToolFallbackExecutor for RecordingFallback {
        fn execute(&self, context: RuntimeNormalToolFallbackContext<'_>) -> ToolResult {
            self.calls.borrow_mut().push(RecordedCall {
                request_id: context.request.id.clone(),
                cwd: context.cwd.display().to_string(),
                additional_roots: context
                    .additional_roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect(),
                timeout: context.shell_timeout_secs,
                truncation: context.output_truncation,
                cancelled: context.is_cancelled(),
            });
            ToolResult::completed(context.request, "fallback".to_string(), false)
        }
    }

    #[test]
    fn normal_tool_executor_delegates_fallback_through_injected_executor() {
        let fallback = RecordingFallback::default();
        let mut executor = RuntimeNormalToolExecutor::with_fallback(&fallback);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };
        let registry = McpRegistry::default();
        let cancel = CancelToken::new();
        cancel.cancel();

        let result = executor.execute(RuntimeNormalToolExecutionContext {
            config: None,
            request: &request,
            cwd: Path::new("/workspace"),
            additional_roots: &[PathBuf::from("/extra")],
            mcp_registry: &registry,
            external_tools: &[],
            output_truncation: ToolOutputTruncation::bytes(256),
            shell_timeout_secs: 42,
            task_registry: None,
            cancel: Some(&cancel),
            permission_handler: None,
            permission_overlay: None,
        });

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("fallback"));
        assert_eq!(
            fallback.calls.borrow().as_slice(),
            [RecordedCall {
                request_id: "tool-1".to_string(),
                cwd: "/workspace".to_string(),
                additional_roots: vec!["/extra".to_string()],
                timeout: 42,
                truncation: ToolOutputTruncation::bytes(256),
                cancelled: true,
            }]
        );
    }
}
