use orca_core::approval_types::ActionKind;
use orca_core::provider_types::{ProviderReplayState, ProviderStep};
use orca_core::tool_types::{ToolName, ToolRequest};

pub fn plan() -> Vec<ProviderStep> {
    let tool_request = ToolRequest {
        id: "fixture-tool-1".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Read,
        target: Some("README.md".to_string()),
        raw_arguments: Some(serde_json::json!({ "path": "README.md" }).to_string()),
    };

    vec![
        ProviderStep::ReasoningDelta(
            "DeepSeek fixture reasoning: inspect the repository context before answering."
                .to_string(),
        ),
        ProviderStep::ReplayState(ProviderReplayState {
            provider: "deepseek",
            reasoning_content:
                "DeepSeek fixture reasoning: inspect the repository context before answering."
                    .to_string(),
            tool_call_ids: vec![tool_request.id.clone()],
        }),
        ProviderStep::ToolCall(tool_request),
    ]
}
