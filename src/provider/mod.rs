pub mod deepseek_fixture;
pub mod deepseek_http;

use serde::{Deserialize, Serialize};

use crate::config::ProviderKind;
use crate::tools::ToolRequest;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderReplayState {
    pub provider: &'static str,
    pub reasoning_content: String,
    pub tool_call_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum ProviderStep {
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCall(ToolRequest),
    ReplayState(ProviderReplayState),
    Error(String),
}

pub fn plan(kind: ProviderKind, prompt: &str) -> Vec<ProviderStep> {
    match kind {
        ProviderKind::Mock => mock_plan(prompt),
        ProviderKind::DeepSeekFixture => deepseek_fixture::plan(prompt),
        ProviderKind::DeepSeek => deepseek_http::plan(prompt),
    }
}

fn mock_plan(_prompt: &str) -> Vec<ProviderStep> {
    vec![
        ProviderStep::ReasoningDelta(
            "Mock runtime is preserving the DeepSeek reasoning channel.".to_string(),
        ),
        ProviderStep::MessageDelta(
            "Mock runtime completed the headless harness contract.".to_string(),
        ),
    ]
}
