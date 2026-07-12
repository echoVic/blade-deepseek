use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};

use crate::retained_output::{
    DEFAULT_RETAINED_OUTPUT_BYTES, RetainedOutputSnapshot, read_to_retained,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerificationResult {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(command: &str) -> VerificationResult {
    run_with_timeout(command, Duration::from_secs(30))
}

fn run_with_timeout(command: &str, timeout: Duration) -> VerificationResult {
    let mut child_command = Command::new("sh");
    child_command
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        child_command.process_group(0);
    }

    let child = child_command.spawn();

    match child {
        Ok(child) => match wait_for_child_output_with_timeout(child, timeout) {
            Ok(output) => VerificationResult {
                command: command.to_string(),
                success: output.status.success() && !output.timed_out,
                exit_code: if output.timed_out {
                    None
                } else {
                    output.status.code()
                },
                stdout: output.stdout_text().trim().to_string(),
                stderr: if output.timed_out {
                    let stderr = output.stderr_text().trim().to_string();
                    if stderr.is_empty() {
                        format!("verifier timed out after {}s", timeout.as_secs())
                    } else {
                        format!("verifier timed out after {}s: {stderr}", timeout.as_secs())
                    }
                } else {
                    output.stderr_text().trim().to_string()
                },
            },
            Err(error) => VerificationResult {
                command: command.to_string(),
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to run verifier: {error}"),
            },
        },
        Err(error) => VerificationResult {
            command: command.to_string(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to run verifier: {error}"),
        },
    }
}

struct CommandOutput {
    stdout: RetainedOutputSnapshot,
    stderr: RetainedOutputSnapshot,
    status: ExitStatus,
    timed_out: bool,
}

impl CommandOutput {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout.rendered_bytes()).to_string()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.rendered_bytes()).to_string()
    }
}

fn wait_for_child_output_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> io::Result<CommandOutput> {
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child process has no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child process has no stderr"))?;
    let stdout_handle =
        thread::spawn(move || read_to_retained(stdout, DEFAULT_RETAINED_OUTPUT_BYTES));
    let stderr_handle =
        thread::spawn(move || read_to_retained(stderr, DEFAULT_RETAINED_OUTPUT_BYTES));
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(std::time::Instant::now);
    let mut timed_out = false;
    let mut status = None;
    let status = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit_status)) => status = Some(exit_status),
                Ok(None) => {}
                Err(error) => {
                    kill_child_tree(&mut child);
                    let _ = child.wait();
                    break Err(error);
                }
            }
        }
        if status.is_some() && stdout_handle.is_finished() && stderr_handle.is_finished() {
            break Ok(status.expect("completed verifier status"));
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            kill_process_group_by_pid(child_pid);
            if status.is_none() {
                let _ = child.kill();
                status = Some(child.wait()?);
            }
            break Ok(status.expect("timed out verifier status"));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = stdout_handle
        .join()
        .map_err(|_| io::Error::other("verifier stdout reader panicked"))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| io::Error::other("verifier stderr reader panicked"))??;
    Ok(CommandOutput {
        stdout,
        stderr,
        status: status?,
        timed_out,
    })
}

fn kill_child_tree(child: &mut Child) {
    kill_process_group_by_pid(child.id());
    let _ = child.kill();
}

fn kill_process_group_by_pid(pid: u32) {
    #[cfg(unix)]
    kill_process_group(pid);
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let pgid = -(pid as i32);
    unsafe {
        let _ = kill(pgid, 15);
    }
    thread::sleep(Duration::from_millis(50));
    unsafe {
        let _ = kill(pgid, 9);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn verifier_command_timeout_kills_descendant_processes() {
        let start = Instant::now();

        let result = run_with_timeout(
            "printf before; sleep 5; printf after",
            Duration::from_millis(200),
        );

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "verifier should not wait for descendant processes"
        );
        assert!(!result.success);
        assert!(result.stderr.contains("timed out"), "{result:?}");
        assert_eq!(result.stdout, "before");
    }

    #[test]
    fn verifier_output_is_bounded_at_ingress() {
        let result = run("printf HEAD; yes x | tr -d '\\n' | head -c 2097144; printf TAIL");

        assert!(result.success, "{}", result.stderr);
        assert!(result.stdout.len() <= 1024 * 1024 + 128);
        assert!(result.stdout.starts_with("HEAD"));
        assert!(result.stdout.ends_with("TAIL"));
        assert!(result.stdout.contains("omitted"));
    }

    #[test]
    #[cfg(unix)]
    fn inherited_pipe_descendant_cannot_extend_verifier_deadline() {
        let start = Instant::now();

        let result = run_with_timeout("(sleep 5) & printf parent-done", Duration::from_millis(200));

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "verifier reader join exceeded deadline: {:?}",
            start.elapsed()
        );
        assert!(!result.success);
        assert_eq!(result.stdout, "parent-done");
        assert!(result.stderr.contains("timed out"), "{result:?}");
    }
}
