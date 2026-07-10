use orca_runtime::task_output::{TaskOutputRead, TaskOutputStore};
use orca_runtime::{
    shell_session::{
        RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
    },
    tasks::TaskRegistry,
};
use std::collections::BTreeMap;
use std::time::Duration;

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

    let delta = store.read_delta(task_id, 6, 7).expect("read delta");
    assert_eq!(
        delta,
        TaskOutputRead {
            stdout: String::new(),
            stderr: "错误\n".to_string(),
            next_offset: 13,
            bytes_read: 7,
            bytes_total: 18,
            omitted_prefix_bytes: 0,
        }
    );

    let tail = store.tail(task_id, 6).expect("read tail");
    assert_eq!(tail.stdout, "last\n");
    assert_eq!(tail.stderr, "");
    assert_eq!(tail.next_offset, 18);
    assert_eq!(tail.bytes_read, 5);
    assert_eq!(tail.bytes_total, 18);
    assert_eq!(tail.omitted_prefix_bytes, 13);
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
    assert_eq!(delta.next_offset, 3);
    assert_eq!(delta.bytes_read, 3);
    assert_eq!(delta.bytes_total, store.size(task_id));
}

#[test]
fn shell_session_writes_process_output_to_task_output_store() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let registry = TaskRegistry::new("shell-output-store".to_string());
    let mut manager = RuntimeShellSessionManager::new(registry);

    let handle = manager
        .spawn(ShellSessionCommand {
            command: "printf stdout; printf stderr >&2".to_string(),
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

    let output = manager
        .wait(&handle.id, Duration::from_secs(2))
        .expect("wait for shell");

    assert_eq!(output.stdout, "stdout");
    assert_eq!(output.stderr, "stderr");

    let stored = manager
        .output_store()
        .read_delta(&handle.task_id, 0, 64)
        .expect("stored output");
    assert_eq!(stored.stdout, "stdout");
    assert_eq!(stored.stderr, "stderr");
}
