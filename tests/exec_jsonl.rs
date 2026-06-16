use std::process::Command;

use serde_json::Value;

#[test]
fn exec_outputs_jsonl_contract_and_success_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 5);
    assert_eq!(events[0]["version"], "1");
    assert_eq!(events[0]["type"], "session.started");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "assistant.reasoning.delta")
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "assistant.message.delta")
    );
    assert_eq!(events.last().unwrap()["type"], "session.completed");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");

    for (seq, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], seq);
        assert!(event["run_id"].as_str().unwrap().starts_with("run-"));
    }
}

#[test]
fn exec_emits_usage_event_when_provider_reports_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let usage = events
        .iter()
        .find(|event| event["type"] == "usage.updated")
        .expect("usage event");
    assert_eq!(usage["payload"]["input_tokens"], 120);
    assert_eq!(usage["payload"]["output_tokens"], 30);
    assert_eq!(usage["payload"]["cache_tokens"], 10);
    assert_eq!(usage["payload"]["total_tokens"], 150);
    assert!(usage["payload"]["estimated_cost_usd"].as_f64().unwrap() > 0.0);
}

#[test]
fn exec_stops_when_usage_exceeds_max_budget() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--max-budget",
            "0.000001",
            "mock_usage",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(4));

    let events = parse_jsonl(&output.stdout);
    assert!(events.iter().any(|event| {
        event["type"] == "error"
            && event["payload"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("budget")
    }));
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "budget_exhausted"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
