use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::Value;

use super::PendingCommandExecPermissionRequest;
use super::cap_text;
use crate::network_proxy::{RuntimeNetworkBlockReport, RuntimeNetworkProxy};
use crate::protocol::{self, ServerEvent};
use crate::runtime_permission::{
    RuntimePermissionDecision, RuntimePermissionEvaluation, RuntimePermissionOrigin,
    RuntimePermissionPolicy, RuntimePermissionRequestKind,
};
use crate::sandbox_denial::{SandboxDenialDiagnostic, diagnose_sandbox_denial};
use crate::shell_session::RuntimeShellSessionManager;

pub(super) struct CommandExecProcess {
    pub(super) shell_id: Option<String>,
    pub(super) command_event_id: Value,
    pub(super) command: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) denied_writable_roots: Vec<PathBuf>,
    pub(super) stream_output: bool,
    pub(super) output_bytes_cap: Option<usize>,
    pub(super) output_offset: usize,
    pub(super) stdout_len: usize,
    pub(super) stderr_len: usize,
    pub(super) stdout_cap_reached: bool,
    pub(super) stderr_cap_reached: bool,
    pub(super) network_permission_blocks: Option<mpsc::Receiver<RuntimeNetworkBlockReport>>,
    pub(super) permission_request: Option<PendingCommandExecPermissionRequest>,
    pub(super) _network_proxy: Option<RuntimeNetworkProxy>,
}

#[derive(Default)]
pub(super) struct CommandExecManager {
    processes: HashMap<String, CommandExecProcess>,
}

pub(super) struct CommandExecProcessSnapshot {
    pub(super) process_id: String,
    pub(super) shell_id: Option<String>,
    pub(super) command: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) status: &'static str,
    pub(super) stream_output: bool,
    pub(super) output_bytes_cap: Option<usize>,
    pub(super) stdout_bytes: usize,
    pub(super) stderr_bytes: usize,
}

pub(super) enum CommandExecDrainOutcome {
    Drained,
    NetworkPermissionRequired {
        request: PendingCommandExecPermissionRequest,
        block: RuntimeNetworkBlockReport,
    },
    NetworkPermissionDenied {
        command_event_id: Value,
        reason: String,
    },
    FileSystemPermissionRequired {
        request: PendingCommandExecPermissionRequest,
        diagnostic: SandboxDenialDiagnostic,
    },
}

pub(super) struct CommandExecPermissionPrompt {
    pub(super) origin: RuntimePermissionOrigin,
    pub(super) kind: RuntimePermissionRequestKind,
    pub(super) reason: String,
    pub(super) permissions: protocol::RequestPermissionProfile,
}

pub(super) struct CommandExecPermissionDenial {
    pub(super) reason: String,
}

impl From<RuntimePermissionDecision> for CommandExecPermissionPrompt {
    fn from(decision: RuntimePermissionDecision) -> Self {
        Self {
            origin: decision.origin,
            kind: decision.kind,
            reason: decision.request.reason.unwrap_or_default(),
            permissions: decision.request.permissions,
        }
    }
}

impl CommandExecPermissionPrompt {
    pub(super) fn into_request_parts(
        self,
    ) -> (
        RuntimePermissionOrigin,
        RuntimePermissionRequestKind,
        String,
        protocol::RequestPermissionProfile,
    ) {
        (self.origin, self.kind, self.reason, self.permissions)
    }
}

pub(super) struct CommandExecPermissionPolicy;

impl CommandExecPermissionPolicy {
    pub(super) fn network_permission_block(
        blocked_hosts: mpsc::Receiver<RuntimeNetworkBlockReport>,
    ) -> Option<RuntimeNetworkBlockReport> {
        blocked_hosts.try_iter().next()
    }

    pub(super) fn network_block_prompt(
        block: &RuntimeNetworkBlockReport,
    ) -> Option<CommandExecPermissionPrompt> {
        RuntimePermissionPolicy::network_block_decision(
            "command-exec",
            RuntimePermissionOrigin::CommandExec,
            block,
        )
        .map(CommandExecPermissionPrompt::from)
    }

    pub(super) fn network_block_denial(
        block: &RuntimeNetworkBlockReport,
    ) -> Option<CommandExecPermissionDenial> {
        match RuntimePermissionPolicy::network_block_evaluation(
            "command-exec",
            RuntimePermissionOrigin::CommandExec,
            block,
        ) {
            RuntimePermissionEvaluation::Request(_) => None,
            RuntimePermissionEvaluation::Deny { reason, .. } => {
                Some(CommandExecPermissionDenial { reason })
            }
        }
    }

    pub(super) fn sandbox_denial_prompt(
        diagnostic: &SandboxDenialDiagnostic,
    ) -> CommandExecPermissionPrompt {
        RuntimePermissionPolicy::sandbox_denial_decision(
            "command-exec",
            RuntimePermissionOrigin::CommandExec,
            diagnostic,
        )
        .into()
    }

    pub(super) fn should_request_filesystem_retry(
        cwd: &std::path::Path,
        diagnostic: &SandboxDenialDiagnostic,
        denied_writable_roots: &[PathBuf],
    ) -> bool {
        RuntimePermissionPolicy::should_request_filesystem_retry(
            cwd,
            diagnostic,
            denied_writable_roots,
        )
    }
}

impl CommandExecManager {
    pub(super) fn insert(
        &mut self,
        process_id: String,
        process: CommandExecProcess,
    ) -> Result<(), String> {
        if self.processes.contains_key(&process_id) {
            return Err(format!(
                "duplicate active command/exec process id: {:?}",
                process_id
            ));
        }
        self.processes.insert(process_id, process);
        Ok(())
    }

    pub(super) fn activate(&mut self, process_id: &str, shell_id: String) -> bool {
        let Some(process) = self.processes.get_mut(process_id) else {
            return false;
        };
        process.shell_id = Some(shell_id);
        true
    }

    pub(super) fn retain_network_proxy(&mut self, process_id: &str, proxy: RuntimeNetworkProxy) {
        if let Some(process) = self.processes.get_mut(process_id) {
            process._network_proxy = Some(proxy);
        }
    }

    pub(super) fn get(&self, process_id: &str) -> Option<&CommandExecProcess> {
        self.processes.get(process_id)
    }

    fn get_mut(&mut self, process_id: &str) -> Option<&mut CommandExecProcess> {
        self.processes.get_mut(process_id)
    }

    pub(super) fn remove(&mut self, process_id: &str) -> Option<CommandExecProcess> {
        self.processes.remove(process_id)
    }

    pub(super) fn tighten_output_cap(&mut self, process_id: &str, output_bytes_cap: usize) {
        if let Some(process) = self.get_mut(process_id) {
            process.output_bytes_cap = Some(
                process
                    .output_bytes_cap
                    .map(|existing| existing.min(output_bytes_cap))
                    .unwrap_or(output_bytes_cap),
            );
        }
    }

    fn process_ids(&self) -> Vec<String> {
        self.processes.keys().cloned().collect()
    }

    pub(super) fn list(&self) -> Vec<CommandExecProcessSnapshot> {
        let mut process_ids = self.process_ids();
        process_ids.sort();
        process_ids
            .into_iter()
            .filter_map(|process_id| {
                self.processes
                    .get(&process_id)
                    .map(|process| CommandExecProcessSnapshot {
                        process_id,
                        shell_id: process.shell_id.clone(),
                        command: process.command.clone(),
                        cwd: process.cwd.clone(),
                        status: if process.shell_id.is_some() {
                            "running"
                        } else {
                            "starting"
                        },
                        stream_output: process.stream_output,
                        output_bytes_cap: process.output_bytes_cap,
                        stdout_bytes: process.stdout_len,
                        stderr_bytes: process.stderr_len,
                    })
            })
            .collect()
    }

    pub(super) fn write_to_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        delta_base64: Option<&str>,
        close_stdin: bool,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<()> {
        let Some(process) = self.get(process_id) else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        let Some(shell_id) = process.shell_id.clone() else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            );
        };
        let Some(manager) = shell_sessions else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        if let Some(delta_base64) = delta_base64 {
            let bytes = match BASE64_STANDARD.decode(delta_base64) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return protocol::write_server_event(
                        writer,
                        id,
                        ServerEvent::error(format!(
                            "invalid command/exec write deltaBase64: {error}"
                        )),
                    );
                }
            };
            let input = String::from_utf8_lossy(&bytes);
            if let Err(error) = manager.write_stdin(&shell_id, &input) {
                return protocol::write_server_event(
                    writer,
                    id,
                    ServerEvent::error(error.to_string()),
                );
            }
        }
        if close_stdin && let Err(error) = manager.close_stdin(&shell_id) {
            return protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()));
        }
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::CommandExecWritten {
                process_id: Value::from(process_id.to_string()),
            },
        )?;
        self.drain_with_timeout(Some(manager), writer, Duration::from_secs(5))
            .map(|_| ())
    }

    pub(super) fn read_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        timeout: Duration,
        output_bytes_cap: Option<usize>,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<CommandExecDrainOutcome> {
        let Some(process) = self.get(process_id) else {
            protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            )?;
            return Ok(CommandExecDrainOutcome::Drained);
        };
        if process.shell_id.is_none() {
            protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            )?;
            return Ok(CommandExecDrainOutcome::Drained);
        }
        let Some(manager) = shell_sessions else {
            protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            )?;
            return Ok(CommandExecDrainOutcome::Drained);
        };
        if let Some(output_bytes_cap) = output_bytes_cap {
            self.tighten_output_cap(process_id, output_bytes_cap);
        }
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::CommandExecRead {
                process_id: Value::from(process_id.to_string()),
                status: Value::from("running"),
            },
        )?;
        self.drain_until_output_or_timeout(Some(manager), writer, timeout)
    }

    pub(super) fn resize_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        cols: u16,
        rows: u16,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<()> {
        let Some(process) = self.get(process_id) else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        let Some(shell_id) = process.shell_id.clone() else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            );
        };
        let Some(manager) = shell_sessions else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        if let Err(error) = manager.resize(&shell_id, cols, rows) {
            return protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()));
        }
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::CommandExecResized {
                process_id: Value::from(process_id.to_string()),
                cols: Value::from(cols),
                rows: Value::from(rows),
            },
        )
    }

    pub(super) fn drain<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
    ) -> io::Result<CommandExecDrainOutcome> {
        self.drain_with_timeout(shell_sessions, writer, Duration::from_millis(1))
    }

    pub(super) fn drain_with_timeout<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
        timeout: Duration,
    ) -> io::Result<CommandExecDrainOutcome> {
        self.drain_inner(shell_sessions, writer, timeout, false)
    }

    pub(super) fn drain_until_output_or_timeout<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
        timeout: Duration,
    ) -> io::Result<CommandExecDrainOutcome> {
        self.drain_inner(shell_sessions, writer, timeout, true)
    }

    fn drain_inner<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
        timeout: Duration,
        return_on_output: bool,
    ) -> io::Result<CommandExecDrainOutcome> {
        let Some(manager) = shell_sessions else {
            return Ok(CommandExecDrainOutcome::Drained);
        };
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let process_ids = self.process_ids();
            if process_ids.is_empty() {
                return Ok(CommandExecDrainOutcome::Drained);
            }
            let mut observed_output = false;
            for process_id in process_ids {
                let Some(shell_id) = self
                    .get(&process_id)
                    .and_then(|process| process.shell_id.clone())
                else {
                    continue;
                };
                let output =
                    match manager.read_preserving_output(&shell_id, Duration::from_millis(1)) {
                        Ok(output) => output,
                        Err(_) => continue,
                    };
                let Some(process) = self.get(&process_id) else {
                    continue;
                };
                if process.stream_output {
                    let output_read = manager
                        .read_output_delta(&output.task_id, process.output_offset, usize::MAX)
                        .unwrap_or_else(|_| crate::task_output::TaskOutputRead {
                            stdout: String::new(),
                            stderr: String::new(),
                            next_offset: process.output_offset,
                            bytes_read: 0,
                            bytes_total: process.output_offset,
                            omitted_prefix_bytes: 0,
                        });
                    let (stdout_delta, stdout_cap_reached) = capped_stream_delta(
                        &output_read.stdout,
                        process.stdout_len,
                        process.output_bytes_cap,
                        process.stdout_cap_reached,
                    );
                    let (stderr_delta, stderr_cap_reached) = capped_stream_delta(
                        &output_read.stderr,
                        process.stderr_len,
                        process.output_bytes_cap,
                        process.stderr_cap_reached,
                    );
                    observed_output |= !stdout_delta.is_empty() || !stderr_delta.is_empty();
                    super::write_command_exec_output_deltas(
                        writer,
                        &process_id,
                        &stdout_delta,
                        &stderr_delta,
                        stdout_cap_reached,
                        stderr_cap_reached,
                        output.status != orca_core::task_types::TaskStatus::Running,
                    )?;
                    if let Some(process) = self.get_mut(&process_id) {
                        process.output_offset = output_read.next_offset;
                        process.stdout_len = process.stdout_len.saturating_add(stdout_delta.len());
                        process.stderr_len = process.stderr_len.saturating_add(stderr_delta.len());
                        process.stdout_cap_reached |= stdout_cap_reached;
                        process.stderr_cap_reached |= stderr_cap_reached;
                    }
                }
                if output.status != orca_core::task_types::TaskStatus::Running {
                    let Some(process) = self.remove(&process_id) else {
                        continue;
                    };
                    if let Some(block) = process
                        .network_permission_blocks
                        .and_then(CommandExecPermissionPolicy::network_permission_block)
                    {
                        if let Some(denial) =
                            CommandExecPermissionPolicy::network_block_denial(&block)
                        {
                            manager.remove_output(&output.task_id);
                            return Ok(CommandExecDrainOutcome::NetworkPermissionDenied {
                                command_event_id: process.command_event_id,
                                reason: denial.reason,
                            });
                        }
                        if let Some(request) = process.permission_request {
                            manager.remove_output(&output.task_id);
                            return Ok(CommandExecDrainOutcome::NetworkPermissionRequired {
                                request,
                                block,
                            });
                        }
                    }
                    if let Some(diagnostic) =
                        diagnose_sandbox_denial(&process.cwd, &output.stdout, &output.stderr)
                    {
                        if CommandExecPermissionPolicy::should_request_filesystem_retry(
                            &process.cwd,
                            &diagnostic,
                            &process.denied_writable_roots,
                        ) && let Some(request) = process.permission_request
                        {
                            manager.remove_output(&output.task_id);
                            return Ok(CommandExecDrainOutcome::FileSystemPermissionRequired {
                                request,
                                diagnostic,
                            });
                        }
                    }
                    protocol::write_server_event(
                        writer,
                        &process.command_event_id,
                        ServerEvent::CommandExecCompleted {
                            process_id: Value::from(process_id),
                            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
                            stdout: if process.stream_output {
                                Value::from("")
                            } else {
                                Value::from(cap_text(&output.stdout, process.output_bytes_cap))
                            },
                            stderr: if process.stream_output {
                                Value::from("")
                            } else {
                                Value::from(cap_text(&output.stderr, process.output_bytes_cap))
                            },
                        },
                    )?;
                    manager.remove_output(&output.task_id);
                }
            }
            if std::time::Instant::now() >= deadline
                || (observed_output && (return_on_output || timeout <= Duration::from_millis(1)))
            {
                return Ok(CommandExecDrainOutcome::Drained);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn terminate_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<()> {
        let Some(process) = self.get(process_id) else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        let Some(shell_id) = process.shell_id.clone() else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            );
        };
        let Some(manager) = shell_sessions else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        match manager.kill(&shell_id) {
            Ok(output) => {
                let Some(process) = self.remove(process_id) else {
                    return protocol::write_server_event(
                        writer,
                        id,
                        ServerEvent::error(format!("unknown command process: {process_id}")),
                    );
                };
                protocol::write_server_event(
                    writer,
                    id,
                    ServerEvent::CommandExecTerminated {
                        process_id: Value::from(process_id.to_string()),
                    },
                )?;
                protocol::write_server_event(
                    writer,
                    &process.command_event_id,
                    ServerEvent::CommandExecCompleted {
                        process_id: Value::from(process_id.to_string()),
                        exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
                        stdout: if process.stream_output {
                            Value::from("")
                        } else {
                            Value::from(cap_text(&output.stdout, process.output_bytes_cap))
                        },
                        stderr: if process.stream_output {
                            Value::from("")
                        } else {
                            Value::from(cap_text(&output.stderr, process.output_bytes_cap))
                        },
                    },
                )
            }
            Err(error) => {
                protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()))
            }
        }
    }

    pub(super) fn terminate_all(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
    ) {
        let Some(manager) = shell_sessions else {
            self.processes.clear();
            return;
        };
        for process_id in self.process_ids() {
            let Some(process) = self.remove(&process_id) else {
                continue;
            };
            let Some(shell_id) = process.shell_id else {
                continue;
            };
            let _ = manager.kill(&shell_id);
        }
    }
}

fn capped_stream_delta(
    delta: &str,
    sent_len: usize,
    cap: Option<usize>,
    cap_already_reached: bool,
) -> (String, bool) {
    let Some(cap) = cap else {
        return (delta.to_string(), false);
    };
    if cap_already_reached || delta.is_empty() {
        return (String::new(), false);
    }
    let remaining = cap.saturating_sub(sent_len);
    let capped = cap_text(delta, Some(remaining));
    (capped, delta.len() >= remaining)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{CommandExecManager, CommandExecPermissionPolicy, CommandExecProcess};
    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::runtime_permission::{RuntimePermissionOrigin, RuntimePermissionRequestKind};
    use crate::sandbox_denial::SandboxDenialDiagnostic;
    use crate::shell_session::{
        RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
    };
    use crate::task_output::TaskOutputStore;
    use crate::tasks::TaskRegistry;
    use serde_json::Value;

    #[test]
    fn command_exec_permission_policy_requests_pathless_sandbox_retry() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: None,
            suggested_write_root: None,
            message: "sandbox denied filesystem access".to_string(),
        };

        assert!(
            CommandExecPermissionPolicy::should_request_filesystem_retry(
                &PathBuf::from("/repo"),
                &diagnostic,
                &[]
            )
        );
    }

    #[test]
    fn command_exec_permission_policy_requests_write_root_unless_denied() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: Some(PathBuf::from("/repo/.git/index.lock")),
            suggested_write_root: Some(PathBuf::from("/repo/.git")),
            message: "sandbox denied filesystem access".to_string(),
        };

        assert!(
            CommandExecPermissionPolicy::should_request_filesystem_retry(
                &PathBuf::from("/repo/worktree"),
                &diagnostic,
                &[]
            )
        );
        assert!(
            !CommandExecPermissionPolicy::should_request_filesystem_retry(
                &PathBuf::from("/repo/worktree"),
                &diagnostic,
                &[PathBuf::from("/repo/.git")]
            )
        );
    }

    #[test]
    fn command_exec_permission_policy_builds_network_prompt_for_allowlist_block() {
        let block = RuntimeNetworkBlockReport {
            host: "api.orca.invalid".to_string(),
            error: "blocked-by-allowlist",
        };

        let prompt =
            CommandExecPermissionPolicy::network_block_prompt(&block).expect("prompt request");

        assert_eq!(prompt.origin, RuntimePermissionOrigin::CommandExec);
        assert_eq!(prompt.kind, RuntimePermissionRequestKind::NetworkBlock);
        assert_eq!(
            prompt.reason,
            "command/exec attempted network access to api.orca.invalid (blocked-by-allowlist)"
        );
        assert_eq!(
            prompt
                .permissions
                .network
                .expect("network permissions")
                .domains
                .get("api.orca.invalid"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
    }

    #[test]
    fn command_exec_permission_policy_does_not_prompt_for_denylist_block() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        assert!(CommandExecPermissionPolicy::network_block_prompt(&block).is_none());
    }

    #[test]
    fn command_exec_permission_policy_explains_network_denylist_blocks() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        let denial =
            CommandExecPermissionPolicy::network_block_denial(&block).expect("policy denial");

        assert_eq!(
            denial.reason,
            "command/exec network access to blocked.orca.invalid was denied by configured network policy"
        );
    }

    #[test]
    fn command_exec_permission_policy_keeps_denylist_blocks_for_final_denial() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeNetworkBlockReport {
                host: "blocked.orca.invalid".to_string(),
                error: "blocked-by-denylist",
            })
            .expect("send block");
        drop(sender);

        let block = CommandExecPermissionPolicy::network_permission_block(receiver)
            .expect("network block should reach command/exec policy");

        assert_eq!(block.host, "blocked.orca.invalid");
        assert!(CommandExecPermissionPolicy::network_block_prompt(&block).is_none());
        assert!(CommandExecPermissionPolicy::network_block_denial(&block).is_some());
    }

    #[test]
    fn command_exec_permission_policy_builds_filesystem_prompt_for_write_root() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: Some(PathBuf::from("/repo/.git/index.lock")),
            suggested_write_root: Some(PathBuf::from("/repo/.git")),
            message: "sandbox denied filesystem access".to_string(),
        };

        let prompt = CommandExecPermissionPolicy::sandbox_denial_prompt(&diagnostic);

        assert_eq!(prompt.origin, RuntimePermissionOrigin::CommandExec);
        assert_eq!(prompt.kind, RuntimePermissionRequestKind::FilesystemWrite);
        assert_eq!(
            prompt.reason,
            "command/exec attempted filesystem write outside the current sandbox: /repo/.git"
        );
        assert_eq!(
            prompt
                .permissions
                .file_system
                .expect("file system permissions")
                .write,
            Some(vec![PathBuf::from("/repo/.git")])
        );
    }

    #[test]
    fn command_exec_permission_policy_builds_unsandboxed_prompt_for_pathless_denial() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: None,
            suggested_write_root: None,
            message: "sandbox denied filesystem access".to_string(),
        };

        let prompt = CommandExecPermissionPolicy::sandbox_denial_prompt(&diagnostic);

        assert_eq!(prompt.origin, RuntimePermissionOrigin::CommandExec);
        assert_eq!(
            prompt.kind,
            RuntimePermissionRequestKind::UnsandboxedShellRetry
        );
        assert!(
            prompt.reason.contains("without the filesystem sandbox"),
            "unsandboxed retry prompt should explain why the sandbox cannot be amended: {}",
            prompt.reason
        );
        assert!(
            prompt
                .permissions
                .shell
                .expect("shell permissions")
                .unsandboxed
        );
    }

    #[test]
    fn streaming_delta_survives_retained_output_rebase() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-output-rebase".to_string());
        let output_store = TaskOutputStore::with_max_retained_bytes(5);
        let mut shell_sessions =
            RuntimeShellSessionManager::with_output_store(task_registry, output_store);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: "printf first; sleep 0.2; printf later; sleep 0.2".to_string(),
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream output across retained tail rebase".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-rebase".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-rebase"),
                    command: vec!["sh".to_string(), "-lc".to_string()],
                    cwd: cwd.path().to_path_buf(),
                    denied_writable_roots: Vec::new(),
                    stream_output: true,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain first output");
        let first_events = parse_test_jsonl(&output);
        assert!(
            first_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "first"
            }),
            "first delta should be emitted: {first_events:?}"
        );

        output.clear();
        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain second output");
        let second_events = parse_test_jsonl(&output);
        assert!(
            second_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "later"
            }),
            "later delta should not be lost after retained output rebases: {second_events:?}"
        );
    }

    #[test]
    fn streaming_delta_respects_total_stdout_cap_across_reads() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-output-cap".to_string());
        let mut shell_sessions = RuntimeShellSessionManager::new(task_registry);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: "printf ab; sleep 0.2; printf cd; sleep 0.2".to_string(),
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream output under cap".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-cap".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-cap"),
                    command: vec!["sh".to_string(), "-lc".to_string()],
                    cwd: cwd.path().to_path_buf(),
                    denied_writable_roots: Vec::new(),
                    stream_output: true,
                    output_bytes_cap: Some(3),
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain first output");
        let first_events = parse_test_jsonl(&output);
        assert!(
            first_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "ab"
                    && event["capReached"] == false
            }),
            "first delta should be under cap: {first_events:?}"
        );

        output.clear();
        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain second output");
        let second_events = parse_test_jsonl(&output);
        assert!(
            second_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "c"
                    && event["capReached"] == true
            }),
            "second delta should stop at the total stdout cap: {second_events:?}"
        );
        assert!(
            second_events.iter().all(|event| event["delta"] != "cd"),
            "second delta must not treat the cap as per-read: {second_events:?}"
        );
    }

    #[test]
    fn command_exec_denial_drain_evicts_finished_process_output() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-denial-evict".to_string());
        let mut shell_sessions = RuntimeShellSessionManager::new(task_registry);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: "printf denied".to_string(),
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "deny after output".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeNetworkBlockReport {
                host: "blocked.orca.invalid".to_string(),
                error: "blocked-by-denylist",
            })
            .expect("send network block");
        drop(sender);

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-denied".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id.clone()),
                    command_event_id: Value::from("cmd-denied"),
                    command: vec!["sh".to_string(), "-lc".to_string()],
                    cwd: cwd.path().to_path_buf(),
                    denied_writable_roots: Vec::new(),
                    stream_output: false,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: Some(receiver),
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        let outcome = manager
            .drain_with_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain denied process");

        assert!(matches!(
            outcome,
            super::CommandExecDrainOutcome::NetworkPermissionDenied { .. }
        ));
        assert_eq!(shell_sessions.output_store().size(&handle.task_id), 0);
    }

    fn parse_test_jsonl(stdout: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
            .collect()
    }
}
