use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[test]
fn read_file_emits_tool_request_and_completed_events() {
    let temp_dir = make_temp_workspace("read-file");
    fs::write(temp_dir.join("note.txt"), "orca read fixture\n").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "read note.txt",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "read_file");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "note.txt");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "read_file");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["truncated"], false);
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("orca read fixture")
    );
}

#[test]
fn git_status_emits_completed_tool_event() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "git status",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "git_status");
    assert_eq!(requested["payload"]["action"], "read");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "git_status");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn grep_emits_completed_tool_event_with_matches() {
    let temp_dir = make_temp_workspace("grep");
    fs::write(
        temp_dir.join("fixture.txt"),
        "unique-orca-grep-fixture\nother line\n",
    )
    .expect("write grep fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "grep unique-orca-grep-fixture",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "grep");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "unique-orca-grep-fixture");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "grep");
    assert_eq!(completed["payload"]["status"], "completed");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("fixture.txt")
    );
}

#[test]
fn suggest_denies_bash_in_jsonl_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "bash printf hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));

    let events = parse_jsonl(&output.stdout);
    let resolved = find_event(&events, "approval.resolved");
    assert_eq!(resolved["payload"]["decision"], "deny");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "denied");
}

#[test]
fn full_auto_allows_bash_tool() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["output"], "hi");
}

#[test]
fn tool_output_truncation_policy_from_config_applies_to_bash() {
    let home = make_temp_workspace("tool-truncation-home");
    fs::write(
        home.join("config.toml"),
        r#"
[tools]
output_truncation = { mode = "tokens", limit = 12 }
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf 'alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma'",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["truncated"], true);
    let text = completed["payload"]["output"].as_str().unwrap();
    assert!(text.contains("Warning: truncated tool output"));
    assert!(text.contains("Original token count:"));
}

#[test]
fn pre_tool_hook_can_modify_tool_target_before_execution() {
    let home = make_temp_workspace("hook-modify-home");
    fs::write(
        home.join("config.toml"),
        r#"
[[hooks]]
event = "pre_tool_use"
tool = "bash"
command = "printf '%s' '{\"action\":\"modify\",\"modified_target\":\"printf sanitized\"}'"
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf unsafe",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["output"], "sanitized");
}

#[test]
fn pre_tool_hook_json_deny_blocks_tool_even_when_exit_succeeds() {
    let home = make_temp_workspace("hook-deny-home");
    fs::write(
        home.join("config.toml"),
        r#"
[[hooks]]
event = "pre_tool_use"
tool = "bash"
command = "printf '%s' '{\"action\":\"deny\",\"reason\":\"blocked by hook\"}'"
"#,
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "bash printf unsafe",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["status"], "failed");
    assert!(
        completed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("blocked by hook")
    );
}

#[test]
fn full_auto_bash_cannot_write_outside_workspace() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let temp_dir = make_temp_workspace("bash-sandbox");
    let outside = std::env::temp_dir().join(format!(
        "orca-outside-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--cwd",
            temp_dir.to_str().unwrap(),
            &format!("bash printf blocked > {}", outside.display()),
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(!outside.exists());

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "bash");
    assert_eq!(completed["payload"]["status"], "failed");
}

fn sandbox_seatbelt_available() -> bool {
    Command::new("sandbox-exec")
        .arg("-p")
        .arg("(version 1) (allow default)")
        .arg("true")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn auto_edit_allows_edit_tool() {
    let temp_dir = make_temp_workspace("edit-success");
    let file_path = temp_dir.join("note.txt");
    fs::write(&file_path, "hello orca\n").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "auto-edit",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "edit note.txt :: hello => hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "hi orca\n");

    let events = parse_jsonl(&output.stdout);
    let resolved = find_event(&events, "approval.resolved");
    assert_eq!(resolved["payload"]["decision"], "allow");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "edit");
    assert_eq!(completed["payload"]["status"], "completed");
}

#[test]
fn suggest_denies_edit_in_jsonl_mode() {
    let temp_dir = make_temp_workspace("edit-denied");
    let file_path = temp_dir.join("note.txt");
    fs::write(&file_path, "hello orca\n").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--cwd",
            temp_dir.to_str().unwrap(),
            "edit note.txt :: hello => hi",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello orca\n");

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "edit");
    assert_eq!(completed["payload"]["status"], "denied");
}

#[test]
fn update_plan_emits_plan_updated_event() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "plan implementing todo support",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "update_plan");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "3 items");

    let plan = find_event(&events, "plan.updated");
    assert_eq!(plan["payload"]["explanation"], "implementing todo support");
    assert_eq!(plan["payload"]["plan"][0]["step"], "Inspect references");
    assert_eq!(plan["payload"]["plan"][0]["status"], "completed");
    assert_eq!(
        plan["payload"]["plan"][1]["step"],
        "Implement task plan support"
    );
    assert_eq!(plan["payload"]["plan"][1]["status"], "in_progress");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "update_plan");
    assert_eq!(completed["payload"]["status"], "completed");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("Plan updated")
    );
}

#[test]
fn external_tool_descriptor_runs_from_orca_tools_dir() {
    let home = make_temp_workspace("external-home");
    let tools_dir = home.join("tools");
    fs::create_dir_all(&tools_dir).expect("tools dir");
    let workspace = make_temp_workspace("external-workspace");
    fs::create_dir_all(workspace.join("scripts")).expect("scripts dir");
    let output_file = workspace.join("deploy-output.txt");
    let script = workspace.join("scripts/deploy.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncat > {}\nprintf 'deploy ok'\n",
            shell_escape(&output_file)
        ),
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    fs::write(
        tools_dir.join("deploy.toml"),
        r#"
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { target = { type = "string", description = "environment" } }
"#,
    )
    .expect("write descriptor");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", &home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "--cwd",
            workspace.to_str().unwrap(),
            r#"external deploy {"target":"staging"}"#,
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&output_file).expect("external tool stdin"),
        r#"{"target":"staging"}"#
    );

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "deploy");
    assert_eq!(requested["payload"]["action"], "write");
    assert_eq!(requested["payload"]["target"], "deploy");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "deploy");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["output"], "deploy ok");
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("missing {event_type}"))
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn make_temp_workspace(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("orca-{name}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp workspace");
    dir
}

fn shell_escape(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
