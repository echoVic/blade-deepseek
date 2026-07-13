use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::AsRawFd;
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
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return child_setup_error(&mut child, "child process has no stdout"),
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return child_setup_error(&mut child, "child process has no stderr"),
    };
    #[cfg(unix)]
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        kill_child_tree(&mut child);
        let _ = child.wait();
        return Err(error);
    }
    let reader_stop = Arc::new(AtomicBool::new(false));
    let stdout_handle = spawn_stoppable_reader(stdout, Arc::clone(&reader_stop));
    let stderr_handle = spawn_stoppable_reader(stderr, Arc::clone(&reader_stop));
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(std::time::Instant::now);
    let mut timed_out = false;
    let mut status = None;
    let status = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit_status)) => {
                    status = Some(exit_status);
                    let drain_deadline = std::time::Instant::now() + Duration::from_millis(20);
                    while (!stdout_handle.is_finished() || !stderr_handle.is_finished())
                        && std::time::Instant::now() < drain_deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    if !stdout_handle.is_finished() || !stderr_handle.is_finished() {
                        timed_out = true;
                    }
                    kill_process_group_by_pid(child_pid);
                    reader_stop.store(true, Ordering::Release);
                }
                Ok(None) => {}
                Err(error) => {
                    kill_child_tree(&mut child);
                    let _ = child.wait();
                    break Err(error);
                }
            }
        }
        if let Some(exit_status) = status
            && stdout_handle.is_finished()
            && stderr_handle.is_finished()
        {
            break Ok(exit_status);
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            if status.is_none() {
                kill_child_tree(&mut child);
                status = Some(child.wait()?);
            }
            reader_stop.store(true, Ordering::Release);
            break Ok(status.expect("timed out verifier status"));
        }
        thread::sleep(Duration::from_millis(50));
    };
    reader_stop.store(true, Ordering::Release);
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

fn spawn_stoppable_reader<R: Read + Send + 'static>(
    reader: R,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || {
        let mut reader = StoppableReader { reader, stop };
        read_to_retained(&mut reader, DEFAULT_RETAINED_OUTPUT_BYTES)
    })
}

struct StoppableReader<R> {
    reader: R,
    stop: Arc<AtomicBool>,
}

impl<R: Read> Read for StoppableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.reader.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if self.stop.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn child_setup_error<T>(child: &mut Child, message: &str) -> io::Result<T> {
    kill_child_tree(child);
    let _ = child.wait();
    Err(io::Error::other(message))
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
    let terminated = unsafe { kill(pgid, 15) } == 0;
    if !terminated {
        return;
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
        assert!(result.stdout.contains("parent-done"), "{result:?}");
        assert!(result.stderr.contains("timed out"), "{result:?}");
    }

    #[test]
    #[cfg(unix)]
    fn escaped_session_descendant_cannot_extend_verifier_deadline() {
        let helper = std::env::current_exe().expect("resolve test executable");
        let command = format!(
            "ORCA_VERIFIER_ESCAPE_HOLDER=1 {helper:?} --exact verification::tests::escaped_verifier_pipe_holder_helper --nocapture & printf parent-done"
        );
        let start = Instant::now();

        let result = run_with_timeout(&command, Duration::from_millis(200));

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "escaped verifier reader join exceeded deadline: {:?}",
            start.elapsed()
        );
        assert!(!result.success);
        assert!(result.stdout.contains("parent-done"), "{result:?}");
        assert!(result.stderr.contains("timed out"), "{result:?}");
    }

    #[test]
    #[cfg(unix)]
    fn escaped_verifier_pipe_holder_helper() {
        if std::env::var_os("ORCA_VERIFIER_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        thread::sleep(Duration::from_secs(5));
    }
}
