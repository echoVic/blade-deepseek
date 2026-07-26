#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PROMPT: &str = "typed TUI PTY submit";
const ASSISTANT_SENTINEL: &str = "Mock runtime completed the headless harness contract.";

#[test]
fn tui_submit_renders_and_restores_the_terminal() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "TUI did not render the typed assistant terminal",
    );

    arm_idle_exit(&mut process, &mut output);

    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert!(
        output
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "TUI did not enter the alternate screen"
    );
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "TUI did not restore the primary screen"
    );
}

#[test]
fn tui_permission_round_trips_through_the_runtime_surface() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    const PERMISSION_SENTINEL: &str = "PTY_PERMISSION_RESUMED";
    let prompt = format!(
        "request_permissions_then_bash {} :: printf '\\120\\124\\131\\137\\120\\105\\122\\115\\111\\123\\123\\111\\117\\116\\137\\122\\105\\123\\125\\115\\105\\104'",
        cwd.path().display()
    );
    assert!(
        !prompt.contains(PERMISSION_SENTINEL),
        "the post-permission sentinel must not be present in the rendered prompt"
    );
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), &prompt)
        .expect("spawn permission TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "Filesystem Permission Required",
        Duration::from_secs(10),
        "TUI did not render the runtime-owned permission",
    );
    process.write(b"1").expect("allow permission once");
    receive_until(
        &process,
        &mut output,
        "Approval Required",
        Duration::from_secs(10),
        "TUI did not advance to the runtime-owned tool approval",
    );
    process.write(b"1").expect("approve bash once");
    receive_until(
        &process,
        &mut output,
        PERMISSION_SENTINEL,
        Duration::from_secs(10),
        "TUI did not resume after the typed permission response",
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_cancel_returns_to_idle_through_the_runtime_surface() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "mock_stream_delay_ms 5000")
            .expect("spawn cancellable TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "Mock slow stream started.",
        Duration::from_secs(10),
        "TUI did not render the first durable stream delta",
    );
    process.write(&[0x03]).expect("cancel the running turn");
    std::thread::sleep(Duration::from_millis(300));
    arm_idle_exit(&mut process, &mut output);

    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert!(
        !String::from_utf8_lossy(&output).contains("Mock slow stream completed."),
        "cancelled PTY turn must not display a post-terminal completion"
    );
}

#[test]
fn tui_restart_recovers_history_from_the_runtime_snapshot() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut source = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "pty restart seed")
        .expect("spawn source TUI in PTY");
    let mut source_output = Vec::new();
    receive_until(
        &source,
        &mut source_output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "source TUI did not complete",
    );
    arm_idle_exit(&mut source, &mut source_output);
    let status = source.wait_for_exit(Duration::from_secs(5));
    source.close_io_and_join();
    assert_eq!(status.code(), Some(130), "source TUI exited with {status}");

    let mut resumed =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");
    let mut resumed_output = Vec::new();
    receive_until(
        &resumed,
        &mut resumed_output,
        "Mock history users: pty restart seed | mock_history_echo",
        Duration::from_secs(10),
        "resumed TUI did not hydrate history from the typed snapshot",
    );
    arm_idle_exit(&mut resumed, &mut resumed_output);
    let status = resumed.wait_for_exit(Duration::from_secs(5));
    resumed.close_io_and_join();
    assert_eq!(status.code(), Some(130), "resumed TUI exited with {status}");
}

struct PtyProcess {
    child: Option<Child>,
    writer: Option<File>,
    reader: Option<JoinHandle<()>>,
    output_rx: Receiver<Vec<u8>>,
}

impl PtyProcess {
    fn spawn(home: &std::path::Path, cwd: &std::path::Path) -> io::Result<Self> {
        Self::spawn_with_prompt(home, cwd, PROMPT)
    }

    fn spawn_with_prompt(
        home: &std::path::Path,
        cwd: &std::path::Path,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history(home, cwd, None, prompt)
    }

    fn spawn_resumed(
        home: &std::path::Path,
        cwd: &std::path::Path,
        selector: &str,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history(home, cwd, Some(selector), prompt)
    }

    fn spawn_with_history(
        home: &std::path::Path,
        cwd: &std::path::Path,
        resume: Option<&str>,
        prompt: &str,
    ) -> io::Result<Self> {
        let (master, slave) = open_pty(120, 40)?;
        let stdout = duplicate_fd(&slave)?;
        let stderr = duplicate_fd(&slave)?;
        let writer = File::from(duplicate_fd(&master)?);
        let mut terminal_reader = File::from(master);
        let stdin = File::from(slave);

        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command.args(["--provider", "mock", "--cwd"]).arg(cwd);
        if let Some(selector) = resume {
            command.args(["--resume", selector]);
        }
        let child = command
            .arg(prompt)
            .env("ORCA_HOME", home)
            .env("ORCA_API_KEY", "pty-test-key")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(File::from(stdout)))
            .stderr(Stdio::from(File::from(stderr)))
            .spawn()?;

        let (output_tx, output_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match terminal_reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if output_tx.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("read TUI PTY: {error}"),
                }
            }
        });

        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            reader: Some(reader),
            output_rx,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer is closed"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn receive_output(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.output_rx.recv_timeout(timeout).ok()
    }

    fn drain_output(&self, output: &mut Vec<u8>) {
        output.extend(self.output_rx.try_iter().flatten());
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("PTY child remains owned")
                .try_wait()
                .expect("poll TUI process")
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not exit after idle Ctrl-C"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn close_io_and_join(&mut self) {
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join PTY reader");
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        self.child.take();
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn arm_idle_exit(process: &mut PtyProcess, output: &mut Vec<u8>) {
    std::thread::sleep(Duration::from_millis(250));
    process.drain_output(output);
    process.write(&[0x03]).expect("send first idle Ctrl-C");
    receive_until(
        process,
        output,
        "Press Ctrl+C again to quit.",
        Duration::from_secs(2),
        "TUI did not arm idle exit",
    );
    process.write(&[0x03]).expect("send second idle Ctrl-C");
}

fn receive_until(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    timeout: Duration,
    failure: &str,
) {
    let deadline = Instant::now() + timeout;
    while !String::from_utf8_lossy(output).contains(expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "{failure}; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn open_pty(columns: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn duplicate_fd(fd: &impl AsRawFd) -> io::Result<OwnedFd> {
    let duplicate = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}
