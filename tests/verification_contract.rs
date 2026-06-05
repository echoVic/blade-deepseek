use std::process::Command;

use serde_json::Value;

#[test]
fn verifier_success_keeps_success_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--verifier",
            "printf ok",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    assert_eq!(
        find_event(&events, "verification.started")["payload"]["command"],
        "printf ok"
    );

    let completed = find_event(&events, "verification.completed");
    assert_eq!(completed["payload"]["success"], true);
    assert_eq!(completed["payload"]["stdout"], "ok");
    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn verifier_failure_maps_to_verification_failed() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--verifier",
            "exit 7",
            "hello",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(2));

    let events = parse_jsonl(&output.stdout);
    let completed = find_event(&events, "verification.completed");
    assert_eq!(completed["payload"]["success"], false);
    assert_eq!(completed["payload"]["exit_code"], 7);
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "verification_failed"
    );
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
