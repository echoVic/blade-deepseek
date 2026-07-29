#[cfg(windows)]
mod windows {
    use std::io::Read;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use orca_platform::process::ProcessJob;
    use orca_platform::terminal::{
        PtyExitStatus, WindowsPtyChild, spawn_windows_pty, spawn_windows_pty_named,
    };

    #[test]
    fn conpty_runs_native_command_and_supports_resize() {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C", "echo ORCA_CONPTY_OK"]);

        let mut process = spawn_windows_pty(&command, Some(100), Some(30)).unwrap();
        let reader = std::thread::spawn(move || {
            let mut output = String::new();
            process.reader.read_to_string(&mut output).unwrap();
            output
        });
        process.input.resize(120, 40).unwrap();
        process.input.close();
        let status = wait_for_exit(&mut process.child, "native ConPTY command");
        std::thread::sleep(Duration::from_millis(200));
        process.input.close_terminal();
        let output = reader.join().expect("join ConPTY output reader");
        assert!(status.success(), "ConPTY command failed: {status:?}");
        assert!(output.contains("ORCA_CONPTY_OK"), "output was {output:?}");
    }

    #[test]
    fn conpty_child_enters_job_before_running_user_code() {
        let temp = tempfile::tempdir().expect("tempdir");
        let result_path = temp.path().join("membership");
        let job_name = format!(r"Local\Orca.ConPtyContract.{}", std::process::id());
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "windows::conpty_job_membership_helper",
                "--nocapture",
            ])
            .env("ORCA_CONPTY_JOB_NAME", &job_name)
            .env("ORCA_CONPTY_JOB_MEMBERSHIP", &result_path);

        let mut process = spawn_windows_pty_named(&command, Some(100), Some(30), &job_name)
            .expect("spawn ConPTY child atomically inside named Job Object");
        process.input.close();
        let status = wait_for_exit(&mut process.child, "ConPTY membership helper");

        assert!(
            status.success(),
            "ConPTY membership helper failed: {status:?}"
        );
        assert_eq!(
            std::fs::read_to_string(result_path).expect("membership result"),
            "owned",
            "the ConPTY child must belong to its Job Object before user code executes"
        );
    }

    #[test]
    fn conpty_job_membership_helper() {
        let Some(job_name) = std::env::var_os("ORCA_CONPTY_JOB_NAME") else {
            return;
        };
        let result_path =
            std::env::var_os("ORCA_CONPTY_JOB_MEMBERSHIP").expect("membership result path");
        let owned = ProcessJob::open_named(&job_name.to_string_lossy())
            .and_then(|job| job.contains_process(std::process::id()))
            .unwrap_or(false);
        std::fs::write(result_path, if owned { "owned" } else { "escaped" })
            .expect("write membership result");
    }

    fn wait_for_exit(child: &mut WindowsPtyChild, label: &str) -> PtyExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("inspect ConPTY child") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{label} did not exit after ConPTY input reached EOF");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
