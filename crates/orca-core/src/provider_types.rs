use serde::{Deserialize, Serialize};

use crate::conversation::RawToolCall;
use crate::tool_types::ToolRequest;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

impl Usage {
    pub fn is_empty(self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0 && self.cache_tokens == 0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderReplayState {
    pub provider: &'static str,
    pub reasoning_content: String,
    pub tool_call_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCallProgress {
    pub id: String,
    pub function_name: Option<String>,
    pub arguments_bytes: usize,
}

#[derive(Clone, Debug)]
pub enum ProviderStep {
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCallProgress(ToolCallProgress),
    ToolCall(ToolRequest),
    ReplayState(ProviderReplayState),
    Error(String),
}

pub struct ProviderResponse {
    pub steps: Vec<ProviderStep>,
    pub assistant_content: Option<String>,
    pub assistant_reasoning: Option<String>,
    pub tool_calls: Vec<RawToolCall>,
    pub usage: Option<Usage>,
}
