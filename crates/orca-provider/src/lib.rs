pub mod context;
pub mod deepseek_fixture;
pub mod deepseek_http;
pub mod http_client;
pub mod streaming;
pub mod summary_cache;
pub mod system_prompt;
pub mod tool_schema;

use orca_core::approval_types::ActionKind;
use orca_core::cancel::CancelToken;
use orca_core::config::ProviderKind;
use orca_core::conversation::{Conversation, Message, RawToolCall};
use orca_core::external_config::ExternalToolConfig;
use orca_core::provider_types::{ProviderResponse, ProviderStep, Usage};
use orca_core::tool_types::{ToolName, ToolRequest};
use orca_mcp::McpRegistry;

#[derive(Clone)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub tools_override: Option<Vec<serde_json::Value>>,
    pub mcp_registry: Option<McpRegistry>,
    pub external_tools: Vec<ExternalToolConfig>,
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
                .any(|m| matches!(m, Message::Tool { .. }));

            if has_tool_results {
                let msg =
                    "DeepSeek fixture completed after reading repository context.".to_string();
                ProviderResponse {
                    steps: vec![ProviderStep::MessageDelta(msg.clone())],
                    assistant_content: Some(msg),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
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
                    usage: None,
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
        .any(|m| matches!(m, Message::Tool { .. }));
    let prompt = conversation.last_user_message().unwrap_or("");

    if prompt.trim() == "bad_plan_then_fix" && has_tool_results {
        let has_fixed_plan = conversation.messages.iter().any(|m| match m {
            Message::Tool { content, .. } => content.contains("Plan updated"),
            _ => false,
        });
        if has_fixed_plan {
            let msg = "Mock completed after fixing malformed tool arguments.".to_string();
            return ProviderResponse {
                steps: vec![ProviderStep::MessageDelta(msg.clone())],
                assistant_content: Some(msg),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
        }
        let saw_schema_error = conversation.messages.iter().any(|m| match m {
            Message::Tool { content, .. } => {
                content.contains("tool arguments failed schema validation")
            }
            _ => false,
        });
        if saw_schema_error {
            let tool_request =
                valid_mock_plan_request(Some("Recovered from schema validation failure"));
            let raw_call = RawToolCall {
                id: tool_request.id.clone(),
                function_name: tool_request.name.as_str().to_string(),
                arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
            };
            return ProviderResponse {
                steps: vec![ProviderStep::ToolCall(tool_request)],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: vec![raw_call],
                usage: None,
            };
        }
    }

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
            usage: None,
        };
    }

    if prompt.trim() == "mock_fail" {
        return ProviderResponse {
            steps: vec![ProviderStep::Error(
                "mock child failure requested".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if let Some(key) = prompt.trim().strip_prefix("mock_flaky_once ") {
        static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::OnceLock::new();
        let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
        let should_fail = seen
            .lock()
            .map(|mut keys| keys.insert(key.to_string()))
            .unwrap_or(false);
        if should_fail {
            return ProviderResponse {
                steps: vec![ProviderStep::Error(format!(
                    "mock transient failure requested for {key}"
                ))],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
        }
        let message = format!("Mock runtime completed after transient failure for {key}.");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "mock_usage" {
        let reasoning = "Mock runtime is preserving the DeepSeek reasoning channel.";
        let message = "Mock runtime completed with usage accounting.";
        return ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta(reasoning.to_string()),
                ProviderStep::MessageDelta(message.to_string()),
            ],
            assistant_content: Some(message.to_string()),
            assistant_reasoning: Some(reasoning.to_string()),
            tool_calls: Vec::new(),
            usage: Some(Usage {
                input_tokens: 120,
                output_tokens: 30,
                cache_tokens: 10,
            }),
        };
    }

    if prompt.trim() == "mock_history_echo" {
        let users = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User { content, .. } => Some(content.as_str()),
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
            usage: None,
        };
    }

    if prompt.trim() == "mock_system_echo" {
        let systems = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::System { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let message = format!("Mock system messages: {systems}");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "mock_silent_final" {
        return ProviderResponse {
            steps: Vec::new(),
            assistant_content: Some("Mock silent final response.".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
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
            usage: None,
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
            usage: None,
        }
    }
}

fn parse_mock_prompt(prompt: &str) -> Option<ToolRequest> {
    let prompt = prompt.trim();

    if let Some(rest) = prompt.strip_prefix("read ") {
        let path = rest.to_string();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some(path.clone()),
            raw_arguments: Some(serde_json::json!({ "path": path }).to_string()),
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
        let (mode, description) = if let Some(description) = rest.trim().strip_prefix("async ") {
            (Some("async"), description.trim())
        } else {
            (None, rest.trim())
        };
        let prompt = if description == "mock_fail" {
            "mock_fail".to_string()
        } else {
            description.to_string()
        };
        let mut arguments = serde_json::json!({
            "description": description,
            "prompt": prompt
        });
        if let Some(mode) = mode {
            arguments["mode"] = serde_json::Value::String(mode.to_string());
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some(description.to_string()),
            raw_arguments: Some(arguments.to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("subagent_status ") {
        let agent_id = rest.trim();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::SubagentStatus,
            action: ActionKind::Read,
            target: Some(agent_id.to_string()),
            raw_arguments: Some(serde_json::json!({ "agent_id": agent_id }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow ") {
        let mode = rest.trim();
        let script = "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;";
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Workflow,
            action: ActionKind::Agent,
            target: Some(mode.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "script": script,
                    "args": { "mode": mode }
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("plan ") {
        let explanation = if rest.trim().is_empty() {
            None
        } else {
            Some(rest.trim())
        };
        return Some(valid_mock_plan_request(explanation));
    }

    if prompt == "bad_plan_then_fix" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::UpdatePlan,
            action: ActionKind::Read,
            target: Some("1 items".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "plan": [
                        {
                            "completed": "Inspect references"
                        }
                    ]
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("ask ") {
        let question = rest.trim();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::RequestUserInput,
            action: ActionKind::Read,
            target: Some(question.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "question": question,
                    "choices": ["yes", "no"]
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("grep ") {
        let pattern = rest.to_string();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some(pattern.clone()),
            raw_arguments: Some(serde_json::json!({ "pattern": pattern }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("bash ") {
        let command = rest.to_string();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.clone()),
            raw_arguments: Some(serde_json::json!({ "command": command }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("external ")
        && let Some((name, args)) = rest.trim().split_once(' ')
    {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::External(name.to_string()),
            action: ActionKind::Write,
            target: Some(name.to_string()),
            raw_arguments: Some(args.to_string()),
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

fn valid_mock_plan_request(explanation: Option<&str>) -> ToolRequest {
    let arguments = serde_json::json!({
        "explanation": explanation,
        "plan": [
            {
                "step": "Inspect references",
                "status": "completed"
            },
            {
                "step": "Implement task plan support",
                "status": "in_progress"
            },
            {
                "step": "Verify behavior",
                "status": "pending"
            }
        ]
    })
    .to_string();
    ToolRequest {
        id: "mock-tool-1".to_string(),
        name: ToolName::UpdatePlan,
        action: ActionKind::Read,
        target: Some("3 items".to_string()),
        raw_arguments: Some(arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_prompt_parses_async_subagent_mode() {
        let request = parse_mock_prompt("subagent async inspect repo").expect("tool request");
        assert_eq!(request.name, ToolName::Subagent);
        assert_eq!(request.target.as_deref(), Some("inspect repo"));

        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(arguments["description"], "inspect repo");
        assert_eq!(arguments["prompt"], "inspect repo");
        assert_eq!(arguments["mode"], "async");
    }

    #[test]
    fn mock_flaky_once_fails_once_then_succeeds() {
        let mut conversation = Conversation::new();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        conversation.add_user(format!("mock_flaky_once {unique}"));

        let first = mock_call(&conversation);
        assert!(matches!(first.steps.first(), Some(ProviderStep::Error(_))));

        let second = mock_call(&conversation);
        assert!(
            second
                .assistant_content
                .as_deref()
                .unwrap_or_default()
                .contains("Mock runtime completed after transient failure")
        );
    }
}
