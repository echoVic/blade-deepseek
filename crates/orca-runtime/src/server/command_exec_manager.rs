use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::Value;

use super::{
    PendingCommandExecPermissionRequest, cap_text, capped_delta, capped_utf8_len,
    command_exec_network_permission_block,
};
use crate::network_proxy::{RuntimeNetworkBlockReport, RuntimeNetworkProxy};
use crate::protocol::{self, ServerEvent};
use crate::sandbox_denial::{
    SandboxDenialDiagnostic, diagnose_sandbox_denial,
    should_request_filesystem_permission_with_denied_roots,
};
use crate::shell_session::RuntimeShellSessionManager;

pub(super) struct CommandExecProcess {
    pub(super) shell_id: Option<String>,
    pub(super) command_event_id: Value,
    pub(super) command: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) denied_writable_roots: Vec<PathBuf>,
    pub(super) stream_output: bool,
    pub(super) output_bytes_cap: Option<usize>,
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
    FileSystemPermissionRequired {
        request: PendingCommandExecPermissionRequest,
        diagnostic: SandboxDenialDiagnostic,
    },
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
                let output = match manager.read(&shell_id, Duration::from_millis(1)) {
                    Ok(output) => output,
                    Err(_) => continue,
                };
                let Some(process) = self.get(&process_id) else {
                    continue;
                };
                if process.stream_output {
                    let stdout_observed_len = output.stdout.len();
                    let stderr_observed_len = output.stderr.len();
                    let stdout_delta =
                        capped_delta(&output.stdout, process.stdout_len, process.output_bytes_cap);
                    let stderr_delta =
                        capped_delta(&output.stderr, process.stderr_len, process.output_bytes_cap);
                    let stdout_cap_reached = process
                        .output_bytes_cap
                        .is_some_and(|cap| output.stdout.len() >= cap)
                        && !process.stdout_cap_reached;
                    let stderr_cap_reached = process
                        .output_bytes_cap
                        .is_some_and(|cap| output.stderr.len() >= cap)
                        && !process.stderr_cap_reached;
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
                        process.stdout_len = match process.output_bytes_cap {
                            Some(cap) => capped_utf8_len(&output.stdout, cap),
                            None => stdout_observed_len,
                        };
                        process.stderr_len = match process.output_bytes_cap {
                            Some(cap) => capped_utf8_len(&output.stderr, cap),
                            None => stderr_observed_len,
                        };
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
                        .and_then(command_exec_network_permission_block)
                    {
                        let request = process.permission_request.expect(
                            "command/exec process with network block reporter has retry request",
                        );
                        return Ok(CommandExecDrainOutcome::NetworkPermissionRequired {
                            request,
                            block,
                        });
                    }
                    if let Some(diagnostic) =
                        diagnose_sandbox_denial(&process.cwd, &output.stdout, &output.stderr)
                    {
                        let should_request_permission =
                            should_request_filesystem_permission_with_denied_roots(
                                &process.cwd,
                                &diagnostic,
                                &process.denied_writable_roots,
                            ) || diagnostic.suggested_write_root.is_none();
                        if should_request_permission
                            && let Some(request) = process.permission_request
                        {
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
