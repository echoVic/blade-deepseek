pub mod context;
pub mod conversation;
pub mod deepseek_fixture;
pub mod deepseek_http;
pub mod http_client;
pub mod streaming;
pub mod system_prompt;
pub mod tool_schema;

use serde::{Deserialize, Serialize};

use crate::config::ProviderKind;
use crate::provider::conversation::{Conversation, RawToolCall};
use crate::runtime::cancel::CancelToken;
use crate::tools::ToolRequest;

pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub tools_override: Option<Vec<serde_json::Value>>,
}

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

pub struct ProviderResponse {
    pub steps: Vec<ProviderStep>,
    pub assistant_content: Option<String>,
    pub assistant_reasoning: Option<String>,
    pub tool_calls: Vec<RawToolCall>,
}

pub fn call(
    kind: ProviderKind,
    conversation: &Conversation,
    config: &ProviderConfig,
) -> ProviderResponse {
    match kind {
        ProviderKind::Mock => mock_call(conversation),
        ProviderKind::DeepSeekFixture => {
            let has_tool_results = conversation
                .messages
                .iter()
                .any(|m| matches!(m, conversation::Message::Tool { .. }));

            if has_tool_results {
                let msg =
                    "DeepSeek fixture completed after reading repository context.".to_string();
                ProviderResponse {
                    steps: vec![ProviderStep::MessageDelta(msg.clone())],
                    assistant_content: Some(msg),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                }
            } else {
                let steps = deepseek_fixture::plan();
                let tool_calls: Vec<RawToolCall> = steps
                    .iter()
                    .filter_map(|s| {
                        if let ProviderStep::ToolCall(req) = s {
                            Some(RawToolCall {
                                id: req.id.clone(),
                                function_name: req.name.as_str().to_string(),
                                arguments: "{}".to_string(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                ProviderResponse {
                    steps,
                    assistant_content: None,
                    assistant_reasoning: Some(
                        "DeepSeek fixture reasoning: inspect the repository context before answering."
                            .to_string(),
                    ),
                    tool_calls,
                }
            }
        }
        ProviderKind::DeepSeek => deepseek_http::call(conversation, config),
    }
}

pub fn call_streaming(
    kind: ProviderKind,
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> ProviderResponse {
    match kind {
        ProviderKind::Mock | ProviderKind::DeepSeekFixture => {
            let response = call(kind, conversation, config);
            for step in &response.steps {
                on_step(step);
            }
            response
        }
        ProviderKind::DeepSeek => {
            deepseek_http::call_streaming(conversation, config, cancel, on_step)
        }
    }
}

fn mock_call(conversation: &Conversation) -> ProviderResponse {
    let has_tool_results = conversation
        .messages
        .iter()
        .any(|m| matches!(m, conversation::Message::Tool { .. }));

    if has_tool_results {
        let msg = "Mock completed after tool execution.".to_string();
        return ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta("Mock reasoning.".to_string()),
                ProviderStep::MessageDelta(msg.clone()),
            ],
            assistant_content: Some(msg),
            assistant_reasoning: Some("Mock reasoning.".to_string()),
            tool_calls: Vec::new(),
        };
    }

    let prompt = conversation.last_user_message().unwrap_or("");

    if prompt.trim() == "mock_fail" {
        return ProviderResponse {
            steps: vec![ProviderStep::Error(
                "mock child failure requested".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
        };
    }

    if prompt.trim() == "mock_history_echo" {
        let users = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                conversation::Message::User(content) => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let message = format!("Mock history users: {users}");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
        };
    }

    if let Some(tool_request) = parse_mock_prompt(prompt) {
        let raw_call = RawToolCall {
            id: tool_request.id.clone(),
            function_name: tool_request.name.as_str().to_string(),
            arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
        };
        let reasoning = "Mock runtime is preserving the DeepSeek reasoning channel.".to_string();
        ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta(reasoning.clone()),
                ProviderStep::ToolCall(tool_request),
            ],
            assistant_content: None,
            assistant_reasoning: Some(reasoning),
            tool_calls: vec![raw_call],
        }
    } else {
        let reasoning = "Mock runtime is preserving the DeepSeek reasoning channel.";
        let message = "Mock runtime completed the headless harness contract.";
        ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta(reasoning.to_string()),
                ProviderStep::MessageDelta(message.to_string()),
            ],
            assistant_content: Some(message.to_string()),
            assistant_reasoning: Some(reasoning.to_string()),
            tool_calls: Vec::new(),
        }
    }
}

fn parse_mock_prompt(prompt: &str) -> Option<ToolRequest> {
    use crate::approval::policy::ActionKind;
    use crate::tools::ToolName;

    let prompt = prompt.trim();

    if let Some(rest) = prompt.strip_prefix("read ") {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some(rest.to_string()),
            raw_arguments: None,
        });
    }

    if prompt == "git status" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::GitStatus,
            action: ActionKind::Read,
            target: Some(".".to_string()),
            raw_arguments: None,
        });
    }

    if let Some(rest) = prompt.strip_prefix("subagent ") {
        let description = rest.trim();
        let prompt = if description == "mock_fail" {
            "mock_fail".to_string()
        } else {
            description.to_string()
        };
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some(description.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": description,
                    "prompt": prompt
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("grep ") {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some(rest.to_string()),
            raw_arguments: None,
        });
    }

    if let Some(rest) = prompt.strip_prefix("bash ") {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(rest.to_string()),
            raw_arguments: None,
        });
    }

    if let Some(rest) = prompt.strip_prefix("edit ")
        && let Some((file, replacement)) = rest.split_once(" :: ")
    {
        let (old, new) = replacement.split_once(" => ").unwrap_or((replacement, ""));
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some(file.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "path": file,
                    "old_text": old,
                    "new_text": new
                })
                .to_string(),
            ),
        });
    }

    if prompt.contains("write") {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("file.txt".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "path": "file.txt",
                    "old_text": "placeholder",
                    "new_text": "content"
                })
                .to_string(),
            ),
        });
    }

    None
}
