use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use orca_core::cancel::CancelToken;
use orca_core::config::{PermissionProfileNetworkAccess, RunConfig};
use orca_core::task_types::TaskStatus;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult};

use crate::extension::RuntimeExtensionStores;
use crate::lifecycle::{
    RuntimePermissionRequest, RuntimePermissionRequestHandler, TurnPermissionOverlay,
};
use crate::network_proxy::{RuntimeNetworkBlockReport, RuntimeNetworkPolicy, RuntimeNetworkProxy};
use crate::protocol::{
    PermissionResponseDecision, RequestFileSystemPermissions, RequestNetworkPermissions,
    RequestPermissionProfile, RequestShellPermissions,
};
use crate::runtime_state::RuntimeTurnReducer;
use crate::sandbox_denial::{
    SandboxDenialDiagnostic, diagnose_sandbox_denial,
    should_request_filesystem_permission_with_denied_roots,
};
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
};
use crate::tasks::TaskRegistry;

pub(crate) struct RuntimeBashInvocationContext<'a> {
    pub(crate) config: Option<&'a RunConfig>,
    pub(crate) request: &'a ToolRequest,
    pub(crate) cwd: &'a Path,
    pub(crate) additional_roots: &'a [PathBuf],
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) shell_timeout_secs: u64,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) cancel: Option<&'a CancelToken>,
    pub(crate) permission_handler: Option<&'a dyn RuntimePermissionRequestHandler>,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
    pub(crate) extension_stores: RuntimeExtensionStores<'a>,
}

pub(crate) fn execute_bash_with_shell_session(
    context: RuntimeBashInvocationContext<'_>,
) -> ToolResult {
    let RuntimeBashInvocationContext {
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
        extension_stores,
    } = context;
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let Some(config) = config else {
        return execute_bash_once(RuntimeBashOnceContext {
            command,
            cwd,
            additional_readable_directories: Vec::new(),
            additional_working_directories: additional_roots.to_vec(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            sandbox: ShellSandboxMode::default(),
            shell_timeout_secs,
            task_registry,
            cancel,
        })
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
    let result = execute_bash_with_sandbox(RuntimeBashSandboxContext {
        command,
        cwd,
        additional_roots,
        sandbox: &sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
    });
    let BashExecutionResult {
        output,
        network_block,
    } = result;
    if let Some(block) = network_block
        && let Some(permission_request) =
            RuntimeBashPermissionPolicy::network_block_request(&request.id, &block)
        && let Some(permission_handler) = permission_handler
    {
        let reducer = RuntimeTurnReducer::from_extension_stores(extension_stores);
        let response = match reducer.request_permission(
            permission_overlay,
            permission_handler,
            permission_request,
        ) {
            Ok(response) => response,
            Err(error) => return ToolResult::failed(request, error.to_string(), None),
        };
        if response.decision == PermissionResponseDecision::Deny {
            return ToolResult::denied(request, "permission request denied".to_string());
        }
        let mut retry_sandbox = sandbox;
        if let Some(network) = response.permissions.network {
            for (domain, access) in network.domains {
                retry_sandbox.network_policy_domains.insert(domain, access);
            }
        }
        return execute_bash_with_sandbox(RuntimeBashSandboxContext {
            command,
            cwd,
            additional_roots,
            sandbox: &retry_sandbox,
            shell_timeout_secs,
            task_registry,
            cancel,
        })
        .output
        .into_tool_result(request, output_truncation, cancel, task_registry);
    }
    if let Some(diagnostic) = output.sandbox_denial_diagnostic(cwd) {
        if should_request_filesystem_permission_with_denied_roots(
            cwd,
            &diagnostic,
            &sandbox.denied_writable_roots,
        ) && let Some(permission_request) =
            RuntimeBashPermissionPolicy::filesystem_write_request(&request.id, &diagnostic)
            && let Some(permission_handler) = permission_handler
        {
            let reducer = RuntimeTurnReducer::from_extension_stores(extension_stores);
            let response = match reducer.request_permission(
                permission_overlay,
                permission_handler,
                permission_request,
            ) {
                Ok(response) => response,
                Err(error) => return ToolResult::failed(request, error.to_string(), None),
            };
            if response.decision == PermissionResponseDecision::Deny {
                return ToolResult::denied(request, "permission request denied".to_string());
            }

            let mut retry_sandbox = sandbox;
            if let Some(file_system) = response.permissions.file_system
                && let Some(write_roots) = file_system.write
            {
                for root in write_roots {
                    push_unique_path(&mut retry_sandbox.additional_writable_roots, root);
                }
            }
            for root in permission_overlay.additional_working_directories() {
                push_unique_path(&mut retry_sandbox.additional_writable_roots, root.clone());
            }

            return execute_bash_with_sandbox(RuntimeBashSandboxContext {
                command,
                cwd,
                additional_roots,
                sandbox: &retry_sandbox,
                shell_timeout_secs,
                task_registry,
                cancel,
            })
            .output
            .with_sandbox_diagnostic(cwd)
            .into_tool_result(request, output_truncation, cancel, task_registry);
        }
        if let Some(permission_request) =
            RuntimeBashPermissionPolicy::unsandboxed_shell_request(&request.id, &diagnostic)
            && let Some(permission_handler) = permission_handler
        {
            let reducer = RuntimeTurnReducer::from_extension_stores(extension_stores);
            let response = match reducer.request_permission(
                permission_overlay,
                permission_handler,
                permission_request,
            ) {
                Ok(response) => response,
                Err(error) => return ToolResult::failed(request, error.to_string(), None),
            };
            if response.decision == PermissionResponseDecision::Deny {
                return ToolResult::denied(request, "permission request denied".to_string());
            }

            return execute_bash_once(RuntimeBashOnceContext {
                command,
                cwd,
                additional_readable_directories: Vec::new(),
                additional_working_directories: additional_roots.to_vec(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: Default::default(),
                sandbox: ShellSandboxMode::DangerFullAccess,
                shell_timeout_secs,
                task_registry,
                cancel,
            })
            .with_sandbox_diagnostic(cwd)
            .into_tool_result(request, output_truncation, cancel, task_registry);
        }
        return output.with_diagnostic(diagnostic).into_tool_result(
            request,
            output_truncation,
            cancel,
            task_registry,
        );
    }
    output.into_tool_result(request, output_truncation, cancel, task_registry)
}

fn bash_sandbox_from_active_permission_profile(
    config: &RunConfig,
    cwd: &Path,
) -> Result<crate::server::CommandExecSandbox, String> {
    crate::server::bash_sandbox_for_cwd(config, cwd)
}

struct BashExecutionResult {
    output: BashShellOutput,
    network_block: Option<RuntimeNetworkBlockReport>,
}

struct BashShellOutput {
    output: Result<crate::shell_session::ShellSessionOutput, String>,
    task_id: Option<String>,
}

struct RuntimeBashSandboxContext<'a> {
    command: &'a str,
    cwd: &'a Path,
    additional_roots: &'a [PathBuf],
    sandbox: &'a crate::server::CommandExecSandbox,
    shell_timeout_secs: u64,
    task_registry: &'a TaskRegistry,
    cancel: Option<&'a CancelToken>,
}

struct RuntimeBashOnceContext<'a> {
    command: &'a str,
    cwd: &'a Path,
    additional_readable_directories: Vec<PathBuf>,
    additional_working_directories: Vec<PathBuf>,
    denied_working_directories: Vec<PathBuf>,
    allowed_unix_socket_roots: Vec<PathBuf>,
    env: BTreeMap<String, Option<String>>,
    sandbox: ShellSandboxMode,
    shell_timeout_secs: u64,
    task_registry: &'a TaskRegistry,
    cancel: Option<&'a CancelToken>,
}

struct RuntimeBashPermissionPolicy;

impl RuntimeBashPermissionPolicy {
    fn network_block_request(
        request_id: &str,
        block: &RuntimeNetworkBlockReport,
    ) -> Option<RuntimePermissionRequest> {
        if block.error == "blocked-by-denylist" {
            return None;
        }

        let mut domains = HashMap::new();
        domains.insert(block.host.clone(), PermissionProfileNetworkAccess::Allow);
        Some(RuntimePermissionRequest {
            id: request_id.to_string(),
            reason: Some(format!(
                "bash attempted network access to {} ({})",
                block.host, block.error
            )),
            permissions: RequestPermissionProfile {
                file_system: None,
                network: Some(RequestNetworkPermissions {
                    enabled: None,
                    domains,
                }),
                shell: None,
            },
        })
    }

    fn filesystem_write_request(
        request_id: &str,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimePermissionRequest> {
        let write_root = diagnostic.suggested_write_root.as_ref()?.clone();
        Some(RuntimePermissionRequest {
            id: request_id.to_string(),
            reason: Some(format!(
                "bash attempted filesystem write outside the current sandbox: {}",
                write_root.display()
            )),
            permissions: RequestPermissionProfile {
                file_system: Some(RequestFileSystemPermissions {
                    read: None,
                    write: Some(vec![write_root]),
                    entries: None,
                }),
                network: None,
                shell: None,
            },
        })
    }

    fn unsandboxed_shell_request(
        request_id: &str,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimePermissionRequest> {
        if diagnostic.suggested_write_root.is_some() {
            return None;
        }

        Some(RuntimePermissionRequest {
            id: request_id.to_string(),
            reason: Some(
                "bash needs to re-run without the filesystem sandbox because the sandbox denied access but did not report a filesystem path to grant".to_string(),
            ),
            permissions: RequestPermissionProfile {
                file_system: None,
                network: None,
                shell: Some(RequestShellPermissions { unsandboxed: true }),
            },
        })
    }
}

impl BashShellOutput {
    fn sandbox_denial_diagnostic(&self, cwd: &Path) -> Option<SandboxDenialDiagnostic> {
        let output = self.output.as_ref().ok()?;
        if output.status == TaskStatus::Completed {
            return None;
        }
        diagnose_sandbox_denial(cwd, &output.stdout, &output.stderr)
    }

    fn with_sandbox_diagnostic(self, cwd: &Path) -> Self {
        let Some(diagnostic) = self.sandbox_denial_diagnostic(cwd) else {
            return self;
        };
        self.with_diagnostic(diagnostic)
    }

    fn with_diagnostic(mut self, diagnostic: SandboxDenialDiagnostic) -> Self {
        if let Ok(output) = &mut self.output {
            if output.stderr.trim_end().is_empty() {
                output.stderr = diagnostic.message;
            } else {
                output
                    .stderr
                    .push_str(&format!("\n\nSandbox diagnostic: {}", diagnostic.message));
            }
        }
        self
    }

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

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn execute_bash_with_sandbox(context: RuntimeBashSandboxContext<'_>) -> BashExecutionResult {
    let RuntimeBashSandboxContext {
        command,
        cwd,
        additional_roots,
        sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
    } = context;
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
                for key in ["NO_PROXY", "no_proxy"] {
                    env.insert(key.to_string(), None);
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
    let output = execute_bash_once(RuntimeBashOnceContext {
        command,
        cwd,
        additional_readable_directories: sandbox.additional_readable_roots.clone(),
        additional_working_directories,
        denied_working_directories: sandbox.denied_writable_roots.clone(),
        allowed_unix_socket_roots: sandbox.allowed_unix_socket_roots.clone(),
        env,
        sandbox: sandbox.mode,
        shell_timeout_secs,
        task_registry,
        cancel,
    });
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

fn execute_bash_once(context: RuntimeBashOnceContext<'_>) -> BashShellOutput {
    let RuntimeBashOnceContext {
        command,
        cwd,
        additional_readable_directories,
        additional_working_directories,
        denied_working_directories,
        allowed_unix_socket_roots,
        env,
        sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
    } = context;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_core::config::PermissionProfileNetworkAccess;

    use super::RuntimeBashPermissionPolicy;
    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::sandbox_denial::SandboxDenialDiagnostic;

    #[test]
    fn runtime_bash_permission_policy_skips_denylist_network_blocks() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        assert!(RuntimeBashPermissionPolicy::network_block_request("tool-1", &block).is_none());
    }

    #[test]
    fn runtime_bash_permission_policy_requests_allow_for_network_policy_blocks() {
        let block = RuntimeNetworkBlockReport {
            host: "api.example.com".to_string(),
            error: "blocked-by-allowlist",
        };

        let request = RuntimeBashPermissionPolicy::network_block_request("tool-1", &block)
            .expect("network permission request");

        assert_eq!(request.id, "tool-1");
        assert_eq!(
            request
                .permissions
                .network
                .as_ref()
                .and_then(|network| network.domains.get("api.example.com")),
            Some(&PermissionProfileNetworkAccess::Allow)
        );
        assert!(request.permissions.file_system.is_none());
        assert!(request.permissions.shell.is_none());
        assert_eq!(
            request.reason.as_deref(),
            Some("bash attempted network access to api.example.com (blocked-by-allowlist)")
        );
    }

    #[test]
    fn runtime_bash_permission_policy_requests_filesystem_write_root() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: Some(PathBuf::from("/repo/.git/index.lock")),
            suggested_write_root: Some(PathBuf::from("/repo/.git")),
            message: "sandbox denied filesystem access".to_string(),
        };

        let request = RuntimeBashPermissionPolicy::filesystem_write_request("tool-1", &diagnostic)
            .expect("filesystem permission request");

        assert_eq!(request.id, "tool-1");
        assert_eq!(
            request
                .permissions
                .file_system
                .as_ref()
                .and_then(|file_system| file_system.write.as_ref()),
            Some(&vec![PathBuf::from("/repo/.git")])
        );
        assert!(request.permissions.network.is_none());
        assert!(request.permissions.shell.is_none());
        assert_eq!(
            request.reason.as_deref(),
            Some("bash attempted filesystem write outside the current sandbox: /repo/.git")
        );
    }

    #[test]
    fn runtime_bash_permission_policy_requests_unsandboxed_shell_when_no_root_is_available() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: None,
            suggested_write_root: None,
            message: "sandbox denied filesystem access".to_string(),
        };

        let request = RuntimeBashPermissionPolicy::unsandboxed_shell_request("tool-1", &diagnostic)
            .expect("unsandboxed permission request");

        assert_eq!(request.id, "tool-1");
        assert!(request.permissions.file_system.is_none());
        assert!(request.permissions.network.is_none());
        assert_eq!(
            request
                .permissions
                .shell
                .as_ref()
                .map(|shell| shell.unsandboxed),
            Some(true)
        );
        assert_eq!(
            request.reason.as_deref(),
            Some(
                "bash needs to re-run without the filesystem sandbox because the sandbox denied access but did not report a filesystem path to grant"
            )
        );
    }
}
