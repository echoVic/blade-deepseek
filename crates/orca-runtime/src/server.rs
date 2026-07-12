use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

mod active_turn_manager;
mod command_exec_manager;
mod command_exec_sandbox;
mod mcp_elicitation_manager;
mod permission_manager;
mod router;
mod shell_manager;
mod user_input_manager;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use orca_core::cancel::CancelToken;
use serde_json::{Value, json};

use crate::lifecycle::{RuntimePermissionResponse, ThreadSteerHandle};
use crate::network_proxy::{RuntimeNetworkBlockReport, RuntimeNetworkPolicy, RuntimeNetworkProxy};
use crate::protocol::{self, ClientOp, ServerEvent, Submission};
use crate::sandbox_denial::{SandboxDenialDiagnostic, diagnose_sandbox_denial};
use crate::server_runtime::{
    PermissionProfileOverride, ServerRequestWriter, ServerThreadRuntime, thread_item_to_json,
    thread_run_config, thread_turn_to_json,
};
use crate::shell_session::{ShellSandboxMode, ShellSessionCommand};
use crate::thread_store::{
    SessionStore, SortDirection, StoredThreadSummary, ThreadListFilters, ThreadMetadataPatch,
    ThreadSortKey, ThreadStore, TurnItemsView,
};
use active_turn_manager::{ActiveTurnControl, ActiveTurnHandle, ActiveTurnManager};
use command_exec_manager::{
    CommandExecDrainOutcome, CommandExecManager, CommandExecPermissionPolicy, CommandExecProcess,
    CommandExecProcessSnapshot,
};
pub use command_exec_sandbox::{CommandExecSandbox, bash_sandbox_for_cwd};
use command_exec_sandbox::{command_exec_sandbox_mode, materialize_workspace_roots_paths};
use mcp_elicitation_manager::{PendingMcpElicitationManager, ServerMcpElicitationRequestHandler};
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
use permission_manager::{
    PendingCommandExecPermissionRequest, PendingPermissionManager, PendingPermissionRequest,
    ServerPermissionRequestHandler,
};
use shell_manager::ServerShellManager;
use user_input_manager::{PendingUserInputManager, ServerUserInputRequestHandler};

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub run_config: RunConfig,
}

pub fn run(config: ServerConfig) -> i32 {
    match run_with_io(config, io::stdin().lock(), io::stdout()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: server error: {error}");
            1
        }
    }
}

fn run_with_io<R: BufRead, W: Write + Send + 'static>(
    config: ServerConfig,
    mut reader: R,
    writer: W,
) -> io::Result<()> {
    let mut line = String::new();
    let mut state = ServerState::default();
    let writer = Arc::new(Mutex::new(writer));
    let result = (|| -> io::Result<()> {
        while reader.read_line(&mut line)? != 0 {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                handle_line(&config, &mut state, trimmed, Arc::clone(&writer))?;
            }
            line.clear();
        }
        Ok(())
    })();
    state.shutdown();
    result
}

#[derive(Default)]
struct ServerState {
    threads: ServerThreadRuntime,
    shells: ServerShellManager,
    command_exec: CommandExecManager,
    active_turns: ActiveTurnManager,
    pending_permissions: PendingPermissionManager,
    pending_user_inputs: PendingUserInputManager,
    pending_mcp_elicitations: PendingMcpElicitationManager,
}

impl ServerState {
    fn shutdown(&mut self) {
        const GRACEFUL_TURN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
        const CANCELLED_TURN_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
        self.command_exec.terminate_all(self.shells.sessions_mut());
        let _ = self.pending_permissions.close();
        let _ = self.pending_user_inputs.close();
        self.shells.terminate_all();
        if self
            .active_turns
            .wait_all_bounded(&mut self.threads, GRACEFUL_TURN_SHUTDOWN_TIMEOUT)
        {
            return;
        }
        self.active_turns.cancel_all();
        if !self
            .active_turns
            .wait_all_bounded(&mut self.threads, CANCELLED_TURN_SHUTDOWN_TIMEOUT)
        {
            self.active_turns.handoff_remaining_to_reaper();
            eprintln!("orca: server shutdown cleanup continues in the background");
        }
    }
}

impl ServerState {
    #[cfg(test)]
    fn join_active_turns(&mut self) {
        self.active_turns.join_all(&mut self.threads);
    }

    fn reclaim_finished_threads(&mut self) {
        self.active_turns.reclaim_finished(&mut self.threads);
    }

    fn reclaim_finished_thread(&mut self, thread_id: &str) {
        self.active_turns
            .reclaim_finished_thread(&mut self.threads, thread_id);
    }
}

fn handle_line<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    line: &str,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    state.reclaim_finished_threads();
    let submission = match Submission::decode(line) {
        Ok(submission) => submission,
        Err(error) => {
            write_locked_event(&writer, &error.id, ServerEvent::error(error.message))?;
            return Ok(());
        }
    };
    if let ClientOp::CommandExecRead {
        process_id,
        output_bytes_cap: Some(output_bytes_cap),
        ..
    } = &submission.op
    {
        state
            .command_exec
            .tighten_output_cap(process_id, *output_bytes_cap);
    }
    {
        let mut writer = writer.lock().map_err(lock_error)?;
        match drain_command_exec_processes(state, &mut *writer)? {
            CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
                request_command_exec_network_permission(state, request, block, &mut *writer)?;
            }
            CommandExecDrainOutcome::NetworkPermissionDenied {
                command_event_id,
                reason,
            } => {
                protocol::write_server_event(
                    &mut *writer,
                    &command_event_id,
                    ServerEvent::error(reason),
                )?;
            }
            CommandExecDrainOutcome::FileSystemPermissionRequired {
                request,
                diagnostic,
            } => {
                request_command_exec_file_system_permission(
                    state,
                    request,
                    diagnostic,
                    &mut *writer,
                )?;
            }
            CommandExecDrainOutcome::Drained => {}
        }
    }

    router::dispatch_submission(config, state, submission, writer)?;
    state.reclaim_finished_threads();
    Ok(())
}

#[cfg(test)]
fn handle_line_for_test(
    config: &ServerConfig,
    state: &mut ServerState,
    line: &str,
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(Vec::new()));
    handle_line(config, state, line, Arc::clone(&writer))?;
    state.join_active_turns();
    let mut writer = writer.lock().map_err(lock_error)?;
    output.extend_from_slice(&writer);
    writer.clear();
    Ok(())
}

fn write_locked_event<W: Write>(
    writer: &Arc<Mutex<W>>,
    id: &Value,
    event: ServerEvent,
) -> io::Result<()> {
    let mut writer = writer.lock().map_err(lock_error)?;
    protocol::write_server_event(&mut *writer, id, event)
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("server writer lock poisoned")
}

struct SharedServerRequestWriter<W: Write> {
    inner: Arc<Mutex<W>>,
    writer: ServerRequestWriter<LockedServerWriter<W>>,
}

impl<W: Write> SharedServerRequestWriter<W> {
    fn new(id: Value, inner: Arc<Mutex<W>>) -> Self {
        let locked = LockedServerWriter {
            inner: Arc::clone(&inner),
        };
        Self {
            inner,
            writer: ServerRequestWriter::new(id, locked),
        }
    }

    fn flush_remaining(&mut self) -> io::Result<()> {
        self.writer.flush_remaining()
    }
}

impl<W: Write> Write for SharedServerRequestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().map_err(lock_error)?.flush()
    }
}

struct LockedServerWriter<W: Write> {
    inner: Arc<Mutex<W>>,
}

impl<W: Write> Write for LockedServerWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.lock().map_err(lock_error)?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().map_err(lock_error)?.flush()
    }
}

struct PersistedSessionPermissionGrant {
    additional_working_directories: Vec<orca_core::config::AdditionalWorkingDirectory>,
    network_domain_permissions: HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
}

fn persist_session_permission_grant(
    thread_id: &str,
    runtime_workspace_roots: &[PathBuf],
    permissions: &protocol::RequestPermissionProfile,
) -> io::Result<PersistedSessionPermissionGrant> {
    let file_system = permissions.file_system.as_ref();
    let roots = file_system
        .into_iter()
        .flat_map(|file_system| {
            file_system
                .write
                .iter()
                .flatten()
                .chain(file_system.read.iter().flatten())
        })
        .filter(|path| !path.as_os_str().is_empty());
    let store = SessionStore::new();
    let mut transcript = store.load_session(thread_id)?;
    for root in roots {
        for root in
            materialize_workspace_roots_paths(&transcript.meta.cwd, runtime_workspace_roots, root)
        {
            if !transcript
                .meta
                .additional_working_directories
                .iter()
                .any(|directory| directory.path == root)
            {
                transcript.meta.additional_working_directories.push(
                    orca_core::config::AdditionalWorkingDirectory::new(root, "session"),
                );
            }
        }
    }
    if let Some(network) = permissions.network.as_ref() {
        for (domain, access) in &network.domains {
            transcript
                .meta
                .network_domain_permissions
                .insert(domain.clone(), *access);
        }
    }
    store.update_thread_metadata(
        thread_id,
        ThreadMetadataPatch {
            title: None,
            active_permission_profile: None,
            approval_mode: transcript.meta.approval_mode,
            runtime_workspace_roots: None,
            permission_rules: Some(transcript.meta.permission_rules),
            additional_working_directories: Some(transcript.meta.additional_working_directories),
            network_domain_permissions: Some(transcript.meta.network_domain_permissions),
        },
    )?;
    let updated = store.load_session(thread_id)?;
    Ok(PersistedSessionPermissionGrant {
        additional_working_directories: updated.meta.additional_working_directories,
        network_domain_permissions: updated.meta.network_domain_permissions,
    })
}

fn run_shell_start<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: Option<&str>,
    command: &str,
    description: Option<String>,
    terminal: crate::shell_session::ShellTerminalMode,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("shell command must not be empty"),
        );
    }
    let cwd = server_cwd(&config.run_config)?;
    let task_registry = match thread_id {
        Some(thread_id) => match state.threads.task_registry(thread_id) {
            Some(registry) => Some(registry),
            None => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!("unknown thread: {thread_id}")),
                );
            }
        },
        None => None,
    };
    let command_text = command.to_string();
    let command = ShellSessionCommand {
        command: command_text.clone(),
        cwd: cwd.clone(),
        additional_readable_directories: Vec::new(),
        additional_working_directories: Vec::new(),
        denied_working_directories: Vec::new(),
        allowed_unix_socket_roots: Vec::new(),
        env: Default::default(),
        description: description.unwrap_or_else(|| command_text.clone()),
        terminal,
        sandbox: ShellSandboxMode::WorkspaceWrite {
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
    };
    let handle = match state.shells.spawn(&cwd, command, task_registry) {
        Ok(handle) => handle,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("failed to start shell: {error}")),
            );
        }
    };
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellStarted {
            shell_id: Value::from(handle.id),
            task_id: Value::from(handle.task_id),
            command: Value::from(command_text),
            status: Value::from("running"),
            requested_terminal_mode: Value::from(handle.requested_terminal.as_str()),
            effective_terminal_mode: Value::from(handle.effective_terminal.as_str()),
        },
    )
}

fn run_shell_capabilities<W: Write>(id: Value, writer: &mut W) -> io::Result<()> {
    let capabilities = crate::shell_session::shell_runtime_capabilities();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellCapabilities {
            platform: Value::from(capabilities.platform),
            supports_pty: Value::from(capabilities.supports_pty),
            supports_pty_resize: Value::from(capabilities.supports_pty_resize),
            supported_terminal_modes: Value::from(vec![Value::from("pipe"), Value::from("pty")]),
            fallback_terminal_mode: Value::from(capabilities.fallback_terminal_mode.as_str()),
            command_exec_streaming_requires_process_id: Value::from(
                capabilities.command_exec_streaming_requires_process_id,
            ),
        },
    )
}

fn run_shell_write<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    input: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(result) = state.shells.write_stdin(shell_id, input) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("running"),
            cols: Value::Null,
            rows: Value::Null,
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::Null,
        },
    )
}

fn run_shell_update<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    description: Option<&str>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(description) = description else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("shell update did not include any supported fields"),
        );
    };
    let Some(result) = state.shells.update_description(shell_id, description) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("updated"),
            cols: Value::Null,
            rows: Value::Null,
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::from(description.trim().to_string()),
        },
    )
}

fn run_shell_close<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(result) = state.shells.close_stdin(shell_id) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("stdin_closed"),
            cols: Value::Null,
            rows: Value::Null,
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::Null,
        },
    )
}

fn run_shell_resize<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    cols: u16,
    rows: u16,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if cols == 0 || rows == 0 {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("shell resize cols and rows must be greater than zero"),
        );
    }
    let Some(result) = state.shells.resize(shell_id, cols, rows) else {
        return unknown_shell(writer, &id, shell_id);
    };
    if let Err(error) = result {
        return protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()));
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellUpdated {
            shell_id: Value::from(shell_id.to_string()),
            status: Value::from("resized"),
            cols: Value::from(cols),
            rows: Value::from(rows),
            stdout: Value::Null,
            stderr: Value::Null,
            exit_code: Value::Null,
            cap_reached: Value::Null,
            description: Value::Null,
        },
    )
}

fn run_shell_list<W: Write>(state: &mut ServerState, id: Value, writer: &mut W) -> io::Result<()> {
    let command_exec_shell_ids = state.command_exec.active_shell_ids();
    for output in state.shells.reap_requested_stops()? {
        write_shell_completed(writer, &id, output)?;
    }
    for output in state
        .shells
        .reap_completed_except(&command_exec_shell_ids)?
    {
        write_shell_completed(writer, &id, output)?;
    }
    let shells = state
        .shells
        .list()
        .into_iter()
        .filter(|snapshot| !command_exec_shell_ids.contains(&snapshot.id))
        .map(shell_snapshot_to_json)
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ShellListed {
            shells: Value::from(shells),
        },
    )
}

fn run_shell_read<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    timeout_ms: u64,
    output_bytes_cap: Option<usize>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    for output in state.shells.reap_requested_stops()? {
        if output.id == shell_id {
            return write_shell_completed_with_cap(writer, &id, output, output_bytes_cap);
        }
        write_shell_completed(writer, &id, output)?;
    }
    let Some(result) = state
        .shells
        .read(shell_id, Duration::from_millis(timeout_ms.max(1)))
    else {
        return unknown_shell(writer, &id, shell_id);
    };
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
    };
    if output.status == orca_core::task_types::TaskStatus::Running {
        let stdout = cap_text(&output.stdout, output_bytes_cap);
        let stderr = cap_text(&output.stderr, output_bytes_cap);
        let cap_reached = shell_output_cap_reached(&output, output_bytes_cap);
        write_shell_output_deltas_with_cap(writer, &id, &output, false, output_bytes_cap)?;
        protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ShellUpdated {
                shell_id: Value::from(output.id),
                status: Value::from("running"),
                cols: Value::Null,
                rows: Value::Null,
                stdout: Value::from(stdout),
                stderr: Value::from(stderr),
                exit_code: Value::Null,
                cap_reached: shell_cap_reached_value(output_bytes_cap, cap_reached),
                description: Value::Null,
            },
        )
    } else {
        write_shell_completed_with_cap(writer, &id, output, output_bytes_cap)
    }
}

fn run_shell_kill<W: Write>(
    state: &mut ServerState,
    shell_id: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(result) = state.shells.kill(shell_id) else {
        return unknown_shell(writer, &id, shell_id);
    };
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
    };
    write_shell_completed(writer, &id, output)
}

fn run_command_exec<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: Option<&str>,
    command: &[String],
    process_id: Option<&str>,
    cwd: Option<&PathBuf>,
    env: &protocol::CommandEnvOverrides,
    options: &protocol::CommandExecOptions,
    terminal: crate::shell_session::ShellTerminalMode,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if command.is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec command must not be empty"),
        );
    }
    if options.sandbox_policy != protocol::CommandSandboxPolicy::Default
        && options.permission_profile.is_some()
    {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("`permissionProfile` cannot be combined with `sandboxPolicy`"),
        );
    }
    if options.disable_timeout && options.timeout_ms.is_some() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec cannot set both timeoutMs and disableTimeout"),
        );
    }
    if options.disable_output_cap && options.output_bytes_cap.is_some() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec cannot set both outputBytesCap and disableOutputCap"),
        );
    }
    if options.has_size && !terminal.is_pty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec size requires tty: true"),
        );
    }
    let (terminal_cols, terminal_rows) = terminal.size();
    if terminal_cols == Some(0) || terminal_rows == Some(0) {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec size rows and cols must be greater than 0"),
        );
    }
    let timeout_ms = match options.timeout_ms {
        Some(timeout_ms) => match u64::try_from(timeout_ms) {
            Ok(timeout_ms) => timeout_ms,
            Err(_) => {
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!(
                        "command/exec timeoutMs must be non-negative, got {timeout_ms}"
                    )),
                );
            }
        },
        None => 120_000,
    };
    if process_id.is_none()
        && (terminal.is_pty() || options.stream_stdin || options.stream_stdout_stderr)
    {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(
                "command/exec tty or streaming requires a client-supplied processId",
            ),
        );
    }
    let command_text = protocol::shell_join(command);
    let cwd = cwd.cloned().unwrap_or(server_cwd(&config.run_config)?);
    let (
        mut additional_working_directories,
        thread_permission_profile,
        runtime_workspace_roots,
        thread_network_domain_permissions,
    ) = match thread_id {
        Some(thread_id) => {
            state.reclaim_finished_thread(thread_id);
            match state.threads.thread(thread_id) {
                Some(thread) => (
                    thread
                        .additional_working_directories()
                        .iter()
                        .map(|directory| directory.path.clone())
                        .collect(),
                    thread.active_permission_profile().cloned(),
                    thread.runtime_workspace_roots().to_vec(),
                    thread.network_domain_permissions().clone(),
                ),
                None => {
                    return protocol::write_server_event(
                        writer,
                        &id,
                        ServerEvent::error(format!("unknown thread: {thread_id}")),
                    );
                }
            }
        }
        None => (
            Vec::new(),
            None,
            config
                .run_config
                .runtime_workspace_roots
                .clone()
                .unwrap_or_default(),
            HashMap::new(),
        ),
    };
    let mut effective_sandbox = match command_exec_sandbox_mode(
        &config.run_config,
        options,
        thread_permission_profile.as_ref(),
        &cwd,
        &runtime_workspace_roots,
        std::env::var_os("TMPDIR").map(PathBuf::from).as_deref(),
    ) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            return protocol::write_server_event(writer, &id, ServerEvent::error(error));
        }
    };
    for (domain, access) in thread_network_domain_permissions {
        match access {
            orca_core::config::PermissionProfileNetworkAccess::Deny => {
                effective_sandbox
                    .network_policy_domains
                    .insert(domain, access);
            }
            orca_core::config::PermissionProfileNetworkAccess::Allow => {
                effective_sandbox
                    .network_policy_domains
                    .entry(domain)
                    .or_insert(access);
            }
        }
    }
    additional_working_directories.extend(effective_sandbox.additional_writable_roots.clone());
    let denied_writable_directories = effective_sandbox.denied_writable_roots.clone();
    if let protocol::CommandSandboxPolicy::WorkspaceWrite { writable_roots, .. } =
        &options.sandbox_policy
    {
        additional_working_directories.extend(writable_roots.iter().cloned());
    }
    let mut retry_block_reporter = None;
    let mut retry_block_receiver = None;
    let command_permission_request =
        thread_id.map(|thread_id| PendingCommandExecPermissionRequest {
            thread_id: thread_id.to_string(),
            runtime_workspace_roots: runtime_workspace_roots.clone(),
            command: command.to_vec(),
            process_id: process_id.map(ToString::to_string),
            cwd: Some(cwd.clone()),
            env: env.clone(),
            options: options.clone(),
            terminal,
            event_id: id.clone(),
        });
    if options.permission_profile.is_some() {
        let (block_sender, block_receiver) = mpsc::channel();
        retry_block_reporter = Some(block_sender);
        retry_block_receiver = Some(block_receiver);
    }
    if let Some(process_id) = process_id {
        if let Err(error) = state.command_exec.insert(
            process_id.to_string(),
            CommandExecProcess {
                shell_id: None,
                command_event_id: id.clone(),
                command: command.to_vec(),
                cwd: cwd.clone(),
                denied_writable_roots: denied_writable_directories.clone(),
                stream_output: terminal.is_pty() || options.stream_stdout_stderr,
                output_bytes_cap: options
                    .output_bytes_cap
                    .and_then(|cap| usize::try_from(cap).ok()),
                output_offset: 0,
                stdout_len: 0,
                stderr_len: 0,
                stdout_cap_reached: false,
                stderr_cap_reached: false,
                network_permission_blocks: retry_block_receiver.take(),
                permission_request: command_permission_request.clone(),
                _network_proxy: None,
            },
        ) {
            return protocol::write_server_event(writer, &id, ServerEvent::error(error));
        }
    }
    let mut network_proxy = if effective_sandbox.network_policy_domains.is_empty() {
        None
    } else {
        match RuntimeNetworkProxy::start_with_block_reporter(
            RuntimeNetworkPolicy::new(effective_sandbox.network_policy_domains.clone()),
            retry_block_reporter,
        ) {
            Ok(proxy) => Some(proxy),
            Err(error) => {
                if let Some(process_id) = process_id {
                    state.command_exec.remove(process_id);
                }
                return protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::error(format!("failed to start network proxy: {error}")),
                );
            }
        }
    };
    let mut command_env = env.clone();
    if let Some(proxy) = network_proxy.as_ref() {
        let proxy_url = proxy.proxy_url().to_string();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command_env.insert(key.to_string(), Some(proxy_url.clone()));
        }
        for key in ["NO_PROXY", "no_proxy"] {
            command_env.insert(key.to_string(), None);
        }
    }
    let handle = match state.shells.spawn(
        &cwd,
        ShellSessionCommand {
            command: command_text.clone(),
            cwd: cwd.clone(),
            additional_readable_directories: effective_sandbox.additional_readable_roots,
            additional_working_directories,
            denied_working_directories: denied_writable_directories.clone(),
            allowed_unix_socket_roots: effective_sandbox.allowed_unix_socket_roots,
            env: command_env,
            description: command_text,
            terminal,
            sandbox: effective_sandbox.mode,
        },
        None,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(process_id) = process_id {
                state.command_exec.remove(process_id);
            }
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("failed to start command: {error}")),
            );
        }
    };
    if let Some(process_id) = process_id {
        if let Some(proxy) = network_proxy.take() {
            state.command_exec.retain_network_proxy(process_id, proxy);
        }
        state.command_exec.activate(process_id, handle.id);
        protocol::write_server_event(
            writer,
            &id,
            ServerEvent::CommandExecStarted {
                process_id: Value::from(process_id.to_string()),
            },
        )?;
        let drain_outcome = if terminal.is_pty() || options.stream_stdout_stderr {
            drain_command_exec_processes_until_output_or_timeout(
                state,
                writer,
                Duration::from_secs(1),
            )?
        } else {
            drain_command_exec_processes_with_timeout(state, writer, Duration::from_millis(250))?
        };
        match drain_outcome {
            CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
                return request_command_exec_network_permission(state, request, block, writer);
            }
            CommandExecDrainOutcome::NetworkPermissionDenied {
                command_event_id,
                reason,
            } => {
                return protocol::write_server_event(
                    writer,
                    &command_event_id,
                    ServerEvent::error(reason),
                );
            }
            CommandExecDrainOutcome::FileSystemPermissionRequired {
                request,
                diagnostic,
            } => {
                return request_command_exec_file_system_permission(
                    state, request, diagnostic, writer,
                );
            }
            CommandExecDrainOutcome::Drained => {}
        }
        return Ok(());
    }

    let mut output = match state
        .shells
        .wait(&handle.id, Duration::from_millis(timeout_ms.max(1)))
    {
        Ok(output) => output,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
    };
    if let Some(blocked_hosts) = retry_block_receiver
        && let Some(block) = CommandExecPermissionPolicy::network_permission_block(blocked_hosts)
    {
        if let Some(denial) = CommandExecPermissionPolicy::network_block_denial(&block) {
            return protocol::write_server_event(writer, &id, ServerEvent::error(denial.reason));
        }
        if let Some(request) = command_permission_request.clone() {
            return request_command_exec_network_permission(state, request, block, writer);
        }
    }
    if let Some(diagnostic) = diagnose_sandbox_denial(&cwd, &output.stdout, &output.stderr) {
        if CommandExecPermissionPolicy::should_request_filesystem_retry(
            &cwd,
            &diagnostic,
            &denied_writable_directories,
        ) && let Some(request) = command_permission_request
        {
            return request_command_exec_file_system_permission(state, request, diagnostic, writer);
        }
        append_sandbox_diagnostic_to_stderr(&mut output.stderr, &diagnostic);
    }
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::CommandExecCompleted {
            process_id: Value::Null,
            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
            stdout: Value::from(cap_text(
                &output.stdout,
                options
                    .output_bytes_cap
                    .and_then(|cap| usize::try_from(cap).ok()),
            )),
            stderr: Value::from(cap_text(
                &output.stderr,
                options
                    .output_bytes_cap
                    .and_then(|cap| usize::try_from(cap).ok()),
            )),
        },
    )
}

fn request_command_exec_network_permission<W: Write>(
    state: &mut ServerState,
    request: PendingCommandExecPermissionRequest,
    block: RuntimeNetworkBlockReport,
    writer: &mut W,
) -> io::Result<()> {
    let prompt = CommandExecPermissionPolicy::network_block_prompt(&block)
        .expect("command/exec network permission prompts require requestable blocks");
    let (_origin, _kind, reason, permissions) = prompt.into_request_parts();
    request_command_exec_permission(state, request, reason, permissions, writer)
}

fn request_command_exec_file_system_permission<W: Write>(
    state: &mut ServerState,
    request: PendingCommandExecPermissionRequest,
    diagnostic: SandboxDenialDiagnostic,
    writer: &mut W,
) -> io::Result<()> {
    let prompt = CommandExecPermissionPolicy::sandbox_denial_prompt(&diagnostic);
    let (_origin, _kind, reason, permissions) = prompt.into_request_parts();
    request_command_exec_permission(state, request, reason, permissions, writer)
}

fn request_command_exec_permission<W: Write>(
    state: &mut ServerState,
    request: PendingCommandExecPermissionRequest,
    reason: String,
    permissions: protocol::RequestPermissionProfile,
    writer: &mut W,
) -> io::Result<()> {
    let thread_id = request.thread_id.clone();
    let request_id = format!(
        "permission-command-{}",
        request
            .event_id
            .as_str()
            .map(ToString::to_string)
            .unwrap_or_else(|| request.event_id.to_string())
    );
    state
        .pending_permissions
        .insert_command_exec(request_id.clone(), request)?;
    protocol::write_server_event(
        writer,
        &Value::from(request_id.clone()),
        ServerEvent::PermissionRequest {
            request_id: json!(request_id),
            thread_id: json!(thread_id),
            turn_id: Value::Null,
            reason: json!(reason),
            permissions: serde_json::to_value(&permissions).unwrap_or(Value::Null),
        },
    )
}

fn append_sandbox_diagnostic_to_stderr(stderr: &mut String, diagnostic: &SandboxDenialDiagnostic) {
    if stderr.trim_end().is_empty() {
        *stderr = diagnostic.message.clone();
    } else {
        stderr.push_str(&format!("\n\nSandbox diagnostic: {}", diagnostic.message));
    }
}

fn run_command_exec_list<W: Write>(
    state: &mut ServerState,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let shell_snapshots = state
        .shells
        .list()
        .into_iter()
        .map(|snapshot| (snapshot.id.clone(), snapshot))
        .collect::<HashMap<_, _>>();
    let processes = state
        .command_exec
        .list()
        .into_iter()
        .map(|snapshot| {
            let shell_snapshot = snapshot
                .shell_id
                .as_ref()
                .and_then(|shell_id| shell_snapshots.get(shell_id));
            command_exec_snapshot_to_json(snapshot, shell_snapshot)
        })
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::CommandExecListed {
            processes: Value::from(processes),
        },
    )
}

fn run_command_exec_write<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    delta_base64: Option<&str>,
    close_stdin: bool,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if delta_base64.is_none() && !close_stdin {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec/write requires deltaBase64 or closeStdin"),
        );
    }
    state.command_exec.write_to_process(
        state.shells.sessions_mut(),
        process_id,
        delta_base64,
        close_stdin,
        &id,
        writer,
    )
}

fn run_command_exec_read<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    timeout_ms: u64,
    output_bytes_cap: Option<usize>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let outcome = state.command_exec.read_process(
        state.shells.sessions_mut(),
        process_id,
        Duration::from_millis(timeout_ms.max(1)),
        output_bytes_cap,
        &id,
        writer,
    )?;
    match outcome {
        CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
            request_command_exec_network_permission(state, request, block, writer)
        }
        CommandExecDrainOutcome::NetworkPermissionDenied {
            command_event_id,
            reason,
        } => protocol::write_server_event(writer, &command_event_id, ServerEvent::error(reason)),
        CommandExecDrainOutcome::FileSystemPermissionRequired {
            request,
            diagnostic,
        } => request_command_exec_file_system_permission(state, request, diagnostic, writer),
        CommandExecDrainOutcome::Drained => Ok(()),
    }
}

fn run_command_exec_resize<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    cols: u16,
    rows: u16,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if cols == 0 || rows == 0 {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("command/exec size rows and cols must be greater than 0"),
        );
    }
    state.command_exec.resize_process(
        state.shells.sessions_mut(),
        process_id,
        cols,
        rows,
        &id,
        writer,
    )
}

fn drain_command_exec_processes<W: Write>(
    state: &mut ServerState,
    writer: &mut W,
) -> io::Result<CommandExecDrainOutcome> {
    state
        .command_exec
        .drain(state.shells.sessions_mut(), writer)
}

fn drain_command_exec_processes_with_timeout<W: Write>(
    state: &mut ServerState,
    writer: &mut W,
    timeout: Duration,
) -> io::Result<CommandExecDrainOutcome> {
    state
        .command_exec
        .drain_with_timeout(state.shells.sessions_mut(), writer, timeout)
}

fn drain_command_exec_processes_until_output_or_timeout<W: Write>(
    state: &mut ServerState,
    writer: &mut W,
    timeout: Duration,
) -> io::Result<CommandExecDrainOutcome> {
    state
        .command_exec
        .drain_until_output_or_timeout(state.shells.sessions_mut(), writer, timeout)
}

fn cap_text(text: &str, cap: Option<usize>) -> String {
    let Some(cap) = cap else {
        return text.to_string();
    };
    let visible_len = capped_utf8_len(text, cap);
    text[..visible_len].to_string()
}

fn capped_utf8_len(text: &str, cap: usize) -> usize {
    if cap >= text.len() {
        return text.len();
    }
    let mut len = cap;
    while len > 0 && !text.is_char_boundary(len) {
        len -= 1;
    }
    len
}

fn write_command_exec_output_deltas<W: Write>(
    writer: &mut W,
    process_id: &str,
    stdout_delta: &str,
    stderr_delta: &str,
    stdout_cap_reached: bool,
    stderr_cap_reached: bool,
    final_chunk: bool,
) -> io::Result<()> {
    if !stdout_delta.is_empty() {
        protocol::write_server_event(
            writer,
            &Value::Null,
            ServerEvent::CommandExecOutputDelta {
                process_id: Value::from(process_id.to_string()),
                stream: Value::from("stdout"),
                delta: Value::from(stdout_delta.to_string()),
                delta_base64: Value::from(BASE64_STANDARD.encode(stdout_delta.as_bytes())),
                cap_reached: Value::from(stdout_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    if !stderr_delta.is_empty() {
        protocol::write_server_event(
            writer,
            &Value::Null,
            ServerEvent::CommandExecOutputDelta {
                process_id: Value::from(process_id.to_string()),
                stream: Value::from("stderr"),
                delta: Value::from(stderr_delta.to_string()),
                delta_base64: Value::from(BASE64_STANDARD.encode(stderr_delta.as_bytes())),
                cap_reached: Value::from(stderr_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    Ok(())
}

fn run_command_exec_terminate<W: Write>(
    state: &mut ServerState,
    process_id: &str,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    state
        .command_exec
        .terminate_process(state.shells.sessions_mut(), process_id, &id, writer)
}

fn write_shell_completed<W: Write>(
    writer: &mut W,
    id: &Value,
    output: crate::shell_session::ShellSessionOutput,
) -> io::Result<()> {
    write_shell_completed_with_cap(writer, id, output, None)
}

fn write_shell_completed_with_cap<W: Write>(
    writer: &mut W,
    id: &Value,
    output: crate::shell_session::ShellSessionOutput,
    output_bytes_cap: Option<usize>,
) -> io::Result<()> {
    let stdout = cap_text(&output.stdout, output_bytes_cap);
    let stderr = cap_text(&output.stderr, output_bytes_cap);
    let cap_reached = shell_output_cap_reached(&output, output_bytes_cap);
    write_shell_output_deltas_with_cap(writer, id, &output, true, output_bytes_cap)?;
    protocol::write_server_event(
        writer,
        id,
        ServerEvent::ShellExited {
            shell_id: Value::from(output.id.clone()),
            task_id: Value::from(output.task_id.clone()),
            status: Value::from(shell_status_label(output.status)),
            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
        },
    )?;
    protocol::write_server_event(
        writer,
        id,
        ServerEvent::ShellCompleted {
            shell_id: Value::from(output.id),
            task_id: Value::from(output.task_id),
            status: Value::from(shell_status_label(output.status)),
            stdout: Value::from(stdout),
            stderr: Value::from(stderr),
            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
            cap_reached: shell_cap_reached_value(output_bytes_cap, cap_reached),
        },
    )
}

fn write_shell_output_deltas_with_cap<W: Write>(
    writer: &mut W,
    id: &Value,
    output: &crate::shell_session::ShellSessionOutput,
    final_chunk: bool,
    output_bytes_cap: Option<usize>,
) -> io::Result<()> {
    let stdout = cap_text(&output.stdout, output_bytes_cap);
    let stderr = cap_text(&output.stderr, output_bytes_cap);
    let stdout_cap_reached =
        output_bytes_cap.is_some_and(|cap| output.stdout.len() >= cap && !output.stdout.is_empty());
    let stderr_cap_reached =
        output_bytes_cap.is_some_and(|cap| output.stderr.len() >= cap && !output.stderr.is_empty());
    if !stdout.is_empty() {
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::ShellOutputDelta {
                shell_id: Value::from(output.id.clone()),
                stream: Value::from("stdout"),
                delta: Value::from(stdout),
                cap_reached: Value::from(stdout_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    if !stderr.is_empty() {
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::ShellOutputDelta {
                shell_id: Value::from(output.id.clone()),
                stream: Value::from("stderr"),
                delta: Value::from(stderr),
                cap_reached: Value::from(stderr_cap_reached),
                final_chunk: Value::from(final_chunk),
            },
        )?;
    }
    Ok(())
}

fn shell_output_cap_reached(
    output: &crate::shell_session::ShellSessionOutput,
    output_bytes_cap: Option<usize>,
) -> bool {
    output_bytes_cap.is_some_and(|cap| output.stdout.len() >= cap || output.stderr.len() >= cap)
}

fn shell_cap_reached_value(output_bytes_cap: Option<usize>, cap_reached: bool) -> Value {
    if output_bytes_cap.is_some() {
        Value::from(cap_reached)
    } else {
        Value::Null
    }
}

fn shell_snapshot_to_json(snapshot: crate::shell_session::ShellSessionSnapshot) -> Value {
    json!({
        "shellId": snapshot.id,
        "taskId": snapshot.task_id,
        "command": snapshot.command,
        "description": snapshot.description,
        "status": shell_status_label(snapshot.status),
        "requestedTerminalMode": snapshot.requested_terminal.as_str(),
        "effectiveTerminalMode": snapshot.effective_terminal.as_str(),
    })
}

fn command_exec_snapshot_to_json(
    snapshot: CommandExecProcessSnapshot,
    shell_snapshot: Option<&crate::shell_session::ShellSessionSnapshot>,
) -> Value {
    json!({
        "processId": snapshot.process_id,
        "shellId": snapshot.shell_id,
        "taskId": shell_snapshot.map(|shell| shell.task_id.clone()),
        "command": snapshot.command,
        "cwd": snapshot.cwd.display().to_string(),
        "status": shell_snapshot
            .map(|shell| shell_status_label(shell.status))
            .unwrap_or(snapshot.status),
        "requestedTerminalMode": shell_snapshot.map(|shell| shell.requested_terminal.as_str()),
        "effectiveTerminalMode": shell_snapshot.map(|shell| shell.effective_terminal.as_str()),
        "streamOutput": snapshot.stream_output,
        "outputBytesCap": snapshot.output_bytes_cap,
        "stdoutBytes": snapshot.stdout_bytes,
        "stderrBytes": snapshot.stderr_bytes,
    })
}

fn unknown_shell<W: Write>(writer: &mut W, id: &Value, shell_id: &str) -> io::Result<()> {
    protocol::write_server_event(
        writer,
        id,
        ServerEvent::error(format!("unknown shell session: {shell_id}")),
    )
}

fn shell_status_label(status: orca_core::task_types::TaskStatus) -> &'static str {
    match status {
        orca_core::task_types::TaskStatus::Completed => "completed",
        orca_core::task_types::TaskStatus::Stopped => "stopped",
        orca_core::task_types::TaskStatus::Failed => "failed",
        orca_core::task_types::TaskStatus::ApprovalRequired => "approval_required",
        orca_core::task_types::TaskStatus::Cancelled => "cancelled",
        orca_core::task_types::TaskStatus::Running => "running",
        orca_core::task_types::TaskStatus::Queued => "queued",
        orca_core::task_types::TaskStatus::Paused => "paused",
        orca_core::task_types::TaskStatus::Stopping => "stopping",
    }
}

fn server_cwd(config: &RunConfig) -> io::Result<PathBuf> {
    config
        .cwd
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
}

fn run_thread_list<W: Write>(
    cursor: Option<&str>,
    limit: usize,
    filters: ThreadListFilters,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    search_term: Option<&str>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let store = SessionStore::new();
    let page = store.list_threads(
        cursor,
        limit,
        filters,
        sort_key,
        sort_direction,
        search_term,
    )?;
    let data = page
        .data
        .into_iter()
        .map(thread_summary_to_json)
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadList {
            data: Value::from(data),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn run_thread_search<W: Write>(
    query: &str,
    cursor: Option<&str>,
    limit: usize,
    include_archived: bool,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    if query.is_empty() {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("thread search term must not be empty"),
        );
    }
    let store = SessionStore::new();
    let page = store.search_threads(
        query,
        cursor,
        limit,
        include_archived,
        sort_key,
        sort_direction,
    )?;
    let data = page
        .data
        .into_iter()
        .map(|hit| {
            serde_json::json!({
                "thread": thread_summary_to_json(hit.thread),
                "snippet": hit.snippet,
            })
        })
        .collect::<Vec<_>>();
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadSearch {
            data: Value::from(data),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn thread_summary_to_json(summary: StoredThreadSummary) -> Value {
    serde_json::json!({
        "threadId": summary.thread_id,
        "title": summary.title,
        "cwd": summary.cwd,
        "provider": summary.provider,
        "model": summary.model,
        "createdAt": summary.created_at.to_rfc3339(),
        "updatedAt": summary.updated_at.to_rfc3339(),
        "archived": summary.archived,
        "parentId": summary.parent_id,
        "forked": summary.forked,
        "approvalMode": summary.approval_mode.map(|mode| mode.as_str()),
        "runtimeWorkspaceRoots": runtime_workspace_roots_to_json(summary.runtime_workspace_roots),
        "activePermissionProfile": active_permission_profile_to_json(summary.active_permission_profile),
        "permissionRuleCount": summary.permission_rule_count,
        "additionalWorkingDirectoryCount": summary.additional_working_directories.len(),
        "additionalWorkingDirectories": additional_working_directories_to_json(summary.additional_working_directories),
        "networkDomainPermissionCount": summary.network_domain_permissions.len(),
        "networkDomainPermissions": network_domain_permissions_to_json(summary.network_domain_permissions),
    })
}

fn network_domain_permissions_to_json(
    permissions: HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
) -> Value {
    serde_json::to_value(permissions).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn additional_working_directories_to_json(
    directories: Vec<orca_core::config::AdditionalWorkingDirectory>,
) -> Value {
    Value::from(
        directories
            .into_iter()
            .map(|directory| {
                serde_json::json!({
                    "path": directory.path,
                    "source": directory.source,
                })
            })
            .collect::<Vec<_>>(),
    )
}

fn runtime_workspace_roots_to_json(roots: Vec<PathBuf>) -> Value {
    Value::from(
        roots
            .into_iter()
            .map(|root| Value::from(root.display().to_string()))
            .collect::<Vec<_>>(),
    )
}

fn active_permission_profile_to_json(
    profile: Option<orca_core::config::ActivePermissionProfile>,
) -> Value {
    profile
        .map(|profile| {
            serde_json::json!({
                "id": profile.id,
                "extends": profile.extends,
            })
        })
        .unwrap_or(Value::Null)
}

fn run_thread_turns_list<W: Write>(
    state: &ServerState,
    thread_id: &str,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
    items_view: TurnItemsView,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let store = SessionStore::new();
    let page = match store.list_thread_turns(thread_id, cursor, limit, sort_direction, items_view) {
        Ok(page) => page,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match state.threads.list_thread_turns(
                thread_id,
                cursor,
                limit,
                sort_direction,
                items_view,
            ) {
                Some(page) => page,
                None => {
                    return protocol::write_server_event(
                        writer,
                        &id,
                        ServerEvent::error(format!("unknown thread: {thread_id}")),
                    );
                }
            }
        }
        Err(error) => return Err(error),
    };

    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadTurnsList {
            data: Value::from(
                page.data
                    .into_iter()
                    .map(thread_turn_to_json)
                    .collect::<Vec<_>>(),
            ),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn run_thread_items_list<W: Write>(
    state: &ServerState,
    thread_id: &str,
    turn_id: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let store = SessionStore::new();
    let page = match store.list_thread_items(thread_id, turn_id, cursor, limit, sort_direction) {
        Ok(page) => page,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match state
                .threads
                .list_thread_items(thread_id, turn_id, cursor, limit, sort_direction)
            {
                Some(page) => page,
                None => {
                    return protocol::write_server_event(
                        writer,
                        &id,
                        ServerEvent::error(format!("unknown thread: {thread_id}")),
                    );
                }
            }
        }
        Err(error) => return Err(error),
    };

    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadItemsList {
            data: Value::from(
                page.data
                    .into_iter()
                    .map(thread_item_to_json)
                    .collect::<Vec<_>>(),
            ),
            next_cursor: optional_string_to_json(page.next_cursor),
            backwards_cursor: optional_string_to_json(page.backwards_cursor),
        },
    )
}

fn optional_string_to_json(value: Option<String>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

fn run_thread_start<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    runtime_workspace_roots: Option<Vec<PathBuf>>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let mut run_config = thread_run_config(&config.run_config);
    if let Some(runtime_workspace_roots) = runtime_workspace_roots {
        run_config.runtime_workspace_roots = Some(runtime_workspace_roots);
    }
    let thread_id = state.threads.start_thread(&run_config)?;
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::ThreadStarted {
            thread_id: Value::from(thread_id),
        },
    )
}

fn run_thread_resume<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: &str,
    permissions: PermissionProfileOverride,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match state
        .threads
        .resume_thread_with_permissions(&config.run_config, thread_id, permissions)
    {
        Ok(thread_id) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadStarted {
                thread_id: Value::from(thread_id),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) => Err(error),
    }
}

fn run_thread_fork<W: Write>(
    config: &ServerConfig,
    state: &mut ServerState,
    thread_id: &str,
    permissions: PermissionProfileOverride,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match state
        .threads
        .fork_thread_with_permissions(&config.run_config, thread_id, permissions)
    {
        Ok(thread_id) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadStarted {
                thread_id: Value::from(thread_id),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) => Err(error),
    }
}

fn run_thread_submit_async<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    id: Value,
    op: ClientOp,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    let run_config = thread_run_config(&config.run_config);
    let ClientOp::Submit {
        thread_id: Some(thread_id),
        prompt,
        permissions,
    } = op
    else {
        return Ok(());
    };

    state.reclaim_finished_thread(&thread_id);
    let Some(mut thread_state) = state.threads.take_thread(&thread_id) else {
        if state.active_turns.has_thread(&thread_id) {
            return write_locked_event(
                &writer,
                &id,
                ServerEvent::error(format!("thread has an active turn: {thread_id}")),
            );
        }
        return write_locked_event(
            &writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        );
    };
    let cancel = CancelToken::new();
    let steer_handle = ThreadSteerHandle::default();
    let active_turn_id = thread_state.next_persisted_turn_id();
    let runtime_workspace_roots = permissions
        .runtime_workspace_roots
        .clone()
        .unwrap_or_else(|| thread_state.runtime_workspace_roots().to_vec());
    state.active_turns.insert_control(
        active_turn_id.clone(),
        ActiveTurnControl::new(thread_id.clone(), cancel.clone(), steer_handle.clone()),
    );

    let writer_for_thread = Arc::clone(&writer);
    let thread_id_for_return = thread_id.clone();
    let active_turn_id_for_return = active_turn_id.clone();
    let permission_handler = Arc::new(ServerPermissionRequestHandler::new(
        Arc::clone(&writer),
        state.pending_permissions.clone(),
        id.clone(),
        thread_id.clone(),
        active_turn_id.clone(),
        runtime_workspace_roots,
    ));
    let user_input_handler = Arc::new(ServerUserInputRequestHandler::new(
        Arc::clone(&writer),
        state.pending_user_inputs.clone(),
        id.clone(),
        thread_id.clone(),
        active_turn_id.clone(),
    ));
    let mcp_elicitation_handler = Arc::new(ServerMcpElicitationRequestHandler::new(
        Arc::clone(&writer),
        state.pending_mcp_elicitations.clone(),
        id.clone(),
        thread_id.clone(),
        active_turn_id.clone(),
        cancel.clone(),
    ));
    let handle = thread::spawn(move || {
        let mut writer = SharedServerRequestWriter::new(id.clone(), Arc::clone(&writer_for_thread));
        let status = thread_state.run_turn_with_permissions_cancel_and_permission_handler(
            &run_config,
            &prompt,
            permissions,
            &mut writer,
            cancel,
            steer_handle,
            permission_handler,
            user_input_handler,
            mcp_elicitation_handler,
        );
        let _ = writer.flush_remaining();
        if let Err(error) = status {
            let _ = write_locked_event(
                &writer_for_thread,
                &id,
                ServerEvent::error(error.to_string()),
            );
        }
        (
            active_turn_id_for_return,
            thread_id_for_return,
            thread_state,
        )
    });
    state
        .active_turns
        .push_running(ActiveTurnHandle::new(handle));
    state.reclaim_finished_threads();
    Ok(())
}

fn run_thread_read<W: Write>(
    state: &ServerState,
    thread_id: &str,
    include_messages: bool,
    include_turns: bool,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let store = SessionStore::new();
    if let Ok(thread) = store.read_thread(thread_id, include_messages, include_turns) {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadRead {
                thread_id: Value::from(thread.thread_id),
                title: Value::from(thread.title),
                cwd: Value::from(thread.cwd),
                runtime_workspace_roots: runtime_workspace_roots_to_json(
                    thread.runtime_workspace_roots,
                ),
                active_permission_profile: active_permission_profile_to_json(
                    thread.active_permission_profile,
                ),
                additional_working_directory_count: Value::from(
                    thread.additional_working_directories.len() as u64,
                ),
                additional_working_directories: additional_working_directories_to_json(
                    thread.additional_working_directories,
                ),
                network_domain_permission_count: Value::from(
                    thread.network_domain_permissions.len() as u64,
                ),
                network_domain_permissions: network_domain_permissions_to_json(
                    thread.network_domain_permissions,
                ),
                message_count: Value::from(thread.message_count as u64),
                messages: Value::from(thread.messages),
                turns: Value::from(
                    thread
                        .turns
                        .into_iter()
                        .map(thread_turn_to_json)
                        .collect::<Vec<_>>(),
                ),
            },
        );
    }

    match state
        .threads
        .read_thread(thread_id, include_messages, include_turns)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown thread"))
    {
        Ok(thread) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadRead {
                thread_id: Value::from(thread.thread_id),
                title: Value::from(thread.title),
                cwd: Value::from(thread.cwd),
                runtime_workspace_roots: runtime_workspace_roots_to_json(
                    thread.runtime_workspace_roots,
                ),
                active_permission_profile: active_permission_profile_to_json(
                    thread.active_permission_profile,
                ),
                additional_working_directory_count: Value::from(
                    thread.additional_working_directories.len() as u64,
                ),
                additional_working_directories: additional_working_directories_to_json(
                    thread.additional_working_directories,
                ),
                network_domain_permission_count: Value::from(
                    thread.network_domain_permissions.len() as u64,
                ),
                network_domain_permissions: network_domain_permissions_to_json(
                    thread.network_domain_permissions,
                ),
                message_count: Value::from(thread.message_count as u64),
                messages: Value::from(thread.messages),
                turns: Value::from(
                    thread
                        .turns
                        .into_iter()
                        .map(thread_turn_to_json)
                        .collect::<Vec<_>>(),
                ),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) => Err(error),
    }
}

fn run_thread_metadata_update<W: Write>(
    state: &mut ServerState,
    thread_id: &str,
    title: Option<String>,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let Some(title) = title else {
        return protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("thread metadata patch did not include any supported fields"),
        );
    };

    let live_thread_updated = state.threads.update_thread_metadata(
        thread_id,
        ThreadMetadataPatch {
            title: Some(title.clone()),
            ..ThreadMetadataPatch::default()
        },
    );

    let store = SessionStore::new();
    match store.update_thread_metadata(
        thread_id,
        ThreadMetadataPatch {
            title: Some(title.clone()),
            ..ThreadMetadataPatch::default()
        },
    ) {
        Ok(_) => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::ThreadMetadataUpdated {
                thread_id: Value::from(thread_id.to_string()),
                title: Value::from(title),
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound && live_thread_updated => {
            protocol::write_server_event(
                writer,
                &id,
                ServerEvent::ThreadMetadataUpdated {
                    thread_id: Value::from(thread_id.to_string()),
                    title: Value::from(title),
                },
            )
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error(format!("unknown thread: {thread_id}")),
        ),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            protocol::write_server_event(writer, &id, ServerEvent::error(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

fn run_submit<W: Write>(
    config: &ServerConfig,
    id: Value,
    op: ClientOp,
    writer: &mut W,
) -> io::Result<()> {
    let mut run_config = config.run_config.clone();
    let ClientOp::Submit { prompt, .. } = op else {
        return Ok(());
    };
    run_config.prompt = prompt;
    // Defensive: force JSONL output and disable history regardless of config file settings.
    run_config.output_format = OutputFormat::Jsonl;
    run_config.history_mode = HistoryMode::Disabled;
    run_config.show_session_picker = false;
    run_config.desktop_notifications = false;

    let mut streaming_writer = ServerRequestWriter::new(id, writer);
    let _exit_code = crate::controller::run_to_writer_with_options(
        run_config,
        &mut streaming_writer,
        crate::controller::ControllerRunOptions {
            wait_for_background_workflows: true,
        },
    );
    streaming_writer.flush_remaining()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use std::io::{Cursor, Read};
    use tempfile::{TempDir, tempdir};

    #[derive(Clone, Default)]
    struct SharedVecWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedVecWriter {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedVecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ErrorAfterPendingUserInputReader {
        phase: u8,
        output: SharedVecWriter,
    }

    impl Read for ErrorAfterPendingUserInputReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl BufRead for ErrorAfterPendingUserInputReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Ok(&[])
        }

        fn consume(&mut self, _amount: usize) {}

        fn read_line(&mut self, buffer: &mut String) -> io::Result<usize> {
            match self.phase {
                0 => {
                    self.phase = 1;
                    let line = r#"{"id":"thread","method":"thread/start","params":{}}"#;
                    buffer.push_str(line);
                    buffer.push('\n');
                    Ok(line.len() + 1)
                }
                1 => {
                    let started = wait_for_event(&self.output.0, Duration::from_secs(2), |event| {
                        event["event"] == "thread_started"
                    })
                    .ok_or_else(|| io::Error::other("thread did not start"))?;
                    let thread_id = started["threadId"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("missing thread id"))?;
                    self.phase = 2;
                    let line = format!(
                        r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"ask Continue?"}}]}}}}"#
                    );
                    buffer.push_str(&line);
                    buffer.push('\n');
                    Ok(line.len() + 1)
                }
                _ => {
                    wait_for_event(&self.output.0, Duration::from_secs(2), |event| {
                        event["event"] == "user_input_request"
                    })
                    .ok_or_else(|| io::Error::other("user input request did not start"))?;
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "injected server input failure",
                    ))
                }
            }
        }
    }

    #[test]
    fn server_input_error_releases_pending_user_input_turn() {
        let temp = tempdir().expect("tempdir");
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let output = SharedVecWriter::default();
        let reader = ErrorAfterPendingUserInputReader {
            phase: 0,
            output: output.clone(),
        };

        let error = run_with_io(ServerConfig { run_config: config }, reader, output.clone())
            .expect_err("reader failure should be returned");

        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&output.0) > 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            Arc::strong_count(&output.0),
            1,
            "server input failure must release the active turn writer"
        );
    }

    #[test]
    fn maps_runtime_tool_events_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"tool.call.requested","payload":{"name":"read_file","target":"src/main.rs"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "tool_requested");
        assert_eq!(mapped["tool"], "read_file");
        assert_eq!(mapped["target"], "src/main.rs");
        assert!(mapped.get("type").is_none());
    }

    #[test]
    fn maps_runtime_plan_updated_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"plan.updated","payload":{"explanation":"ship it","plan":[{"step":"Inspect","status":"completed"},{"step":"Implement","status":"in_progress"}]}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(7), mapped);

        assert_eq!(mapped["event"], "turn_plan_updated");
        assert!(mapped["threadId"].is_null());
        assert!(mapped["turnId"].is_null());
        assert_eq!(mapped["explanation"], "ship it");
        assert_eq!(mapped["plan"][0]["step"], "Inspect");
        assert_eq!(mapped["plan"][0]["status"], "completed");
        assert_eq!(mapped["plan"][1]["step"], "Implement");
        assert_eq!(mapped["plan"][1]["status"], "in_progress");
    }

    #[test]
    fn maps_runtime_workflow_events_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_started");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["workflowName"], "audit");
    }

    #[test]
    fn maps_runtime_workflow_result_available_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.result.available","payload":{"taskId":"task-1","runId":"workflow-run-1","result":"done"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_result_available");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["result"], "done");
    }

    #[test]
    fn maps_runtime_workflow_completed_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.completed","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_completed");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["workflowName"], "audit");
    }

    #[test]
    fn maps_runtime_workflow_failed_event_to_protocol_shape() {
        let mapped = protocol::map_runtime_event_line(
            r#"{"type":"workflow.failed","payload":{"taskId":"task-1","runId":"workflow-run-1","error":"boom"}}"#,
        )
        .expect("mapped event");
        let mapped = protocol::legacy_json_event(Value::from(1), mapped);

        assert_eq!(mapped["event"], "workflow_failed");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["error"], "boom");
    }

    #[test]
    fn server_writer_streams_events_as_lines_arrive() {
        let mut output = Vec::new();
        let id = Value::from(42);
        {
            let mut writer = ServerRequestWriter::new(id, &mut output);
            writer
                .write_all(
                    b"{\"type\":\"assistant.message.delta\",\"payload\":{\"text\":\"hi\"}}\n",
                )
                .unwrap();
        }
        let events = parse_jsonl(&output);
        assert!(events.iter().all(|event| event["id"] == 42));
        assert!(events.iter().any(|event| {
            event["event"] == "item_started" && event["item"]["type"] == "agent_message"
        }));
        assert!(events.iter().any(|event| {
            event["event"] == "item_message_delta"
                && event["itemId"] == "item-agent-message-1"
                && event["delta"] == "hi"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "message_delta" && event["text"] == "hi")
        );
    }

    #[test]
    fn server_writer_streams_tool_call_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"tool-1","name":"bash","target":"cargo test"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"tool-1","name":"bash","status":"completed","output":"ok","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "commandExecution"
                    && event["item"]["id"] == "tool-1"
            })
            .expect("tool item_started");
        assert_eq!(started["item"]["tool"], "bash");
        assert_eq!(started["item"]["command"], "cargo test");
        assert_eq!(started["item"]["status"], "in_progress");

        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_requested" && event["tool"] == "bash")
        );

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "commandExecution"
                    && event["item"]["id"] == "tool-1"
            })
            .expect("tool item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["aggregatedOutput"], "ok");
        assert!(completed["item"].get("output").is_none());
        assert_eq!(completed["item"]["exitCode"], 0);

        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_completed" && event["status"] == "completed")
        );
        let legacy_completed = events
            .iter()
            .find(|event| event["event"] == "tool_completed" && event["tool"] == "bash")
            .expect("legacy tool_completed");
        assert_eq!(legacy_completed["exitCode"], 0);
    }

    #[test]
    fn server_writer_preserves_failed_command_execution_output_for_diagnostics() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"tool-1","name":"bash","target":"cargo test"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"tool-1","name":"bash","status":"failed","output":"test failure details","error":"command failed","exit_code":101}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "commandExecution"
                    && event["item"]["id"] == "tool-1"
            })
            .expect("tool item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert_eq!(
            completed["item"]["aggregatedOutput"],
            "test failure details"
        );
        assert!(completed["item"].get("output").is_none());
        assert_eq!(completed["item"]["error"], "command failed");
        assert_eq!(completed["item"]["exitCode"], 101);
    }

    #[test]
    fn command_exec_manager_rejects_duplicate_active_process_id_until_removed() {
        let mut manager = CommandExecManager::default();
        let first = command_exec_process("shell-1");
        let duplicate = command_exec_process("shell-2");

        assert!(manager.insert("proc-1".to_string(), first).is_ok());
        let duplicate_error = manager
            .insert("proc-1".to_string(), duplicate)
            .expect_err("duplicate process id should be rejected");
        assert_eq!(
            duplicate_error,
            "duplicate active command/exec process id: \"proc-1\""
        );

        assert_eq!(
            manager
                .get("proc-1")
                .expect("registered process")
                .shell_id
                .as_deref(),
            Some("shell-1")
        );
        manager.remove("proc-1");
        assert!(
            manager
                .insert("proc-1".to_string(), command_exec_process("shell-3"))
                .is_ok()
        );
        assert_eq!(
            manager
                .get("proc-1")
                .expect("re-registered process")
                .shell_id
                .as_deref(),
            Some("shell-3")
        );
    }

    #[test]
    fn shell_list_does_not_reap_command_exec_owned_shells() {
        let cwd = tempdir().expect("tempdir");
        let mut state = ServerState::default();
        let handle = state
            .shells
            .spawn(
                cwd.path(),
                ShellSessionCommand {
                    command: "printf command-owned".to_string(),
                    cwd: cwd.path().to_path_buf(),
                    additional_readable_directories: Vec::new(),
                    additional_working_directories: Vec::new(),
                    denied_working_directories: Vec::new(),
                    allowed_unix_socket_roots: Vec::new(),
                    env: std::collections::BTreeMap::new(),
                    description: "command exec owned shell".to_string(),
                    terminal: crate::shell_session::ShellTerminalMode::pipe(),
                    sandbox: ShellSandboxMode::DangerFullAccess,
                },
                None,
            )
            .expect("spawn command exec shell");
        state
            .command_exec
            .insert(
                "proc-shell-list".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-shell-list"),
                    command: vec!["sh".to_string(), "-lc".to_string()],
                    cwd: cwd.path().to_path_buf(),
                    denied_writable_roots: Vec::new(),
                    stream_output: false,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        std::thread::sleep(Duration::from_millis(100));

        let mut output = Vec::new();
        run_shell_list(&mut state, Value::from("shell-list"), &mut output).expect("shell/list");
        drain_command_exec_processes_with_timeout(
            &mut state,
            &mut output,
            Duration::from_millis(50),
        )
        .expect("drain command exec");
        let events = parse_jsonl(&output);

        assert!(
            events
                .iter()
                .all(|event| event["event"] != "shell_completed"),
            "shell/list should not complete command/exec-owned shells: {events:?}"
        );
        let listed = events
            .iter()
            .find(|event| event["event"] == "shell_listed")
            .expect("shell/list response");
        assert_eq!(
            listed["shells"].as_array().expect("shell list").len(),
            0,
            "shell/list should hide command/exec-owned shells: {events:?}"
        );
        assert!(
            events.iter().any(|event| {
                event["event"] == "command_exec_completed"
                    && event["processId"] == "proc-shell-list"
                    && event["stdout"] == "command-owned"
            }),
            "command/exec owner should still emit completion after shell/list: {events:?}"
        );
    }

    #[test]
    fn command_exec_sandbox_resolution_is_owned_by_focused_module() {
        let server_source = include_str!("server.rs");

        assert!(
            !server_source.contains("\nfn shell_sandbox_mode_from_permission_profile("),
            "command/exec permission-profile sandbox resolution should live outside server.rs"
        );
        assert!(
            !server_source.contains("\nfn resolve_permission_profile("),
            "command/exec permission-profile traversal should live outside server.rs"
        );
    }

    #[test]
    fn command_exec_sandbox_resolves_custom_permission_profile_chain() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "locked-down".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("read-base".to_string()),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "read-base".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("locked-down".to_string()),
            ..Default::default()
        };

        let sandbox =
            test_profile_sandbox(&config, &options).expect("custom permission profile sandbox");

        assert_eq!(
            sandbox.mode,
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false
            }
        );
    }

    #[test]
    fn command_exec_sandbox_applies_custom_permission_profile_network_override() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "read-network".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                network: orca_core::config::PermissionProfileNetworkConfig {
                    enabled: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "workspace-offline".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":workspace".to_string()),
                network: orca_core::config::PermissionProfileNetworkConfig {
                    enabled: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let read_options = protocol::CommandExecOptions {
            permission_profile: Some("read-network".to_string()),
            ..Default::default()
        };
        let workspace_options = protocol::CommandExecOptions {
            permission_profile: Some("workspace-offline".to_string()),
            ..Default::default()
        };

        let read_sandbox =
            test_profile_sandbox(&config, &read_options).expect("read-only network profile");
        let workspace_sandbox =
            test_profile_sandbox(&config, &workspace_options).expect("workspace network profile");

        assert_eq!(
            read_sandbox.mode,
            ShellSandboxMode::ReadOnly {
                network_access: true,
                allow_global_read: false
            }
        );
        assert_eq!(
            workspace_sandbox.mode,
            ShellSandboxMode::WorkspaceWrite {
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false
            }
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_domain_policy() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.example.com" = "allow"
"blocked.example.com" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let options = protocol::CommandExecOptions {
            permission_profile: Some("limited-network".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("domain policy profile");

        assert_eq!(
            sandbox.network_policy_domains.get("api.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
        assert_eq!(
            sandbox.network_policy_domains.get("blocked.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Deny)
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_unix_socket_allowlist() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.browser-socket]
extends = ":workspace"

[permission_profiles.browser-socket.network.unix_sockets]
"/tmp/orca-browser.sock" = "allow"
"/tmp/orca-blocked.sock" = "deny"
"#,
        )
        .expect("unix socket policy config");
        config.permission_profiles = file_config.permission_profiles;
        let options = protocol::CommandExecOptions {
            permission_profile: Some("browser-socket".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("unix socket policy profile");

        assert_eq!(
            sandbox.allowed_unix_socket_roots,
            vec![PathBuf::from("/tmp/orca-browser.sock")]
        );
    }

    #[test]
    fn command_exec_sandbox_child_domain_policy_overrides_parent_policy() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.parent]
extends = ":workspace"

[permission_profiles.parent.network.domains]
"api.example.com" = "deny"

[permission_profiles.child]
extends = "parent"

[permission_profiles.child.network.domains]
"api.example.com" = "allow"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let options = protocol::CommandExecOptions {
            permission_profile: Some("child".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("domain policy profile");

        assert_eq!(
            sandbox.network_policy_domains.get("api.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
    }

    #[test]
    fn command_exec_permission_profile_domain_policy_blocks_denied_http_request() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let input = Cursor::new(
            br#"{"id":"cmd-deny","method":"command/exec","params":{"command":["sh","-lc","curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true"],"permissionProfile":"limited-network","timeoutMs":5000}}"#
                .to_vec(),
        );
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let error = events
            .iter()
            .find(|event| event["event"] == "error")
            .expect("policy denial error");
        assert_eq!(
            error["message"],
            "command/exec network access to blocked.orca.invalid was denied by configured network policy"
        );
    }

    #[test]
    fn command_exec_permission_profile_domain_policy_reports_blocked_host() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let input = Cursor::new(
            br#"{"id":"cmd-allowlist","method":"command/exec","params":{"command":["sh","-lc","curl --noproxy '' -sS -D - -o /dev/null http://other.orca.invalid/ || true"],"permissionProfile":"limited-network","timeoutMs":5000}}"#
                .to_vec(),
        );
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        let stdout = completed["stdout"].as_str().expect("stdout");
        assert!(
            stdout.contains("x-proxy-error: blocked-by-allowlist"),
            "stdout should include structured proxy block reason: {completed:?}"
        );
        assert!(
            stdout.contains("x-proxy-host: other.orca.invalid"),
            "stdout should include blocked host for permission attribution: {completed:?}"
        );
        assert_eq!(completed["exitCode"], 0);
    }

    #[test]
    fn command_exec_permission_profile_allowlist_miss_requests_permission_and_retries() {
        with_orca_home(|home| {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let port = listener.local_addr().expect("server addr").port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                while reader.read_line(&mut line).expect("read request") != 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 15\r\n\r\nnetwork-granted")
                    .expect("write response");
            });
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = format!(
                r#"{{"id":"cmd-network","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc","curl --noproxy '' -sS http://127.0.0.1:{port}/"],"permissionProfile":"limited-network","timeoutMs":5000}}}}"#
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "command should wait for permission before completing: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            server.join().expect("server joined");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let resolved = events
                .iter()
                .find(|event| event["event"] == "permission_resolved")
                .expect("permission resolved");
            assert_eq!(resolved["requestId"], request_id);
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["stdout"], "network-granted");
            assert_eq!(completed["exitCode"], 0);
            let read = crate::thread_store::SessionStore::new()
                .load_session(&thread_id)
                .expect("stored thread");
            assert_eq!(
                read.meta.network_domain_permissions.get("127.0.0.1"),
                Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
            );
        });
    }

    #[test]
    fn command_exec_filesystem_sandbox_denial_requests_permission_and_retries() {
        if !std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return;
        }

        with_orca_home(|home| {
            let repo = home.join("repo");
            let git_dir = repo.join(".git");
            std::fs::create_dir_all(&git_dir).expect("git dir");
            let index_lock = git_dir.join("index.lock");
            let mut config = test_run_config();
            config.cwd = Some(repo.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = format!(
                r#"{{"id":"cmd-fs","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc",{}],"timeoutMs":5000}}}}"#,
                serde_json::to_string(&format!("printf locked > {}", index_lock.display()))
                    .expect("command json")
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["fileSystem"]["write"][0],
                git_dir.display().to_string()
            );
            assert!(
                permission_request["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains(
                        "command/exec attempted filesystem write outside the current sandbox"
                    )),
                "permission request should explain sandbox denial: {permission_request:?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "command should wait for permission before completing: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
                git_dir.display()
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert_eq!(std::fs::read_to_string(&index_lock).unwrap(), "locked");
            let read = crate::thread_store::SessionStore::new()
                .load_session(&thread_id)
                .expect("stored thread");
            assert!(
                read.meta
                    .additional_working_directories
                    .iter()
                    .any(|directory| directory.path == git_dir)
            );
        });
    }

    #[test]
    fn command_exec_pathless_sandbox_denial_requests_unsandboxed_permission_and_retries() {
        if !std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return;
        }

        with_orca_home(|_| {
            let parent = sandbox_test_parent("server-unsandboxed-");
            let workspace = parent.path().join("workspace-unsandboxed");
            let outside = parent.path().join("outside-unsandboxed");
            std::fs::create_dir_all(&workspace).expect("workspace dir");
            std::fs::create_dir_all(&outside).expect("outside dir");
            let marker = outside.join("credential-helper-output");
            let mut config = test_run_config();
            config.cwd = Some(workspace.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let command = format!(
                "touch {} 2>/dev/null || {{ printf %s\\\\n \"fatal: could not read Username for 'https://github.com': Operation not permitted\" >&2; exit 128; }}",
                marker.display()
            );
            let request = format!(
                r#"{{"id":"cmd-unsandboxed","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc",{}],"timeoutMs":5000}}}}"#,
                serde_json::to_string(&command).expect("command json")
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(permission_request["threadId"], thread_id);
            assert_eq!(
                permission_request["permissions"]["shell"]["unsandboxed"],
                true
            );
            assert!(
                permission_request["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("without the filesystem sandbox")),
                "permission request should explain unsandboxed retry: {permission_request:?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "command should wait for permission before completing: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"turn","permissions":{{"shell":{{"unsandboxed":true}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert!(marker.exists());
        });
    }

    #[test]
    fn command_exec_streaming_pathless_sandbox_denial_requests_unsandboxed_permission_and_retries()
    {
        if !std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return;
        }

        with_orca_home(|_| {
            let parent = sandbox_test_parent("server-unsandboxed-stream-");
            let workspace = parent.path().join("workspace-unsandboxed-stream");
            let outside = parent.path().join("outside-unsandboxed-stream");
            std::fs::create_dir_all(&workspace).expect("workspace dir");
            std::fs::create_dir_all(&outside).expect("outside dir");
            let marker = outside.join("credential-helper-output");
            let mut config = test_run_config();
            config.cwd = Some(workspace.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let command = format!(
                "touch {} 2>/dev/null || {{ printf %s\\\\n \"fatal: could not read Username for 'https://github.com': Operation not permitted\" >&2; exit 128; }}",
                marker.display()
            );
            let request = format!(
                r#"{{"id":"cmd-unsandboxed-stream","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc",{}],"processId":"unsandboxed-stream-1","streamStdoutStderr":true,"timeoutMs":5000}}}}"#,
                serde_json::to_string(&command).expect("command json")
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(6),
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| panic!("permission request; events: {events:?}"));
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(
                permission_request["permissions"]["shell"]["unsandboxed"],
                true
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"turn","permissions":{{"shell":{{"unsandboxed":true}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let completed = events
                .iter()
                .find(|event| event["event"] == "command_exec_completed")
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert!(marker.exists());
        });
    }

    #[test]
    fn command_exec_streaming_filesystem_sandbox_denial_requests_permission_and_retries() {
        if !std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return;
        }

        with_orca_home(|home| {
            let repo = home.join("repo-stream");
            let git_dir = repo.join(".git");
            std::fs::create_dir_all(&git_dir).expect("git dir");
            let index_lock = git_dir.join("index.lock");
            let mut config = test_run_config();
            config.cwd = Some(repo.clone());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = format!(
                r#"{{"id":"cmd-fs-stream","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc",{}],"processId":"fs-stream-1","streamStdoutStderr":true,"timeoutMs":5000}}}}"#,
                serde_json::to_string(&format!("printf locked > {}", index_lock.display()))
                    .expect("command json")
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = drain_until_command_exec_permission_request(
                &mut state,
                &writer,
                Duration::from_secs(2),
            );
            assert!(
                events.iter().any(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "fs-stream-1"
                }),
                "streaming command should initially start: {events:?}"
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .expect("permission request");
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(
                permission_request["permissions"]["fileSystem"]["write"][0],
                git_dir.display().to_string()
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
                git_dir.display()
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let starts = events
                .iter()
                .filter(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "fs-stream-1"
                })
                .count();
            assert_eq!(
                starts, 2,
                "same process id should restart after grant: {events:?}"
            );
            let completed = events
                .iter()
                .find(|event| {
                    event["event"] == "command_exec_completed"
                        && event["processId"] == "fs-stream-1"
                })
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
            assert_eq!(std::fs::read_to_string(&index_lock).unwrap(), "locked");
        });
    }

    #[test]
    fn command_exec_streaming_permission_profile_block_requests_permission_and_retries_process() {
        with_orca_home(|home| {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
            let port = listener.local_addr().expect("server addr").port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                while reader.read_line(&mut line).expect("read request") != 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 14\r\n\r\nstream-granted")
                    .expect("write response");
            });
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = format!(
                r#"{{"id":"cmd-stream","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc","curl --noproxy '' -sS http://127.0.0.1:{port}/"],"processId":"net-stream-1","streamStdoutStderr":true,"permissionProfile":"limited-network","timeoutMs":5000}}}}"#
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(
                events.iter().any(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "net-stream-1"
                }),
                "streaming command should initially start: {events:?}"
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .expect("permission request");
            let request_id = permission_request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "command_exec_completed"),
                "streaming command should wait for permission before completion: {events:?}"
            );

            let response = format!(
                r#"{{"id":"perm-allow","method":"permission/respond","params":{{"requestId":"{request_id}","decision":"allow","scope":"session","permissions":{{"network":{{"domains":{{"127.0.0.1":"allow"}}}}}}}}}}"#
            );
            handle_line(&server_config, &mut state, &response, Arc::clone(&writer))
                .expect("permission response");
            drain_command_exec_processes_with_timeout(
                &mut state,
                &mut *writer.lock().expect("writer"),
                Duration::from_secs(2),
            )
            .expect("drain retried process");
            server.join().expect("server joined");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let starts = events
                .iter()
                .filter(|event| {
                    event["event"] == "command_exec_started" && event["processId"] == "net-stream-1"
                })
                .count();
            assert_eq!(
                starts, 2,
                "same process id should restart after grant: {events:?}"
            );
            assert!(events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["processId"] == "net-stream-1"
                    && event["stream"] == "stdout"
                    && event["delta"]
                        .as_str()
                        .is_some_and(|delta| delta.contains("stream-granted"))
            }));
            let completed = events
                .iter()
                .find(|event| {
                    event["event"] == "command_exec_completed"
                        && event["processId"] == "net-stream-1"
                })
                .expect("command completed");
            assert_eq!(completed["exitCode"], 0);
        });
    }

    #[test]
    fn command_exec_streaming_permission_profile_delayed_block_requests_permission_on_next_drain() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.orca.invalid" = "allow"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = format!(
                r#"{{"id":"cmd-stream","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc","sleep 1.2; curl --noproxy '' -sS http://127.0.0.1:9/"],"processId":"net-stream-delayed","streamStdoutStderr":true,"permissionProfile":"limited-network","timeoutMs":5000}}}}"#
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "permission_request"),
                "delayed block should not be observed during initial drain: {events:?}"
            );

            let events = handle_thread_list_until_event(
                &server_config,
                &mut state,
                &writer,
                Duration::from_secs(3),
                |event| event["event"] == "permission_request",
            );
            let permission_request = events
                .iter()
                .find(|event| event["event"] == "permission_request")
                .unwrap_or_else(|| {
                    panic!("permission request after delayed process drain: {events:?}")
                });
            assert_eq!(
                permission_request["permissions"]["network"]["domains"]["127.0.0.1"],
                "allow"
            );
            assert!(
                events.iter().all(|event| {
                    !(event["event"] == "command_exec_completed"
                        && event["processId"] == "net-stream-delayed")
                }),
                "delayed block should request permission before completion: {events:?}"
            );
        });
    }

    #[test]
    fn command_exec_permission_profile_denylist_block_reports_policy_denial() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            let file_config: orca_core::config::file::FileConfig = toml::from_str(
                r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
            )
            .expect("domain policy config");
            config.permission_profiles = file_config.permission_profiles;
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let request = format!(
                r#"{{"id":"cmd-deny","method":"command/exec","params":{{"threadId":"{thread_id}","command":["sh","-lc","curl --noproxy '' -sS -D - -o /dev/null http://blocked.orca.invalid/ || true"],"permissionProfile":"limited-network","timeoutMs":5000}}}}"#
            );
            handle_line(&server_config, &mut state, &request, Arc::clone(&writer))
                .expect("command exec");

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            assert!(
                events
                    .iter()
                    .all(|event| event["event"] != "permission_request"),
                "denylist should not be escalated into a permission request: {events:?}"
            );
            let error = events
                .iter()
                .find(|event| event["event"] == "error")
                .expect("policy denial error");
            assert_eq!(
                error["message"],
                "command/exec network access to blocked.orca.invalid was denied by configured network policy"
            );
        });
    }

    #[test]
    fn command_exec_permission_profile_domain_policy_allows_http_request() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
        let port = listener.local_addr().expect("server addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read request") != 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\nallowed")
                .expect("write response");
        });
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"127.0.0.1" = "allow"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let request = format!(
            r#"{{"id":"cmd-allow","method":"command/exec","params":{{"command":["sh","-lc","curl --noproxy '' -sS http://127.0.0.1:{port}/"],"permissionProfile":"limited-network","timeoutMs":5000}}}}"#
        );
        let input = Cursor::new(request.into_bytes());
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        server.join().expect("server joined");
        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        assert_eq!(completed["stdout"], "allowed");
        assert_eq!(completed["exitCode"], 0);
    }

    #[test]
    fn command_exec_permission_profile_domain_policy_blocks_unallowlisted_local_request() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
        let port = listener.local_addr().expect("server addr").port();
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let request = format!(
            r#"{{"id":"cmd-local-deny","method":"command/exec","params":{{"command":["sh","-lc","curl --noproxy '' -sS -D - -o /dev/null http://127.0.0.1:{port}/ || true"],"permissionProfile":"limited-network","timeoutMs":5000}}}}"#
        );
        let input = Cursor::new(request.into_bytes());
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        drop(listener);
        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        assert!(
            completed["stdout"]
                .as_str()
                .expect("stdout")
                .contains("x-proxy-error: blocked-by-policy"),
            "stdout should include local-network policy block: {completed:?}"
        );
        assert_eq!(completed["exitCode"], 0);
    }

    #[test]
    fn command_exec_permission_profile_domain_policy_blocks_localhost_resolution() {
        let mut config = test_run_config();
        let file_config: orca_core::config::file::FileConfig = toml::from_str(
            r#"
[permission_profiles.limited-network]
extends = ":workspace"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"blocked.orca.invalid" = "deny"
"#,
        )
        .expect("domain policy config");
        config.permission_profiles = file_config.permission_profiles;
        let cwd = tempdir().expect("cwd");
        config.cwd = Some(cwd.path().to_path_buf());
        let request = br#"{"id":"cmd-localhost-deny","method":"command/exec","params":{"command":["sh","-lc","curl --noproxy '' -sS -D - -o /dev/null http://localhost/ || true"],"permissionProfile":"limited-network","timeoutMs":5000}}"#;
        let input = Cursor::new(request.to_vec());
        let output = SharedVecWriter::default();

        run_with_io(ServerConfig { run_config: config }, input, output.clone())
            .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let completed = events
            .iter()
            .find(|event| event["event"] == "command_exec_completed")
            .expect("command completed");
        assert!(
            completed["stdout"]
                .as_str()
                .expect("stdout")
                .contains("x-proxy-error: blocked-by-policy"),
            "stdout should include resolved localhost policy block: {completed:?}"
        );
        assert_eq!(completed["exitCode"], 0);
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_workspace_roots() {
        let mut config = test_run_config();
        let runtime_root = std::env::current_dir().unwrap().join("runtime-root");
        let docs = runtime_root.join("docs");
        config.permission_profiles.insert(
            "docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from(":workspace_roots/docs"),
                    orca_core::config::PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("docs".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            std::slice::from_ref(&runtime_root),
            None,
        )
        .expect("workspace roots profile");

        assert_eq!(sandbox.additional_writable_roots, vec![docs]);
    }

    #[test]
    fn command_exec_sandbox_collects_custom_permission_profile_read_roots() {
        let mut config = test_run_config();
        let readable = std::env::current_dir().unwrap().join("readable-root");
        config.permission_profiles.insert(
            "docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    readable.clone(),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("read roots profile");

        assert!(sandbox.additional_readable_roots.contains(&readable));
        assert_includes_platform_default_read_roots(&sandbox.additional_readable_roots);
        assert!(sandbox.additional_writable_roots.is_empty());
    }

    #[test]
    fn command_exec_custom_read_profile_uses_strict_read_roots() {
        let mut config = test_run_config();
        let readable = std::env::current_dir()
            .unwrap()
            .join("strict-readable-root");
        config.permission_profiles.insert(
            "strict-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    readable.clone(),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("strict-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("strict read profile");

        assert_eq!(
            sandbox.mode,
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false,
            }
        );
        assert!(sandbox.additional_readable_roots.contains(&readable));
        assert_includes_platform_default_read_roots(&sandbox.additional_readable_roots);
    }

    #[test]
    fn command_exec_sandbox_collects_custom_permission_profile_read_write_roots() {
        let mut config = test_run_config();
        let root = std::env::current_dir().unwrap().join("read-write-root");
        config.permission_profiles.insert(
            "docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    root.clone(),
                    orca_core::config::PermissionProfileFileAccess::ReadWrite,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("read-write roots profile");

        assert!(sandbox.additional_readable_roots.contains(&root));
        assert_includes_platform_default_read_roots(&sandbox.additional_readable_roots);
        assert_eq!(sandbox.additional_writable_roots, vec![root]);
    }

    #[test]
    fn command_exec_sandbox_expands_custom_permission_profile_deny_globs() {
        let temp = tempdir().expect("temp");
        let secret = temp.path().join("secret.env");
        let ordinary = temp.path().join("ordinary.txt");
        std::fs::write(&secret, "secret").expect("write secret");
        std::fs::write(&ordinary, "ordinary").expect("write ordinary");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "deny-env".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([
                    (
                        temp.path().to_path_buf(),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                    (
                        temp.path().join("*.env"),
                        orca_core::config::PermissionProfileFileAccess::Deny,
                    ),
                ])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("deny-env".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("deny glob profile");

        assert_eq!(sandbox.additional_writable_roots, vec![temp.path()]);
        assert!(sandbox.denied_writable_roots.contains(&secret));
        assert!(!sandbox.denied_writable_roots.contains(&ordinary));
        assert!(
            !sandbox
                .denied_writable_roots
                .contains(&temp.path().to_path_buf())
        );
    }

    #[test]
    fn command_exec_sandbox_expands_custom_permission_profile_write_globs() {
        let temp = tempdir().expect("temp");
        let writable = temp.path().join("allowed.txt");
        let ordinary = temp.path().join("ordinary.md");
        std::fs::write(&writable, "allowed").expect("write allowed");
        std::fs::write(&ordinary, "ordinary").expect("write ordinary");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "write-glob".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    temp.path().join("*.txt"),
                    orca_core::config::PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("write-glob".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("write glob profile");

        assert!(sandbox.additional_writable_roots.contains(&writable));
        assert!(!sandbox.additional_writable_roots.contains(&ordinary));
        assert!(
            !sandbox
                .additional_writable_roots
                .contains(&temp.path().to_path_buf())
        );
    }

    #[test]
    fn command_exec_sandbox_expands_custom_permission_profile_read_write_globs() {
        let temp = tempdir().expect("temp");
        let shared = temp.path().join("shared");
        let nested = shared.join("docs");
        let matched = nested.join("guide.md");
        let ignored = nested.join("image.png");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        std::fs::write(&matched, "guide").expect("write matched");
        std::fs::write(&ignored, "image").expect("write ignored");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "rw-glob".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    shared.join("**/*.md"),
                    orca_core::config::PermissionProfileFileAccess::ReadWrite,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("rw-glob".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("read-write glob profile");

        assert!(sandbox.additional_readable_roots.contains(&matched));
        assert!(sandbox.additional_writable_roots.contains(&matched));
        assert!(!sandbox.additional_readable_roots.contains(&ignored));
        assert!(!sandbox.additional_writable_roots.contains(&ignored));
    }

    #[test]
    fn command_exec_sandbox_respects_permission_profile_glob_scan_max_depth() {
        let temp = tempdir().expect("temp");
        let shallow = temp.path().join("docs");
        let deep = shallow.join("nested");
        let shallow_match = shallow.join("guide.md");
        let deep_match = deep.join("hidden.md");
        std::fs::create_dir_all(&deep).expect("mkdir nested");
        std::fs::write(&shallow_match, "guide").expect("write shallow");
        std::fs::write(&deep_match, "hidden").expect("write deep");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "shallow-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(2),
                    std::collections::HashMap::from([(
                        temp.path().join("**/*.md"),
                        orca_core::config::PermissionProfileFileAccess::Read,
                    )]),
                ),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("shallow-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("shallow glob profile");

        assert!(sandbox.additional_readable_roots.contains(&shallow_match));
        assert!(!sandbox.additional_readable_roots.contains(&deep_match));
    }

    #[test]
    fn command_exec_sandbox_inherits_permission_profile_glob_scan_max_depth() {
        let temp = tempdir().expect("temp");
        let shallow = temp.path().join("docs");
        let deep = shallow.join("nested");
        let shallow_match = shallow.join("guide.md");
        let deep_match = deep.join("hidden.md");
        std::fs::create_dir_all(&deep).expect("mkdir nested");
        std::fs::write(&shallow_match, "guide").expect("write shallow");
        std::fs::write(&deep_match, "hidden").expect("write deep");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "base-depth".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(2),
                    Default::default(),
                ),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "child-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("base-depth".to_string()),
                filesystem: std::collections::HashMap::from([(
                    temp.path().join("**/*.md"),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("child-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("inherited depth profile");

        assert!(sandbox.additional_readable_roots.contains(&shallow_match));
        assert!(!sandbox.additional_readable_roots.contains(&deep_match));
    }

    #[test]
    fn command_exec_sandbox_overrides_inherited_permission_profile_glob_scan_max_depth() {
        let temp = tempdir().expect("temp");
        let shallow = temp.path().join("docs");
        let deep = shallow.join("nested");
        let shallow_match = shallow.join("guide.md");
        let deep_match = deep.join("hidden.md");
        std::fs::create_dir_all(&deep).expect("mkdir nested");
        std::fs::write(&shallow_match, "guide").expect("write shallow");
        std::fs::write(&deep_match, "hidden").expect("write deep");
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "base-depth".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(2),
                    Default::default(),
                ),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "child-docs".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("base-depth".to_string()),
                filesystem: orca_core::config::PermissionProfileFilesystemConfig::from_parts(
                    Some(4),
                    std::collections::HashMap::from([(
                        temp.path().join("**/*.md"),
                        orca_core::config::PermissionProfileFileAccess::Read,
                    )]),
                ),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("child-docs".to_string()),
            ..Default::default()
        };

        let sandbox = test_profile_sandbox(&config, &options).expect("overridden depth profile");

        assert!(sandbox.additional_readable_roots.contains(&shallow_match));
        assert!(sandbox.additional_readable_roots.contains(&deep_match));
    }

    #[test]
    fn command_exec_sandbox_rejects_broad_custom_permission_profile_deny_globs() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "broad-glob".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from("/*.env"),
                    orca_core::config::PermissionProfileFileAccess::Deny,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("broad-glob".to_string()),
            ..Default::default()
        };

        let error = test_profile_sandbox(&config, &options).expect_err("broad glob error");

        assert_eq!(
            error,
            "command/exec permissionProfile filesystem glob is too broad to scan safely: /*.env"
        );
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_special_tmp_roots() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "tmp".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([
                    (
                        PathBuf::from(":slash_tmp"),
                        orca_core::config::PermissionProfileFileAccess::Write,
                    ),
                    (
                        PathBuf::from(":tmpdir"),
                        orca_core::config::PermissionProfileFileAccess::Deny,
                    ),
                ])
                .into(),
                ..Default::default()
            },
        );
        let tmpdir = std::env::temp_dir().join("orca-special-tmpdir");
        let options = protocol::CommandExecOptions {
            permission_profile: Some("tmp".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            Some(&tmpdir),
        )
        .expect("special tmp profile");

        assert_eq!(
            sandbox.additional_writable_roots,
            vec![PathBuf::from("/tmp")]
        );
        assert_eq!(sandbox.denied_writable_roots, vec![tmpdir]);
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_root_path() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "root".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from(":root"),
                    orca_core::config::PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("root".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            None,
        )
        .expect("root profile");

        assert_eq!(sandbox.additional_writable_roots, vec![PathBuf::from("/")]);
    }

    #[test]
    fn command_exec_sandbox_materializes_custom_permission_profile_minimal_path() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "minimal".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some(":read-only".to_string()),
                filesystem: std::collections::HashMap::from([(
                    PathBuf::from(":minimal"),
                    orca_core::config::PermissionProfileFileAccess::Read,
                )])
                .into(),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("minimal".to_string()),
            ..Default::default()
        };

        let sandbox = command_exec_sandbox_mode(
            &config,
            &options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            None,
        )
        .expect("minimal profile");

        assert_eq!(
            sandbox.additional_readable_roots,
            orca_tools::sandbox::platform_default_read_roots()
        );
    }

    fn assert_includes_platform_default_read_roots(actual_roots: &[PathBuf]) {
        for root in orca_tools::sandbox::platform_default_read_roots() {
            assert!(
                actual_roots.contains(&root),
                "missing platform default read root: {root:?}"
            );
        }
    }

    #[test]
    fn command_exec_sandbox_rejects_custom_permission_profile_cycle() {
        let mut config = test_run_config();
        config.permission_profiles.insert(
            "a".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("b".to_string()),
                ..Default::default()
            },
        );
        config.permission_profiles.insert(
            "b".to_string(),
            orca_core::config::PermissionProfileConfig {
                extends: Some("a".to_string()),
                ..Default::default()
            },
        );
        let options = protocol::CommandExecOptions {
            permission_profile: Some("a".to_string()),
            ..Default::default()
        };

        let error = test_profile_sandbox(&config, &options).expect_err("cycle error");

        assert_eq!(error, "command/exec permissionProfile cycle: a -> b -> a");
    }

    fn test_profile_sandbox(
        config: &RunConfig,
        options: &protocol::CommandExecOptions,
    ) -> Result<CommandExecSandbox, String> {
        command_exec_sandbox_mode(
            config,
            options,
            None,
            std::path::Path::new("/workspace"),
            &[],
            None,
        )
    }

    fn command_exec_process(shell_id: &str) -> CommandExecProcess {
        CommandExecProcess {
            shell_id: Some(shell_id.to_string()),
            command_event_id: Value::from("cmd"),
            command: vec!["sh".to_string(), "-lc".to_string(), "true".to_string()],
            cwd: PathBuf::from("/tmp"),
            denied_writable_roots: Vec::new(),
            stream_output: false,
            output_bytes_cap: None,
            output_offset: 0,
            stdout_len: 0,
            stderr_len: 0,
            stdout_cap_reached: false,
            stderr_cap_reached: false,
            network_permission_blocks: None,
            permission_request: None,
            _network_proxy: None,
        }
    }

    #[test]
    fn server_writer_streams_mcp_tool_call_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"mcp-1","name":"mcp__local__search","target":"{\"query\":\"orca\"}","raw_arguments":"{\"query\":\"orca\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"mcp-1","name":"mcp__local__search","status":"completed","output":"{\"content\":[{\"type\":\"text\",\"text\":\"found\"}],\"structuredContent\":{\"count\":1},\"_meta\":{\"source\":\"test\"}}","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "mcpToolCall"
                    && event["item"]["id"] == "mcp-1"
            })
            .expect("mcp item_started");
        assert_eq!(started["item"]["server"], "local");
        assert_eq!(started["item"]["tool"], "search");
        assert_eq!(started["item"]["status"], "in_progress");
        assert_eq!(started["item"]["arguments"]["query"], "orca");

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "mcpToolCall"
                    && event["item"]["id"] == "mcp-1"
            })
            .expect("mcp item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["server"], "local");
        assert_eq!(completed["item"]["tool"], "search");
        assert_eq!(completed["item"]["result"]["content"][0]["text"], "found");
        assert_eq!(completed["item"]["result"]["structuredContent"]["count"], 1);
        assert_eq!(completed["item"]["result"]["_meta"]["source"], "test");
        assert!(completed["item"]["error"].is_null());
    }

    #[test]
    fn server_writer_streams_failed_mcp_tool_exit_code_in_item_error() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"mcp-1","name":"mcp__local__search","target":"{\"query\":\"orca\"}","raw_arguments":"{\"query\":\"orca\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"mcp-1","name":"mcp__local__search","status":"failed","error":"MCP request timed out","exit_code":124}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "mcpToolCall"
                    && event["item"]["id"] == "mcp-1"
            })
            .expect("mcp item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert!(completed["item"]["result"].is_null());
        assert_eq!(
            completed["item"]["error"]["message"],
            "MCP request timed out"
        );
        assert_eq!(completed["item"]["error"]["exitCode"], 124);
    }

    #[test]
    fn server_writer_streams_external_tool_as_dynamic_tool_call_item() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"external-1","name":"deploy","target":"{\"env\":\"staging\"}","raw_arguments":"{\"env\":\"staging\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"external-1","name":"deploy","status":"completed","output":"deployed staging","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-1"
            })
            .expect("external item_started");
        assert!(started["item"]["namespace"].is_null());
        assert_eq!(started["item"]["tool"], "deploy");
        assert_eq!(started["item"]["status"], "in_progress");
        assert_eq!(started["item"]["arguments"]["env"], "staging");

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-1"
            })
            .expect("external item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["success"], true);
        assert_eq!(completed["item"]["contentItems"][0]["type"], "text");
        assert_eq!(
            completed["item"]["contentItems"][0]["text"],
            "deployed staging"
        );
        assert!(completed["item"]["error"].is_null());
    }

    #[test]
    fn server_writer_streams_denied_external_tool_as_failed_dynamic_item() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"external-denied-1","name":"deploy","target":"{\"env\":\"production\"}","raw_arguments":"{\"env\":\"production\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"external-denied-1","name":"deploy","status":"denied","output":"policy denied deploy"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-denied-1"
            })
            .expect("external item_completed");
        assert_eq!(completed["item"]["status"], "denied");
        assert_eq!(completed["item"]["success"], false);
        assert!(completed["item"]["contentItems"].is_null());
        assert_eq!(
            completed["item"]["error"]["message"],
            "policy denied deploy"
        );
    }

    #[test]
    fn server_writer_streams_failed_external_tool_exit_code_in_dynamic_item_error() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"external-1","name":"deploy","target":"{\"env\":\"staging\"}","raw_arguments":"{\"env\":\"staging\"}"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"external-1","name":"deploy","status":"failed","error":"deploy failed","exit_code":42}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "external-1"
            })
            .expect("external item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert_eq!(completed["item"]["success"], false);
        assert!(completed["item"]["contentItems"].is_null());
        assert_eq!(completed["item"]["error"]["message"], "deploy failed");
        assert_eq!(completed["item"]["error"]["exitCode"], 42);
    }

    #[test]
    fn server_writer_streams_file_change_item_lifecycle_for_edit() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"edit-1","name":"edit","target":"note.txt :: hello => hi"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"edit-1","name":"edit","status":"completed","output":"edited note.txt","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_started");
        assert_eq!(started["item"]["status"], "inProgress");
        assert!(started["item"].get("tool").is_none());
        assert_eq!(started["item"]["changes"][0]["path"], "note.txt");
        assert_eq!(started["item"]["changes"][0]["kind"], "edit");
        assert!(started["item"]["changes"][0]["diff"].as_str().is_some());

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_completed" && event["tool"] == "edit")
        );
    }

    #[test]
    fn server_writer_streams_failed_file_change_item_lifecycle_for_edit() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"edit-1","name":"edit","target":"note.txt :: hello => hi"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"edit-1","name":"edit","status":"failed","error":"edit old text was not found","exit_code":1}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
        assert_eq!(completed["item"]["changes"][0]["kind"], "edit");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "tool_completed" && event["tool"] == "edit")
        );
    }

    #[test]
    fn server_writer_streams_failed_file_change_output_as_error_detail() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"edit-1","name":"edit","target":"note.txt :: hello => hi"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"edit-1","name":"edit","status":"failed","output":"edit old text was not found","exit_code":1}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "edit-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "failed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
        assert_eq!(completed["item"]["changes"][0]["kind"], "edit");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
    }

    #[test]
    fn server_writer_streams_file_change_item_lifecycle_for_write_file() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"tool.call.requested","payload":{"id":"write-1","name":"write_file","target":"new.txt"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"tool.call.completed","payload":{"id":"write-1","name":"write_file","status":"completed","output":"wrote 3 bytes to new.txt","exit_code":0}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "write-1:file-change"
            })
            .expect("file_change item_started");
        assert!(started["item"].get("tool").is_none());
        assert_eq!(started["item"]["status"], "inProgress");
        assert_eq!(started["item"]["changes"][0]["path"], "new.txt");
        assert_eq!(started["item"]["changes"][0]["kind"], "write");
        assert!(started["item"]["changes"][0]["diff"].as_str().is_some());

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "fileChange"
                    && event["item"]["id"] == "write-1:file-change"
            })
            .expect("file_change item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert!(completed["item"].get("output").is_none());
        assert!(completed["item"].get("error").is_none());
        assert!(completed["item"].get("tool").is_none());
        assert_eq!(completed["item"]["changes"][0]["path"], "new.txt");
        assert_eq!(completed["item"]["changes"][0]["kind"], "write");
        assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
    }

    #[test]
    fn server_writer_streams_workflow_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"kind":"workflow","status":"running"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"workflow.result.available","payload":{"taskId":"task-1","runId":"workflow-run-1","result":"done","task":{"kind":"workflow","status":"running"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"workflow.completed","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"kind":"workflow","status":"completed"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "workflow"
                    && event["item"]["id"] == "workflow-run-1"
            })
            .expect("workflow item_started");
        assert_eq!(started["item"]["workflowName"], "audit");
        assert_eq!(started["item"]["taskId"], "task-1");
        assert_eq!(started["item"]["status"], "running");

        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_started")
        );
        assert!(events.iter().any(|event| {
            event["event"] == "workflow_result_available" && event["result"] == "done"
        }));

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "workflow"
                    && event["item"]["id"] == "workflow-run-1"
            })
            .expect("workflow item_completed");
        assert_eq!(completed["item"]["workflowName"], "audit");
        assert_eq!(completed["item"]["taskId"], "task-1");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["result"], "done");
    }

    #[test]
    fn server_writer_streams_failed_workflow_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"kind":"workflow","status":"running"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(
                    br#"{"type":"workflow.failed","payload":{"taskId":"task-1","runId":"workflow-run-1","error":"boom","task":{"kind":"workflow","status":"failed"}}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "workflow"
                    && event["item"]["id"] == "workflow-run-1"
            })
            .expect("workflow item_completed");
        assert_eq!(completed["item"]["workflowName"], "audit");
        assert_eq!(completed["item"]["status"], "failed");
        assert_eq!(completed["item"]["error"], "boom");
        assert!(completed["item"]["result"].is_null());
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_failed" && event["error"] == "boom")
        );
    }

    #[test]
    fn server_writer_streams_reasoning_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(br#"{"type":"assistant.reasoning.delta","payload":{"text":"thinking"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(br#"{"type":"session.completed","payload":{"status":"completed"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "reasoning"
                    && event["item"]["id"] == "item-reasoning-1"
            })
            .expect("reasoning item_started");
        assert_eq!(started["item"]["summary"], "");
        assert_eq!(started["item"]["content"], "");

        assert!(events.iter().any(|event| {
            event["event"] == "item_reasoning_delta"
                && event["itemId"] == "item-reasoning-1"
                && event["delta"] == "thinking"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "reasoning_delta" && event["text"] == "thinking")
        );

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "reasoning"
                    && event["item"]["id"] == "item-reasoning-1"
            })
            .expect("reasoning item_completed");
        assert_eq!(completed["item"]["summary"], "thinking");
        assert_eq!(completed["item"]["content"], "");
    }

    #[test]
    fn server_writer_streams_proposed_plan_item_lifecycle() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"assistant.message.delta","payload":{"text":"Preface\n<proposed_plan>\n# Final plan\n- first\n- second\n</proposed_plan>\nPostscript"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(br#"{"type":"session.completed","payload":{"status":"completed"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let plan_started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "plan"
                    && event["item"]["id"] == "item-plan-1"
            })
            .expect("plan item_started");
        assert_eq!(plan_started["item"]["text"], "");

        let plan_delta = events
            .iter()
            .find(|event| event["event"] == "item_plan_delta")
            .expect("plan delta");
        assert_eq!(plan_delta["itemId"], "item-plan-1");
        assert_eq!(plan_delta["delta"], "# Final plan\n- first\n- second\n");

        let plan_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "plan"
                    && event["item"]["id"] == "item-plan-1"
            })
            .expect("plan item_completed");
        assert_eq!(
            plan_completed["item"]["text"],
            "# Final plan\n- first\n- second\n"
        );

        let message_delta_text = events
            .iter()
            .filter(|event| event["event"] == "item_message_delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<String>();
        assert_eq!(message_delta_text, "Preface\n\nPostscript");

        let agent_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
            })
            .expect("agent message item_completed");
        assert_eq!(agent_completed["item"]["text"], "Preface\n\nPostscript");
    }

    #[test]
    fn server_writer_parses_proposed_plan_tag_split_across_deltas() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(
                    br#"{"type":"assistant.message.delta","payload":{"text":"Intro\n<proposed"}}"#,
                )
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(br#"{"type":"assistant.message.delta","payload":{"text":"_plan>\n- Step 1\n</proposed_plan>\nOutro"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(br#"{"type":"session.completed","payload":{"status":"completed"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        let plan_delta = events
            .iter()
            .find(|event| event["event"] == "item_plan_delta")
            .expect("plan delta");
        assert_eq!(plan_delta["delta"], "- Step 1\n");

        let message_delta_text = events
            .iter()
            .filter(|event| event["event"] == "item_message_delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<String>();
        assert_eq!(message_delta_text, "Intro\n\nOutro");

        let agent_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
            })
            .expect("agent message item_completed");
        assert_eq!(agent_completed["item"]["text"], "Intro\n\nOutro");
    }

    #[test]
    fn server_writer_leaves_incomplete_proposed_plan_tag_as_agent_message() {
        let mut output = Vec::new();
        {
            let mut writer = ServerRequestWriter::new(Value::from("turn"), &mut output);
            writer
                .write_all(br#"{"type":"assistant.message.delta","payload":{"text":"Intro\n<proposed_plan> not a complete block"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
            writer
                .write_all(br#"{"type":"session.completed","payload":{"status":"completed"}}"#)
                .unwrap();
            writer.write_all(b"\n").unwrap();
        }

        let events = parse_jsonl(&output);
        assert!(
            !events
                .iter()
                .any(|event| event["event"] == "item_started" && event["item"]["type"] == "plan")
        );
        let agent_completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
            })
            .expect("agent message item_completed");
        assert_eq!(
            agent_completed["item"]["text"],
            "Intro\n<proposed_plan> not a complete block"
        );
    }

    #[test]
    fn workflow_submit_streams_background_result() {
        let input = Cursor::new(br#"{"id":7,"op":"submit","prompt":"workflow inline"}"#.to_vec());
        let output = SharedVecWriter::default();

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            output.clone(),
        )
        .expect("server run");

        let events = parse_jsonl(&output.bytes());
        assert!(events.iter().all(|event| event["id"] == 7));
        assert!(events.iter().any(|event| {
            event["event"] == "tool_completed"
                && event["tool"] == "Workflow"
                && event["status"] == "completed"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_started")
        );
        let workflow_started = events
            .iter()
            .find(|event| event["event"] == "workflow_started")
            .expect("workflow started event");
        assert_eq!(workflow_started["task"]["kind"], "workflow");
        assert_eq!(workflow_started["task"]["status"], "running");
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_result_available")
        );
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_completed")
        );
        assert!(events.iter().any(|event| {
            event["event"] == "item_completed" && event["item"]["type"] == "workflow"
        }));
    }

    #[test]
    fn submit_turn_started_event_preserves_task_lifecycle_metadata() {
        let input = Cursor::new(br#"{"id":7,"op":"submit","prompt":"reply once"}"#.to_vec());
        let output = SharedVecWriter::default();

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            output.clone(),
        )
        .expect("server run");

        let events = parse_jsonl(&output.bytes());
        let turn_started = events
            .iter()
            .find(|event| event["event"] == "turn_started")
            .expect("turn started event");

        assert_eq!(turn_started["turn"], 1);
        assert_eq!(turn_started["task"]["kind"], "agent");
        assert_eq!(turn_started["task"]["status"], "running");
        assert_eq!(turn_started["task"]["turn"], 1);
        assert!(
            turn_started["task"]["task_id"]
                .as_str()
                .unwrap()
                .contains(":task-1")
        );
    }

    #[test]
    fn thread_start_materializes_recorded_history_when_enabled() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");

            let events = parse_jsonl(&output);
            let thread_id = events
                .iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str())
                .expect("thread id")
                .to_string();

            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"persist this server thread"}}]}}}}"#
                ),
                &mut output,
            )
            .expect("thread turn");

            let store = crate::thread_store::SessionStore::new();
            let transcript = store.load_session("latest").expect("latest transcript");
            assert_eq!(transcript.meta.session_id, thread_id);
            assert!(transcript.messages.iter().any(|message| {
                matches!(message, Message::User { content, .. } if content == "persist this server thread")
            }));
            assert!(transcript.messages.iter().any(|message| {
                matches!(message, Message::Assistant { content: Some(content), .. } if content == "Mock runtime completed the headless harness contract.")
            }));
        });
    }

    #[test]
    fn thread_read_returns_persisted_thread_projection() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"readable server thread"}}]}}}}"#
                ),
                &mut output,
            )
            .expect("thread turn");

            let mut read_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}","includeMessages":true}}}}"#
                ),
                &mut read_output,
            )
            .expect("thread read");

            let events = parse_jsonl(&read_output);
            assert_eq!(events.len(), 1);
            let read = &events[0];
            assert_eq!(read["id"], "read");
            assert_eq!(read["event"], "thread_read");
            assert_eq!(read["threadId"], thread_id);
            let messages = read["messages"].as_array().expect("messages");
            assert_eq!(read["messageCount"], messages.len());
            assert!(messages.iter().any(|message| {
                message["role"] == "user" && message["content"] == "readable server thread"
            }));
            assert!(
                messages
                    .iter()
                    .any(|message| message["role"] == "assistant")
            );

            let mut turns_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read-turns","method":"thread/read","params":{{"threadId":"{thread_id}","includeTurns":true}}}}"#
                ),
                &mut turns_output,
            )
            .expect("thread read with turns");

            let turn_events = parse_jsonl(&turns_output);
            assert_eq!(turn_events.len(), 1);
            assert_eq!(turn_events[0]["event"], "thread_read");
            let turns = turn_events[0]["turns"].as_array().expect("turns");
            assert!(turns.iter().any(|turn| {
                turn["items"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["role"] == "user" && item["content"] == "readable server thread"
                    }) && items.iter().any(|item| item["role"] == "assistant")
                })
            }));
        });
    }

    #[test]
    fn thread_read_returns_in_memory_thread_projection() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Disabled;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"readable memory thread"}}]}}}}"#
                ),
                &mut output,
            )
            .expect("thread turn");

            let mut read_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}","includeMessages":true}}}}"#
                ),
                &mut read_output,
            )
            .expect("thread read");

            let events = parse_jsonl(&read_output);
            assert_eq!(events.len(), 1);
            let read = &events[0];
            assert_eq!(read["event"], "thread_read");
            assert_eq!(read["threadId"], thread_id);
            let messages = read["messages"].as_array().expect("messages");
            assert!(messages.iter().any(|message| {
                message["role"] == "user" && message["content"] == "readable memory thread"
            }));
        });
    }

    #[test]
    fn completed_background_turn_is_reclaimed_before_next_thread_turn() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Disabled;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let writer = Arc::new(Mutex::new(Vec::new()));
            let first = format!(
                r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"first prompt"}}]}}}}"#
            );
            handle_line(&server_config, &mut state, &first, Arc::clone(&writer))
                .expect("first turn");
            loop {
                let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
                if events
                    .iter()
                    .any(|event| event["id"] == "turn-1" && event["event"] == "turn_completed")
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            let second = format!(
                r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#
            );
            handle_line(&server_config, &mut state, &second, Arc::clone(&writer))
                .expect("second turn");
            state.join_active_turns();
            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let echoed = events
                .iter()
                .filter(|event| event["id"] == "turn-2" && event["event"] == "message_delta")
                .filter_map(|event| event["text"].as_str())
                .collect::<String>();

            assert!(
                echoed.contains("first prompt | mock_history_echo"),
                "expected second turn to see prior thread history, got: {echoed}"
            );
            assert!(
                !events.iter().any(|event| {
                    event["id"] == "turn-2"
                        && event["event"] == "error"
                        && event["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("unknown thread"))
                }),
                "second turn must not race with thread reclamation"
            );
        });
    }

    #[test]
    fn thread_metadata_update_changes_read_title() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.history_mode = HistoryMode::Record;
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                &mut output,
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&output)
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            let mut metadata_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"rename","method":"thread/metadata/update","params":{{"threadId":"{thread_id}","title":"renamed from server"}}}}"#
                ),
                &mut metadata_output,
            )
            .expect("metadata update");
            let metadata_events = parse_jsonl(&metadata_output);
            assert_eq!(metadata_events.len(), 1);
            assert_eq!(metadata_events[0]["event"], "thread_metadata_updated");
            assert_eq!(metadata_events[0]["threadId"], thread_id);
            assert_eq!(metadata_events[0]["title"], "renamed from server");

            let mut read_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{thread_id}"}}}}"#
                ),
                &mut read_output,
            )
            .expect("thread read");

            let read_events = parse_jsonl(&read_output);
            assert_eq!(read_events.len(), 1);
            assert_eq!(read_events[0]["event"], "thread_read");
            assert_eq!(read_events[0]["title"], "renamed from server");
        });
    }

    #[test]
    fn thread_list_returns_persisted_thread_summaries() {
        with_orca_home(|home| {
            let store = SessionStore::new();
            let mut first = store
                .create_live_thread(home, "mock", None, "first listed thread")
                .expect("create first thread");
            first.complete("success").expect("complete first");
            let mut second = store
                .create_live_thread(home, "mock", None, "second listed thread")
                .expect("create second thread");
            second.complete("success").expect("complete second");

            let server_config = ServerConfig {
                run_config: test_run_config(),
            };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"list","method":"thread/list","params":{"limit":1}}"#,
                &mut output,
            )
            .expect("thread list");

            let events = parse_jsonl(&output);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["event"], "thread_list");
            let data = events[0]["data"].as_array().expect("thread list data");
            assert_eq!(data.len(), 1);
            let first_page_title = data[0]["title"].as_str().expect("thread title");
            assert!(matches!(
                first_page_title,
                "first listed thread" | "second listed thread"
            ));
            assert_eq!(data[0]["cwd"], home.display().to_string());
            assert_eq!(events[0]["nextCursor"], "1");
            assert_eq!(events[0]["backwardsCursor"], "0");

            let mut page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"list-page","method":"thread/list","params":{"cursor":"1","limit":1}}"#,
                &mut page_output,
            )
            .expect("thread list page");

            let page_events = parse_jsonl(&page_output);
            assert_eq!(page_events.len(), 1);
            assert_eq!(page_events[0]["event"], "thread_list");
            let page_data = page_events[0]["data"]
                .as_array()
                .expect("thread list page data");
            assert_eq!(page_data.len(), 1);
            let second_page_title = page_data[0]["title"].as_str().expect("thread title");
            assert!(matches!(
                second_page_title,
                "first listed thread" | "second listed thread"
            ));
            assert_ne!(first_page_title, second_page_title);
            assert_eq!(page_events[0]["nextCursor"], Value::Null);
            assert_eq!(page_events[0]["backwardsCursor"], "1");

            let mut filtered_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"list-filtered","method":"thread/list","params":{"searchTerm":"second listed","limit":10}}"#,
                &mut filtered_output,
            )
            .expect("filtered thread list");

            let filtered_events = parse_jsonl(&filtered_output);
            assert_eq!(filtered_events.len(), 1);
            assert_eq!(filtered_events[0]["event"], "thread_list");
            let filtered_data = filtered_events[0]["data"]
                .as_array()
                .expect("filtered thread list data");
            assert_eq!(filtered_data.len(), 1);
            assert_eq!(filtered_data[0]["title"], "second listed thread");
            assert_eq!(filtered_events[0]["nextCursor"], Value::Null);
        });
    }

    #[test]
    fn thread_search_returns_persisted_hits() {
        with_orca_home(|home| {
            let store = SessionStore::new();
            let mut thread = store
                .create_live_thread(home, "mock", None, "searchable thread")
                .expect("create thread");
            let thread_id = thread.thread_id().to_string();
            thread
                .append_items(&[Message::User {
                    content: "needle appears in this transcript".to_string(),
                    pinned: false,
                }])
                .expect("append search message");
            thread.complete("success").expect("complete thread");
            let mut second = store
                .create_live_thread(home, "mock", None, "searchable thread second")
                .expect("create second thread");
            let second_id = second.thread_id().to_string();
            second
                .append_items(&[Message::User {
                    content: "needle appears again".to_string(),
                    pinned: false,
                }])
                .expect("append second search message");
            second.complete("success").expect("complete second thread");

            let server_config = ServerConfig {
                run_config: test_run_config(),
            };
            let mut state = ServerState::default();
            let mut output = Vec::new();

            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"search","method":"thread/search","params":{"searchTerm":"needle","limit":1}}"#,
                &mut output,
            )
            .expect("thread search");

            let events = parse_jsonl(&output);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["event"], "thread_search");
            let data = events[0]["data"].as_array().expect("thread search data");
            assert_eq!(data.len(), 1);
            let first_hit_id = data[0]["thread"]["threadId"]
                .as_str()
                .expect("thread id")
                .to_string();
            assert!(first_hit_id == thread_id || first_hit_id == second_id);
            assert!(
                data[0]["snippet"]
                    .as_str()
                    .is_some_and(|snippet| snippet.contains("needle"))
            );
            assert_eq!(events[0]["nextCursor"], "1");

            let mut page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                r#"{"id":"search-page","method":"thread/search","params":{"searchTerm":"needle","cursor":"1","limit":1}}"#,
                &mut page_output,
            )
            .expect("thread search page");

            let page_events = parse_jsonl(&page_output);
            assert_eq!(page_events.len(), 1);
            assert_eq!(page_events[0]["event"], "thread_search");
            let page_data = page_events[0]["data"]
                .as_array()
                .expect("thread search page data");
            assert_eq!(page_data.len(), 1);
            let second_hit_id = page_data[0]["thread"]["threadId"]
                .as_str()
                .expect("thread id")
                .to_string();
            assert!(second_hit_id == thread_id || second_hit_id == second_id);
            assert_ne!(first_hit_id, second_hit_id);
            assert_eq!(page_events[0]["nextCursor"], Value::Null);
            assert_eq!(page_events[0]["backwardsCursor"], "1");
        });
    }

    #[test]
    fn thread_turns_and_items_list_return_persisted_projection() {
        with_orca_home(|home| {
            let store = SessionStore::new();
            let mut thread = store
                .create_live_thread(home, "mock", None, "projected server thread")
                .expect("create thread");
            let thread_id = thread.thread_id().to_string();
            thread
                .append_items(&[
                    Message::User {
                        content: "server projected user".to_string(),
                        pinned: false,
                    },
                    Message::Assistant {
                        content: Some("server projected assistant".to_string()),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        pinned: false,
                    },
                    Message::User {
                        content: "server projected second user".to_string(),
                        pinned: false,
                    },
                    Message::Assistant {
                        content: Some("server projected second assistant".to_string()),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                        pinned: false,
                    },
                ])
                .expect("append projection messages");
            thread.complete("success").expect("complete thread");

            let server_config = ServerConfig {
                run_config: test_run_config(),
            };
            let mut state = ServerState::default();
            let mut turns_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":10}}}}"#
                ),
                &mut turns_output,
            )
            .expect("thread turns list");

            let turn_events = parse_jsonl(&turns_output);
            assert_eq!(turn_events.len(), 1);
            assert_eq!(turn_events[0]["event"], "thread_turns_list");
            let turns = turn_events[0]["data"].as_array().expect("turn data");
            assert_eq!(turns.len(), 2);
            assert_eq!(turns[0]["turnId"], "turn-1");
            assert_eq!(turns[0]["role"], "user");
            assert_eq!(turns[0]["itemsView"], "full");
            assert_eq!(turns[0]["items"][0]["content"], "server projected user");
            assert_eq!(
                turns[0]["items"][1]["content"],
                "server projected assistant"
            );
            assert_eq!(turn_events[0]["nextCursor"], Value::Null);
            assert_eq!(turn_events[0]["backwardsCursor"], "0");

            let mut second_turn_page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn-page","method":"thread/turns/list","params":{{"threadId":"{thread_id}","cursor":"1","limit":1}}}}"#
                ),
                &mut second_turn_page_output,
            )
            .expect("second thread turns page");

            let second_turn_page_events = parse_jsonl(&second_turn_page_output);
            assert_eq!(second_turn_page_events.len(), 1);
            assert_eq!(second_turn_page_events[0]["event"], "thread_turns_list");
            let page_turns = second_turn_page_events[0]["data"]
                .as_array()
                .expect("paged turn data");
            assert_eq!(page_turns.len(), 1);
            assert_eq!(page_turns[0]["turnId"], "turn-2");
            assert_eq!(
                page_turns[0]["items"][0]["content"],
                "server projected second user"
            );
            assert_eq!(second_turn_page_events[0]["nextCursor"], Value::Null);
            assert_eq!(second_turn_page_events[0]["backwardsCursor"], "1");

            let mut latest_turn_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn-desc","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":1,"sortDirection":"desc"}}}}"#
                ),
                &mut latest_turn_output,
            )
            .expect("latest thread turns page");

            let latest_turn_events = parse_jsonl(&latest_turn_output);
            assert_eq!(latest_turn_events.len(), 1);
            assert_eq!(latest_turn_events[0]["event"], "thread_turns_list");
            let latest_turns = latest_turn_events[0]["data"]
                .as_array()
                .expect("latest turn data");
            assert_eq!(latest_turns.len(), 1);
            assert_eq!(latest_turns[0]["turnId"], "turn-2");
            assert_eq!(
                latest_turns[0]["items"][1]["content"],
                "server projected second assistant"
            );
            assert_eq!(latest_turn_events[0]["nextCursor"], "1");

            let mut unloaded_turn_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn-unloaded","method":"thread/turns/list","params":{{"threadId":"{thread_id}","limit":1,"itemsView":"notLoaded"}}}}"#
                ),
                &mut unloaded_turn_output,
            )
            .expect("unloaded thread turns page");

            let unloaded_turn_events = parse_jsonl(&unloaded_turn_output);
            assert_eq!(unloaded_turn_events.len(), 1);
            assert_eq!(unloaded_turn_events[0]["event"], "thread_turns_list");
            let unloaded_turns = unloaded_turn_events[0]["data"]
                .as_array()
                .expect("unloaded turn data");
            assert_eq!(unloaded_turns.len(), 1);
            assert_eq!(unloaded_turns[0]["turnId"], "turn-1");
            assert_eq!(unloaded_turns[0]["itemsView"], "notLoaded");
            assert_eq!(
                unloaded_turns[0]["items"].as_array().expect("items").len(),
                0
            );

            let mut items_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{thread_id}","turnId":"turn-1","limit":10}}}}"#
                ),
                &mut items_output,
            )
            .expect("thread items list");

            let item_events = parse_jsonl(&items_output);
            assert_eq!(item_events.len(), 1);
            assert_eq!(item_events[0]["event"], "thread_items_list");
            let items = item_events[0]["data"].as_array().expect("item data");
            assert_eq!(items.len(), 2);
            assert_eq!(items[1]["itemId"], "item-2");
            assert_eq!(items[1]["turnId"], "turn-1");
            assert_eq!(items[1]["item"]["content"], "server projected assistant");
            assert_eq!(item_events[0]["nextCursor"], Value::Null);
            assert_eq!(item_events[0]["backwardsCursor"], "0");

            let mut second_items_page_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"items-page","method":"thread/items/list","params":{{"threadId":"{thread_id}","cursor":"2","limit":2}}}}"#
                ),
                &mut second_items_page_output,
            )
            .expect("second thread items page");

            let second_items_page_events = parse_jsonl(&second_items_page_output);
            assert_eq!(second_items_page_events.len(), 1);
            assert_eq!(second_items_page_events[0]["event"], "thread_items_list");
            let page_items = second_items_page_events[0]["data"]
                .as_array()
                .expect("paged item data");
            assert_eq!(page_items.len(), 2);
            assert_eq!(page_items[0]["itemId"], "item-3");
            assert_eq!(page_items[0]["turnId"], "turn-2");
            assert_eq!(
                page_items[0]["item"]["content"],
                "server projected second user"
            );
            assert_eq!(second_items_page_events[0]["nextCursor"], Value::Null);
            assert_eq!(second_items_page_events[0]["backwardsCursor"], "2");

            let mut latest_item_output = Vec::new();
            handle_line_for_test(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"item-desc","method":"thread/items/list","params":{{"threadId":"{thread_id}","limit":1,"sortDirection":"desc"}}}}"#
                ),
                &mut latest_item_output,
            )
            .expect("latest thread items page");

            let latest_item_events = parse_jsonl(&latest_item_output);
            assert_eq!(latest_item_events.len(), 1);
            assert_eq!(latest_item_events[0]["event"], "thread_items_list");
            let latest_items = latest_item_events[0]["data"]
                .as_array()
                .expect("latest item data");
            assert_eq!(latest_items.len(), 1);
            assert_eq!(latest_items[0]["itemId"], "item-4");
            assert_eq!(
                latest_items[0]["item"]["content"],
                "server projected second assistant"
            );
            assert_eq!(latest_item_events[0]["nextCursor"], "1");
        });
    }

    #[test]
    fn user_input_response_with_unknown_request_id_reports_error() {
        let server_config = ServerConfig {
            run_config: test_run_config(),
        };
        let mut state = ServerState::default();
        let mut output = Vec::new();

        handle_line_for_test(
            &server_config,
            &mut state,
            r#"{"id":"input-response","method":"user_input/respond","params":{"requestId":"missing-input","answer":"ship it"}}"#,
            &mut output,
        )
        .expect("user input response");

        let events = parse_jsonl(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["id"], "input-response");
        assert_eq!(events[0]["event"], "error");
        assert_eq!(
            events[0]["message"],
            "unknown user input request: missing-input"
        );
    }

    #[test]
    fn mcp_elicitation_response_with_unknown_request_id_reports_error() {
        let server_config = ServerConfig {
            run_config: test_run_config(),
        };
        let mut state = ServerState::default();
        let mut output = Vec::new();

        handle_line_for_test(
            &server_config,
            &mut state,
            r#"{"id":"mcp-response","method":"mcp_elicitation/respond","params":{"requestId":"mcp_elicitation:github:missing","accepted":false}}"#,
            &mut output,
        )
        .expect("mcp elicitation response");

        let events = parse_jsonl(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["id"], "mcp-response");
        assert_eq!(events[0]["event"], "error");
        assert_eq!(
            events[0]["message"],
            "unknown MCP elicitation request: mcp_elicitation:github:missing"
        );
    }

    #[test]
    fn turn_user_input_request_waits_for_protocol_response() {
        with_orca_home(|home| {
            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"ask Continue?"}}]}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("turn start");

            let request = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "user_input_request"
            })
            .expect("user input request");
            let request_id = request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(request["threadId"], thread_id);
            assert!(
                request["question"]
                    .as_str()
                    .is_some_and(|question| question == "Continue?")
            );
            assert_eq!(request["choices"], json!(["yes", "no"]));

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"input-response","method":"user_input/respond","params":{{"requestId":"{request_id}","answer":"yes"}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("user input response");
            state.join_active_turns();

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let resolved = events
                .iter()
                .find(|event| event["event"] == "user_input_resolved")
                .expect("user input resolved");
            assert_eq!(resolved["id"], "input-response");
            assert_eq!(resolved["requestId"], request_id);
            assert_eq!(resolved["answered"], true);
            assert!(
                events
                    .iter()
                    .any(|event| event["event"] == "turn_completed"),
                "turn should complete after user input response: {events:?}"
            );
        });
    }

    #[test]
    fn turn_mcp_elicitation_request_waits_for_protocol_response() {
        with_orca_home(|home| {
            let script = home.join("eliciting-mcp-server.js");
            std::fs::write(
                &script,
                r#"
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
let pendingToolCallId = null;

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

rl.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    send({ jsonrpc: "2.0", id: message.id, result: { capabilities: {} } });
    return;
  }
  if (message.method === "notifications/initialized") {
    return;
  }
  if (message.method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        tools: [
          {
            name: "wait",
            description: "Waits for an elicitation response",
            inputSchema: { type: "object", properties: {}, required: [] }
          }
        ]
      }
    });
    return;
  }
  if (message.method === "tools/call") {
    pendingToolCallId = message.id;
    send({
      jsonrpc: "2.0",
      id: "device-flow",
      method: "elicitation/create",
      params: {
        message: "Authorize slow wait",
        url: "https://example.test/device",
        requestedSchema: {
          type: "object",
          properties: { code: { type: "string" } },
          required: ["code"]
        }
      }
    });
    return;
  }
  if (message.id === "device-flow" && pendingToolCallId !== null) {
    const action = message.result?.action || "missing";
    const code = message.result?.content?.code || "";
    send({
      jsonrpc: "2.0",
      id: pendingToolCallId,
      result: {
        content: [{ type: "text", text: `elicitation ${action} ${code}` }],
        isError: false
      }
    });
    pendingToolCallId = null;
  }
});
"#,
            )
            .expect("write fake MCP server");

            let mut config = test_run_config();
            config.cwd = Some(home.to_path_buf());
            config.mcp_servers = vec![McpServerConfig {
                name: "slow".to_string(),
                command: Some("node".to_string()),
                args: vec![script.display().to_string()],
                startup_timeout_ms: Some(2_000),
                tool_timeout_ms: Some(5_000),
                ..Default::default()
            }];
            let server_config = ServerConfig { run_config: config };
            let mut state = ServerState::default();
            let writer = Arc::new(Mutex::new(Vec::new()));

            handle_line(
                &server_config,
                &mut state,
                r#"{"id":"thread","method":"thread/start","params":{}}"#,
                Arc::clone(&writer),
            )
            .expect("thread start");
            let thread_id = parse_jsonl(&writer.lock().expect("writer").clone())
                .into_iter()
                .find(|event| event["event"] == "thread_started")
                .and_then(|event| event["threadId"].as_str().map(ToString::to_string))
                .expect("thread id");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"mcp__slow__wait"}}]}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("turn start");

            let request = wait_for_event(&writer, Duration::from_secs(2), |event| {
                event["event"] == "mcp_elicitation_request"
            })
            .expect("MCP elicitation request");
            let request_id = request["requestId"]
                .as_str()
                .expect("request id")
                .to_string();
            assert_eq!(request["threadId"], thread_id);
            assert_eq!(request["turnId"], "turn-1");
            assert_eq!(request["serverName"], "slow");
            assert_eq!(request["mode"], "url");
            assert_eq!(request["message"], "Authorize slow wait");
            assert_eq!(request["url"], "https://example.test/device");

            handle_line(
                &server_config,
                &mut state,
                &format!(
                    r#"{{"id":"mcp-response","method":"mcp_elicitation/respond","params":{{"requestId":"{request_id}","accepted":true,"contentJson":{{"code":"ABCD-1234"}}}}}}"#
                ),
                Arc::clone(&writer),
            )
            .expect("MCP elicitation response");
            state.join_active_turns();

            let events = parse_jsonl(&writer.lock().expect("writer").clone());
            let resolved = events
                .iter()
                .find(|event| event["event"] == "mcp_elicitation_resolved")
                .expect("MCP elicitation resolved");
            assert_eq!(resolved["id"], "mcp-response");
            assert_eq!(resolved["requestId"], request_id);
            assert_eq!(resolved["accepted"], true);
            assert!(
                events
                    .iter()
                    .any(|event| event["event"] == "turn_completed"),
                "turn should complete after MCP elicitation response: {events:?}"
            );
            assert!(
                events.iter().any(|event| event["event"] == "tool_completed"
                    && event["output"]
                        .as_str()
                        .is_some_and(|output| output.contains("elicitation accept ABCD-1234"))),
                "tool output should include the accepted elicitation content: {events:?}"
            );
        });
    }

    fn test_run_config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
            .collect()
    }

    fn drain_until_command_exec_permission_request(
        state: &mut ServerState,
        writer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
    ) -> Vec<Value> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if events
                .iter()
                .any(|event| event["event"] == "permission_request")
            {
                return events;
            }
            let drain_outcome = {
                let mut output = writer.lock().expect("writer");
                drain_command_exec_processes_until_output_or_timeout(
                    state,
                    &mut *output,
                    Duration::from_millis(100),
                )
                .expect("drain command/exec process")
            };
            match drain_outcome {
                CommandExecDrainOutcome::NetworkPermissionRequired { request, block } => {
                    let mut output = writer.lock().expect("writer");
                    request_command_exec_network_permission(state, request, block, &mut *output)
                        .expect("request network permission");
                }
                CommandExecDrainOutcome::NetworkPermissionDenied {
                    command_event_id,
                    reason,
                } => {
                    let mut output = writer.lock().expect("writer");
                    protocol::write_server_event(
                        &mut *output,
                        &command_event_id,
                        ServerEvent::error(reason),
                    )
                    .expect("write network denial");
                }
                CommandExecDrainOutcome::FileSystemPermissionRequired {
                    request,
                    diagnostic,
                } => {
                    let mut output = writer.lock().expect("writer");
                    request_command_exec_file_system_permission(
                        state,
                        request,
                        diagnostic,
                        &mut *output,
                    )
                    .expect("request file-system permission");
                }
                CommandExecDrainOutcome::Drained => {}
            }
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if events
                .iter()
                .any(|event| event["event"] == "permission_request")
            {
                return events;
            }
            if std::time::Instant::now() >= deadline {
                return events;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_event(
        writer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if let Some(event) = events.into_iter().find(|event| predicate(event)) {
                return Some(event);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn handle_thread_list_until_event(
        config: &ServerConfig,
        state: &mut ServerState,
        writer: &Arc<Mutex<Vec<u8>>>,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> Vec<Value> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            handle_line(
                config,
                state,
                r#"{"id":"threads","method":"thread/list","params":{}}"#,
                Arc::clone(writer),
            )
            .expect("thread list");
            let events = parse_complete_jsonl(&writer.lock().expect("writer").clone());
            if events.iter().any(&mut predicate) {
                return events;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for server event: {events:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn parse_complete_jsonl(stdout: &[u8]) -> Vec<Value> {
        let text = String::from_utf8_lossy(stdout);
        let lines = text.lines();
        let has_trailing_newline = stdout.ends_with(b"\n");
        let last_index = lines.clone().count().saturating_sub(1);
        lines
            .enumerate()
            .filter_map(|(index, line)| {
                if !has_trailing_newline && index == last_index {
                    return None;
                }
                Some(serde_json::from_str(line).expect("valid complete jsonl line"))
            })
            .collect()
    }

    #[test]
    fn parse_complete_jsonl_ignores_trailing_partial_line_while_writer_is_active() {
        let output = br#"{"event":"turn_completed"}
{"event":"message_delta","text":"partial"#;

        let events = parse_complete_jsonl(output);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "turn_completed");
    }

    fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = crate::history::lock_test_env();
        let home = tempdir().expect("temp home");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let result = f(home.path());
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        result
    }

    fn sandbox_test_parent(prefix: &str) -> TempDir {
        #[cfg(target_os = "macos")]
        {
            let home = PathBuf::from(
                std::env::var_os("HOME").expect("HOME is required for macOS Seatbelt tests"),
            )
            .canonicalize()
            .expect("canonical macOS HOME");
            for root in [
                Some(PathBuf::from("/tmp")),
                std::env::var_os("TMPDIR").map(PathBuf::from),
            ]
            .into_iter()
            .flatten()
            {
                let root = root.canonicalize().unwrap_or(root);
                assert!(
                    !home.starts_with(&root),
                    "macOS Seatbelt fixtures require HOME outside temporary allow root {}",
                    root.display()
                );
            }
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(home)
                .expect("sandbox parent outside temporary allow roots")
        }
        #[cfg(not(target_os = "macos"))]
        {
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("sandbox parent")
        }
    }
}
