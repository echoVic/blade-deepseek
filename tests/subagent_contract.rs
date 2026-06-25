use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn subagent_tool_runs_child_agent_and_emits_events() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let requested = find_event(&events, "tool.call.requested");
    assert_eq!(requested["payload"]["name"], "subagent");
    assert_eq!(requested["payload"]["action"], "read");
    assert_eq!(requested["payload"]["target"], "inspect repo");

    let started = find_event(&events, "subagent.started");
    assert_eq!(started["payload"]["id"], "mock-tool-1");
    assert_eq!(started["payload"]["description"], "inspect repo");

    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["id"], "mock-tool-1");
    assert_eq!(completed["payload"]["description"], "inspect repo");
    assert_eq!(completed["payload"]["status"], "success");
    assert!(
        completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("Mock runtime completed")
    );
    assert_eq!(completed["payload"]["error"], Value::Null);

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "completed");
    assert!(
        tool_completed["payload"]["output"]
            .as_str()
            .unwrap()
            .contains("Subagent status: success")
    );
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn async_subagent_launches_without_blocking_parent_tool() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "subagent async inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "tool.call.completed");
    assert_eq!(completed["payload"]["name"], "subagent");
    assert_eq!(completed["payload"]["status"], "completed");

    let payload: Value =
        serde_json::from_str(completed["payload"]["output"].as_str().unwrap()).unwrap();
    assert_eq!(payload["status"], "async_launched");
    assert!(payload["agent_id"].as_str().unwrap().starts_with("task-"));
    assert_eq!(payload["description"], "inspect repo");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn nested_subagent_calls_are_rejected() {
    let orca_home = tempdir().expect("temp orca home");
    std::fs::write(
        orca_home.path().join("config.toml"),
        "[subagents]\nmax_depth = 1\n",
    )
    .expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", orca_home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent subagent inner task",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["status"], "failed");
    assert!(
        completed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("subagent max depth 1 reached")
    );

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "failed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

#[test]
fn subagent_child_failure_fails_parent_run() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "subagent mock_fail",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "subagent.completed");
    assert_eq!(completed["payload"]["status"], "failed");
    assert!(
        completed["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("mock child failure requested")
    );

    let tool_completed = find_event(&events, "tool.call.completed");
    assert_eq!(tool_completed["payload"]["name"], "subagent");
    assert_eq!(tool_completed["payload"]["status"], "failed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
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
