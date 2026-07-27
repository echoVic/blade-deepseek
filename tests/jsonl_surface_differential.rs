use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

#[test]
fn released_v0_2_50_submit_wire_remains_byte_stable_after_identity_normalization() {
    let home = tempfile::tempdir().expect("create isolated ORCA_HOME");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        Path::new("/"),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust fixture workspace");

    let mut child = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn JSONL server");
    child
        .stdin
        .take()
        .expect("server stdin")
        .write_all(include_bytes!("fixtures/jsonl-v0.2.50/requests.jsonl"))
        .expect("write fixture requests");
    let output = child.wait_with_output().expect("wait for JSONL server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut actual = parse_jsonl(&output.stdout);
    normalize_dynamic_identities(&mut actual);
    let expected = parse_jsonl(include_bytes!(
        "fixtures/jsonl-v0.2.50/expected-events.jsonl"
    ));
    assert_eq!(actual, expected);
}

fn parse_jsonl(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid fixture JSONL"))
        .collect()
}

fn normalize_dynamic_identities(events: &mut [Value]) {
    let mut identities = HashMap::new();
    for event in events.iter() {
        record_identity(&mut identities, event.get("threadId"), "<thread>");
        record_identity(&mut identities, event.get("turnId"), "<turn>");
        record_identity(
            &mut identities,
            event.get("task").and_then(|task| task.get("task_id")),
            "<task>",
        );
        if let Some(item) = event.get("item") {
            let placeholder = match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => Some("<reasoning-item>"),
                Some("agent_message") => Some("<message-item>"),
                _ => None,
            };
            if let Some(placeholder) = placeholder {
                record_identity(&mut identities, item.get("id"), placeholder);
            }
        }
    }
    for event in events {
        replace_identities(event, &identities);
    }
}

fn record_identity(
    identities: &mut HashMap<String, &'static str>,
    value: Option<&Value>,
    placeholder: &'static str,
) {
    if let Some(value) = value.and_then(Value::as_str) {
        identities.insert(value.to_string(), placeholder);
    }
}

fn replace_identities(value: &mut Value, identities: &HashMap<String, &'static str>) {
    match value {
        Value::String(text) => {
            if let Some(replacement) = identities.get(text) {
                *text = (*replacement).to_string();
            }
        }
        Value::Array(values) => {
            for value in values {
                replace_identities(value, identities);
            }
        }
        Value::Object(fields) => {
            for value in fields.values_mut() {
                replace_identities(value, identities);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
