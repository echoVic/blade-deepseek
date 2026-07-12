use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn agent_loop_fixture_completes_multi_turn() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "deepseek-fixture",
            "inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);

    // Should have two turns: first with tool call, second with final message
    let turn_events: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "turn.started")
        .collect();
    assert_eq!(turn_events.len(), 2, "expected exactly 2 turns");

    // First turn should have reasoning + tool call
    assert!(find_event(&events, "assistant.reasoning.delta").is_some());
    assert!(find_event(&events, "tool.call.requested").is_some());
    assert!(find_event(&events, "tool.call.completed").is_some());

    // Second turn should produce the final message
    let message_events: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "assistant.message.delta")
        .collect();
    assert!(
        message_events.iter().any(|e| e["payload"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("fixture completed")),
        "expected final message from fixture"
    );

    // Session should complete with success
    let last = events.last().unwrap();
    assert_eq!(last["type"], "session.completed");
    assert_eq!(last["payload"]["status"], "success");
}

#[test]
fn agent_loop_deepseek_without_key_fails_on_first_turn() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env_remove("DEEPSEEK_API_KEY")
        .env("HOME", "/tmp/orca_test_no_home")
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "deepseek",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .expect("should have error event");
    assert!(
        error["payload"]["message"]
            .as_str()
            .unwrap()
            .contains("DEEPSEEK_API_KEY")
    );
}

#[test]
fn failed_multi_call_turn_has_one_terminal_per_assistant_call() {
    let home = tempdir().expect("temporary ORCA_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--save-history",
            "--provider",
            "mock",
            "subagent batch terminal_boundary",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let records = parse_jsonl(&fs::read(only_session_file(home.path())).expect("read session"));
    let assistant_call_ids = records
        .iter()
        .filter(|record| {
            record["type"] == "conversation.message" && record["message"]["role"] == "assistant"
        })
        .flat_map(|record| {
            record["message"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|call| call["id"].as_str())
        .collect::<Vec<_>>();
    let persisted_terminal_ids = records
        .iter()
        .filter(|record| {
            record["type"] == "conversation.message" && record["message"]["role"] == "tool"
        })
        .filter_map(|record| record["message"]["tool_call_id"].as_str())
        .collect::<Vec<_>>();
    let requested_event_ids = event_ids(&events, "tool.call.requested");
    let terminal_result_ids = event_ids(&events, "tool.call.completed");

    assert_eq!(
        assistant_call_ids,
        ["mock-tool-1", "mock-tool-2", "mock-tool-3"]
    );
    assert_eq!(persisted_terminal_ids, assistant_call_ids);
    assert_eq!(requested_event_ids, assistant_call_ids);
    assert_eq!(terminal_result_ids, assistant_call_ids);
    for id in assistant_call_ids {
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["type"] == "tool.call.requested" && event["payload"]["id"] == id
                })
                .count(),
            1,
            "expected one requested event for {id}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["type"] == "tool.call.completed" && event["payload"]["id"] == id
                })
                .count(),
            1,
            "expected one terminal event for {id}"
        );
    }

    let statuses = events
        .iter()
        .filter(|event| event["type"] == "tool.call.completed")
        .map(|event| event["payload"]["status"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(statuses, ["failed", "completed", "completed"]);
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> Option<&'a Value> {
    events.iter().find(|event| event["type"] == event_type)
}

fn event_ids<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter(|event| event["type"] == event_type)
        .filter_map(|event| event["payload"]["id"].as_str())
        .collect()
}

fn only_session_file(home: &Path) -> PathBuf {
    let mut files = Vec::new();
    collect_jsonl_files(&home.join("sessions"), &mut files);
    assert_eq!(files.len(), 1, "expected exactly one persisted session");
    files.pop().unwrap()
}

fn collect_jsonl_files(path: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).expect("read session directory") {
        let path = entry.expect("session entry").path();
        if path.is_dir() {
            collect_jsonl_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
