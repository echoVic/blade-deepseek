use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::task_types::TaskStatus;
use uuid::Uuid;

use crate::tasks::TaskRegistry;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::io::FromRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[derive(Clone, Debug)]
pub struct ShellSessionCommand {
    pub command: String,
    pub cwd: PathBuf,
    pub additional_readable_directories: Vec<PathBuf>,
    pub additional_working_directories: Vec<PathBuf>,
    pub denied_working_directories: Vec<PathBuf>,
    pub allowed_unix_socket_roots: Vec<PathBuf>,
    pub env: BTreeMap<String, Option<String>>,
    pub description: String,
    pub terminal: ShellTerminalMode,
    pub sandbox: ShellSandboxMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellTerminalMode {
    Pipe,
    Pty {
        cols: Option<u16>,
        rows: Option<u16>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellSandboxMode {
    WorkspaceWrite {
        network_access: bool,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    },
    ReadOnly {
        network_access: bool,
        allow_global_read: bool,
    },
    DangerFullAccess,
}

impl Default for ShellSandboxMode {
    fn default() -> Self {
        Self::WorkspaceWrite {
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        }
    }
}

impl ShellTerminalMode {
    pub fn pipe() -> Self {
        Self::Pipe
    }

    pub fn pty(cols: Option<u16>, rows: Option<u16>) -> Self {
        Self::Pty { cols, rows }
    }

    pub fn is_pty(self) -> bool {
        matches!(self, Self::Pty { .. })
    }

    pub fn size(self) -> (Option<u16>, Option<u16>) {
        match self {
            Self::Pipe => (None, None),
            Self::Pty { cols, rows } => (cols, rows),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipe => "pipe",
            Self::Pty { .. } => "pty",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSessionHandle {
    pub id: String,
    pub task_id: String,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSessionOutput {
    pub id: String,
    pub task_id: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub status: TaskStatus,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSessionSnapshot {
    pub id: String,
    pub task_id: String,
    pub command: String,
    pub description: String,
    pub status: TaskStatus,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
}

pub struct RuntimeShellSessionManager {
    tasks: TaskRegistry,
    sessions: HashMap<String, ShellSession>,
}

struct ShellSession {
    tasks: TaskRegistry,
    task_id: String,
    command: String,
    description: String,
    child: Child,
    stdin: ShellInput,
    stdout: SharedOutput,
    stderr: SharedOutput,
    stdout_handle: Option<thread::JoinHandle<()>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
    requested_terminal: ShellTerminalMode,
    effective_terminal: ShellTerminalMode,
}

enum ShellInput {
    Pipe(Option<ChildStdin>),
    Pty(File),
}

type SharedOutput = Arc<Mutex<Vec<u8>>>;

impl RuntimeShellSessionManager {
    pub fn new(tasks: TaskRegistry) -> Self {
        Self {
            tasks,
            sessions: HashMap::new(),
        }
    }

    pub fn spawn(&mut self, command: ShellSessionCommand) -> io::Result<ShellSessionHandle> {
        self.spawn_with_task_registry(command, self.tasks.clone())
    }

    pub fn spawn_with_task_registry(
        &mut self,
        command: ShellSessionCommand,
        tasks: TaskRegistry,
    ) -> io::Result<ShellSessionHandle> {
        let requested_terminal = command.terminal;
        let description = command.description.clone();
        let task = tasks.create_shell(description.clone(), command.command.clone());
        tasks.mark_running(&task.id).map_err(io::Error::other)?;

        let mut process = match command.sandbox {
            ShellSandboxMode::WorkspaceWrite {
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            } => orca_tools::sandbox::workspace_write_bash_command(
                orca_tools::sandbox::WorkspaceWriteSandboxCommandContext {
                    command: &command.command,
                    cwd: &command.cwd,
                    readable_roots: &command.additional_readable_directories,
                    additional_roots: &command.additional_working_directories,
                    denied_roots: &command.denied_working_directories,
                    network_access,
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                    allowed_unix_socket_roots: &command.allowed_unix_socket_roots,
                },
            ),
            ShellSandboxMode::ReadOnly {
                network_access,
                allow_global_read,
            } => orca_tools::sandbox::read_only_bash_command(
                orca_tools::sandbox::ReadOnlySandboxCommandContext {
                    command: &command.command,
                    cwd: &command.cwd,
                    readable_roots: &command.additional_readable_directories,
                    additional_roots: &command.additional_working_directories,
                    denied_roots: &command.denied_working_directories,
                    network_access,
                    allow_global_read,
                    allowed_unix_socket_roots: &command.allowed_unix_socket_roots,
                },
            ),
            ShellSandboxMode::DangerFullAccess => {
                orca_tools::sandbox::plain_bash_command(&command.command, &command.cwd)
            }
        };
        process.env_remove("ORCA_API_KEY");
        for (key, value) in &command.env {
            match value {
                Some(value) => {
                    process.env(key, value);
                }
                None => {
                    process.env_remove(key);
                }
            }
        }
        let stdio = configure_shell_stdio(&mut process, requested_terminal)?;
        let effective_terminal = stdio.effective_terminal();
        let mut child = process.spawn().inspect_err(|error| {
            let _ = tasks.fail(&task.id, format!("failed to run shell: {error}"));
        })?;

        tasks
            .mark_worker_spawned(&task.id, child.id())
            .map_err(io::Error::other)?;
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let (stdin, stdout_reader, stderr_reader) = stdio.finish(&mut child)?;
        let stdout_handle = Some(spawn_output_reader(stdout_reader, Arc::clone(&stdout)));
        let stderr_handle =
            stderr_reader.map(|reader| spawn_output_reader(reader, Arc::clone(&stderr)));
        let id = format!("shell-{}", Uuid::new_v4());
        self.sessions.insert(
            id.clone(),
            ShellSession {
                tasks,
                task_id: task.id.clone(),
                command: command.command.clone(),
                description,
                stdin,
                child,
                stdout,
                stderr,
                stdout_handle,
                stderr_handle,
                requested_terminal,
                effective_terminal,
            },
        );

        Ok(ShellSessionHandle {
            id,
            task_id: task.id,
            requested_terminal,
            effective_terminal,
        })
    }

    pub fn write_stdin(&mut self, id: &str, input: &str) -> io::Result<()> {
        let session = self.session_mut(id)?;
        session.stdin.write_all(id, input.as_bytes())
    }

    pub fn close_stdin(&mut self, id: &str) -> io::Result<()> {
        let session = self.session_mut(id)?;
        session.stdin.close();
        Ok(())
    }

    pub fn update_description(&mut self, id: &str, description: &str) -> io::Result<()> {
        let description = description.trim();
        if description.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shell description must not be empty",
            ));
        }
        let session = self.session_mut(id)?;
        session.description = description.to_string();
        Ok(())
    }

    pub fn resize(&mut self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        let session = self.session_mut(id)?;
        session.stdin.resize_pty(id, cols, rows)
    }

    pub fn list(&mut self) -> Vec<ShellSessionSnapshot> {
        self.sessions
            .iter_mut()
            .map(|(id, session)| {
                let status = match session.child.try_wait() {
                    Ok(Some(status)) if status.success() => TaskStatus::Completed,
                    Ok(Some(_)) => TaskStatus::Failed,
                    Ok(None) | Err(_) => TaskStatus::Running,
                };
                ShellSessionSnapshot {
                    id: id.clone(),
                    task_id: session.task_id.clone(),
                    command: session.command.clone(),
                    description: session.description.clone(),
                    status,
                    requested_terminal: session.requested_terminal,
                    effective_terminal: session.effective_terminal,
                }
            })
            .collect()
    }

    pub fn reap_requested_stops(&mut self) -> io::Result<Vec<ShellSessionOutput>> {
        let ids = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                if session.tasks.is_cancelled(&session.task_id) {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let mut stopped = Vec::new();
        for id in ids {
            stopped.push(self.kill(&id)?);
        }
        Ok(stopped)
    }

    pub fn read(&mut self, id: &str, timeout: Duration) -> io::Result<ShellSessionOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let session = self.session_mut(id)?;
            if let Some(status) = session.child.try_wait()? {
                let mut session = self.take_session(id)?;
                session.join_readers();
                let tasks = session.tasks.clone();
                let output = session.output(
                    id,
                    if status.success() {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    },
                    process_exit_code(status),
                );
                Self::record_terminal_output(&tasks, &output)?;
                return Ok(output);
            }
            if output_len(&session.stdout) > 0
                || output_len(&session.stderr) > 0
                || Instant::now() >= deadline
            {
                return Ok(session.output(id, TaskStatus::Running, None));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn wait(&mut self, id: &str, timeout: Duration) -> io::Result<ShellSessionOutput> {
        self.wait_or_cancel(id, timeout, || false)
    }

    pub fn wait_or_cancel(
        &mut self,
        id: &str,
        timeout: Duration,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<ShellSessionOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let session = self.session_mut(id)?;
            if session.child.try_wait()?.is_some() {
                break;
            }
            if should_cancel() {
                return self.kill(id);
            }
            if Instant::now() >= deadline {
                return self.kill(id);
            }
            thread::sleep(Duration::from_millis(25));
        }

        let mut session = self.take_session(id)?;
        let status = session.child.wait()?;
        session.join_readers();
        let tasks = session.tasks.clone();
        let output = session.output(
            id,
            if status.success() {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            },
            process_exit_code(status),
        );
        Self::record_terminal_output(&tasks, &output)?;
        Ok(output)
    }

    pub fn kill(&mut self, id: &str) -> io::Result<ShellSessionOutput> {
        let mut session = self.take_session(id)?;
        let tasks = session.tasks.clone();
        if let Some(status) = wait_for_output_or_exit(&mut session, Duration::from_millis(150))? {
            session.join_readers();
            let output = session.output(
                id,
                if status.success() {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                },
                process_exit_code(status),
            );
            Self::record_terminal_output(&tasks, &output)?;
            return Ok(output);
        }
        orca_tools::process::kill_child_tree(&mut session.child);
        let status = session.child.wait()?;
        session.join_readers();
        let output = session.output(id, TaskStatus::Stopped, process_exit_code(status));
        tasks
            .stop(&output.task_id, output.stdout.clone())
            .map_err(io::Error::other)?;
        Ok(output)
    }

    fn record_terminal_output(tasks: &TaskRegistry, output: &ShellSessionOutput) -> io::Result<()> {
        if output.status == TaskStatus::Completed {
            tasks
                .complete(&output.task_id, output.stdout.clone())
                .map_err(io::Error::other)
        } else {
            tasks
                .fail(&output.task_id, output.stderr_or_stdout())
                .map_err(io::Error::other)
        }
    }

    fn session_mut(&mut self, id: &str) -> io::Result<&mut ShellSession> {
        self.sessions.get_mut(id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("shell session '{id}' not found"),
            )
        })
    }

    fn take_session(&mut self, id: &str) -> io::Result<ShellSession> {
        self.sessions.remove(id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("shell session '{id}' not found"),
            )
        })
    }
}

impl ShellSession {
    fn join_readers(&mut self) {
        if let Some(handle) = self.stdout_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
    }

    fn output(&self, id: &str, status: TaskStatus, exit_code: Option<i32>) -> ShellSessionOutput {
        ShellSessionOutput {
            id: id.to_string(),
            task_id: self.task_id.clone(),
            stdout: output_string(&self.stdout),
            stderr: output_string(&self.stderr),
            exit_code,
            status,
            requested_terminal: self.requested_terminal,
            effective_terminal: self.effective_terminal,
        }
    }
}

impl ShellInput {
    fn write_all(&mut self, id: &str, input: &[u8]) -> io::Result<()> {
        match self {
            Self::Pipe(Some(stdin)) => stdin.write_all(input),
            Self::Pipe(None) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("shell session '{id}' stdin is closed"),
            )),
            Self::Pty(master) => master.write_all(input),
        }
    }

    fn close(&mut self) {
        if let Self::Pipe(stdin) = self {
            stdin.take();
        }
    }

    fn resize_pty(&mut self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        match self {
            Self::Pipe(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("shell session '{id}' is not a PTY"),
            )),
            Self::Pty(master) => resize_pty(master, cols, rows),
        }
    }
}

enum ShellStdio {
    Pipe,
    #[cfg(unix)]
    Pty {
        master: File,
        cols: Option<u16>,
        rows: Option<u16>,
    },
}

impl ShellStdio {
    fn effective_terminal(&self) -> ShellTerminalMode {
        match self {
            Self::Pipe => ShellTerminalMode::pipe(),
            #[cfg(unix)]
            Self::Pty { cols, rows, .. } => ShellTerminalMode::pty(*cols, *rows),
        }
    }
}

fn configure_shell_stdio(
    process: &mut std::process::Command,
    terminal: ShellTerminalMode,
) -> io::Result<ShellStdio> {
    match resolve_terminal_support(terminal, cfg!(unix)) {
        ShellTerminalMode::Pipe => {
            process
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(unix)]
            {
                process.process_group(0);
            }
            Ok(ShellStdio::Pipe)
        }
        ShellTerminalMode::Pty { cols, rows } => configure_pty_stdio(process, cols, rows),
    }
}

fn resolve_terminal_support(
    requested: ShellTerminalMode,
    platform_supports_pty: bool,
) -> ShellTerminalMode {
    match requested {
        ShellTerminalMode::Pty { .. } if !platform_supports_pty => ShellTerminalMode::Pipe,
        other => other,
    }
}

#[cfg(unix)]
fn configure_pty_stdio(
    process: &mut std::process::Command,
    cols: Option<u16>,
    rows: Option<u16>,
) -> io::Result<ShellStdio> {
    let (master_fd, slave_fd) = open_pty(cols, rows)?;
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    process
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    process.process_group(0);
    Ok(ShellStdio::Pty { master, cols, rows })
}

#[cfg(not(unix))]
fn configure_pty_stdio(
    process: &mut std::process::Command,
    _cols: Option<u16>,
    _rows: Option<u16>,
) -> io::Result<ShellStdio> {
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(ShellStdio::Pipe)
}

#[cfg(unix)]
fn resize_pty(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const TIOCSWINSZ: u64 = 0x8008_7467;
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
    const TIOCSWINSZ: u64 = 0x5414;

    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { ioctl(master.as_raw_fd(), TIOCSWINSZ, &winsize) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn resize_pty(_master: &File, _cols: u16, _rows: u16) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "PTY resize is only supported on Unix",
    ))
}

impl ShellStdio {
    fn finish(
        self,
        child: &mut Child,
    ) -> io::Result<(
        ShellInput,
        Box<dyn Read + Send>,
        Option<Box<dyn Read + Send>>,
    )> {
        match self {
            Self::Pipe => {
                let stdin = child.stdin.take();
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| io::Error::other("child process has no stdout"))?;
                let stderr = child
                    .stderr
                    .take()
                    .map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
                Ok((ShellInput::Pipe(stdin), Box::new(stdout), stderr))
            }
            #[cfg(unix)]
            Self::Pty { master, .. } => {
                let reader = master.try_clone()?;
                Ok((ShellInput::Pty(master), Box::new(reader), None))
            }
        }
    }
}

#[cfg(unix)]
fn open_pty(cols: Option<u16>, rows: Option<u16>) -> io::Result<(i32, i32)> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    unsafe extern "C" {
        fn openpty(
            amaster: *mut i32,
            aslave: *mut i32,
            name: *mut std::ffi::c_char,
            termp: *const std::ffi::c_void,
            winp: *const std::ffi::c_void,
        ) -> i32;
    }

    let mut master = -1;
    let mut slave = -1;
    let winsize = match (cols, rows) {
        (Some(cols), Some(rows)) => Some(Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        _ => None,
    };
    let winsize_ptr = winsize
        .as_ref()
        .map(|winsize| winsize as *const Winsize as *const std::ffi::c_void)
        .unwrap_or(std::ptr::null());
    let result = unsafe {
        openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            winsize_ptr,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok((master, slave))
    }
}

impl ShellSessionOutput {
    fn stderr_or_stdout(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout.trim_end(), self.stderr)
        }
    }
}

fn spawn_output_reader<R: Read + Send + 'static>(
    mut reader: R,
    output: SharedOutput,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut output) = output.lock() {
                        output.extend_from_slice(&buffer[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn wait_for_output_or_exit(
    session: &mut ShellSession,
    timeout: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        if output_len(&session.stdout) > 0 || output_len(&session.stderr) > 0 {
            return Ok(None);
        }
        if let Some(status) = session.child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn output_len(output: &SharedOutput) -> usize {
    output.lock().map(|output| output.len()).unwrap_or_default()
}

fn output_string(output: &SharedOutput) -> String {
    output
        .lock()
        .map(|output| String::from_utf8_lossy(&output).into_owned())
        .unwrap_or_default()
}

fn process_exit_code(status: ExitStatus) -> Option<i32> {
    status.code().or_else(|| {
        #[cfg(unix)]
        {
            status.signal().map(|signal| 128 + signal)
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_support_resolution_falls_back_to_pipe_without_pty_support() {
        assert_eq!(
            resolve_terminal_support(ShellTerminalMode::pty(Some(120), Some(33)), false),
            ShellTerminalMode::pipe()
        );
        assert_eq!(
            resolve_terminal_support(ShellTerminalMode::pty(Some(120), Some(33)), true),
            ShellTerminalMode::pty(Some(120), Some(33))
        );
        assert_eq!(
            resolve_terminal_support(ShellTerminalMode::pipe(), false),
            ShellTerminalMode::pipe()
        );
    }
}
