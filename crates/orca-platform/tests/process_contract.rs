#[test]
fn named_or_inherited_spawn_owns_a_child_process() {
    use orca_platform::process::ProcessJob;

    let job_name = format!(
        r"Local\Orca.ProcessContract.Inherited.{}",
        std::process::id()
    );
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("cmd.exe");
        command.args(["/C", "exit", "0"]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exit 0"]);
        command
    };
    let (mut child, _job) = ProcessJob::spawn_named_or_inherited(&mut command, &job_name)
        .expect("spawn child in a named or inherited Job Object");
    let status = child.wait().expect("wait child");
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn windows_job_object_can_terminate_a_child_tree() {
    use std::time::{Duration, Instant};

    use orca_platform::process::ProcessJob;

    let temp = tempfile::tempdir().expect("tempdir");
    let release = temp.path().join("release");
    let descendant_pid = temp.path().join("descendant-pid");
    let job_name = format!(r"Local\Orca.ProcessContract.{}", std::process::id());
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args([
            "--exact",
            "windows_job_object_descendant_helper",
            "--nocapture",
        ])
        .env("ORCA_JOB_TEST_RELEASE", &release)
        .env("ORCA_JOB_TEST_DESCENDANT_PID", &descendant_pid);
    let (mut child, job) = ProcessJob::spawn_named(&mut command, &job_name)
        .expect("spawn Windows job helper inside named Job Object");
    let recovered_job = ProcessJob::open_named(&job_name).expect("open named Job Object");
    assert!(
        recovered_job
            .contains_process(child.id())
            .expect("inspect named Job Object"),
        "reopened Job Object must retain process identity"
    );
    std::fs::write(&release, []).expect("release helper");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !descendant_pid.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let descendant_pid = std::fs::read_to_string(&descendant_pid)
        .expect("descendant PID marker")
        .trim()
        .parse::<u32>()
        .expect("valid descendant PID");

    drop(job);
    assert!(
        child.try_wait().expect("inspect child").is_none(),
        "a recovered Job Object handle must preserve ownership"
    );
    drop(recovered_job);
    let _status = child
        .wait_timeout(Duration::from_secs(3))
        .expect("wait for terminated child")
        .expect("child must terminate");

    let deadline = Instant::now() + Duration::from_secs(3);
    while windows_process_is_running(descendant_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !windows_process_is_running(descendant_pid),
        "closing a kill-on-close Job Object must terminate descendants"
    );
}

#[cfg(windows)]
#[test]
fn windows_spawned_child_enters_job_before_running_user_code() {
    use std::time::Duration;

    use orca_platform::process::ProcessJob;

    let temp = tempfile::tempdir().expect("tempdir");
    let result_path = temp.path().join("membership");
    let job_name = format!(
        r"Local\Orca.ProcessContract.FirstInstruction.{}",
        std::process::id()
    );
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", "windows_job_membership_helper", "--nocapture"])
        .env("ORCA_JOB_TEST_NAME", &job_name)
        .env("ORCA_JOB_TEST_MEMBERSHIP", &result_path);

    let (mut child, _job) = ProcessJob::spawn_named(&mut command, &job_name)
        .expect("spawn child atomically inside named Job Object");
    let status = child
        .wait_timeout(Duration::from_secs(3))
        .expect("wait for membership helper")
        .expect("membership helper must exit");

    assert!(status.success(), "membership helper failed: {status}");
    assert_eq!(
        std::fs::read_to_string(result_path).expect("membership result"),
        "owned",
        "the child must belong to its Job Object before user code executes"
    );
}

#[cfg(windows)]
#[test]
fn windows_job_membership_helper() {
    use orca_platform::process::ProcessJob;

    let Some(job_name) = std::env::var_os("ORCA_JOB_TEST_NAME") else {
        return;
    };
    let result_path = std::env::var_os("ORCA_JOB_TEST_MEMBERSHIP").expect("membership result path");
    let owned = ProcessJob::open_named(&job_name.to_string_lossy())
        .and_then(|job| job.contains_process(std::process::id()))
        .unwrap_or(false);
    std::fs::write(result_path, if owned { "owned" } else { "escaped" })
        .expect("write membership result");
}

#[cfg(windows)]
#[test]
fn windows_job_object_descendant_helper() {
    use std::time::{Duration, Instant};

    let Some(release) = std::env::var_os("ORCA_JOB_TEST_RELEASE") else {
        return;
    };
    let descendant_pid =
        std::env::var_os("ORCA_JOB_TEST_DESCENDANT_PID").expect("descendant PID marker path");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(&release).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        std::path::Path::new(&release).exists(),
        "helper release timed out"
    );

    let mut descendant = std::process::Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "ping", "-n", "30", "127.0.0.1"])
        .spawn()
        .expect("spawn Windows descendant");
    std::fs::write(descendant_pid, descendant.id().to_string()).expect("write descendant PID");
    let _ = descendant.wait();
}

#[cfg(windows)]
#[test]
fn windows_background_child_does_not_hold_parent_capture_open() {
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().expect("tempdir");
    let child_pid = temp.path().join("background-child-pid");
    let started = Instant::now();
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "windows_background_capture_parent_helper",
            "--nocapture",
        ])
        .env("ORCA_BACKGROUND_CHILD_PID", &child_pid)
        .output()
        .expect("run background capture parent");
    let elapsed = started.elapsed();

    assert!(output.status.success(), "helper failed: {output:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "captured parent output stayed open for {elapsed:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("parent-exit"),
        "parent marker missing: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let pid = std::fs::read_to_string(&child_pid)
        .expect("background child pid")
        .trim()
        .parse::<u32>()
        .expect("valid pid");
    assert!(windows_process_is_running(pid));
    terminate_windows_process(pid).expect("terminate background fixture");
}

#[cfg(windows)]
#[test]
fn windows_background_capture_parent_helper() {
    let Some(child_pid) = std::env::var_os("ORCA_BACKGROUND_CHILD_PID") else {
        return;
    };

    orca_platform::process::clear_current_process_std_handle_inheritance()
        .expect("clear inherited std handles");
    let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "windows_background_capture_child_helper",
            "--nocapture",
        ])
        .env("ORCA_BACKGROUND_CAPTURE_CHILD", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn background child");
    std::fs::write(child_pid, child.id().to_string()).expect("write child pid");
    println!("parent-exit");
}

#[cfg(windows)]
#[test]
fn windows_background_capture_child_helper() {
    if std::env::var_os("ORCA_BACKGROUND_CAPTURE_CHILD").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

#[cfg(windows)]
#[test]
fn windows_child_pipe_read_observes_stop_before_pipe_eof() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use orca_platform::process::{ProcessJob, read_child_pipe_interruptibly};

    let mut command = std::process::Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C", "ping -n 30 127.0.0.1 > nul"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let (mut child, job) = ProcessJob::spawn(&mut command).expect("spawn pipe holder");
    let mut stdout = child.stdout.take().expect("captured stdout");
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader = std::thread::spawn(move || {
        let mut buffer = [0_u8; 64];
        read_child_pipe_interruptibly(&mut stdout, reader_stop.as_ref(), &mut buffer)
    });

    assert!(
        child.try_wait().expect("inspect pipe holder").is_none(),
        "fixture exited before the stop request"
    );
    stop.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let stopped_before_eof = reader.is_finished();

    job.terminate(1).expect("terminate pipe holder");
    let _ = child.wait();
    let read = reader.join().expect("join pipe reader");

    assert!(
        stopped_before_eof,
        "pipe read did not observe stop until the child closed its handle"
    );
    assert_eq!(read.expect("interruptible pipe read"), 0);
}

#[cfg(windows)]
fn windows_process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    let running = unsafe { WaitForSingleObject(process, 0) } != 0;
    unsafe { CloseHandle(process) };
    running
}

#[cfg(windows)]
fn terminate_windows_process(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return Ok(());
    }
    let terminated = unsafe { TerminateProcess(process, 1) };
    unsafe { CloseHandle(process) };
    if terminated == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
trait ChildWaitTimeout {
    fn wait_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

#[cfg(windows)]
impl ChildWaitTimeout for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
