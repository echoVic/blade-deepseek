use std::process::Command;

use serde_json::Value;

#[test]
fn workflow_schema_is_registered_with_official_fields() {
    let registry = orca_tools::registry::default_tool_registry();
    let tool = registry.get("Workflow").expect("Workflow tool registered");
    let schema = tool.schema();
    let properties = &schema["function"]["parameters"]["properties"];

    assert_eq!(schema["function"]["name"], "Workflow");
    assert!(properties.get("script").is_some());
    assert!(properties.get("name").is_some());
    assert!(properties.get("description").is_some());
    assert!(properties.get("title").is_some());
    assert!(properties.get("args").is_some());
    assert!(properties.get("scriptPath").is_some());
    assert!(properties.get("resumeFromRunId").is_some());
}

#[test]
fn mock_provider_can_request_workflow_tool() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "workflow inline",
        ])
        .output()
        .expect("run orca");

    let events = parse_jsonl(&output.stdout);
    let requested = events
        .iter()
        .find(|event| event["type"] == "tool.call.requested")
        .expect("tool requested");
    assert_eq!(requested["payload"]["name"], "Workflow");
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
