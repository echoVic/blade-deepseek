use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use orca_core::cancel::CancelToken;
use orca_core::config::{PermissionProfileNetworkAccess, RunConfig};
use orca_core::task_types::TaskStatus;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult};

use crate::lifecycle::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, TurnPermissionOverlay,
};
use crate::network_proxy::{RuntimeNetworkBlockReport, RuntimeNetworkPolicy, RuntimeNetworkProxy};
use crate::protocol::{
    CommandExecOptions, PermissionResponseDecision, RequestNetworkPermissions,
    RequestPermissionProfile,
};
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
};
use crate::tasks::TaskRegistry;

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_bash_with_shell_session(
    config: Option<&RunConfig>,
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    task_registry: &TaskRegistry,
    cancel: Option<&CancelToken>,
    permission_handler: Option<&dyn RuntimePermissionRequestHandler>,
    permission_overlay: &mut TurnPermissionOverlay,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let Some(config) = config else {
        return execute_bash_once(
            command,
            cwd,
            Vec::new(),
            additional_roots.to_vec(),
            Vec::new(),
            Vec::new(),
            Default::default(),
            ShellSandboxMode::default(),
            shell_timeout_secs,
            task_registry,
            cancel,
        )
        .into_tool_result(request, output_truncation, cancel, task_registry);
    };
    let mut sandbox = match bash_sandbox_from_active_permission_profile(config, cwd) {
        Ok(sandbox) => sandbox,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    for (domain, access) in permission_overlay.network_domain_permissions() {
        match access {
            PermissionProfileNetworkAccess::Deny => {
                sandbox
                    .network_policy_domains
                    .insert(domain.clone(), *access);
            }
            PermissionProfileNetworkAccess::Allow => {
                sandbox
                    .network_policy_domains
                    .entry(domain.clone())
                    .or_insert(*access);
            }
        }
    }
    let result = execute_bash_with_sandbox(
        command,
        cwd,
        additional_roots,
        &sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
    );
    let BashExecutionResult {
        output,
        network_block,
    } = result;
    if let Some(block) = network_block
        && block.error != "blocked-by-denylist"
        && let Some(permission_handler) = permission_handler
    {
        let mut domains = std::collections::HashMap::new();
        domains.insert(block.host.clone(), PermissionProfileNetworkAccess::Allow);
        let permissions = RequestPermissionProfile {
            file_system: None,
            network: Some(RequestNetworkPermissions {
                enabled: None,
                domains,
            }),
        };
        let permission_request = RuntimePermissionRequest {
            id: request.id.clone(),
            reason: Some(format!(
                "bash attempted network access to {} ({})",
                block.host, block.error
            )),
            permissions,
        };
        let response = match permission_handler.request_permissions(&permission_request) {
            Ok(response) => response,
            Err(error) => return ToolResult::failed(request, error.to_string(), None),
        };
        if response.decision == PermissionResponseDecision::Deny {
            return ToolResult::denied(request, "permission request denied".to_string());
        }
        permission_overlay.merge_network_permissions(&response.permissions);
        permission_overlay.merge_strict_auto_review(response.strict_auto_review);
        let mut retry_sandbox = sandbox;
        if let Some(network) = response.permissions.network {
            for (domain, access) in network.domains {
                retry_sandbox.network_policy_domains.insert(domain, access);
            }
        }
        return execute_bash_with_sandbox(
            command,
            cwd,
            additional_roots,
            &retry_sandbox,
            shell_timeout_secs,
            task_registry,
            cancel,
        )
        .output
        .into_tool_result(request, output_truncation, cancel, task_registry);
    }
    output.into_tool_result(request, output_truncation, cancel, task_registry)
}

fn bash_sandbox_from_active_permission_profile(
    config: &RunConfig,
    cwd: &Path,
) -> Result<crate::server::CommandExecSandbox, String> {
    let runtime_workspace_roots = config.runtime_workspace_roots.clone().unwrap_or_default();
    let profile = config.active_permission_profile.as_ref();
    let options = CommandExecOptions::default();
    crate::server::command_exec_sandbox_mode(
        config,
        &options,
        profile,
        cwd,
        &runtime_workspace_roots,
        std::env::var_os("TMPDIR").map(PathBuf::from).as_deref(),
    )
}

struct BashExecutionResult {
    output: BashShellOutput,
    network_block: Option<RuntimeNetworkBlockReport>,
}

struct BashShellOutput {
    output: Result<crate::shell_session::ShellSessionOutput, String>,
    task_id: Option<String>,
}

impl BashShellOutput {
    fn into_tool_result(
        self,
        request: &ToolRequest,
        output_truncation: ToolOutputTruncation,
        cancel: Option<&CancelToken>,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let output = match self.output {
            Ok(output) => output,
            Err(error) => return ToolResult::failed(request, error, None),
        };
        let stdout = output.stdout.trim_end().to_string();
        let stderr = output.stderr.trim_end().to_string();
        if cancel.is_some_and(CancelToken::is_cancelled)
            || self
                .task_id
                .as_deref()
                .is_some_and(|task_id| task_registry.is_cancelled(task_id))
        {
            let message = if stderr.is_empty() && stdout.is_empty() {
                "shell command cancelled".to_string()
            } else if stderr.is_empty() {
                format!("shell command cancelled: {stdout}")
            } else if stdout.is_empty() {
                format!("shell command cancelled: {stderr}")
            } else {
                format!("shell command cancelled: {stdout}\n{stderr}")
            };
            let (message, truncated) =
                orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
            let mut result = ToolResult::failed(request, message, output.exit_code);
            result.truncated = truncated;
            return result;
        }
        if output.status == TaskStatus::Completed {
            let (stdout, truncated) =
                orca_core::tool_types::truncate_output_with_policy(stdout, output_truncation);
            return ToolResult::completed(request, stdout, truncated);
        }

        let message = if stderr.is_empty() {
            stdout
        } else if stdout.is_empty() {
            stderr
        } else {
            format!("{stdout}\n{stderr}")
        };
        let (message, truncated) =
            orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
        let mut result = ToolResult::failed(request, message, output.exit_code);
        result.truncated = truncated;
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_bash_with_sandbox(
    command: &str,
    cwd: &Path,
    additional_roots: &[PathBuf],
    sandbox: &crate::server::CommandExecSandbox,
    shell_timeout_secs: u64,
    task_registry: &TaskRegistry,
    cancel: Option<&CancelToken>,
) -> BashExecutionResult {
    let mut additional_working_directories = additional_roots.to_vec();
    additional_working_directories.extend(sandbox.additional_writable_roots.clone());
    let mut env = BTreeMap::new();
    let mut block_receiver = None;
    let _network_proxy = if sandbox.network_policy_domains.is_empty() {
        None
    } else {
        let (sender, receiver) = mpsc::channel();
        block_receiver = Some(receiver);
        match RuntimeNetworkProxy::start_with_block_reporter(
            RuntimeNetworkPolicy::new(sandbox.network_policy_domains.clone()),
            Some(sender),
        ) {
            Ok(proxy) => {
                for key in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ] {
                    env.insert(key.to_string(), Some(proxy.proxy_url().to_string()));
                }
                Some(proxy)
            }
            Err(error) => {
                return BashExecutionResult {
                    output: BashShellOutput {
                        output: Err(format!("failed to start network proxy: {error}")),
                        task_id: None,
                    },
                    network_block: None,
                };
            }
        }
    };
    let output = execute_bash_once(
        command,
        cwd,
        sandbox.additional_readable_roots.clone(),
        additional_working_directories,
        sandbox.denied_writable_roots.clone(),
        sandbox.allowed_unix_socket_roots.clone(),
        env,
        sandbox.mode.clone(),
        shell_timeout_secs,
        task_registry,
        cancel,
    );
    let network_block = block_receiver.and_then(|receiver| {
        receiver
            .try_iter()
            .find(|block| block.error != "blocked-by-denylist")
    });
    BashExecutionResult {
        output,
        network_block,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_bash_once(
    command: &str,
    cwd: &Path,
    additional_readable_directories: Vec<PathBuf>,
    additional_working_directories: Vec<PathBuf>,
    denied_working_directories: Vec<PathBuf>,
    allowed_unix_socket_roots: Vec<PathBuf>,
    env: BTreeMap<String, Option<String>>,
    sandbox: ShellSandboxMode,
    shell_timeout_secs: u64,
    task_registry: &TaskRegistry,
    cancel: Option<&CancelToken>,
) -> BashShellOutput {
    let mut manager = RuntimeShellSessionManager::new(task_registry.clone());
    let handle = match manager.spawn(ShellSessionCommand {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        additional_readable_directories,
        additional_working_directories,
        denied_working_directories,
        allowed_unix_socket_roots,
        env,
        description: command.to_string(),
        terminal: ShellTerminalMode::pipe(),
        sandbox,
    }) {
        Ok(handle) => handle,
        Err(error) => {
            return BashShellOutput {
                output: Err(format!("failed to run shell command: {error}")),
                task_id: None,
            };
        }
    };
    let _ = manager.close_stdin(&handle.id);
    let output = match manager.wait_or_cancel(
        &handle.id,
        std::time::Duration::from_secs(shell_timeout_secs.max(1)),
        || {
            cancel.is_some_and(CancelToken::is_cancelled)
                || task_registry.is_cancelled(&handle.task_id)
        },
    ) {
        Ok(output) => Ok(output),
        Err(error) => Err(format!("failed to wait for shell command: {error}")),
    };
    BashShellOutput {
        output,
        task_id: Some(handle.task_id),
    }
}
