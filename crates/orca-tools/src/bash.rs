use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{io::BufRead, io::BufReader};

use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};

use crate::process;
use crate::sandbox;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_with_policy(
        request,
        cwd,
        ToolOutputTruncation::bytes(max_bytes),
        Duration::from_secs(120),
    )
}

pub fn execute_with_policy(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
) -> ToolResult {
    execute_with_policy_or_cancel(request, cwd, output_truncation, shell_timeout, || false)
}

pub fn execute_with_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_policy_roots_or_cancel(
        request,
        cwd,
        &[],
        output_truncation,
        shell_timeout,
        should_cancel,
    )
}

pub fn execute_with_policy_roots_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[std::path::PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let child = match sandbox::bash_command_with_additional_roots(command, cwd, additional_roots)
        .env_remove("ORCA_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to run shell command: {error}"),
                None,
            );
        }
    };

    let output = match process::wait_for_child_output_with_timeout_or_cancel(
        child,
        shell_timeout,
        &should_cancel,
    ) {
        Ok(output) => output,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to wait for shell command: {error}"),
                None,
            );
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_string();
    if should_cancel() {
        let message = if stderr.is_empty() && stdout.is_empty() {
            "shell command cancelled".to_string()
        } else if stderr.is_empty() {
            format!("shell command cancelled: {stdout}")
        } else if stdout.is_empty() {
            format!("shell command cancelled: {stderr}")
        } else {
            format!("shell command cancelled: {stdout}\n{stderr}")
        };
        let (message, truncated) = truncate_output_with_policy(message, output_truncation);
        let mut result = ToolResult::failed(request, message, output.status.code());
        result.truncated = truncated;
        return result;
    }
    let timed_out = output.timed_out;
    if output.status.success() && !timed_out {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        return ToolResult::completed(request, stdout, truncated);
    }

    let message = if timed_out {
        if stderr.is_empty() && stdout.is_empty() {
            format!("shell command timed out after {}s", shell_timeout.as_secs())
        } else if stderr.is_empty() {
            format!(
                "shell command timed out after {}s: {stdout}",
                shell_timeout.as_secs()
            )
        } else if stdout.is_empty() {
            format!(
                "shell command timed out after {}s: {stderr}",
                shell_timeout.as_secs()
            )
        } else {
            format!(
                "shell command timed out after {}s: {stdout}\n{stderr}",
                shell_timeout.as_secs()
            )
        }
    } else if stderr.is_empty() {
        stdout
    } else if stdout.is_empty() {
        stderr
    } else {
        format!("{stdout}\n{stderr}")
    };
    let (message, truncated) = truncate_output_with_policy(message, output_truncation);
    let mut result = ToolResult::failed(
        request,
        message,
        if timed_out {
            None
        } else {
            output.status.code()
        },
    );
    result.truncated = truncated;
    result
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
        Duration::from_secs(120),
        on_output,
    )
}

pub fn execute_streaming_with_policy(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
) -> ToolResult {
    execute_streaming_with_policy_or_cancel(
        request,
        cwd,
        output_truncation,
        shell_timeout,
        on_output,
        || false,
    )
}

pub fn execute_streaming_with_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_streaming_with_policy_roots_or_cancel(
        request,
        cwd,
        &[],
        output_truncation,
        shell_timeout,
        on_output,
        should_cancel,
    )
}

pub fn execute_streaming_with_policy_roots_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[std::path::PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let mut child =
        match sandbox::bash_command_with_additional_roots(command, cwd, additional_roots)
            .env_remove("ORCA_API_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
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
    let deadline = Instant::now()
        .checked_add(shell_timeout)
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let mut cancelled = false;
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
        if should_cancel() {
            cancelled = true;
            process::kill_child_tree(&mut child);
            match child.wait() {
                Ok(status) => break status,
                Err(error) => {
                    return ToolResult::failed(
                        request,
                        format!("failed to wait for shell command: {error}"),
                        None,
                    );
                }
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            process::kill_child_tree(&mut child);
            match child.wait() {
                Ok(status) => break status,
                Err(error) => {
                    return ToolResult::failed(
                        request,
                        format!("failed to wait for shell command: {error}"),
                        None,
                    );
                }
            }
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
    if status.success() && !timed_out && !cancelled {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        ToolResult::completed(request, stdout, truncated)
    } else {
        let message = if cancelled {
            cancelled_message(&stdout, &stderr)
        } else if timed_out {
            timeout_message(shell_timeout, &stdout, &stderr)
        } else if stderr.is_empty() {
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

fn cancelled_message(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() && stdout.is_empty() {
        "shell command cancelled".to_string()
    } else if stderr.is_empty() {
        format!("shell command cancelled: {stdout}")
    } else if stdout.is_empty() {
        format!("shell command cancelled: {stderr}")
    } else {
        format!("shell command cancelled: {stdout}\n{stderr}")
    }
}

fn timeout_message(shell_timeout: Duration, stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() && stdout.is_empty() {
        format!("shell command timed out after {}s", shell_timeout.as_secs())
    } else if stderr.is_empty() {
        format!(
            "shell command timed out after {}s: {stdout}",
            shell_timeout.as_secs()
        )
    } else if stdout.is_empty() {
        format!(
            "shell command timed out after {}s: {stderr}",
            shell_timeout.as_secs()
        )
    } else {
        format!(
            "shell command timed out after {}s: {stdout}\n{stderr}",
            shell_timeout.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use std::time::Instant;

    fn bash_request(command: &str) -> ToolRequest {
        ToolRequest {
            id: "bash-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.to_string()),
            raw_arguments: None,
        }
    }

    #[test]
    fn streaming_reports_output_chunks_and_final_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("printf 'one\\ntwo\\n'");
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

    #[test]
    fn bash_commands_receive_eof_on_stdin_instead_of_inheriting_terminal() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("read line; printf done");
        let start = Instant::now();

        let result = execute_with_policy(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(2),
        );

        assert!(
            start.elapsed() < Duration::from_millis(500),
            "stdin should be closed without waiting for timeout"
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("done"));
    }

    #[test]
    fn streaming_respects_shell_timeout_and_returns_partial_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("printf before; sleep 5; printf after");
        let mut chunks = Vec::new();
        let start = Instant::now();

        let result = execute_streaming_with_policy(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_millis(200),
            &mut |chunk| chunks.push(chunk.to_string()),
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "streaming command should not wait for the child to finish"
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("shell command timed out after 0s"),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            chunks.join("").contains("before"),
            "partial output should be streamed before timeout"
        );
    }

    #[test]
    fn bash_wait_observes_cancel_callback() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("printf before; sleep 5; printf after");
        let start = Instant::now();

        let result = execute_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            || start.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancelled command should not wait for the shell timeout"
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("shell command cancelled"),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[test]
    fn streaming_bash_wait_observes_cancel_callback() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("printf before; sleep 5; printf after");
        let mut chunks = Vec::new();
        let start = Instant::now();

        let result = execute_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            &mut |chunk| chunks.push(chunk.to_string()),
            || start.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancelled streaming command should not wait for the shell timeout"
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("shell command cancelled"),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            chunks.join("").contains("before"),
            "partial output should still stream before cancellation"
        );
    }

    #[test]
    fn bash_command_allows_additional_working_directory_writes() {
        if !crate::sandbox::seatbelt_available() {
            return;
        }

        let parent = tempfile::TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        let extra = parent.path().join("extra");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&extra).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let extra_file = extra.join("allowed.txt");
        let outside_file = outside.join("blocked.txt");
        let request = bash_request(&format!(
            "printf allowed > {} && printf blocked > {}",
            extra_file.display(),
            outside_file.display()
        ));

        let result = execute_with_policy_roots_or_cancel(
            &request,
            &workspace,
            std::slice::from_ref(&extra),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(5),
            || false,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(std::fs::read_to_string(extra_file).unwrap(), "allowed");
        assert!(!outside_file.exists());
    }
}
