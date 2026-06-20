use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    PreModelCall,
    PostModelCall,
    OnBudgetWarning,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::PreModelCall => "pre_model_call",
            Self::PostModelCall => "post_model_call",
            Self::OnBudgetWarning => "on_budget_warning",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookConfig {
    pub event: HookEvent,
    pub command: String,
    pub tool: Option<String>,
}
