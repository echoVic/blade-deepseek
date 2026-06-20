use orca_core::approval_types::ActionKind;
use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::provider_types::ProviderStep;
use orca_core::tool_types::ToolName;
use orca_provider::{call, ProviderConfig};
use serde_json::Value;

#[test]
fn workflow_schema_is_registered_with_official_fields() {
    let registry = orca_tools::registry::default_tool_registry();
    let tool = registry.get("Workflow").expect("Workflow tool registered");
    let schema = tool.schema();
    let properties = &schema["function"]["parameters"]["properties"];

    assert_eq!(schema["function"]["name"], "Workflow");
    assert_eq!(tool.action_kind(), ActionKind::Agent);
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
    let mut conversation = Conversation::new();
    conversation.add_user("workflow inline".to_string());

    let response = call(
        ProviderKind::Mock,
        &conversation,
        &ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        },
    );

    let tool_request = response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(request),
            _ => None,
        })
        .expect("tool request");

    assert_eq!(tool_request.name, ToolName::Workflow);
    assert_eq!(tool_request.action, ActionKind::Agent);

    let raw_arguments = tool_request
        .raw_arguments
        .as_deref()
        .expect("raw arguments");
    let raw_arguments: Value = serde_json::from_str(raw_arguments).expect("valid raw arguments");
    assert_eq!(raw_arguments["script"], expected_workflow_script());
    assert_eq!(raw_arguments["args"]["mode"], "inline");
}

fn expected_workflow_script() -> &'static str {
    "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;"
}

#[test]
fn workflow_tool_launches_background_task_and_returns_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orca"))
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

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);

    let completed = events
        .iter()
        .find(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["name"] == "Workflow"
        })
        .expect("workflow tool completed");
    assert_eq!(completed["payload"]["status"], "completed");

    let output_text = completed["payload"]["output"].as_str().unwrap();
    let workflow_output: Value = serde_json::from_str(output_text).unwrap();
    assert_eq!(workflow_output["status"], "async_launched");
    assert_eq!(workflow_output["taskType"], "local_workflow");
    assert!(workflow_output["taskId"]
        .as_str()
        .unwrap()
        .starts_with("task-"));
    assert!(workflow_output["runId"]
        .as_str()
        .unwrap()
        .starts_with("workflow-run-"));

    assert!(events
        .iter()
        .any(|event| event["type"] == "workflow.started"));
    assert!(events
        .iter()
        .any(|event| event["type"] == "workflow.result.available"));
    let result_available_index = events
        .iter()
        .position(|event| event["type"] == "workflow.result.available")
        .expect("workflow result available event");
    let session_completed_index = events
        .iter()
        .position(|event| event["type"] == "session.completed")
        .expect("session completed event");
    assert!(
        result_available_index < session_completed_index,
        "workflow result should be emitted before session completion"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
