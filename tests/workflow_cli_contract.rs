use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn workflow_run_command_executes_script() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--cwd",
            temp.path().to_str().unwrap(),
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "async_launched");
    assert_eq!(value["workflowName"], "audit");
}

#[test]
fn workflow_run_named_script_resolves_project_workflow() {
    let temp = tempdir().unwrap();
    let dir = temp.path().join(".orca/workflows");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("audit.js"),
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            "--cwd",
            temp.path().to_str().unwrap(),
            "audit",
        ])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["workflowName"], "audit");
}

#[test]
fn disable_workflows_setting_blocks_launch() {
    let temp = tempdir().unwrap();
    fs::write(temp.path().join("config.toml"), "disableWorkflows = true\n").unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default 'blocked';",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", temp.path())
        .args(["workflow", "run", script.to_str().unwrap()])
        .output()
        .expect("run workflow");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("workflows are disabled"));
}

#[test]
fn workflow_list_and_show_inspect_persisted_runs() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("audit.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", temp.path().join("home"))
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let run_id = launched["runId"].as_str().unwrap();

    wait_for_workflow_terminal_status(temp.path(), task_id);

    let list = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args(["workflow", "list"])
        .output()
        .expect("list workflows");

    assert_eq!(list.status.code(), Some(0));
    let listed: Value = serde_json::from_slice(&list.stdout).unwrap();
    let runs = listed.as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["taskId"], task_id);
    assert_eq!(runs[0]["runId"], run_id);
    assert_eq!(runs[0]["workflowName"], "audit");
    assert_eq!(runs[0]["status"], "completed");

    let show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args(["workflow", "show", task_id])
        .output()
        .expect("show workflow");

    assert_eq!(show.status.code(), Some(0));
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["taskId"], task_id);
    assert_eq!(shown["runId"], run_id);
    assert_eq!(shown["workflowName"], "audit");
    assert_eq!(shown["status"], "completed");
}

#[test]
fn workflow_run_returns_before_slow_workflow_completes() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    write_sleep_hook_config(&home, 1.5);
    let script = temp.path().join("slow.js");
    fs::write(
        &script,
        "export const meta = { name: 'slow', description: 'Slow workflow', phases: [] };\nexport default await agent('inspect repo');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let show = workflow_show(temp.path(), task_id);
    assert!(show["status"] == "queued" || show["status"] == "running");

    wait_for_workflow_terminal_status(temp.path(), task_id);
    let completed = workflow_show(temp.path(), task_id);
    assert_eq!(completed["status"], "completed");
}

#[test]
fn workflow_stop_requests_real_background_stop() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    write_sleep_hook_config(&home, 1.0);
    let script = temp.path().join("stoppable.js");
    fs::write(
        &script,
        "export const meta = { name: 'stoppable', description: 'Stoppable workflow', phases: [] };\nawait agent('first');\nexport default await agent('second');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .env("ORCA_HOME", &home)
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let task_id = launched["taskId"].as_str().unwrap();
    let run_id = launched["runId"].as_str().unwrap();

    thread::sleep(Duration::from_millis(250));

    let stop = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args(["workflow", "stop", task_id])
        .output()
        .expect("stop workflow");

    assert_eq!(stop.status.code(), Some(0));
    let stop_value: Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert_eq!(stop_value["status"], "stop_requested");
    assert_eq!(stop_value["taskId"], task_id);
    assert_eq!(stop_value["runId"], run_id);

    wait_for_workflow_terminal_status(temp.path(), task_id);
    let stopped = workflow_show(temp.path(), task_id);
    assert_eq!(stopped["status"], "stopped");
}

#[test]
fn workflow_resume_rejects_cross_process_cache_resume() {
    let temp = tempdir().unwrap();
    let script = temp.path().join("resumable.js");
    fs::write(
        &script,
        "export const meta = { name: 'resumable', description: 'Resumable workflow', phases: [] };\nexport default await agent('first');",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args([
            "workflow",
            "run",
            "--provider",
            "mock",
            script.to_str().unwrap(),
        ])
        .output()
        .expect("run workflow");

    assert_eq!(run.status.code(), Some(0));
    let launched: Value = serde_json::from_slice(&run.stdout).unwrap();
    let run_id = launched["runId"].as_str().unwrap();

    let resume = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args(["workflow", "resume", run_id])
        .output()
        .expect("resume workflow");

    assert!(
        !resume.status.success(),
        "standalone CLI resume should not reuse a persisted cache"
    );
    let stderr = String::from_utf8_lossy(&resume.stderr);
    assert!(
        stderr.contains("only available inside that active Orca session"),
        "unexpected stderr: {stderr}"
    );
}

fn workflow_show(cwd: &std::path::Path, task_id: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd)
        .args(["workflow", "show", task_id])
        .output()
        .expect("show workflow");

    assert_eq!(output.status.code(), Some(0));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_for_workflow_terminal_status(cwd: &std::path::Path, task_id: &str) {
    for _ in 0..80 {
        let shown = workflow_show(cwd, task_id);
        let status = shown["status"].as_str().unwrap_or_default();
        if matches!(status, "completed" | "failed" | "stopped" | "cancelled") {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("workflow task {task_id} did not reach a terminal state");
}

fn write_sleep_hook_config(home: &std::path::Path, seconds: f32) {
    fs::create_dir_all(home).unwrap();
    fs::write(
        home.join("config.toml"),
        format!("[[hooks]]\nevent = \"pre_model_call\"\ncommand = \"sleep {seconds}\"\n"),
    )
    .unwrap();
}
