use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use orca_platform::PlatformError;
use orca_platform::fs::ExclusiveFileLock;

const LOCK_PROBE_PATH: &str = "ORCA_LOCK_PROBE_PATH";
const LOCK_PROBE_MODE: &str = "ORCA_LOCK_PROBE_MODE";

#[test]
fn exclusive_lock_rejects_second_process_and_releases_on_drop() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("owner.lock");
    let first = ExclusiveFileLock::try_acquire(&path).expect("first owner");

    assert_probe(&run_probe(&path, "try"), "contended");
    drop(first);
    assert_probe(&run_probe(&path, "try"), "acquired");
}

#[test]
fn blocking_lock_waits_for_the_current_owner_then_acquires() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("blocking.lock");
    let first = ExclusiveFileLock::try_acquire(&path).expect("first owner");
    let mut child = spawn_probe(&path, "blocking");

    thread::sleep(Duration::from_millis(150));
    assert!(
        child.try_wait().expect("inspect blocking child").is_none(),
        "blocking acquisition returned while the first owner was live"
    );
    drop(first);

    let output = wait_for_child(&mut child, Duration::from_secs(5));
    assert_probe(&output, "acquired");
}

#[test]
fn operating_system_releases_lock_when_owner_process_exits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("process-exit.lock");

    assert_probe(&run_probe(&path, "exit-holding"), "holding");
    let owner = ExclusiveFileLock::try_acquire(&path).expect("lock released after process exit");
    assert_eq!(owner.path(), path);
}

#[test]
fn lock_probe_child() {
    let Some(path) = std::env::var_os(LOCK_PROBE_PATH).map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var(LOCK_PROBE_MODE).expect("probe mode");
    match mode.as_str() {
        "try" => match ExclusiveFileLock::try_acquire(&path) {
            Ok(_lock) => println!("lock-probe:acquired"),
            Err(PlatformError::LockContended { .. }) => println!("lock-probe:contended"),
            Err(error) => panic!("unexpected lock probe error: {error}"),
        },
        "blocking" => {
            let _lock = ExclusiveFileLock::acquire(&path).expect("blocking acquisition");
            println!("lock-probe:acquired");
        }
        "exit-holding" => {
            let _lock = ExclusiveFileLock::try_acquire(&path).expect("exit owner");
            println!("lock-probe:holding");
            std::io::stdout().flush().expect("flush probe marker");
            std::process::exit(0);
        }
        other => panic!("unknown probe mode: {other}"),
    }
}

fn run_probe(path: &Path, mode: &str) -> Output {
    spawn_probe(path, mode)
        .wait_with_output()
        .expect("wait for lock probe")
}

fn spawn_probe(path: &Path, mode: &str) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "lock_probe_child", "--nocapture"])
        .env(LOCK_PROBE_PATH, path)
        .env(LOCK_PROBE_MODE, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lock probe")
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll lock probe").is_some() {
            return collect_finished_child(child);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let output = collect_finished_child(child);
    panic!(
        "lock probe timed out: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn collect_finished_child(child: &mut Child) -> Output {
    let stdout = child
        .stdout
        .take()
        .map(|mut stdout| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut stdout, &mut bytes).expect("read probe stdout");
            bytes
        })
        .unwrap_or_default();
    let stderr = child
        .stderr
        .take()
        .map(|mut stderr| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut stderr, &mut bytes).expect("read probe stderr");
            bytes
        })
        .unwrap_or_default();
    let status = child.wait().expect("reap lock probe");
    Output {
        status,
        stdout,
        stderr,
    }
}

fn assert_probe(output: &Output, marker: &str) {
    assert!(
        output.status.success(),
        "probe failed: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("lock-probe:{marker}")),
        "missing marker {marker:?}: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
