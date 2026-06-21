use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::{io::BufRead, io::BufReader};

use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};

use crate::sandbox;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_with_policy(request, cwd, ToolOutputTruncation::bytes(max_bytes))
}

pub fn execute_with_policy(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let output = sandbox::bash_command(command, cwd)
        .env_remove("ORCA_API_KEY")
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
            ToolResult::completed(request, stdout, truncated)
        }
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            let stderr = String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string();
            let message = if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                stderr
            } else {
                format!("{stdout}\n{stderr}")
            };
            let (message, truncated) = truncate_output_with_policy(message, output_truncation);
            let mut result = ToolResult::failed(request, message, output.status.code());
            result.truncated = truncated;
            result
        }
        Err(error) => ToolResult::failed(
            request,
            format!("failed to run shell command: {error}"),
            None,
        ),
    }
}

enum StreamEvent {
    Stdout(String),
    Stderr(String),
}

pub fn execute_streaming(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    on_output: &mut dyn FnMut(&str),
) -> ToolResult {
    execute_streaming_with_policy(
        request,
        cwd,
        ToolOutputTruncation::bytes(max_bytes),
        on_output,
    )
}

pub fn execute_streaming_with_policy(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    on_output: &mut dyn FnMut(&str),
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let mut command = sandbox::bash_command(command, cwd);
    let child = command
        .env_remove("ORCA_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to run shell command: {error}"),
                None,
            );
        }
    };

    let (tx, rx) = mpsc::channel();
    let stdout_handle = child.stdout.take().map(|stdout| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let _ = tx.send(StreamEvent::Stdout(format!("{line}\n")));
            }
        })
    });
    let stderr_handle = child.stderr.take().map(|stderr| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let _ = tx.send(StreamEvent::Stderr(format!("{line}\n")));
            }
        })
    });
    drop(tx);

    let mut stdout = String::new();
    let mut stderr = String::new();
    let status = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(StreamEvent::Stdout(chunk)) => {
                on_output(&chunk);
                stdout.push_str(&chunk);
            }
            Ok(StreamEvent::Stderr(chunk)) => {
                on_output(&chunk);
                stderr.push_str(&chunk);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => match child.wait() {
                Ok(status) => break status,
                Err(error) => {
                    return ToolResult::failed(
                        request,
                        format!("failed to wait for shell command: {error}"),
                        None,
                    );
                }
            },
        }
    };

    if let Some(handle) = stdout_handle {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_handle {
        let _ = handle.join();
    }
    while let Ok(event) = rx.try_recv() {
        match event {
            StreamEvent::Stdout(chunk) => {
                on_output(&chunk);
                stdout.push_str(&chunk);
            }
            StreamEvent::Stderr(chunk) => {
                on_output(&chunk);
                stderr.push_str(&chunk);
            }
        }
    }

    let stdout = stdout.trim_end().to_string();
    let stderr = stderr.trim_end().to_string();
    if status.success() {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        ToolResult::completed(request, stdout, truncated)
    } else {
        let message = if stderr.is_empty() {
            stdout
        } else if stdout.is_empty() {
            stderr
        } else {
            format!("{stdout}\n{stderr}")
        };
        let (message, truncated) = truncate_output_with_policy(message, output_truncation);
        let mut result = ToolResult::failed(request, message, status.code());
        result.truncated = truncated;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};

    #[test]
    fn streaming_reports_output_chunks_and_final_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = ToolRequest {
            id: "bash-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf 'one\\ntwo\\n'".to_string()),
            raw_arguments: None,
        };
        let mut chunks = Vec::new();

        let result = execute_streaming(&request, dir.path(), 1024, &mut |chunk| {
            chunks.push(chunk.to_string());
        });

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("one\ntwo"));
        let joined = chunks.join("");
        assert!(joined.contains("one\n"), "expected stdout in chunks");
        assert!(joined.contains("two\n"), "expected stdout in chunks");
    }
}
