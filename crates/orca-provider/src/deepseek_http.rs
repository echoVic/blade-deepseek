use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::approval_types::ActionKind;
use orca_core::cancel::CancelToken;
use orca_core::conversation::{
    Conversation, Message, RawToolCall, SummaryState, normalize_tool_boundaries,
};
use orca_core::provider_types::{ProviderReplayState, ProviderResponse, ProviderStep, Usage};
use orca_core::tool_types::ToolRequest;
use orca_tools::registry;

use crate::ProviderConfig;
use crate::tool_schema::deepseek_tools_schema_with_mcp_and_external;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ApiMessage {
    pub(crate) role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCallRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
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
    usage: Option<ApiUsage>,
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

#[derive(Debug, Deserialize)]
pub struct ApiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
}

impl From<ApiUsage> for Usage {
    fn from(usage: ApiUsage) -> Self {
        let input_tokens = usage.prompt_tokens.unwrap_or_else(|| {
            usage.prompt_cache_hit_tokens.unwrap_or(0) + usage.prompt_cache_miss_tokens.unwrap_or(0)
        });
        let output_tokens = usage.completion_tokens.unwrap_or_else(|| {
            usage
                .total_tokens
                .unwrap_or(input_tokens)
                .saturating_sub(input_tokens)
        });
        Self {
            input_tokens,
            output_tokens,
            cache_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
        }
    }
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
            usage: None,
        },
    }
}

pub fn call_streaming(
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> ProviderResponse {
    match request_chat_streaming(conversation, config, cancel, on_step) {
        Ok(response) => response,
        Err(error) => {
            let step = ProviderStep::Error(format!("DeepSeek provider error: {error}"));
            on_step(&step);
            ProviderResponse {
                steps: vec![step],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            }
        }
    }
}

fn request_chat_streaming(
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> Result<ProviderResponse, String> {
    let api_key = config.api_key.as_deref().ok_or_else(|| {
        "DEEPSEEK_API_KEY is required (set via env var or ~/.orca/auth.json)".to_string()
    })?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let messages = conversation_to_api_messages(conversation);
    let tools = config.tools_override.clone().unwrap_or_else(|| {
        deepseek_tools_schema_with_mcp_and_external(
            config.mcp_registry.as_ref(),
            &config.external_tools,
        )
    });

    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        tools: Some(tools),
    };

    let response = crate::http_client::execute_streaming_with_retry(|client| {
        client.post(&url).bearer_auth(api_key).json(&request)
    })?;

    let mut steps = Vec::new();

    let stream_result = crate::streaming::parse_sse_stream(response, cancel, |delta| {
        use crate::streaming::StreamEvent;
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
        raw_calls_for_history.push(RawToolCall {
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
        match parse_tool_call(&tc_response, &config.external_tools) {
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
            let tool_call_ids: Vec<String> = raw_calls_for_history
                .iter()
                .map(|tc| tc.id.clone())
                .collect();
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
        usage: stream_result.usage,
    })
}

fn request_chat(
    conversation: &Conversation,
    config: &ProviderConfig,
) -> Result<ProviderResponse, String> {
    let api_key = config.api_key.as_deref().ok_or_else(|| {
        "DEEPSEEK_API_KEY is required (set via env var or ~/.orca/auth.json)".to_string()
    })?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let messages = conversation_to_api_messages(conversation);
    let tools = config.tools_override.clone().unwrap_or_else(|| {
        deepseek_tools_schema_with_mcp_and_external(
            config.mcp_registry.as_ref(),
            &config.external_tools,
        )
    });

    let request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: false,
        stream_options: None,
        tools: Some(tools),
    };

    let response = crate::http_client::execute_with_retry(|client| {
        client.post(&url).bearer_auth(api_key).json(&request)
    })?
    .json::<ChatResponse>()
    .map_err(|error| format!("invalid response: {error}"))?;

    let usage = response.usage.map(Usage::from);
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
            return Err(
                "Response truncated: model hit max_tokens limit (finish_reason=length)".to_string(),
            );
        }
        "content_filter" => {
            return Err("Response blocked by content filter".to_string());
        }
        "stop" | "tool_calls" | "" => {}
        other => {
            steps.push(ProviderStep::Error(format!(
                "Unexpected finish_reason: {other}"
            )));
        }
    }

    let assistant_reasoning = message.reasoning_content.filter(|text| !text.is_empty());
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
            raw_calls_for_history.push(RawToolCall {
                id: tc.id.clone(),
                function_name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            });

            match parse_tool_call(tc, &config.external_tools) {
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
        usage,
    })
}

fn parse_tool_call(
    tc: &ApiToolCallResponse,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
) -> Result<ToolRequest, String> {
    let args: Value = serde_json::from_str(&tc.function.arguments)
        .map_err(|e| format!("invalid arguments JSON: {e}"))?;

    let schema_name = tc.function.name.as_str();
    // ToolName::from_str has a catch-all that wraps unknown names as External(...),
    // so we must gate on the registry to reject truly unknown tools.
    let reg = registry::tool_registry_with_mcp_and_external(None, external_tools);
    let resolved = reg.resolve(schema_name);
    let is_known_tool = resolved.is_some()
        || schema_name.starts_with("mcp__")
        || external_tools.iter().any(|tool| tool.name == schema_name);
    if !is_known_tool {
        return Err(format!("unknown tool: {schema_name}"));
    }
    let name = registry::tool_name_from_schema_name(schema_name)
        .expect("known tool must parse to ToolName");
    let action = reg
        .resolve(schema_name)
        .map(|resolved| resolved.spec.capabilities.action_kind())
        .unwrap_or(ActionKind::Read);
    let target = match schema_name {
        "read_file" => args["path"].as_str().map(String::from),
        "list_files" => args["path"]
            .as_str()
            .map(String::from)
            .or(Some(".".to_string())),
        "glob" => args["path"]
            .as_str()
            .map(String::from)
            .or(Some(".".to_string())),
        "grep" => args["pattern"].as_str().map(String::from),
        "bash" => args["command"].as_str().map(String::from),
        "edit" => args["path"].as_str().map(String::from),
        "write_file" => args["path"].as_str().map(String::from),
        "git_status" => Some(".".to_string()),
        "subagent" => args["description"]
            .as_str()
            .map(String::from)
            .or_else(|| args["prompt"].as_str().map(String::from)),
        "web_search" => args["query"].as_str().map(String::from),
        "update_plan" => {
            let count = args["plan"].as_array().map(|plan| plan.len()).unwrap_or(0);
            Some(format!("{count} items"))
        }
        other if other.starts_with("mcp__") => Some(other.to_string()),
        other if external_tools.iter().any(|tool| tool.name == other) => Some(other.to_string()),
        _ => None,
    };

    Ok(ToolRequest {
        id: tc.id.clone(),
        name,
        action,
        target,
        raw_arguments: Some(tc.function.arguments.clone()),
    })
}

pub(crate) fn conversation_to_api_messages(conversation: &Conversation) -> Vec<ApiMessage> {
    let mut messages: Vec<ApiMessage> = Vec::new();
    let mut first_system_done = false;
    let mut safe_messages = conversation.messages.clone();
    normalize_tool_boundaries(&mut safe_messages);

    for msg in &safe_messages {
        let api_msg = match msg {
            Message::System { content, .. } => {
                let result = ApiMessage {
                    role: "system".to_string(),
                    content: Some(content.clone()),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                };
                if !first_system_done {
                    first_system_done = true;
                    messages.push(result);
                    inject_summary_messages(&conversation.summary, &mut messages);
                    continue;
                }
                result
            }
            Message::User { content, .. } => ApiMessage {
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
                ..
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
                ..
            } => ApiMessage {
                role: "tool".to_string(),
                content: Some(content.clone()),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(tool_call_id.clone()),
            },
        };
        messages.push(api_msg);
    }

    if !first_system_done && !conversation.summary.is_empty() {
        inject_summary_messages(&conversation.summary, &mut messages);
    }

    if !conversation.volatile.is_empty() {
        let overlay = conversation.volatile.render();
        if let Some(last) = messages.last_mut() {
            let existing = last.content.take().unwrap_or_default();
            last.content = Some(if existing.is_empty() {
                overlay
            } else {
                format!("{existing}\n\n{overlay}")
            });
        }
    }

    messages
}

fn inject_summary_messages(summary: &SummaryState, messages: &mut Vec<ApiMessage>) {
    if let Some(baseline) = &summary.baseline {
        messages.push(ApiMessage {
            role: "system".to_string(),
            content: Some(format!("[Summary baseline]\n{baseline}")),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for (i, delta) in summary.deltas.iter().enumerate() {
        messages.push(ApiMessage {
            role: "system".to_string(),
            content: Some(format!("[Summary update {}]\n{delta}", i + 1)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

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
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::ReadFile);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src/main.rs"));
        assert_eq!(req.id, "call_123");
    }

    #[test]
    fn parse_list_files_with_path() {
        let tc = make_tc("list_files", r#"{"path":"src/provider"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src/provider"));
    }

    #[test]
    fn parse_list_files_without_path_defaults_to_dot() {
        let tc = make_tc("list_files", r#"{}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.target.as_deref(), Some("."));
    }

    #[test]
    fn parse_glob_with_pattern_and_path() {
        let tc = make_tc("glob", r#"{"pattern":"**/*.rs","path":"src"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Glob);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src"));
        assert_eq!(
            req.raw_arguments.as_deref(),
            Some(r#"{"pattern":"**/*.rs","path":"src"}"#)
        );
    }

    #[test]
    fn parse_glob_with_pattern_only_defaults_path_to_dot() {
        let tc = make_tc("glob", r#"{"pattern":"*.rs"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Glob);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("."));
        assert_eq!(req.raw_arguments.as_deref(), Some(r#"{"pattern":"*.rs"}"#));
    }

    #[test]
    fn parse_grep() {
        let tc = make_tc("grep", r#"{"pattern":"fn main","path":"src"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Grep);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("fn main"));
    }

    #[test]
    fn parse_bash() {
        let tc = make_tc("bash", r#"{"command":"cargo test"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Bash);
        assert_eq!(req.action, ActionKind::Shell);
        assert_eq!(req.target.as_deref(), Some("cargo test"));
    }

    #[test]
    fn parse_edit() {
        let tc = make_tc("edit", r#"{"path":"foo.rs","old_text":"a","new_text":"b"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Edit);
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.target.as_deref(), Some("foo.rs"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_git_status() {
        let tc = make_tc("git_status", r#"{}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::GitStatus);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("."));
    }

    #[test]
    fn parse_subagent() {
        let tc = make_tc(
            "subagent",
            r#"{"description":"inspect repo","prompt":"inspect the repo and report"}"#,
        );
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Subagent);
        assert_eq!(req.action, ActionKind::Agent);
        assert_eq!(req.target.as_deref(), Some("inspect repo"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_mcp_tool() {
        let tc = make_tc("mcp__demo__search", r#"{"query":"orca"}"#);
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::Mcp("mcp__demo__search".to_string()));
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("mcp__demo__search"));
        assert_eq!(req.raw_arguments.as_deref(), Some(r#"{"query":"orca"}"#));
    }

    #[test]
    fn parse_web_search() {
        let tc = make_tc(
            "web_search",
            r#"{"query":"deepseek latest","count":3,"fresh_days":30}"#,
        );
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::WebSearch);
        assert_eq!(req.action, ActionKind::Network);
        assert_eq!(req.target.as_deref(), Some("deepseek latest"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_update_plan() {
        let tc = make_tc(
            "update_plan",
            r#"{"plan":[{"step":"Inspect references","status":"completed"},{"step":"Patch Orca","status":"in_progress"}]}"#,
        );
        let req = parse_tool_call(&tc, &[]).unwrap();
        assert_eq!(req.name, ToolName::UpdatePlan);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("2 items"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_unknown_tool_returns_error() {
        let tc = make_tc("unknown_tool", r#"{}"#);
        let err = parse_tool_call(&tc, &[]).unwrap_err();
        assert!(err.contains("unknown tool"));
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let tc = make_tc("read_file", "not json");
        let err = parse_tool_call(&tc, &[]).unwrap_err();
        assert!(err.contains("invalid arguments JSON"));
    }

    #[test]
    fn volatile_overlay_appended_to_last_message() {
        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("do something".to_string());
        conv.replace_plan_state("[Plan]\n1. step one".to_string());
        conv.replace_goal_state("build a widget".to_string());

        let messages = conversation_to_api_messages(&conv);
        assert_eq!(messages.len(), 2);
        let last_content = messages.last().unwrap().content.as_deref().unwrap();
        assert!(last_content.starts_with("do something"));
        assert!(last_content.contains("[Goal state]"));
        assert!(last_content.contains("[Plan]"));
    }

    #[test]
    fn volatile_overlay_on_tool_result() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("read a file".to_string());
        conv.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());
        conv.replace_plan_state("updated plan".to_string());

        let messages = conversation_to_api_messages(&conv);
        assert_eq!(messages.len(), 4);
        let last = messages.last().unwrap();
        assert_eq!(last.role, "tool");
        assert!(last.content.as_deref().unwrap().contains("updated plan"));
        assert!(
            last.content
                .as_deref()
                .unwrap()
                .starts_with("file contents")
        );
    }

    #[test]
    fn no_volatile_means_no_overlay() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("hello".to_string());

        let messages = conversation_to_api_messages(&conv);
        assert_eq!(messages[1].content.as_deref(), Some("hello"));
    }

    #[test]
    fn volatile_overlay_does_not_mutate_source_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("original text".to_string());
        conv.replace_plan_state("plan data".to_string());

        let _ = conversation_to_api_messages(&conv);
        assert_eq!(conv.messages.len(), 2);
        assert!(
            matches!(&conv.messages[1], Message::User { content, .. } if content == "original text")
        );
    }

    #[test]
    fn api_messages_drop_incomplete_tool_call_boundaries_without_mutating_source() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("start".to_string());
        conv.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_user("resume after failed turn".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert!(
            messages.iter().all(|message| message.tool_calls.is_none()),
            "provider request must not include assistant tool calls without matching tool results"
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["system", "user", "user"]
        );
        assert!(matches!(
            &conv.messages[2],
            Message::Assistant { tool_calls, .. } if tool_calls.len() == 1
        ));
    }
}
