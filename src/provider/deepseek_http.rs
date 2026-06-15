use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::policy::ActionKind;
use crate::provider::conversation::Conversation;
use crate::provider::tool_schema::deepseek_tools_schema;
use crate::provider::{ProviderConfig, ProviderReplayState, ProviderResponse, ProviderStep};
use crate::tools::{ToolName, ToolRequest};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCallRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiToolCallRequest {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ApiFunctionRequest,
}

#[derive(Debug, Serialize)]
struct ApiFunctionRequest {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ApiToolCallResponse>>,
}

#[derive(Debug, Deserialize)]
struct ApiToolCallResponse {
    id: String,
    function: ApiFunctionResponse,
}

#[derive(Debug, Deserialize)]
struct ApiFunctionResponse {
    name: String,
    arguments: String,
}

pub fn call(conversation: &Conversation, config: &ProviderConfig) -> ProviderResponse {
    match request_chat(conversation, config) {
        Ok(response) => response,
        Err(error) => ProviderResponse {
            steps: vec![ProviderStep::Error(format!(
                "DeepSeek provider error: {error}"
            ))],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
        },
    }
}

pub fn call_streaming(
    conversation: &Conversation,
    config: &ProviderConfig,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> ProviderResponse {
    match request_chat_streaming(conversation, config, on_step) {
        Ok(response) => response,
        Err(error) => {
            let step = ProviderStep::Error(format!("DeepSeek provider error: {error}"));
            on_step(&step);
            ProviderResponse {
                steps: vec![step],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
            }
        }
    }
}

fn request_chat_streaming(
    conversation: &Conversation,
    config: &ProviderConfig,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> Result<ProviderResponse, String> {
    let api_key = config.api_key.as_deref()
        .ok_or_else(|| "DEEPSEEK_API_KEY is required for --provider deepseek (set via config file, env var, or ~/.config/orca/config.toml)".to_string())?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let messages = conversation_to_api_messages(conversation);
    let tools = deepseek_tools_schema();

    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        tools: Some(tools),
    };

    let response = super::http_client::execute_streaming_with_retry(|client| {
        client.post(&url).bearer_auth(api_key).json(&request)
    })?;

    let mut steps = Vec::new();

    let stream_result = super::streaming::parse_sse_stream(response, |delta| {
        use super::streaming::StreamEvent;
        let step = match delta {
            StreamEvent::Reasoning(text) => ProviderStep::ReasoningDelta(text.to_string()),
            StreamEvent::Content(text) => ProviderStep::MessageDelta(text.to_string()),
        };
        on_step(&step);
        steps.push(step);
    })?;

    match stream_result.finish_reason.as_deref() {
        Some("length") => {
            return Err(
                "Response truncated: model hit max_tokens limit (finish_reason=length)".to_string(),
            );
        }
        Some("content_filter") => {
            return Err("Response blocked by content filter".to_string());
        }
        _ => {}
    }

    let mut raw_calls_for_history = Vec::new();
    for tc in &stream_result.tool_calls {
        raw_calls_for_history.push(crate::provider::conversation::RawToolCall {
            id: tc.id.clone(),
            function_name: tc.function_name.clone(),
            arguments: tc.arguments.clone(),
        });

        let tc_response = ApiToolCallResponse {
            id: tc.id.clone(),
            function: ApiFunctionResponse {
                name: tc.function_name.clone(),
                arguments: tc.arguments.clone(),
            },
        };
        match parse_tool_call(&tc_response) {
            Ok(tool_request) => {
                steps.push(ProviderStep::ToolCall(tool_request));
            }
            Err(error) => {
                steps.push(ProviderStep::Error(format!(
                    "failed to parse tool call '{}': {error}",
                    tc.function_name
                )));
            }
        }
    }

    let assistant_reasoning = if stream_result.reasoning.is_empty() {
        if !raw_calls_for_history.is_empty() {
            Some("(reasoning omitted)".to_string())
        } else {
            None
        }
    } else {
        if !raw_calls_for_history.is_empty() {
            let tool_call_ids: Vec<String> =
                raw_calls_for_history.iter().map(|tc| tc.id.clone()).collect();
            steps.push(ProviderStep::ReplayState(ProviderReplayState {
                provider: "deepseek",
                reasoning_content: stream_result.reasoning.clone(),
                tool_call_ids,
            }));
        }
        Some(stream_result.reasoning)
    };

    let assistant_content = if stream_result.content.is_empty() {
        None
    } else {
        Some(stream_result.content)
    };

    if steps.is_empty() {
        return Err("response did not contain content or tool calls".to_string());
    }

    Ok(ProviderResponse {
        steps,
        assistant_content,
        assistant_reasoning,
        tool_calls: raw_calls_for_history,
    })
}

fn request_chat(conversation: &Conversation, config: &ProviderConfig) -> Result<ProviderResponse, String> {
    let api_key = config.api_key.as_deref()
        .ok_or_else(|| "DEEPSEEK_API_KEY is required for --provider deepseek (set via config file, env var, or ~/.config/orca/config.toml)".to_string())?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let messages = conversation_to_api_messages(conversation);
    let tools = deepseek_tools_schema();

    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: false,
        tools: Some(tools),
    };

    let response = super::http_client::execute_with_retry(|client| {
        client.post(&url).bearer_auth(api_key).json(&request)
    })?
    .json::<ChatResponse>()
    .map_err(|error| format!("invalid response: {error}"))?;

    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "response did not contain choices".to_string())?;

    let message = choice.message;
    let finish_reason = choice.finish_reason.unwrap_or_default();

    let mut steps = Vec::new();

    match finish_reason.as_str() {
        "length" => {
            return Err("Response truncated: model hit max_tokens limit (finish_reason=length)".to_string());
        }
        "content_filter" => {
            return Err("Response blocked by content filter".to_string());
        }
        "stop" | "tool_calls" | "" => {}
        other => {
            steps.push(ProviderStep::Error(format!("Unexpected finish_reason: {other}")));
        }
    }

    let assistant_reasoning = message
        .reasoning_content
        .filter(|text| !text.is_empty());
    let assistant_content = message.content.filter(|text| !text.is_empty());

    if let Some(ref reasoning) = assistant_reasoning {
        steps.push(ProviderStep::ReasoningDelta(reasoning.clone()));
    }

    let raw_tool_calls = message.tool_calls.unwrap_or_default();
    let mut raw_calls_for_history = Vec::new();

    if !raw_tool_calls.is_empty() {
        let tool_call_ids: Vec<String> = raw_tool_calls.iter().map(|tc| tc.id.clone()).collect();

        if let Some(ref reasoning) = assistant_reasoning {
            steps.push(ProviderStep::ReplayState(ProviderReplayState {
                provider: "deepseek",
                reasoning_content: reasoning.clone(),
                tool_call_ids,
            }));
        }

        for tc in &raw_tool_calls {
            raw_calls_for_history.push(crate::provider::conversation::RawToolCall {
                id: tc.id.clone(),
                function_name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            });

            match parse_tool_call(tc) {
                Ok(tool_request) => {
                    steps.push(ProviderStep::ToolCall(tool_request));
                }
                Err(error) => {
                    steps.push(ProviderStep::Error(format!(
                        "failed to parse tool call '{}': {error}",
                        tc.function.name
                    )));
                }
            }
        }
    }

    if let Some(ref content) = assistant_content {
        steps.push(ProviderStep::MessageDelta(content.clone()));
    }

    if steps.is_empty() {
        return Err("response did not contain content or tool calls".to_string());
    }

    Ok(ProviderResponse {
        steps,
        assistant_content,
        assistant_reasoning,
        tool_calls: raw_calls_for_history,
    })
}

fn parse_tool_call(tc: &ApiToolCallResponse) -> Result<ToolRequest, String> {
    let args: Value = serde_json::from_str(&tc.function.arguments)
        .map_err(|e| format!("invalid arguments JSON: {e}"))?;

    let (name, action, target) = match tc.function.name.as_str() {
        "read_file" => (
            ToolName::ReadFile,
            ActionKind::Read,
            args["path"].as_str().map(String::from),
        ),
        "list_files" => (
            ToolName::ListFiles,
            ActionKind::Read,
            args["path"].as_str().map(String::from).or(Some(".".to_string())),
        ),
        "grep" => (
            ToolName::Grep,
            ActionKind::Read,
            args["pattern"].as_str().map(String::from),
        ),
        "bash" => (
            ToolName::Bash,
            ActionKind::Shell,
            args["command"].as_str().map(String::from),
        ),
        "edit" => (
            ToolName::Edit,
            ActionKind::Write,
            args["path"].as_str().map(String::from),
        ),
        "git_status" => (
            ToolName::GitStatus,
            ActionKind::Read,
            Some(".".to_string()),
        ),
        other => return Err(format!("unknown tool: {other}")),
    };

    Ok(ToolRequest {
        id: tc.id.clone(),
        name,
        action,
        target,
        raw_arguments: Some(tc.function.arguments.clone()),
    })
}

fn conversation_to_api_messages(conversation: &Conversation) -> Vec<ApiMessage> {
    use crate::provider::conversation::Message;

    conversation
        .messages
        .iter()
        .map(|msg| match msg {
            Message::System(content) => ApiMessage {
                role: "system".to_string(),
                content: Some(content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message::User(content) => ApiMessage {
                role: "user".to_string(),
                content: Some(content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                let api_tool_calls = if tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        tool_calls
                            .iter()
                            .map(|tc| ApiToolCallRequest {
                                id: tc.id.clone(),
                                call_type: "function".to_string(),
                                function: ApiFunctionRequest {
                                    name: tc.function_name.clone(),
                                    arguments: tc.arguments.clone(),
                                },
                            })
                            .collect(),
                    )
                };
                ApiMessage {
                    role: "assistant".to_string(),
                    content: content.clone(),
                    reasoning_content: reasoning_content.clone(),
                    tool_calls: api_tool_calls,
                    tool_call_id: None,
                }
            }
            Message::Tool {
                tool_call_id,
                content,
            } => ApiMessage {
                role: "tool".to_string(),
                content: Some(content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(tool_call_id.clone()),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::ActionKind;
    use crate::tools::ToolName;

    fn make_tc(name: &str, arguments: &str) -> ApiToolCallResponse {
        ApiToolCallResponse {
            id: "call_123".to_string(),
            function: ApiFunctionResponse {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    #[test]
    fn parse_read_file() {
        let tc = make_tc("read_file", r#"{"path":"src/main.rs"}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::ReadFile);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src/main.rs"));
        assert_eq!(req.id, "call_123");
    }

    #[test]
    fn parse_list_files_with_path() {
        let tc = make_tc("list_files", r#"{"path":"src/provider"}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src/provider"));
    }

    #[test]
    fn parse_list_files_without_path_defaults_to_dot() {
        let tc = make_tc("list_files", r#"{}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.target.as_deref(), Some("."));
    }

    #[test]
    fn parse_grep() {
        let tc = make_tc("grep", r#"{"pattern":"fn main","path":"src"}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::Grep);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("fn main"));
    }

    #[test]
    fn parse_bash() {
        let tc = make_tc("bash", r#"{"command":"cargo test"}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::Bash);
        assert_eq!(req.action, ActionKind::Shell);
        assert_eq!(req.target.as_deref(), Some("cargo test"));
    }

    #[test]
    fn parse_edit() {
        let tc = make_tc("edit", r#"{"path":"foo.rs","old_text":"a","new_text":"b"}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::Edit);
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.target.as_deref(), Some("foo.rs"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_git_status() {
        let tc = make_tc("git_status", r#"{}"#);
        let req = parse_tool_call(&tc).unwrap();
        assert_eq!(req.name, ToolName::GitStatus);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("."));
    }

    #[test]
    fn parse_unknown_tool_returns_error() {
        let tc = make_tc("unknown_tool", r#"{}"#);
        let err = parse_tool_call(&tc).unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let tc = make_tc("read_file", "not json");
        let err = parse_tool_call(&tc).unwrap_err();
        assert!(err.contains("invalid arguments JSON"));
    }
}
