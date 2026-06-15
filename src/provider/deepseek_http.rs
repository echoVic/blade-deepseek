use std::env;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::policy::ActionKind;
use crate::provider::conversation::Conversation;
use crate::provider::tool_schema::deepseek_tools_schema;
use crate::provider::{ProviderReplayState, ProviderResponse, ProviderStep};
use crate::tools::{ToolName, ToolRequest};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-chat";

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

pub fn call(conversation: &Conversation) -> ProviderResponse {
    match request_chat(conversation) {
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

fn request_chat(conversation: &Conversation) -> Result<ProviderResponse, String> {
    let api_key = env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY is required for --provider deepseek".to_string())?;
    let base_url = env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let model = env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let messages = conversation_to_api_messages(conversation);
    let tools = deepseek_tools_schema();

    let request = ChatRequest {
        model,
        messages,
        stream: false,
        tools: Some(tools),
    };

    let response = reqwest::blocking::Client::new()
        .post(url)
        .bearer_auth(api_key)
        .json(&request)
        .send()
        .map_err(|error| format!("request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("request returned error: {error}"))?
        .json::<ChatResponse>()
        .map_err(|error| format!("invalid response: {error}"))?;

    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "response did not contain choices".to_string())?;

    let message = choice.message;
    let _finish_reason = choice.finish_reason.unwrap_or_default();

    let mut steps = Vec::new();
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
