use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};

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
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: if output.timed_out {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    if stderr.is_empty() {
                        format!("verifier timed out after {}s", timeout.as_secs())
                    } else {
                        format!("verifier timed out after {}s: {stderr}", timeout.as_secs())
                    }
                } else {
                    String::from_utf8_lossy(&output.stderr).trim().to_string()
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
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: ExitStatus,
    timed_out: bool,
}

fn wait_for_child_output_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> io::Result<CommandOutput> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child process has no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child process has no stderr"))?;

    let stdout_buf = Arc::new(Mutex::new(Some(Vec::new())));
    let stderr_buf = Arc::new(Mutex::new(Some(Vec::new())));
    let stdout_handle = spawn_reader(stdout, Arc::clone(&stdout_buf));
    let stderr_handle = spawn_reader(stderr, Arc::clone(&stderr_buf));

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_child_tree(&mut child);
                    break child.wait()?;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    Ok(CommandOutput {
        stdout: take_buffer(stdout_buf),
        stderr: take_buffer(stderr_buf),
        status,
        timed_out,
    })
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    buffer: Arc<Mutex<Option<Vec<u8>>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut data = Vec::new();
        let _ = reader.read_to_end(&mut data);
        if let Ok(mut slot) = buffer.lock() {
            *slot = Some(data);
        }
    })
}

fn take_buffer(buffer: Arc<Mutex<Option<Vec<u8>>>>) -> Vec<u8> {
    match Arc::try_unwrap(buffer) {
        Ok(mutex) => mutex.into_inner().ok().flatten().unwrap_or_default(),
        Err(buffer) => buffer
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_default(),
    }
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        kill_process_group(child.id());
    }
    let _ = child.kill();
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    let pgid = -(pid as i32);
    unsafe {
        let _ = kill(pgid, SIGTERM);
    }
    thread::sleep(Duration::from_millis(50));
    unsafe {
        let _ = kill(pgid, SIGKILL);
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
}
