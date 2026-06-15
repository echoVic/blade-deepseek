use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[test]
fn read_file_emits_tool_request_and_completed_events() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "read README.md",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "read_file");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "README.md");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "read_file");
    assert_eq!(completed["payload"]["status"], "completed");
    assert_eq!(completed["payload"]["truncated"], false);
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("# Orca")
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
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "grep Orca",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "grep");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "Orca");

    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "grep");
    assert_eq!(completed["payload"]["status"], "completed");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("README.md")
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
