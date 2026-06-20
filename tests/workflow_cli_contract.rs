use std::fs;
use std::process::Command;

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
    let dir = temp.path().join(".claude/workflows");
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
fn workflow_stop_and_resume_fail_clearly_without_live_registry_support() {
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

    let stop = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args(["workflow", "stop", task_id])
        .output()
        .expect("stop workflow");

    assert_eq!(stop.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&stop.stderr)
            .contains("cross-process workflow stop is not available")
    );

    let resume = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args(["workflow", "resume", run_id])
        .output()
        .expect("resume workflow");

    assert_eq!(resume.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&resume.stderr)
            .contains("workflow resume from persisted state is not yet supported")
    );
}
