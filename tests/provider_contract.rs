use std::process::Command;

use serde_json::Value;

#[test]
fn deepseek_fixture_preserves_reasoning_and_replay_state() {
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
    assert_eq!(events[0]["payload"]["provider"], "deepseek-fixture");

    let reasoning = find_event(&events, "assistant.reasoning.delta");
    assert!(
        reasoning["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("DeepSeek fixture reasoning")
    );

    let replay = find_event(&events, "provider.replay.updated");
    assert_eq!(replay["payload"]["provider"], "deepseek");
    assert!(
        replay["payload"]["reasoning_content"]
            .as_str()
            .unwrap()
            .contains("DeepSeek fixture reasoning")
    );
    assert_eq!(replay["payload"]["tool_call_ids"][0], "fixture-tool-1");

    let tool = find_event(&events, "tool.call.requested");
    assert_eq!(tool["payload"]["id"], "fixture-tool-1");
    assert_eq!(tool["payload"]["name"], "read_file");

    assert!(!events.iter().any(|event| {
        event["type"] == "assistant.message.delta"
            && event["payload"]["text"]
                .as_str()
                .unwrap_or("")
                .contains("Mock runtime completed one tool request")
    }));

    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn deepseek_provider_without_api_key_emits_error_and_fails() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env_remove("DEEPSEEK_API_KEY")
        .env("HOME", "/tmp/orca_test_no_home")
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "deepseek",
            "inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let error = find_event(&events, "error");
    assert!(
        error["payload"]["message"]
            .as_str()
            .unwrap()
            .contains("DEEPSEEK_API_KEY")
    );
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
