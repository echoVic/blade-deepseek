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

    std::thread::sleep(Duration::from_millis(250));
    process.drain_output(&mut output);
    process.write(&[0x03]).expect("send first idle Ctrl-C");
    receive_until(
        &process,
        &mut output,
        "Press Ctrl+C again to quit.",
        Duration::from_secs(2),
        "TUI did not arm idle exit",
    );
    process.write(&[0x03]).expect("send second idle Ctrl-C");

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

struct PtyProcess {
    child: Option<Child>,
    writer: Option<File>,
    reader: Option<JoinHandle<()>>,
    output_rx: Receiver<Vec<u8>>,
}

impl PtyProcess {
    fn spawn(home: &std::path::Path, cwd: &std::path::Path) -> io::Result<Self> {
        let (master, slave) = open_pty(120, 40)?;
        let stdout = duplicate_fd(&slave)?;
        let stderr = duplicate_fd(&slave)?;
        let writer = File::from(duplicate_fd(&master)?);
        let mut terminal_reader = File::from(master);
        let stdin = File::from(slave);

        let child = Command::new(env!("CARGO_BIN_EXE_orca"))
            .args(["--provider", "mock", "--cwd"])
            .arg(cwd)
            .arg(PROMPT)
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
