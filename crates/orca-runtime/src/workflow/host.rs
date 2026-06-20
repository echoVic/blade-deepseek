use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

    pub fn run_collecting_events_with_agent<F>(
        script_path: &Path,
        args: Value,
        mut on_agent_call: F,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: FnMut(AgentCall) -> io::Result<HostCommand>,
    {
        let host_path = ensure_host_file()?;
        let args_json = serde_json::to_string(&args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

        let mut child = Command::new("node")
            .arg(&host_path)
            .arg(script_path)
            .arg(args_json)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdin"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdout"))?;
        let reader = BufReader::new(stdout);

        let mut events = Vec::new();
        let mut workflow_failed = None;
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
            if let HostEvent::AgentCall {
                call_id,
                call_path,
                phase,
                prompt,
                opts,
            } = &event
            {
                let command = on_agent_call(AgentCall {
                    call_id: call_id.clone(),
                    call_path: call_path.clone(),
                    phase: phase.clone(),
                    prompt: prompt.clone(),
                    opts: opts.clone(),
                })?;
                let command_json = serde_json::to_string(&command)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                stdin.write_all(command_json.as_bytes())?;
                stdin.write_all(b"\n")?;
                stdin.flush()?;
            }
            events.push(event);
            if is_terminal {
                break;
            }
        }
        drop(stdin);

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
    let path = env::temp_dir().join("orca-workflow-host.mjs");
    fs::write(&path, include_str!("host.mjs"))?;
    Ok(path)
}
