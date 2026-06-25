use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    PhaseStarted {
        name: String,
    },
    PhaseCompleted {
        name: String,
    },
    PhaseFailed {
        name: String,
        error: String,
        #[serde(default)]
        fallback: Option<String>,
    },
    AgentCall {
        call_id: String,
        call_path: String,
        phase: Option<String>,
        prompt: String,
        opts: Value,
    },
    WorkflowCompleted {
        result: Value,
    },
    WorkflowFailed {
        error: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostCommand {
    AgentResult { call_id: String, result: Value },
    AgentError { call_id: String, error: String },
}

#[derive(Clone, Debug, Default)]
pub struct WorkflowHost;

#[derive(Clone, Debug)]
pub struct WorkflowHostIpcPaths {
    pub mailbox_path: PathBuf,
    pub task_lists_path: PathBuf,
}

impl WorkflowHost {
    pub fn node_available() -> bool {
        Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    pub fn run_collecting_events(script_path: &Path, args: Value) -> io::Result<Vec<HostEvent>> {
        Self::run_collecting_events_with_agent(script_path, args, |call| {
            Ok(HostCommand::AgentResult {
                call_id: call.call_id.clone(),
                result: serde_json::json!({
                    "callId": call.call_id,
                    "prompt": call.prompt,
                    "cached": false,
                }),
            })
        })
    }

    pub fn run_collecting_events_with_ipc_paths(
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
    ) -> io::Result<Vec<HostEvent>> {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            script_path,
            args,
            Some(ipc_paths),
            |call| {
                Ok(HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({
                        "callId": call.call_id,
                        "prompt": call.prompt,
                        "cached": false,
                    }),
                })
            },
            |_| Ok(()),
        )
    }

    pub fn run_collecting_events_with_agent<F>(
        script_path: &Path,
        args: Value,
        on_agent_call: F,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
    {
        Self::run_collecting_events_with_agent_and_event_callback(
            script_path,
            args,
            on_agent_call,
            |_| Ok(()),
        )
    }

    pub fn run_collecting_events_with_agent_and_event_callback<F, E>(
        script_path: &Path,
        args: Value,
        on_agent_call: F,
        on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            script_path,
            args,
            None,
            on_agent_call,
            on_event,
        )
    }

    pub fn run_collecting_events_with_agent_and_event_callback_with_ipc_paths<F, E>(
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
        on_agent_call: F,
        on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            script_path,
            args,
            Some(ipc_paths),
            on_agent_call,
            on_event,
        )
    }

    fn run_collecting_events_with_agent_and_event_callback_inner<F, E>(
        script_path: &Path,
        args: Value,
        ipc_paths: Option<&WorkflowHostIpcPaths>,
        on_agent_call: F,
        mut on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        let host_path = ensure_host_file()?;
        let args_json = serde_json::to_string(&args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

        let mut command = Command::new("node");
        command.arg(&host_path).arg(script_path).arg(args_json);
        if let Some(ipc_paths) = ipc_paths {
            command
                .arg(&ipc_paths.mailbox_path)
                .arg(&ipc_paths.task_lists_path);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdin"))?;
        let stdin = Arc::new(Mutex::new(stdin));

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdout"))?;
        let reader = BufReader::new(stdout);

        let mut events = Vec::new();
        let mut workflow_failed = None;
        let agent_error = Arc::new(Mutex::new(None));
        let on_agent_call = &on_agent_call;
        thread::scope(|scope| -> io::Result<()> {
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }

                let event: HostEvent = serde_json::from_str(&line)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if let HostEvent::WorkflowFailed { error } = &event {
                    workflow_failed = Some(error.clone());
                }
                let is_terminal = matches!(
                    event,
                    HostEvent::WorkflowCompleted { .. } | HostEvent::WorkflowFailed { .. }
                );
                on_event(&event)?;
                if let HostEvent::AgentCall {
                    call_id,
                    call_path,
                    phase,
                    prompt,
                    opts,
                } = &event
                {
                    let call = AgentCall {
                        call_id: call_id.clone(),
                        call_path: call_path.clone(),
                        phase: phase.clone(),
                        prompt: prompt.clone(),
                        opts: opts.clone(),
                    };
                    let writer = Arc::clone(&stdin);
                    let error_slot = Arc::clone(&agent_error);
                    scope.spawn(move || {
                        let call_id = call.call_id.clone();
                        let command = match on_agent_call(call) {
                            Ok(command) => command,
                            Err(error) => {
                                record_first_agent_error(&error_slot, error.to_string());
                                HostCommand::AgentError {
                                    call_id,
                                    error: "workflow host failed to answer agent call".to_string(),
                                }
                            }
                        };

                        if let Err(error) = write_host_command(&writer, &command) {
                            record_first_agent_error(&error_slot, error.to_string());
                        }
                    });
                }
                events.push(event);
                if is_terminal {
                    break;
                }
            }
            Ok(())
        })?;
        drop(stdin);

        if let Some(error) = agent_error
            .lock()
            .map_err(|_| io::Error::other("workflow agent error lock poisoned"))?
            .clone()
        {
            return Err(io::Error::other(error));
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            if workflow_failed.is_some() {
                return Ok(events);
            }

            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let message = if stderr.is_empty() {
                format!("workflow host exited with status {}", output.status)
            } else {
                format!(
                    "workflow host exited with status {}: {stderr}",
                    output.status
                )
            };
            return Err(io::Error::other(message));
        }

        Ok(events)
    }
}

fn write_host_command(writer: &Arc<Mutex<impl Write>>, command: &HostCommand) -> io::Result<()> {
    let command_json = serde_json::to_string(command)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("workflow host stdin lock poisoned"))?;
    writer.write_all(command_json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn record_first_agent_error(error_slot: &Arc<Mutex<Option<String>>>, error: String) {
    if let Ok(mut slot) = error_slot.lock() {
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCall {
    pub call_id: String,
    pub call_path: String,
    pub phase: Option<String>,
    pub prompt: String,
    pub opts: Value,
}

fn ensure_host_file() -> io::Result<PathBuf> {
    static HOST_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = HOST_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "orca-workflow-host-{}-{seq}.mjs",
        std::process::id()
    ));
    fs::write(&path, include_str!("host.mjs"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_file_paths_are_unique_for_parallel_tests() {
        let first = ensure_host_file().unwrap();
        let second = ensure_host_file().unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
    }
}
