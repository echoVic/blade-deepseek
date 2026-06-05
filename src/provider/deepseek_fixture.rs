use crate::approval::policy::ActionKind;
use crate::provider::{ProviderReplayState, ProviderStep};
use crate::tools::{ToolName, ToolRequest};

pub fn plan(_prompt: &str) -> Vec<ProviderStep> {
    let tool_request = ToolRequest {
        id: "fixture-tool-1".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Read,
        target: Some("README.md".to_string()),
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
        ProviderStep::MessageDelta(
            "DeepSeek fixture completed after reading repository context.".to_string(),
        ),
    ]
}
