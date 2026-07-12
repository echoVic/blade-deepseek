use orca_core::retained_output::RetainedOutput;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

    let ingress_truncated = output.output_was_omitted();
    let stdout = output.stdout_text().trim_end().to_string();
    let stderr = output.stderr_text().trim_end().to_string();
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
        let message = process::preserve_ingress_omission_notice(
            message,
            output
                .stdout_omitted_bytes
                .saturating_add(output.stderr_omitted_bytes),
        );
        let mut result = ToolResult::failed(request, message, output.status.code());
        result.truncated = ingress_truncated || truncated;
        return result;
    }
    let timed_out = output.timed_out;
    if output.status.success() && !timed_out {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        let stdout = process::preserve_ingress_omission_notice(stdout, output.stdout_omitted_bytes);
        return ToolResult::completed(request, stdout, ingress_truncated || truncated);
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
    let message = process::preserve_ingress_omission_notice(
        message,
        output
            .stdout_omitted_bytes
            .saturating_add(output.stderr_omitted_bytes),
    );
    let mut result = ToolResult::failed(
        request,
        message,
        if timed_out {
            None
        } else {
            output.status.code()
        },
    );
    result.truncated = ingress_truncated || truncated;
    result
}

enum StreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

const STREAM_OUTPUT_CHANNEL_CAPACITY: usize = 8;
const STREAM_OUTPUT_READ_CHUNK_BYTES: usize = 8 * 1024;
const STREAM_LIVE_PREVIEW_BYTES: usize = orca_core::tool_types::MAX_TOOL_OUTPUT_BYTES;

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
    execute_streaming_command_or_cancel(
        request,
        sandbox::bash_command_with_additional_roots(command, cwd, additional_roots),
        output_truncation,
        shell_timeout,
        on_output,
        should_cancel,
    )
}

/// Stream a prebuilt (typically sandboxed) shell command. Callers that derive
/// their own sandbox profile (e.g. from a permission profile) build the
/// `Command` via `sandbox::*` and pass it here.
pub fn execute_streaming_command_or_cancel(
    request: &ToolRequest,
    mut command: std::process::Command,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let mut child = match command
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
    let (tx, rx) = mpsc::sync_channel(STREAM_OUTPUT_CHANNEL_CAPACITY);
    let stdout_handle = child.stdout.take().map(|stdout| {
        let tx = tx.clone();
        std::thread::spawn(move || stream_pipe(stdout, tx, StreamEvent::Stdout))
    });
    let stderr_handle = child.stderr.take().map(|stderr| {
        let tx = tx.clone();
        std::thread::spawn(move || stream_pipe(stderr, tx, StreamEvent::Stderr))
    });
    drop(tx);

    let mut stdout = RetainedOutput::new(process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM);
    let mut stderr = RetainedOutput::new(process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM);
    let mut preview_remaining = STREAM_LIVE_PREVIEW_BYTES;
    let deadline = Instant::now()
        .checked_add(shell_timeout)
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(StreamEvent::Stdout(chunk)) => {
                emit_live_preview(&chunk, &mut preview_remaining, on_output);
                stdout.append(&chunk);
            }
            Ok(StreamEvent::Stderr(chunk)) => {
                emit_live_preview(&chunk, &mut preview_remaining, on_output);
                stderr.append(&chunk);
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

    while let Ok(event) = rx.recv() {
        match event {
            StreamEvent::Stdout(chunk) => {
                emit_live_preview(&chunk, &mut preview_remaining, on_output);
                stdout.append(&chunk);
            }
            StreamEvent::Stderr(chunk) => {
                emit_live_preview(&chunk, &mut preview_remaining, on_output);
                stderr.append(&chunk);
            }
        }
    }
    let stdout_reader = join_stream_reader(stdout_handle, "stdout");
    let stderr_reader = join_stream_reader(stderr_handle, "stderr");

    if let Err(error) = stdout_reader.and(stderr_reader) {
        return ToolResult::failed(request, error, status.code());
    }

    let stdout = stdout.into_snapshot();
    let stderr = stderr.into_snapshot();
    let stdout_omitted_bytes = stdout.omitted_bytes;
    let stderr_omitted_bytes = stderr.omitted_bytes;
    let ingress_truncated = stdout.is_truncated() || stderr.is_truncated();
    let stdout = String::from_utf8_lossy(&stdout.rendered_bytes())
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&stderr.rendered_bytes())
        .trim_end()
        .to_string();
    if status.success() && !timed_out && !cancelled {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        let stdout = process::preserve_ingress_omission_notice(stdout, stdout_omitted_bytes);
        ToolResult::completed(request, stdout, ingress_truncated || truncated)
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
        let message = process::preserve_ingress_omission_notice(
            message,
            stdout_omitted_bytes.saturating_add(stderr_omitted_bytes),
        );
        let mut result = ToolResult::failed(request, message, status.code());
        result.truncated = ingress_truncated || truncated;
        result
    }
}

fn stream_pipe(
    mut pipe: impl Read,
    tx: mpsc::SyncSender<StreamEvent>,
    event: fn(Vec<u8>) -> StreamEvent,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; STREAM_OUTPUT_READ_CHUNK_BYTES];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                if tx.send(event(buffer[..read].to_vec())).is_err() {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn join_stream_reader(
    handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    stream: &str,
) -> Result<(), String> {
    let Some(handle) = handle else {
        return Ok(());
    };
    handle
        .join()
        .map_err(|_| format!("{stream} reader thread panicked"))?
        .map_err(|error| format!("failed to read shell {stream}: {error}"))
}

fn emit_live_preview(bytes: &[u8], remaining: &mut usize, on_output: &mut dyn FnMut(&str)) {
    if *remaining == 0 {
        return;
    }
    let admitted = bytes.len().min(*remaining);
    if admitted > 0 {
        on_output(&String::from_utf8_lossy(&bytes[..admitted]));
        *remaining -= admitted;
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

    #[test]
    fn streaming_reader_io_failure_is_returned() {
        let handle = std::thread::spawn(|| Err(std::io::Error::other("reader failed")));

        let error = join_stream_reader(Some(handle), "stdout").unwrap_err();

        assert!(error.contains("failed to read shell stdout"));
        assert!(error.contains("reader failed"));
    }

    #[test]
    fn streaming_reader_panic_is_returned() {
        let handle = std::thread::spawn(|| -> std::io::Result<()> { panic!("reader panicked") });

        let error = join_stream_reader(Some(handle), "stderr").unwrap_err();

        assert!(error.contains("stderr reader thread panicked"));
    }
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
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
    fn streaming_large_unterminated_output_is_bounded_before_result_truncation() {
        let dir = tempfile::TempDir::new().unwrap();
        let logical_bytes = process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM * 2;
        let request = bash_request(&format!(
            "printf HEAD; yes x | tr -d '\\n' | head -c {}; printf TAIL",
            logical_bytes - 8
        ));
        let mut streamed_bytes = 0usize;

        let result = execute_streaming_with_policy(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(4096),
            Duration::from_secs(10),
            &mut |chunk| streamed_bytes = streamed_bytes.saturating_add(chunk.len()),
        );

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.truncated);
        let output = result.output.as_deref().expect("bounded output");
        assert!(
            output.starts_with("HEAD"),
            "missing stable prefix: {output}"
        );
        assert!(output.ends_with("TAIL"), "missing rolling suffix: {output}");
        assert!(
            output.contains("omitted"),
            "missing omission marker: {output}"
        );
        assert!(
            streamed_bytes <= process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
            "live callback admitted {streamed_bytes} bytes"
        );
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
    fn noisy_streaming_timeout_does_not_deadlock_reader_shutdown() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("while :; do printf 1234567890; done");
        let start = Instant::now();
        let mut delayed_callback = false;

        let result = execute_streaming_with_policy(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_millis(100),
            &mut |_| {
                if !delayed_callback {
                    delayed_callback = true;
                    std::thread::sleep(Duration::from_millis(250));
                }
            },
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "noisy streaming timeout deadlocked reader shutdown: {:?}",
            start.elapsed()
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
        let request = bash_request("printf 'before\\n'; sleep 5; printf after");
        let mut chunks = Vec::new();
        let start = Instant::now();
        let saw_output = Arc::new(AtomicBool::new(false));
        let saw_output_for_chunk = Arc::clone(&saw_output);
        let saw_output_for_cancel = Arc::clone(&saw_output);

        let result = execute_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            &mut |chunk| {
                if chunk.contains("before") {
                    saw_output_for_chunk.store(true, Ordering::SeqCst);
                }
                chunks.push(chunk.to_string());
            },
            || {
                (saw_output_for_cancel.load(Ordering::SeqCst)
                    && start.elapsed() >= Duration::from_millis(100))
                    || start.elapsed() >= Duration::from_secs(1)
            },
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
    fn noisy_streaming_cancel_does_not_deadlock_reader_shutdown() {
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("while :; do printf 1234567890; done");
        let start = Instant::now();
        let mut delayed_callback = false;

        let result = execute_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            &mut |_| {
                if !delayed_callback {
                    delayed_callback = true;
                    std::thread::sleep(Duration::from_millis(250));
                }
            },
            || start.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "noisy streaming cancel deadlocked reader shutdown: {:?}",
            start.elapsed()
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
    fn bash_command_allows_additional_working_directory_writes() {
        if !crate::sandbox::seatbelt_available() {
            return;
        }

        let parent = crate::sandbox::sandbox_test_parent("bash-additional-roots-");
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
