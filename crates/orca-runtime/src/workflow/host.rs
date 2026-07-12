use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::retained_output::{RetainedOutput, RetainedOutputSnapshot};

const WORKFLOW_STDERR_RETAINED_BYTES: usize =
    orca_tools::process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM;
const WORKFLOW_STDERR_READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    PhaseStarted {
        name: String,
    },
    PhaseCompleted {
        name: String,
    },
    PhaseFailed {
        name: String,
        error: String,
        #[serde(default)]
        fallback: Option<String>,
    },
    AgentCall {
        call_id: String,
        call_path: String,
        phase: Option<String>,
        prompt: String,
        opts: Value,
    },
    WorkflowCompleted {
        result: Value,
    },
    WorkflowFailed {
        error: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostCommand {
    AgentResult { call_id: String, result: Value },
    AgentError { call_id: String, error: String },
}

#[derive(Clone, Debug, Default)]
pub struct WorkflowHost;

#[derive(Clone, Debug)]
pub struct WorkflowHostIpcPaths {
    pub mailbox_path: PathBuf,
    pub task_lists_path: PathBuf,
}

struct WorkflowChild {
    child: Option<Child>,
}

impl WorkflowChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("workflow child is available")
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child_mut().wait();
        if status.is_ok() {
            self.child.take();
        }
        status
    }
}

impl Drop for WorkflowChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        orca_tools::process::kill_child_tree(&mut child);
        let _ = child.wait();
    }
}

impl WorkflowHost {
    pub fn node_executable() -> PathBuf {
        node_command()
    }

    pub fn node_available() -> bool {
        Command::new(Self::node_executable())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    pub fn run_collecting_events(script_path: &Path, args: Value) -> io::Result<Vec<HostEvent>> {
        Self::run_collecting_events_with_agent(script_path, args, |call| {
            Ok(HostCommand::AgentResult {
                call_id: call.call_id.clone(),
                result: serde_json::json!({
                    "callId": call.call_id,
                    "prompt": call.prompt,
                    "cached": false,
                }),
            })
        })
    }

    pub fn run_collecting_events_with_ipc_paths(
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
    ) -> io::Result<Vec<HostEvent>> {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            script_path,
            args,
            Some(ipc_paths),
            |call| {
                Ok(HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({
                        "callId": call.call_id,
                        "prompt": call.prompt,
                        "cached": false,
                    }),
                })
            },
            |_| Ok(()),
        )
    }

    pub fn run_collecting_events_with_agent<F>(
        script_path: &Path,
        args: Value,
        on_agent_call: F,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
    {
        Self::run_collecting_events_with_agent_and_event_callback(
            script_path,
            args,
            on_agent_call,
            |_| Ok(()),
        )
    }

    pub fn run_collecting_events_with_agent_and_event_callback<F, E>(
        script_path: &Path,
        args: Value,
        on_agent_call: F,
        on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            script_path,
            args,
            None,
            on_agent_call,
            on_event,
        )
    }

    pub fn run_collecting_events_with_agent_and_event_callback_with_ipc_paths<F, E>(
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
        on_agent_call: F,
        on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            script_path,
            args,
            Some(ipc_paths),
            on_agent_call,
            on_event,
        )
    }

    fn run_collecting_events_with_agent_and_event_callback_inner<F, E>(
        script_path: &Path,
        args: Value,
        ipc_paths: Option<&WorkflowHostIpcPaths>,
        on_agent_call: F,
        mut on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        let host_path = ensure_host_file()?;
        let args_json = serde_json::to_string(&args)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

        let mut command = Command::new(Self::node_executable());
        command.arg(&host_path).arg(script_path).arg(args_json);
        if let Some(ipc_paths) = ipc_paths {
            command
                .arg(&ipc_paths.mailbox_path)
                .arg(&ipc_paths.task_lists_path);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        let child = command.spawn()?;
        let mut child = WorkflowChild::new(child);
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdin"))?;
        let stdin = Arc::new(Mutex::new(stdin));

        let stdout = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdout"))?;
        let stderr = child
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stderr"))?;
        let stderr_reader = spawn_stderr_reader(stderr);
        let reader = BufReader::new(stdout);

        let mut events = Vec::new();
        let mut workflow_failed = None;
        let agent_error = Arc::new(Mutex::new(None));
        let on_agent_call = &on_agent_call;
        thread::scope(|scope| -> io::Result<()> {
            for line in reader.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }

                let event: HostEvent = serde_json::from_str(&line)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if let HostEvent::WorkflowFailed { error } = &event {
                    workflow_failed = Some(error.clone());
                }
                let is_terminal = matches!(
                    event,
                    HostEvent::WorkflowCompleted { .. } | HostEvent::WorkflowFailed { .. }
                );
                on_event(&event)?;
                if let HostEvent::AgentCall {
                    call_id,
                    call_path,
                    phase,
                    prompt,
                    opts,
                } = &event
                {
                    let call = AgentCall {
                        call_id: call_id.clone(),
                        call_path: call_path.clone(),
                        phase: phase.clone(),
                        prompt: prompt.clone(),
                        opts: opts.clone(),
                    };
                    let writer = Arc::clone(&stdin);
                    let error_slot = Arc::clone(&agent_error);
                    scope.spawn(move || {
                        let call_id = call.call_id.clone();
                        let command = match on_agent_call(call) {
                            Ok(command) => command,
                            Err(error) => {
                                record_first_agent_error(&error_slot, error.to_string());
                                HostCommand::AgentError {
                                    call_id,
                                    error: "workflow host failed to answer agent call".to_string(),
                                }
                            }
                        };

                        if let Err(error) = write_host_command(&writer, &command) {
                            record_first_agent_error(&error_slot, error.to_string());
                        }
                    });
                }
                events.push(event);
                if is_terminal {
                    break;
                }
            }
            Ok(())
        })?;
        drop(stdin);

        if let Some(error) = agent_error
            .lock()
            .map_err(|_| io::Error::other("workflow agent error lock poisoned"))?
            .clone()
        {
            return Err(io::Error::other(error));
        }

        let status = child.wait()?;
        let stderr = join_stderr_reader(stderr_reader)?;
        if !status.success() {
            if workflow_failed.is_some() {
                return Ok(events);
            }

            let stderr = format_stderr(stderr);
            let message = if stderr.is_empty() {
                format!("workflow host exited with status {status}")
            } else {
                format!("workflow host exited with status {status}: {stderr}")
            };
            return Err(io::Error::other(message));
        }

        Ok(events)
    }
}

fn spawn_stderr_reader(
    mut stderr: impl Read + Send + 'static,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || {
        let mut output = RetainedOutput::new(WORKFLOW_STDERR_RETAINED_BYTES);
        let mut buffer = [0_u8; WORKFLOW_STDERR_READ_CHUNK_BYTES];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => return Ok(output.into_snapshot()),
                Ok(read) => output.append(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    })
}

fn join_stderr_reader(
    reader: thread::JoinHandle<io::Result<RetainedOutputSnapshot>>,
) -> io::Result<RetainedOutputSnapshot> {
    reader
        .join()
        .map_err(|_| io::Error::other("workflow host stderr reader thread panicked"))?
}

fn format_stderr(stderr: RetainedOutputSnapshot) -> String {
    let mut message = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
    if stderr.omitted_bytes > 0 {
        message.push_str(&format!(
            "\n[{} workflow stderr bytes omitted]",
            stderr.omitted_bytes
        ));
    }
    message
}

fn write_host_command(writer: &Arc<Mutex<impl Write>>, command: &HostCommand) -> io::Result<()> {
    let command_json = serde_json::to_string(command)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("workflow host stdin lock poisoned"))?;
    writer.write_all(command_json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn record_first_agent_error(error_slot: &Arc<Mutex<Option<String>>>, error: String) {
    if let Ok(mut slot) = error_slot.lock() {
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCall {
    pub call_id: String,
    pub call_path: String,
    pub phase: Option<String>,
    pub prompt: String,
    pub opts: Value,
}

fn ensure_host_file() -> io::Result<PathBuf> {
    static HOST_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = HOST_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "orca-workflow-host-{}-{seq}.mjs",
        std::process::id()
    ));
    fs::write(&path, include_str!("host.mjs"))?;
    Ok(path)
}

fn node_command() -> PathBuf {
    for key in ["ORCA_NODE_PATH", "ORCA_NODE"] {
        if let Some(path) = env::var_os(key).filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
    }

    if let Some(path) = node_from_npm_package_root() {
        return path;
    }

    if let Some(path) = node_from_path_sibling() {
        return path;
    }

    PathBuf::from("node")
}

fn node_from_npm_package_root() -> Option<PathBuf> {
    let package_root = env::var_os("ORCA_MANAGED_PACKAGE_ROOT")?;
    let package_root = PathBuf::from(package_root);
    for candidate in [
        package_root.join("node").join("bin").join("node"),
        package_root
            .join("..")
            .join("node")
            .join("bin")
            .join("node"),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn node_from_path_sibling() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("..").join("node").join("bin").join("node");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct TestEnvVar {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl TestEnvVar {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    #[cfg(unix)]
    impl Drop for TestEnvVar {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    env::set_var(self.key, previous);
                } else {
                    env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn host_file_paths_are_unique_for_parallel_tests() {
        let first = ensure_host_file().unwrap();
        let second = ensure_host_file().unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    #[cfg(unix)]
    fn node_available_accepts_explicit_node_path_env() {
        let _guard = crate::history::lock_test_env();
        let previous_path = env::var_os("PATH");
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let previous_node = env::var_os("ORCA_NODE");
        let previous_package_root = env::var_os("ORCA_MANAGED_PACKAGE_ROOT");
        let temp = tempfile::tempdir().expect("tempdir");
        let node = write_fake_node(temp.path());

        unsafe {
            env::set_var("PATH", "");
            env::set_var("ORCA_NODE_PATH", &node);
            env::remove_var("ORCA_NODE");
            env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
        }

        assert!(WorkflowHost::node_available());

        unsafe {
            if let Some(previous) = previous_path {
                env::set_var("PATH", previous);
            } else {
                env::remove_var("PATH");
            }
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
            if let Some(previous) = previous_node {
                env::set_var("ORCA_NODE", previous);
            } else {
                env::remove_var("ORCA_NODE");
            }
            if let Some(previous) = previous_package_root {
                env::set_var("ORCA_MANAGED_PACKAGE_ROOT", previous);
            } else {
                env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn node_available_accepts_sibling_node_bin_from_path_layout() {
        let _guard = crate::history::lock_test_env();
        let previous_path = env::var_os("PATH");
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let previous_node = env::var_os("ORCA_NODE");
        let previous_package_root = env::var_os("ORCA_MANAGED_PACKAGE_ROOT");
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("dependencies").join("bin");
        let node_bin = temp.path().join("dependencies").join("node").join("bin");
        std::fs::create_dir_all(&bin).expect("create fake bin");
        std::fs::create_dir_all(&node_bin).expect("create fake node bin");
        write_fake_node(&node_bin);

        unsafe {
            env::set_var("PATH", &bin);
            env::remove_var("ORCA_NODE_PATH");
            env::remove_var("ORCA_NODE");
            env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
        }

        assert!(WorkflowHost::node_available());

        unsafe {
            if let Some(previous) = previous_path {
                env::set_var("PATH", previous);
            } else {
                env::remove_var("PATH");
            }
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
            if let Some(previous) = previous_node {
                env::set_var("ORCA_NODE", previous);
            } else {
                env::remove_var("ORCA_NODE");
            }
            if let Some(previous) = previous_package_root {
                env::set_var("ORCA_MANAGED_PACKAGE_ROOT", previous);
            } else {
                env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn event_callback_error_reaps_workflow_process_group() {
        let _guard = crate::history::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let survivor_marker = temp.path().join("workflow-survivor");
        let node = write_fake_node_script(
            temp.path(),
            r#"#!/bin/sh
(sleep 0.4; : > "$ORCA_WORKFLOW_TEST_MARKER") &
printf '{"type":"phase_started","name":"scan"}\n'
wait
"#,
        );
        let _node_path = TestEnvVar::set("ORCA_NODE_PATH", &node);
        let _marker = TestEnvVar::set("ORCA_WORKFLOW_TEST_MARKER", &survivor_marker);

        let error = WorkflowHost::run_collecting_events_with_agent_and_event_callback(
            &temp.path().join("unused-workflow.js"),
            serde_json::json!(null),
            |_| unreachable!("fixture does not emit agent calls"),
            |_| Err(io::Error::other("event callback failed")),
        )
        .expect_err("event callback should fail");

        assert_eq!(error.to_string(), "event callback failed");
        thread::sleep(std::time::Duration::from_millis(600));
        assert!(
            !survivor_marker.exists(),
            "workflow descendant continued after callback error"
        );
    }

    #[test]
    #[cfg(unix)]
    fn workflow_host_drains_stderr_while_reading_events() {
        let _guard = crate::history::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_path = temp.path().join("workflow-host.pid");
        let node = write_fake_node_script(
            temp.path(),
            r#"#!/bin/sh
printf '%s' "$$" > "$ORCA_WORKFLOW_TEST_PID"
i=0
while [ "$i" -lt 20000 ]; do
  printf 'workflow stderr padding 0123456789012345678901234567890123456789\n' >&2
  i=$((i + 1))
done
printf '{"type":"workflow_completed","result":{"ok":true}}\n'
"#,
        );
        let _node_path = TestEnvVar::set("ORCA_NODE_PATH", &node);
        let _pid_path = TestEnvVar::set("ORCA_WORKFLOW_TEST_PID", &pid_path);
        let script = temp.path().join("unused-workflow.js");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = WorkflowHost::run_collecting_events(&script, serde_json::json!(null));
            let _ = sender.send(result);
        });

        let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
        if result.is_err() {
            terminate_fixture_process(&pid_path);
            let _ = receiver.recv_timeout(std::time::Duration::from_secs(2));
        }
        worker.join().expect("workflow host worker");

        let events = result
            .expect("workflow host blocked on its stderr pipe")
            .expect("workflow host result");
        assert!(events.iter().any(|event| {
            matches!(event, HostEvent::WorkflowCompleted { result } if result["ok"] == true)
        }));
    }

    #[test]
    fn workflow_stderr_reader_bounds_retained_output() {
        let observed_bytes = WORKFLOW_STDERR_RETAINED_BYTES + 4096;
        let stderr = std::io::Cursor::new(vec![b'x'; observed_bytes]);

        let retained = join_stderr_reader(spawn_stderr_reader(stderr)).expect("read stderr");

        assert_eq!(retained.observed_bytes, observed_bytes);
        assert_eq!(retained.bytes.len(), WORKFLOW_STDERR_RETAINED_BYTES);
        assert_eq!(retained.omitted_bytes, 4096);
        assert!(format_stderr(retained).ends_with("[4096 workflow stderr bytes omitted]"));
    }

    #[cfg(unix)]
    fn terminate_fixture_process(pid_path: &Path) {
        let Ok(pid) = std::fs::read_to_string(pid_path) else {
            return;
        };
        let _ = Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .status();
    }

    #[cfg(unix)]
    fn write_fake_node(dir: &Path) -> PathBuf {
        write_fake_node_script(dir, "#!/bin/sh\nexit 0\n")
    }

    #[cfg(unix)]
    fn write_fake_node_script(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let node = dir.join("node");
        std::fs::write(&node, script).expect("write fake node");
        let mut permissions = std::fs::metadata(&node)
            .expect("fake node metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&node, permissions).expect("chmod fake node");
        node
    }
}
