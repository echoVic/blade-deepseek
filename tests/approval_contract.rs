use std::process::Command;

use serde_json::Value;

#[test]
fn read_only_denies_write_requests() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "read-only",
            "write a file",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(3));

    let events = parse_jsonl(&output.stdout);
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "approval.requested")
    );
    assert!(events.iter().any(|event| {
        event["type"] == "approval.resolved" && event["payload"]["decision"] == "deny"
    }));
    assert_eq!(
        events.last().unwrap()["payload"]["status"],
        "approval_required"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
