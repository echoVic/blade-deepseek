use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ExitStatus, Stdio};
use std::str;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::task_types::TaskStatus;
use uuid::Uuid;

use crate::task_output::{TaskOutputRead, TaskOutputStore};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellRuntimeCapabilities {
    pub platform: &'static str,
    pub supports_pty: bool,
    pub supports_pty_resize: bool,
    pub fallback_terminal_mode: ShellTerminalMode,
    pub command_exec_streaming_requires_process_id: bool,
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

pub fn shell_runtime_capabilities() -> ShellRuntimeCapabilities {
    ShellRuntimeCapabilities {
        platform: shell_runtime_platform(),
        supports_pty: cfg!(unix),
        supports_pty_resize: cfg!(unix),
        fallback_terminal_mode: ShellTerminalMode::pipe(),
        command_exec_streaming_requires_process_id: true,
    }
}

fn shell_runtime_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(all(
        not(target_os = "macos"),
        not(target_os = "linux"),
        not(target_os = "windows")
    ))]
    {
        std::env::consts::OS
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
    pub termination: ShellSessionTermination,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellSessionTermination {
    Running,
    Exited,
    Cancelled,
    TimedOut,
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
    output_store: TaskOutputStore,
    sessions: HashMap<String, ShellSession>,
}

struct ShellSession {
    tasks: TaskRegistry,
    task_id: String,
    command: String,
    description: String,
    child: Child,
    stdin: ShellInput,
    output_store: TaskOutputStore,
    stdout_handle: Option<thread::JoinHandle<()>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
    reader_stop: Arc<AtomicBool>,
    requested_terminal: ShellTerminalMode,
    effective_terminal: ShellTerminalMode,
}

struct SpawnedShellChild {
    child: Option<Child>,
}

enum ShellInput {
    Pipe(Option<ChildStdin>),
    Pty(File),
}

impl RuntimeShellSessionManager {
    pub fn new(tasks: TaskRegistry) -> Self {
        Self::with_output_store(tasks, TaskOutputStore::new())
    }

    pub fn with_output_store(tasks: TaskRegistry, output_store: TaskOutputStore) -> Self {
        Self {
            tasks,
            output_store,
            sessions: HashMap::new(),
        }
    }

    pub fn output_store(&self) -> TaskOutputStore {
        self.output_store.clone()
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
        let child = process.spawn().inspect_err(|error| {
            let _ = tasks.fail(&task.id, format!("failed to run shell: {error}"));
        })?;
        let initialized = initialize_spawned_shell(child, stdio, |pid| {
            tasks
                .mark_worker_spawned(&task.id, pid)
                .map_err(io::Error::other)
        });
        let (child, stdin, stdout_reader, stderr_reader) = match initialized {
            Ok(initialized) => initialized,
            Err(error) => {
                let _ = tasks.fail(&task.id, format!("failed to initialize shell: {error}"));
                return Err(error);
            }
        };
        let output_store = self.output_store.clone();
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stdout_handle = Some(spawn_output_reader(
            stdout_reader,
            output_store.clone(),
            task.id.clone(),
            ShellOutputStream::Stdout,
            Arc::clone(&reader_stop),
        ));
        let stderr_handle = stderr_reader.map(|reader| {
            spawn_output_reader(
                reader,
                output_store.clone(),
                task.id.clone(),
                ShellOutputStream::Stderr,
                Arc::clone(&reader_stop),
            )
        });
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
                output_store,
                stdout_handle,
                stderr_handle,
                reader_stop,
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

    pub fn reap_completed(&mut self) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_completed_where(|_| true)
    }

    pub fn reap_completed_except(
        &mut self,
        protected_ids: &HashSet<String>,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_completed_where(|id| !protected_ids.contains(id))
    }

    fn reap_completed_where(
        &mut self,
        should_reap: impl Fn(&str) -> bool,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        let ids = self
            .sessions
            .iter_mut()
            .filter_map(|(id, session)| match session.child.try_wait() {
                Ok(Some(status)) if should_reap(id) => Some(Ok((id.clone(), status))),
                Ok(Some(_)) => None,
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut completed = Vec::new();
        for (id, status) in ids {
            completed.push(self.finish_terminal_session(&id, status, true)?);
        }
        Ok(completed)
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
        self.read_inner(id, timeout, true)
    }

    pub(crate) fn read_preserving_output(
        &mut self,
        id: &str,
        timeout: Duration,
    ) -> io::Result<ShellSessionOutput> {
        self.read_inner(id, timeout, false)
    }

    fn read_inner(
        &mut self,
        id: &str,
        timeout: Duration,
        remove_completed_output: bool,
    ) -> io::Result<ShellSessionOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let session = self.session_mut(id)?;
            if let Some(status) = session.child.try_wait()? {
                return self.finish_terminal_session(id, status, remove_completed_output);
            }
            if session.output_size() > 0 || Instant::now() >= deadline {
                return Ok(session.output(
                    id,
                    TaskStatus::Running,
                    None,
                    ShellSessionTermination::Running,
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn read_output_delta(
        &self,
        task_id: &str,
        from_offset: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        self.output_store
            .read_delta(task_id, from_offset, max_bytes)
    }

    pub(crate) fn remove_output(&self, task_id: &str) -> bool {
        self.output_store.remove(task_id)
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
        self.wait_or_cancel_with_output(id, timeout, should_cancel, &mut |_| {})
    }

    pub(crate) fn wait_or_cancel_with_output(
        &mut self,
        id: &str,
        timeout: Duration,
        should_cancel: impl Fn() -> bool,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<ShellSessionOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let task_id = self.session_mut(id)?.task_id.clone();
        let mut output_offset = 0;
        loop {
            let completed = self.session_mut(id)?.child.try_wait()?.is_some();
            output_offset = self.emit_available_output(&task_id, output_offset, on_output)?;
            if completed {
                break;
            }
            if should_cancel() {
                return self.kill(id);
            }
            if Instant::now() >= deadline {
                return self.terminate(id, ShellSessionTermination::TimedOut);
            }
            thread::sleep(Duration::from_millis(25));
        }

        let mut session = self.take_session(id)?;
        let status = session.child.wait()?;
        session.join_readers();
        self.emit_available_output(&task_id, output_offset, on_output)?;
        let tasks = session.tasks.clone();
        let output = session.output(
            id,
            if status.success() {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            },
            process_exit_code(status),
            ShellSessionTermination::Exited,
        );
        Self::record_terminal_output(&tasks, &output)?;
        self.output_store.remove(&output.task_id);
        Ok(output)
    }

    fn emit_available_output(
        &self,
        task_id: &str,
        from_offset: usize,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<usize> {
        let output = self
            .output_store
            .read_delta(task_id, from_offset, usize::MAX)?;
        if !output.combined.is_empty() {
            on_output(&output.combined);
        }
        Ok(output.next_offset)
    }

    pub fn kill(&mut self, id: &str) -> io::Result<ShellSessionOutput> {
        self.terminate(id, ShellSessionTermination::Cancelled)
    }

    fn terminate(
        &mut self,
        id: &str,
        termination: ShellSessionTermination,
    ) -> io::Result<ShellSessionOutput> {
        debug_assert!(matches!(
            termination,
            ShellSessionTermination::Cancelled | ShellSessionTermination::TimedOut
        ));
        let mut session = self.take_session(id)?;
        let tasks = session.tasks.clone();
        if let Some(status) = wait_for_process_exit(&mut session, Duration::from_millis(150))? {
            session.join_readers();
            let output = session.output(
                id,
                if status.success() {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                },
                process_exit_code(status),
                ShellSessionTermination::Exited,
            );
            Self::record_terminal_output(&tasks, &output)?;
            self.output_store.remove(&output.task_id);
            return Ok(output);
        }
        orca_tools::process::kill_child_tree(&mut session.child);
        let status = session.child.wait()?;
        session.join_readers();
        let output = session.output(
            id,
            TaskStatus::Stopped,
            process_exit_code(status),
            termination,
        );
        tasks
            .stop(&output.task_id, output.stdout.clone())
            .map_err(io::Error::other)?;
        self.output_store.remove(&output.task_id);
        Ok(output)
    }

    pub fn terminate_all(&mut self) {
        let ids = self.sessions.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let _ = self.terminate(&id, ShellSessionTermination::Cancelled);
        }
    }

    fn finish_terminal_session(
        &mut self,
        id: &str,
        status: ExitStatus,
        remove_completed_output: bool,
    ) -> io::Result<ShellSessionOutput> {
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
            ShellSessionTermination::Exited,
        );
        Self::record_terminal_output(&tasks, &output)?;
        if remove_completed_output {
            self.output_store.remove(&output.task_id);
        }
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

impl Drop for RuntimeShellSessionManager {
    fn drop(&mut self) {
        self.terminate_all();
    }
}

impl ShellSession {
    fn join_readers(&mut self) {
        orca_tools::process::kill_child_tree(&mut self.child);
        let _ = self.child.wait();
        self.reader_stop.store(true, Ordering::Release);
        if let Some(handle) = self.stdout_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
    }

    fn output(
        &self,
        id: &str,
        status: TaskStatus,
        exit_code: Option<i32>,
        termination: ShellSessionTermination,
    ) -> ShellSessionOutput {
        let output = self
            .output_store
            .read_delta(&self.task_id, 0, usize::MAX)
            .unwrap_or_else(|_| TaskOutputRead {
                stdout: String::new(),
                stderr: String::new(),
                combined: String::new(),
                next_offset: 0,
                bytes_read: 0,
                bytes_total: self.output_size(),
                omitted_prefix_bytes: 0,
                stdout_prefix_bytes: 0,
                stderr_prefix_bytes: 0,
            });
        let (stdout, stderr) = shell_output_text_with_omitted_prefix(
            output.stdout,
            output.stderr,
            output.omitted_prefix_bytes,
        );
        ShellSessionOutput {
            id: id.to_string(),
            task_id: self.task_id.clone(),
            stdout,
            stderr,
            exit_code,
            status,
            termination,
            requested_terminal: self.requested_terminal,
            effective_terminal: self.effective_terminal,
        }
    }

    fn output_size(&self) -> usize {
        self.output_store.size(&self.task_id)
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        self.stdin.close();
        self.join_readers();
    }
}

impl SpawnedShellChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child(&self) -> &Child {
        self.child.as_ref().expect("spawned shell child")
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("spawned shell child")
    }

    fn into_child(mut self) -> Child {
        self.child.take().expect("spawned shell child")
    }
}

impl Drop for SpawnedShellChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            orca_tools::process::kill_child_tree(child);
            let _ = child.wait();
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

fn initialize_spawned_shell(
    child: Child,
    stdio: ShellStdio,
    register_worker: impl FnOnce(u32) -> io::Result<()>,
) -> io::Result<(
    Child,
    ShellInput,
    Box<dyn Read + Send>,
    Option<Box<dyn Read + Send>>,
)> {
    let mut child = SpawnedShellChild::new(child);
    register_worker(child.child().id())?;
    let (stdin, stdout_reader, stderr_reader) = stdio.finish(child.child_mut())?;
    Ok((child.into_child(), stdin, stdout_reader, stderr_reader))
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
                let stderr = child.stderr.take();
                #[cfg(unix)]
                {
                    set_nonblocking(&stdout)?;
                    if let Some(stderr) = stderr.as_ref() {
                        set_nonblocking(stderr)?;
                    }
                }
                let stderr = stderr.map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
                Ok((ShellInput::Pipe(stdin), Box::new(stdout), stderr))
            }
            #[cfg(unix)]
            Self::Pty { master, .. } => {
                set_nonblocking(&master)?;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellOutputStream {
    Stdout,
    Stderr,
}

fn spawn_output_reader<R: Read + Send + 'static>(
    mut reader: R,
    output_store: TaskOutputStore,
    task_id: String,
    stream: ShellOutputStream,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buffer[..n]);
                    drain_valid_utf8_output(&output_store, &task_id, stream, &mut pending);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        flush_lossy_output(&output_store, &task_id, stream, &mut pending);
    })
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

fn wait_for_process_exit(
    session: &mut ShellSession,
    timeout: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        if let Some(status) = session.child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn drain_valid_utf8_output(
    output_store: &TaskOutputStore,
    task_id: &str,
    stream: ShellOutputStream,
    pending: &mut Vec<u8>,
) {
    loop {
        match str::from_utf8(pending) {
            Ok(text) => {
                append_shell_output(output_store, task_id, stream, text);
                pending.clear();
                return;
            }
            Err(error) if error.valid_up_to() > 0 => {
                let valid = error.valid_up_to();
                let text = str::from_utf8(&pending[..valid]).unwrap_or_default();
                append_shell_output(output_store, task_id, stream, text);
                pending.drain(..valid);
            }
            Err(error) if error.error_len().is_some() => {
                append_shell_output(output_store, task_id, stream, "\u{FFFD}");
                pending.drain(..error.error_len().unwrap_or(1));
            }
            Err(_) => return,
        }
    }
}

fn flush_lossy_output(
    output_store: &TaskOutputStore,
    task_id: &str,
    stream: ShellOutputStream,
    pending: &mut Vec<u8>,
) {
    if pending.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(pending).into_owned();
    append_shell_output(output_store, task_id, stream, &text);
    pending.clear();
}

fn append_shell_output(
    output_store: &TaskOutputStore,
    task_id: &str,
    stream: ShellOutputStream,
    text: &str,
) {
    let _ = match stream {
        ShellOutputStream::Stdout => output_store.append_stdout(task_id, text),
        ShellOutputStream::Stderr => output_store.append_stderr(task_id, text),
    };
}

fn shell_output_text_with_omitted_prefix(
    mut stdout: String,
    mut stderr: String,
    omitted_prefix_bytes: usize,
) -> (String, String) {
    if omitted_prefix_bytes == 0 {
        return (stdout, stderr);
    }
    let notice = format!("[{omitted_prefix_bytes} bytes of earlier output omitted]\n");
    if stdout.is_empty() {
        stderr = format!("{notice}{stderr}");
    } else {
        stdout = format!("{notice}{stdout}");
    }
    (stdout, stderr)
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

    #[cfg(unix)]
    #[test]
    fn registration_failure_reaps_spawned_shell_process_group() {
        let temp = tempfile::tempdir().expect("tempdir");
        let started_marker = temp.path().join("started");
        let release_marker = temp.path().join("release");
        let leaked_marker = temp.path().join("leaked");
        let mut process = std::process::Command::new("sh");
        process
            .arg("-c")
            .arg(
                "printf started > \"$STARTED\"; (while [ ! -e \"$RELEASE\" ]; do sleep 0.05; done; printf leaked > \"$LEAKED\") & wait",
            )
            .env("STARTED", &started_marker)
            .env("RELEASE", &release_marker)
            .env("LEAKED", &leaked_marker)
            .current_dir(temp.path());
        let stdio = configure_shell_stdio(&mut process, ShellTerminalMode::pipe())
            .expect("configure shell stdio");
        let child = process.spawn().expect("spawn shell child");

        let error = match initialize_spawned_shell(child, stdio, |_| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !started_marker.exists() {
                assert!(Instant::now() < deadline, "shell child did not start");
                thread::sleep(Duration::from_millis(10));
            }
            Err(io::Error::other("injected registration failure"))
        }) {
            Ok(_) => panic!("registration should fail"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "injected registration failure");

        std::fs::write(&release_marker, "release").expect("release descendant");
        thread::sleep(Duration::from_millis(200));
        assert!(
            !leaked_marker.exists(),
            "registration failure must reap the spawned process group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn escaped_session_descendant_cannot_block_terminal_shell_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = std::env::current_exe().expect("resolve test executable");
        let mut env = BTreeMap::new();
        env.insert(
            "ORCA_SHELL_ESCAPE_HELPER".to_string(),
            Some(helper.display().to_string()),
        );
        env.insert(
            "ORCA_SHELL_ESCAPE_HOLDER".to_string(),
            Some("1".to_string()),
        );
        let tasks = TaskRegistry::new("escaped-shell-session".to_string());
        let mut sessions = RuntimeShellSessionManager::new(tasks);
        let handle = sessions
            .spawn(ShellSessionCommand {
                command: "\"$ORCA_SHELL_ESCAPE_HELPER\" --exact shell_session::tests::escaped_shell_pipe_holder_helper --nocapture & printf parent-done".to_string(),
                cwd: temp.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env,
                description: "escaped shell pipe holder".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn escaped shell fixture");
        let started = Instant::now();

        let output = sessions
            .wait(&handle.id, Duration::from_millis(200))
            .expect("terminal shell read should remain bounded");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "terminal shell reader join exceeded timeout: {:?}",
            started.elapsed()
        );
        assert!(output.stdout.contains("parent-done"));
    }

    #[cfg(unix)]
    #[test]
    fn escaped_shell_pipe_holder_helper() {
        if std::env::var_os("ORCA_SHELL_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        thread::sleep(Duration::from_secs(5));
    }
}
