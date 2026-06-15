use std::process::Command;

use serde_json::Value;

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
fn agent_loop_fixture_max_turns_exhausted() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "deepseek-fixture",
            "--max-turns",
            "1",
            "inspect repo",
        ])
        .output()
        .expect("run orca");

    // max_turns=1: first turn does tool call, then tries turn 2 but exceeds limit
    assert_eq!(output.status.code(), Some(4), "expected exit code 4 (budget_exhausted)");

    let events = parse_jsonl(&output.stdout);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "session.completed");
    assert_eq!(last["payload"]["status"], "budget_exhausted");
}

#[test]
fn agent_loop_deepseek_without_key_fails_on_first_turn() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env_remove("DEEPSEEK_API_KEY")
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

fn find_event<'a>(events: &'a [Value], event_type: &str) -> Option<&'a Value> {
    events.iter().find(|event| event["type"] == event_type)
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
