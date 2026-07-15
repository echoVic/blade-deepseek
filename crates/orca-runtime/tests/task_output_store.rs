use orca_runtime::task_output::{TaskOutputRead, TaskOutputStore};
use orca_runtime::{
    shell_session::{
        RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
    },
    tasks::TaskRegistry,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[test]
fn task_output_store_reads_delta_and_tail_without_splitting_utf8() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-1";

    store
        .append_stdout(task_id, "first\n")
        .expect("append stdout");
    store
        .append_stderr(task_id, "错误\n")
        .expect("append stderr");
    store
        .append_stdout(task_id, "last\n")
        .expect("append stdout again");

    assert_eq!(store.size(task_id), 18);

    let snapshot = store.read_delta(task_id, 0, 64).expect("read snapshot");
    assert_eq!(snapshot.stdout, "first\nlast\n");
    assert_eq!(snapshot.stderr, "错误\n");
    assert_eq!(snapshot.combined, "first\n错误\nlast\n");

    let delta = store.read_delta(task_id, 6, 7).expect("read delta");
    assert_eq!(
        delta,
        TaskOutputRead {
            stdout: String::new(),
            stderr: "错误\n".to_string(),
            combined: "错误\n".to_string(),
            next_offset: 13,
            bytes_read: 7,
            bytes_total: 18,
            omitted_prefix_bytes: 0,
            stdout_prefix_bytes: 6,
            stderr_prefix_bytes: 0,
        }
    );

    let tail = store.tail(task_id, 6).expect("read tail");
    assert_eq!(tail.stdout, "last\n");
    assert_eq!(tail.stderr, "");
    assert_eq!(tail.combined, "last\n");
    assert_eq!(tail.next_offset, 18);
    assert_eq!(tail.bytes_read, 5);
    assert_eq!(tail.bytes_total, 18);
    assert_eq!(tail.omitted_prefix_bytes, 13);
    assert_eq!(tail.stdout_prefix_bytes, 6);
    assert_eq!(tail.stderr_prefix_bytes, 7);
}

#[test]
fn task_output_store_skips_partial_utf8_at_delta_start() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-utf8-start";

    store
        .append_stdout(task_id, "a错误b")
        .expect("append stdout");

    let delta = store
        .read_delta(task_id, 2, 64)
        .expect("read from inside utf8 codepoint");

    assert_eq!(delta.stdout, "误b");
    assert_eq!(delta.stderr, "");
    assert_eq!(delta.combined, "误b");
    assert_eq!(delta.next_offset, store.size(task_id));
    assert_eq!(delta.bytes_read, store.size(task_id) - 2);
}

#[test]
fn task_output_store_advances_when_delta_cap_splits_first_utf8_codepoint() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-utf8-cap";

    store.append_stdout(task_id, "错误").expect("append stdout");

    let delta = store
        .read_delta(task_id, 0, 2)
        .expect("read capped inside utf8 codepoint");

    assert_eq!(delta.stdout, "错");
    assert_eq!(delta.stderr, "");
    assert_eq!(delta.combined, "错");
    assert_eq!(delta.next_offset, 3);
    assert_eq!(delta.bytes_read, 3);
    assert_eq!(delta.bytes_total, store.size(task_id));
}

#[test]
fn task_output_store_retains_bounded_tail_and_reports_omitted_prefix() {
    let store = TaskOutputStore::with_max_retained_bytes(5);
    let task_id = "task-output-bounded-tail";

    store
        .append_stdout(task_id, "first\n")
        .expect("append stdout");
    store
        .append_stderr(task_id, "错误\n")
        .expect("append stderr");
    store
        .append_stdout(task_id, "last\n")
        .expect("append stdout again");

    let snapshot = store.read_delta(task_id, 0, 64).expect("read snapshot");
    assert_eq!(snapshot.stdout, "last\n");
    assert_eq!(snapshot.stderr, "");
    assert_eq!(snapshot.combined, "last\n");
    assert_eq!(snapshot.next_offset, 18);
    assert_eq!(snapshot.bytes_read, 5);
    assert_eq!(snapshot.bytes_total, 18);
    assert_eq!(snapshot.omitted_prefix_bytes, 13);
    assert_eq!(snapshot.stdout_prefix_bytes, 6);
    assert_eq!(snapshot.stderr_prefix_bytes, 7);

    let tail = store.tail(task_id, 64).expect("read tail");
    assert_eq!(tail.stdout, "last\n");
    assert_eq!(tail.stderr, "");
    assert_eq!(tail.combined, "last\n");
    assert_eq!(tail.omitted_prefix_bytes, 13);
    assert_eq!(tail.stdout_prefix_bytes, 6);
    assert_eq!(tail.stderr_prefix_bytes, 7);
}

#[test]
fn task_output_store_trims_to_utf8_boundary_when_cap_splits_multibyte_character() {
    let store = TaskOutputStore::with_max_retained_bytes(4);
    let task_id = "task-output-bounded-utf8";

    store
        .append_stdout(task_id, "a错误b")
        .expect("append stdout");

    let snapshot = store.read_delta(task_id, 0, 64).expect("read snapshot");
    assert_eq!(snapshot.stdout, "误b");
    assert_eq!(snapshot.stderr, "");
    assert_eq!(snapshot.combined, "误b");
    assert_eq!(snapshot.next_offset, store.size(task_id));
    assert_eq!(snapshot.bytes_total, 8);
    assert_eq!(snapshot.omitted_prefix_bytes, 4);
    assert_eq!(snapshot.stdout_prefix_bytes, 4);
    assert_eq!(snapshot.stderr_prefix_bytes, 0);
}

#[test]
fn task_output_store_remove_drops_task_buffer() {
    let store = TaskOutputStore::new();
    let task_id = "task-output-remove";

    store
        .append_stdout(task_id, "output")
        .expect("append stdout");
    assert_eq!(store.size(task_id), 6);

    assert!(store.remove(task_id));
    assert_eq!(store.size(task_id), 0);

    let snapshot = store.read_delta(task_id, 0, 64).expect("read removed");
    assert_eq!(snapshot.stdout, "");
    assert_eq!(snapshot.stderr, "");
    assert_eq!(snapshot.next_offset, 0);
}

#[test]
fn shell_session_writes_process_output_to_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-store".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf stdout; printf stderr >&2; read -r _ || true".to_string(),
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "capture output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let deadline = Instant::now() + Duration::from_secs(2);
    let running_output = loop {
        let output = manager
            .read(&handle.id, Duration::from_millis(50))
            .expect("read running shell");
        if output.stdout == "stdout" && output.stderr == "stderr" {
            break output;
        }
        assert!(
            Instant::now() < deadline,
            "both shell readers did not publish output"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(running_output.stdout, "stdout");
    assert_eq!(running_output.stderr, "stderr");

    let stored = manager
        .output_store()
        .read_delta(&handle.task_id, 0, 64)
        .expect("stored output");
    assert_eq!(stored.stdout, "stdout");
    assert_eq!(stored.stderr, "stderr");

    manager
        .write_stdin(&handle.id, "\n")
        .expect("release running shell");
    let final_output = manager
        .wait(&handle.id, Duration::from_secs(2))
        .expect("wait for shell");
    assert_eq!(final_output.stdout, "stdout");
    assert_eq!(final_output.stderr, "stderr");
}

#[test]
fn shell_session_evicts_completed_process_output_from_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-evict".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf done".to_string(),
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "evict output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let output = manager
        .wait(&handle.id, Duration::from_secs(2))
        .expect("wait for shell");

    assert_eq!(output.stdout, "done");
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_evicts_completed_process_output_when_read_observes_exit() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-read-evict".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf done".to_string(),
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "read evicts output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    std::thread::sleep(Duration::from_millis(100));
    let output = manager
        .read(&handle.id, Duration::from_secs(1))
        .expect("read completed shell");

    assert_eq!(output.stdout, "done");
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_reap_completed_removes_process_output_from_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-list-reap".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf listed".to_string(),
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "list reaps output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    std::thread::sleep(Duration::from_millis(100));
    let completed = manager.reap_completed().expect("reap completed shell");

    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].id, handle.id);
    assert!(
        manager.list().iter().all(|shell| shell.id != handle.id),
        "completed shell should be removed after explicit reap"
    );
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_evicts_stopped_process_output_from_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-kill-evict".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf running; sleep 5".to_string(),
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "kill evicts output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let output = manager.kill(&handle.id).expect("kill shell");

    assert_eq!(output.status, orca_core::task_types::TaskStatus::Stopped);
    assert!(output.stdout.contains("running"));
    assert_eq!(manager.output_store().size(&handle.task_id), 0);
}

#[test]
fn shell_session_reports_when_retained_output_omits_prefix() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-cap".to_string());
    let output_store = TaskOutputStore::with_max_retained_bytes(5);
    let mut manager = RuntimeShellSessionManager::with_output_store(registry, output_store);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf 'first\\nlast\\n'".to_string(),
            cwd: cwd.path().to_path_buf(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "cap output".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::DangerFullAccess,
        })
        .expect("spawn shell");

    let output = manager
        .wait(&handle.id, Duration::from_secs(2))
        .expect("wait for shell");

    assert_eq!(output.stdout, "[6 bytes of earlier output omitted]\nlast\n");
}
