use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

#[test]
fn server_mode_accepts_submit_and_streams_protocol_events() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":1,"op":"submit","prompt":"hello from server"}}"#
        )
        .expect("write submit request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 4);
    assert!(events.iter().all(|event| event["id"] == 1));
    assert!(events.iter().all(|event| event.get("type").is_none()));

    assert!(has_event(&events, "turn_started"));
    assert!(has_event(&events, "reasoning_delta"));
    assert!(has_event(&events, "message_delta"));

    let completed = events
        .iter()
        .find(|event| event["event"] == "turn_completed")
        .expect("turn_completed event");
    assert_eq!(completed["status"], "success");
}

fn has_event(events: &[Value], event: &str) -> bool {
    events.iter().any(|value| value["event"] == event)
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
