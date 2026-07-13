use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use orca_core::retained_output::{
    DEFAULT_RETAINED_OUTPUT_BYTES, RetainedOutputSnapshot, read_to_retained,
};

pub const DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM: usize = DEFAULT_RETAINED_OUTPUT_BYTES;

pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_observed_bytes: usize,
    pub stderr_observed_bytes: usize,
    pub stdout_omitted_bytes: usize,
    pub stderr_omitted_bytes: usize,
    pub status: ExitStatus,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn stdout_text(&self) -> String {
        render_retained_text(&self.stdout, self.stdout_omitted_bytes)
    }

    pub fn stderr_text(&self) -> String {
        render_retained_text(&self.stderr, self.stderr_omitted_bytes)
    }

    pub fn output_was_omitted(&self) -> bool {
        self.stdout_omitted_bytes > 0 || self.stderr_omitted_bytes > 0
    }
}

fn render_retained_text(bytes: &[u8], omitted_bytes: usize) -> String {
    if omitted_bytes == 0 {
        return String::from_utf8_lossy(bytes).to_string();
    }
    let split = bytes.len().div_ceil(2);
    format!(
        "{}\n[{omitted_bytes} bytes of output omitted]\n{}",
        String::from_utf8_lossy(&bytes[..split]),
        String::from_utf8_lossy(&bytes[split..])
    )
}

pub fn preserve_ingress_omission_notice(output: String, omitted_bytes: usize) -> String {
    if omitted_bytes == 0 || output.contains("bytes of output omitted") {
        return output;
    }
    let compacted = "\n[... tool output micro-compacted ...]\n";
    let notice = format!(
        "\n[{omitted_bytes} bytes of output omitted at ingress; retained output micro-compacted]\n"
    );
    if output.contains(compacted) {
        output.replacen(compacted, &notice, 1)
    } else {
        format!("{output}{notice}")
    }
}

pub fn wait_for_child_output_with_timeout(
    child: Child,
    timeout: Duration,
) -> io::Result<CommandOutput> {
    wait_for_child_output_with_timeout_or_cancel(child, timeout, || false)
}

pub fn wait_for_child_output_with_timeout_or_cancel(
    child: Child,
    timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> io::Result<CommandOutput> {
    wait_for_child_output_with_timeout_or_cancel_and_limit(
        child,
        timeout,
        should_cancel,
        DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
    )
}

pub fn wait_for_child_output_with_timeout_or_cancel_and_limit(
    mut child: Child,
    timeout: Duration,
    should_cancel: impl Fn() -> bool,
    max_retained_bytes_per_stream: usize,
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
    let stdout_handle = spawn_stoppable_reader(
        stdout,
        max_retained_bytes_per_stream,
        Arc::clone(&reader_stop),
    );
    let stderr_handle = spawn_stoppable_reader(
        stderr,
        max_retained_bytes_per_stream,
        Arc::clone(&reader_stop),
    );

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;

    let mut status = None;
    let status = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit_status)) => {
                    status = Some(exit_status);
                    // Retire the process-group lease immediately after observing the
                    // leader exit. This closes ordinary inherited-pipe descendants
                    // without retaining a stale numeric PID until the deadline.
                    kill_process_group_by_pid(child_pid);
                    reader_stop.store(true, Ordering::Release);
                }
                Err(error) => {
                    kill_child_tree(&mut child);
                    let _ = child.wait();
                    break Err(error);
                }
                Ok(None) => {}
            }
        }
        if let Some(exit_status) = status
            && stdout_handle.is_finished()
            && stderr_handle.is_finished()
        {
            break Ok(exit_status);
        }
        if should_cancel() {
            if status.is_none() {
                kill_child_tree(&mut child);
                status = Some(child.wait()?);
            }
            reader_stop.store(true, Ordering::Release);
            break Ok(status.expect("cancelled child status"));
        }
        if Instant::now() >= deadline {
            timed_out = true;
            if status.is_none() {
                kill_child_tree(&mut child);
                status = Some(child.wait()?);
            }
            reader_stop.store(true, Ordering::Release);
            break Ok(status.expect("timed out child status"));
        }
        thread::sleep(Duration::from_millis(50));
    };

    reader_stop.store(true, Ordering::Release);
    let stdout = join_reader(stdout_handle, "stdout");
    let stderr = join_reader(stderr_handle, "stderr");
    let status = status?;
    let stdout = stdout?;
    let stderr = stderr?;

    Ok(CommandOutput {
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_observed_bytes: stdout.observed_bytes,
        stderr_observed_bytes: stderr.observed_bytes,
        stdout_omitted_bytes: stdout.omitted_bytes,
        stderr_omitted_bytes: stderr.omitted_bytes,
        status,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_output_is_bounded_at_ingress_with_exact_omission_counts() {
        let logical_bytes = 256 * 1024;
        let retained_bytes = 4096;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "printf HEAD; yes x | tr -d '\\n' | head -c {}; printf TAIL",
                logical_bytes - 8
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let child = command.spawn().expect("spawn noisy child");

        let output = wait_for_child_output_with_timeout_or_cancel_and_limit(
            child,
            Duration::from_secs(5),
            || false,
            retained_bytes,
        )
        .expect("collect bounded output");

        assert!(output.status.success());
        assert_eq!(output.stdout_observed_bytes, logical_bytes);
        assert_eq!(output.stdout.len(), retained_bytes);
        assert_eq!(output.stdout_omitted_bytes, logical_bytes - retained_bytes);
        assert!(output.stdout.starts_with(b"HEAD"));
        assert!(output.stdout.ends_with(b"TAIL"));
        assert_eq!(output.stderr_observed_bytes, 0);
        assert_eq!(output.stderr_omitted_bytes, 0);
    }

    #[test]
    #[cfg(unix)]
    fn inherited_pipe_descendant_cannot_extend_wait_past_deadline() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("(sleep 5) & printf parent-done")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let child = command.spawn().expect("spawn shell with pipe descendant");
        let start = Instant::now();

        let output = wait_for_child_output_with_timeout_or_cancel_and_limit(
            child,
            Duration::from_millis(200),
            || false,
            1024,
        )
        .expect("bounded wait");

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "reader join exceeded process deadline: {:?}",
            start.elapsed()
        );
        assert_eq!(output.stdout_text(), "parent-done");
    }

    #[test]
    #[cfg(unix)]
    fn escaped_session_descendant_cannot_extend_wait_past_deadline() {
        let helper = std::env::current_exe().expect("resolve test executable");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(
                "\"$ORCA_PROCESS_ESCAPE_HELPER\" --exact process::tests::escaped_pipe_holder_helper --nocapture & printf parent-done",
            )
            .env("ORCA_PROCESS_ESCAPE_HELPER", helper)
            .env("ORCA_PROCESS_ESCAPE_HOLDER", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let child = command
            .spawn()
            .expect("spawn shell with escaped pipe descendant");
        let start = Instant::now();

        let output = wait_for_child_output_with_timeout_or_cancel_and_limit(
            child,
            Duration::from_millis(200),
            || false,
            1024,
        )
        .expect("bounded escaped-session wait");

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "escaped reader join exceeded process deadline: {:?}",
            start.elapsed()
        );
        assert!(output.stdout_text().contains("parent-done"));
    }

    #[test]
    #[cfg(unix)]
    fn escaped_pipe_holder_helper() {
        if std::env::var_os("ORCA_PROCESS_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        thread::sleep(Duration::from_secs(5));
    }

    #[test]
    fn reader_io_failure_is_returned() {
        struct FailingReader;

        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("reader failed"))
            }
        }

        let error = join_reader(spawn_reader(FailingReader, 16), "stdout").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("reader failed"));
    }

    #[test]
    fn reader_panic_is_returned() {
        let handle =
            thread::spawn(|| -> io::Result<RetainedOutputSnapshot> { panic!("reader panicked") });

        let error = join_reader(handle, "stderr").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("stderr reader thread panicked"));
    }
}

pub fn prepare_non_interactive_command(command: &mut Command) {
    command.stdin(Stdio::null());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

#[cfg(test)]
fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    max_retained_bytes: usize,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    spawn_stoppable_reader(reader, max_retained_bytes, Arc::new(AtomicBool::new(false)))
}

fn spawn_stoppable_reader<R: Read + Send + 'static>(
    reader: R,
    max_retained_bytes: usize,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || {
        let mut reader = StoppableReader { reader, stop };
        read_to_retained(&mut reader, max_retained_bytes)
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
pub(crate) fn set_nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
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

fn join_reader(
    handle: thread::JoinHandle<io::Result<RetainedOutputSnapshot>>,
    stream: &str,
) -> io::Result<RetainedOutputSnapshot> {
    handle
        .join()
        .map_err(|_| io::Error::other(format!("{stream} reader thread panicked")))?
}

fn child_setup_error<T>(child: &mut Child, message: &str) -> io::Result<T> {
    kill_child_tree(child);
    let _ = child.wait();
    Err(io::Error::other(message))
}

pub fn kill_child_tree(child: &mut Child) {
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

    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    let pgid = -(pid as i32);
    let terminated = unsafe { kill(pgid, SIGTERM) } == 0;
    if !terminated {
        return;
    }
    thread::sleep(Duration::from_millis(50));
    unsafe {
        let _ = kill(pgid, SIGKILL);
    }
}
