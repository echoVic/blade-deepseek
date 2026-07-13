use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::approval_types::ActionKind;
use orca_core::cancel::CancelToken;
use orca_core::conversation::{
    Conversation, Message, RawToolCall, SummaryState, assistant_message_has_payload,
    normalize_tool_boundaries,
};
use orca_core::provider_types::{ProviderReplayState, ProviderResponse, ProviderStep, Usage};
use orca_core::tool_types::{ToolName, ToolRequest};
use orca_tools::registry;

use crate::ProviderConfig;
use crate::tool_schema::deepseek_tools_schema_with_mcp_and_external;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";
const DEFAULT_CHAT_MAX_TOKENS: u32 = 128_000;
const DEEPSEEK_MAX_TOOLS: usize = 128;
const EMPTY_RESPONSE_RETRIES: usize = 1;
const STREAM_INTEGRITY_RETRIES: usize = 1;
const EMPTY_RESPONSE_ERROR: &str = "response did not contain content or tool calls";
const EMPTY_RESPONSE_RECOVERY_PROMPT: &str = "Continue the current turn. The previous response ended without visible assistant content or tool calls. Return a user-facing answer in content, or call an available tool. Do not return reasoning only.";

#[derive(Debug, Eq, PartialEq)]
struct DeepSeekRequestError {
    message: String,
    usage: Option<Usage>,
}

impl DeepSeekRequestError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            usage: None,
        }
    }

    fn with_usage(message: impl Into<String>, usage: Option<Usage>) -> Self {
        Self {
            message: message.into(),
            usage,
        }
    }
}

impl From<String> for DeepSeekRequestError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl std::fmt::Display for DeepSeekRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// DeepSeek strict function calling (Beta) is only served on the /beta endpoint.
fn is_strict_capable_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/').ends_with("/beta")
}

/// Returns a strict-mode copy of the tool list for beta endpoints: allowlisted
/// tools get `strict: true` plus the schema shape strict mode demands (every
/// object property listed in `required`; optional fields are already nullable
/// in the base schemas). Returns None when the endpoint does not support strict
/// mode or no tool qualified, so callers know whether a fallback retry without
/// strict is meaningful. Public so real-API harnesses can probe the exact
/// strict payload the provider sends.
pub fn strict_tools_for_endpoint(tools: &[Value], base_url: &str) -> Option<Vec<Value>> {
    if !is_strict_capable_endpoint(base_url) {
        return None;
    }
    let mut strict_tools = tools.to_vec();
    let mut changed = false;
    for tool in &mut strict_tools {
        let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
            continue;
        };
        let strict_capable = function
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| registry::STRICT_MODE_TOOL_NAMES.contains(&name));
        if !strict_capable {
            continue;
        }
        if let Some(parameters) = function.get_mut("parameters") {
            require_all_properties(parameters);
        }
        function.insert("strict".to_string(), Value::Bool(true));
        changed = true;
    }
    changed.then_some(strict_tools)
}

/// Strict mode rejects object schemas whose `required` omits any property.
fn require_all_properties(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let is_typed_object = object.get("type").and_then(Value::as_str) == Some("object");
    if is_typed_object && let Some(properties) = object.get("properties").and_then(Value::as_object)
    {
        let names: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
        object.insert("required".to_string(), Value::Array(names));
    }
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for child in properties.values_mut() {
            require_all_properties(child);
        }
    }
    if let Some(items) = object.get_mut("items") {
        require_all_properties(items);
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for branch in branches {
                require_all_properties(branch);
            }
        }
    }
}

/// The beta endpoint reports strict-schema rejections as HTTP 400; both retry
/// helpers embed the status in their error strings.
fn is_strict_schema_rejection(error: &str) -> bool {
    error.contains("(400") || error.contains("400 Bad Request")
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<orca_core::config::ReasoningEffort>,
}

fn add_empty_response_recovery_instruction(request: &mut ChatRequest) {
    if let Some(last) = request.messages.last_mut()
        && last.role == "user"
        && let Some(content) = &mut last.content
    {
        content.push_str("\n\n");
        content.push_str(EMPTY_RESPONSE_RECOVERY_PROMPT);
        return;
    }

    request.messages.push(ApiMessage {
        role: "user".to_string(),
        content: Some(EMPTY_RESPONSE_RECOVERY_PROMPT.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    });
}

fn merge_usage(total: &mut Option<Usage>, usage: Option<Usage>) {
    let Some(usage) = usage else {
        return;
    };
    let total = total.get_or_insert_with(Usage::default);
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_tokens = total.cache_tokens.saturating_add(usage.cache_tokens);
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
            usage: error.usage,
        },
    }
}

pub async fn call_streaming_async(
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    mut on_step: impl FnMut(&ProviderStep),
) -> ProviderResponse {
    match request_chat_streaming(conversation, config, cancel, &mut on_step).await {
        Ok(response) => response,
        Err(error) => {
            let step = ProviderStep::Error(format!("DeepSeek provider error: {error}"));
            on_step(&step);
            ProviderResponse {
                steps: vec![step],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: error.usage,
            }
        }
    }
}

async fn request_chat_streaming(
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    on_step: &mut impl FnMut(&ProviderStep),
) -> Result<ProviderResponse, DeepSeekRequestError> {
    let api_key = config.api_key.as_deref().ok_or_else(|| {
        "DEEPSEEK_API_KEY is required (set via env var or ~/.orca/auth.json)".to_string()
    })?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let streaming_client = crate::http_client::streaming_client()?;

    let messages = conversation_to_api_messages(conversation);
    let tools = config.tools_override.clone().unwrap_or_else(|| {
        deepseek_tools_schema_with_mcp_and_external(
            config.mcp_registry.as_ref(),
            &config.external_tools,
        )
    });
    let tools = cap_tools_for_deepseek(tools);
    let strict_tools = strict_tools_for_endpoint(&tools, base_url);
    let strict_applied = strict_tools.is_some();

    let mut request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        tools: Some(strict_tools.unwrap_or_else(|| tools.clone())),
        max_tokens: Some(DEFAULT_CHAT_MAX_TOKENS),
        reasoning_effort: Some(config.reasoning_effort),
    };

    let mut empty_response_retries = 0;
    let mut stream_integrity_retries = 0;
    let mut suppress_retry_reasoning = false;
    let mut accumulated_usage = None;
    loop {
        let response = match crate::http_client::execute_streaming_with_retry(
            &streaming_client,
            |client| client.post(&url).bearer_auth(api_key).json(&request),
            cancel,
        )
        .await
        {
            Ok(response) => response,
            // Strict mode is Beta; if the server rejects the strict schema, retry
            // once with the plain tool list rather than failing the whole turn.
            Err(error) if strict_applied && is_strict_schema_rejection(&error) => {
                request.tools = Some(tools.clone());
                crate::http_client::execute_streaming_with_retry(
                    &streaming_client,
                    |client| client.post(&url).bearer_auth(api_key).json(&request),
                    cancel,
                )
                .await
                .map_err(|error| DeepSeekRequestError::with_usage(error, accumulated_usage))?
            }
            Err(error) => {
                return Err(DeepSeekRequestError::with_usage(error, accumulated_usage));
            }
        };

        let mut steps = Vec::new();
        let mut emitted_step = false;
        let mut emitted_reasoning = false;

        let stream_result = match crate::streaming::parse_sse_response(
            response,
            cancel,
            crate::http_client::streaming_idle_read_timeout(),
            |delta| {
                let step = provider_step_from_stream_event(delta);
                let is_reasoning_delta = matches!(&step, ProviderStep::ReasoningDelta(_));
                if is_reasoning_delta {
                    emitted_reasoning = true;
                }
                if !(suppress_retry_reasoning && is_reasoning_delta) {
                    emitted_step = true;
                    on_step(&step);
                }
                if stream_step_belongs_in_response_steps(&step) {
                    steps.push(step);
                }
            },
        )
        .await
        {
            Ok(result) => result,
            Err(error)
                if !emitted_step
                    && stream_integrity_retries < STREAM_INTEGRITY_RETRIES
                    && crate::streaming::is_stream_integrity_error(&error) =>
            {
                stream_integrity_retries += 1;
                continue;
            }
            Err(error) => {
                return Err(DeepSeekRequestError::with_usage(error, accumulated_usage));
            }
        };

        merge_usage(&mut accumulated_usage, stream_result.usage);

        match stream_result.finish_reason.as_deref() {
            Some("length") => {
                return Err(DeepSeekRequestError::with_usage(
                    length_finish_reason_error(),
                    accumulated_usage,
                ));
            }
            Some("content_filter") => {
                return Err(DeepSeekRequestError::with_usage(
                    "Response blocked by content filter",
                    accumulated_usage,
                ));
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
            steps.push(ProviderStep::ToolCall(parse_tool_call(
                &tc_response,
                &config.external_tools,
            )));
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

        if !assistant_message_has_payload(assistant_content.as_deref(), &raw_calls_for_history)
            && !steps
                .iter()
                .any(|step| matches!(step, ProviderStep::Error(_)))
        {
            if empty_response_retries < EMPTY_RESPONSE_RETRIES {
                empty_response_retries += 1;
                suppress_retry_reasoning = emitted_reasoning;
                add_empty_response_recovery_instruction(&mut request);
                continue;
            }
            return Err(DeepSeekRequestError::with_usage(
                EMPTY_RESPONSE_ERROR,
                accumulated_usage,
            ));
        }

        return Ok(ProviderResponse {
            steps,
            assistant_content,
            assistant_reasoning,
            tool_calls: raw_calls_for_history,
            usage: accumulated_usage,
        });
    }
}

fn provider_step_from_stream_event(delta: crate::streaming::StreamEvent<'_>) -> ProviderStep {
    use crate::streaming::StreamEvent;
    match delta {
        StreamEvent::Reasoning(text) => ProviderStep::ReasoningDelta(text.to_string()),
        StreamEvent::Content(text) => ProviderStep::MessageDelta(text.to_string()),
        StreamEvent::ToolCallProgress(progress) => ProviderStep::ToolCallProgress(progress),
    }
}

fn stream_step_belongs_in_response_steps(step: &ProviderStep) -> bool {
    !matches!(step, ProviderStep::ToolCallProgress(_))
}

fn request_chat(
    conversation: &Conversation,
    config: &ProviderConfig,
) -> Result<ProviderResponse, DeepSeekRequestError> {
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
    let tools = cap_tools_for_deepseek(tools);
    let strict_tools = strict_tools_for_endpoint(&tools, base_url);
    let strict_applied = strict_tools.is_some();

    let mut request = ChatRequest {
        model: model.to_string(),
        messages,
        stream: false,
        stream_options: None,
        tools: Some(strict_tools.unwrap_or_else(|| tools.clone())),
        max_tokens: Some(DEFAULT_CHAT_MAX_TOKENS),
        reasoning_effort: Some(config.reasoning_effort),
    };

    let mut accumulated_usage = None;
    for empty_attempt in 0..=EMPTY_RESPONSE_RETRIES {
        let response = match crate::http_client::execute_with_retry(|client| {
            client.post(&url).bearer_auth(api_key).json(&request)
        }) {
            Ok(response) => response,
            // Strict mode is Beta; if the server rejects the strict schema, retry
            // once with the plain tool list rather than failing the whole turn.
            Err(error) if strict_applied && is_strict_schema_rejection(&error) => {
                request.tools = Some(tools.clone());
                crate::http_client::execute_with_retry(|client| {
                    client.post(&url).bearer_auth(api_key).json(&request)
                })
                .map_err(|error| DeepSeekRequestError::with_usage(error, accumulated_usage))?
            }
            Err(error) => {
                return Err(DeepSeekRequestError::with_usage(error, accumulated_usage));
            }
        };
        let response = response.json::<ChatResponse>().map_err(|error| {
            DeepSeekRequestError::with_usage(
                format!("invalid response: {error}"),
                accumulated_usage,
            )
        })?;

        let usage = response.usage.map(Usage::from);
        merge_usage(&mut accumulated_usage, usage);
        let Some(choice) = response.choices.into_iter().next() else {
            if empty_attempt < EMPTY_RESPONSE_RETRIES {
                add_empty_response_recovery_instruction(&mut request);
                continue;
            }
            return Err(DeepSeekRequestError::with_usage(
                "response did not contain choices",
                accumulated_usage,
            ));
        };

        let message = choice.message;
        let finish_reason = choice.finish_reason.unwrap_or_default();

        let mut steps = Vec::new();

        match finish_reason.as_str() {
            "length" => {
                return Err(DeepSeekRequestError::with_usage(
                    length_finish_reason_error(),
                    accumulated_usage,
                ));
            }
            "content_filter" => {
                return Err(DeepSeekRequestError::with_usage(
                    "Response blocked by content filter",
                    accumulated_usage,
                ));
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
            let tool_call_ids: Vec<String> =
                raw_tool_calls.iter().map(|tc| tc.id.clone()).collect();

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

                steps.push(ProviderStep::ToolCall(parse_tool_call(
                    tc,
                    &config.external_tools,
                )));
            }
        }

        if let Some(ref content) = assistant_content {
            steps.push(ProviderStep::MessageDelta(content.clone()));
        }

        if !assistant_message_has_payload(assistant_content.as_deref(), &raw_calls_for_history)
            && !steps
                .iter()
                .any(|step| matches!(step, ProviderStep::Error(_)))
        {
            if empty_attempt < EMPTY_RESPONSE_RETRIES {
                add_empty_response_recovery_instruction(&mut request);
                continue;
            }
            return Err(DeepSeekRequestError::with_usage(
                EMPTY_RESPONSE_ERROR,
                accumulated_usage,
            ));
        }

        return Ok(ProviderResponse {
            steps,
            assistant_content,
            assistant_reasoning,
            tool_calls: raw_calls_for_history,
            usage: accumulated_usage,
        });
    }

    Err(DeepSeekRequestError::with_usage(
        EMPTY_RESPONSE_ERROR,
        accumulated_usage,
    ))
}

fn length_finish_reason_error() -> String {
    "Response truncated: model hit max_tokens limit (finish_reason=length); ask the model to continue in smaller chunks"
        .to_string()
}

fn parse_tool_call(
    tc: &ApiToolCallResponse,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
) -> ToolRequest {
    let schema_name = tc.function.name.as_str();
    let reg = registry::tool_registry_with_mcp_and_external(None, external_tools);
    let resolved = reg.resolve(schema_name);
    let name = match resolved.as_ref() {
        // Resolution is the source of truth for collisions. External winners
        // keep their registered identity, while built-ins use requested_name
        // so aliases such as list_files retain their dedicated ToolName.
        Some(resolved) if matches!(resolved.spec.name, ToolName::External(_)) => {
            resolved.spec.name.clone()
        }
        Some(resolved) => resolved.requested_name.clone(),
        None if schema_name.starts_with("mcp__") => ToolName::Mcp(schema_name.to_string()),
        None => ToolName::External(schema_name.to_string()),
    };
    let action = resolved
        .as_ref()
        .map(|resolved| resolved.spec.capabilities.action_kind())
        .unwrap_or(ActionKind::Read);
    let target = serde_json::from_str::<Value>(&tc.function.arguments)
        .ok()
        .and_then(|args| match schema_name {
            "read_file" => args["path"].as_str().map(String::from),
            "list_files" | "glob" => args["path"]
                .as_str()
                .map(String::from)
                .or(Some(".".to_string())),
            "grep" => args["pattern"].as_str().map(String::from),
            "bash" => args["command"].as_str().map(String::from),
            "edit" | "write_file" => args["path"].as_str().map(String::from),
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
            other if external_tools.iter().any(|tool| tool.name == other) => {
                Some(other.to_string())
            }
            _ => None,
        })
        .or_else(|| resolved.is_none().then(|| schema_name.to_string()));

    ToolRequest {
        id: tc.id.clone(),
        name,
        action,
        target,
        raw_arguments: Some(normalized_raw_arguments(
            schema_name,
            &tc.function.arguments,
        )),
    }
}

/// Rewrites known-recoverable argument shapes before schema validation sees them.
/// DeepSeek has no strict-mode guarantee on the default endpoint and regularly
/// emits `update_plan` items with boolean status flags, which would otherwise be
/// rejected and silently strand the pinned plan state.
fn normalized_raw_arguments(schema_name: &str, raw: &str) -> String {
    if schema_name == "update_plan"
        && let Some(normalized) = orca_tools::update_plan::normalize_raw_arguments(raw)
    {
        return normalized;
    }
    if schema_name == "update_goal" {
        return orca_tools::update_goal::normalized_update_raw_arguments(raw);
    }
    raw.to_string()
}

fn cap_tools_for_deepseek(mut tools: Vec<Value>) -> Vec<Value> {
    if tools.len() > DEEPSEEK_MAX_TOOLS {
        eprintln!(
            "orca: warning: DeepSeek supports at most {DEEPSEEK_MAX_TOOLS} tools; truncating {} advertised tools",
            tools.len()
        );
        tools.truncate(DEEPSEEK_MAX_TOOLS);
    }
    tools
}

fn replayable_reasoning_content(
    reasoning_content: &Option<String>,
    has_tool_calls: bool,
) -> Option<String> {
    if !has_tool_calls {
        return None;
    }
    reasoning_content
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty() && *text != "(reasoning omitted)")
        .map(str::to_string)
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
                    reasoning_content: replayable_reasoning_content(
                        reasoning_content,
                        !tool_calls.is_empty(),
                    ),
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    fn make_tc(name: &str, arguments: &str) -> ApiToolCallResponse {
        ApiToolCallResponse {
            id: "call_123".to_string(),
            function: ApiFunctionResponse {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn read_http_request_body(stream: &mut std::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).expect("read request");
            assert!(read > 0, "client closed before sending full request");
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("content-length header");
            let body_start = header_end + 4;
            if buffer.len() >= body_start + content_length {
                return String::from_utf8(buffer[body_start..body_start + content_length].to_vec())
                    .expect("request body utf8");
            }
        }
    }

    fn spawn_response_sequence_server(
        responses: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let body = read_http_request_body(&mut stream);
                captured.lock().expect("lock captured bodies").push(body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).expect("write response");
            }
        });
        (base_url, bodies)
    }

    fn spawn_two_response_server(
        first: &'static str,
        second: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        spawn_response_sequence_server(vec![first, second])
    }

    fn incident_plan_boundary_conversation() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.add_user("finish the migration".to_string());
        conversation.add_assistant(
            None,
            Some("The migration is complete; update the plan and report.".to_string()),
            vec![RawToolCall {
                id: "call_update_plan".to_string(),
                function_name: "update_plan".to_string(),
                arguments: r#"{"plan":[{"step":"migrate tools","status":"completed"}]}"#
                    .to_string(),
            }],
        );
        conversation.add_tool_result(
            "call_update_plan".to_string(),
            "Plan updated (1 item). [x] migrate tools".to_string(),
        );
        conversation
    }

    fn spawn_two_streaming_response_server(
        response: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for _ in 0..=EMPTY_RESPONSE_RETRIES {
                let (mut stream, _) = listener.accept().expect("accept request");
                let body = read_http_request_body(&mut stream);
                captured.lock().expect("lock captured bodies").push(body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).expect("write response");
            }
        });
        (base_url, bodies)
    }

    fn spawn_streaming_response_sequence_server(
        responses: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let body = read_http_request_body(&mut stream);
                captured.lock().expect("lock captured bodies").push(body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).expect("write response");
            }
        });
        (base_url, bodies)
    }

    #[test]
    fn tool_call_progress_is_transient_stream_state() {
        let progress = orca_core::provider_types::ToolCallProgress {
            id: "call_1".to_string(),
            function_name: Some("write_file".to_string()),
            arguments_bytes: 8192,
        };

        assert!(!stream_step_belongs_in_response_steps(
            &ProviderStep::ToolCallProgress(progress)
        ));
        assert!(stream_step_belongs_in_response_steps(
            &ProviderStep::MessageDelta("hello".to_string())
        ));
        assert!(stream_step_belongs_in_response_steps(
            &ProviderStep::ReasoningDelta("thinking".to_string())
        ));
    }

    #[test]
    fn parse_read_file() {
        let tc = make_tc("read_file", r#"{"path":"src/main.rs"}"#);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::ReadFile);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src/main.rs"));
        assert_eq!(req.id, "call_123");
    }

    #[test]
    fn parse_list_files_with_path() {
        let tc = make_tc("list_files", r#"{"path":"src/provider"}"#);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("src/provider"));
    }

    #[test]
    fn parse_list_files_without_path_defaults_to_dot() {
        let tc = make_tc("list_files", r#"{}"#);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.target.as_deref(), Some("."));
    }

    #[test]
    fn parse_external_list_files_collision_preserves_builtin_alias_precedence() {
        let external = orca_core::external_config::ExternalToolConfig {
            name: "list_files".to_string(),
            description: "Shadow the built-in glob alias".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo shadowed".to_string(),
            schema: serde_json::json!({}),
        };
        let tc = make_tc("list_files", r#"{}"#);
        let request = parse_tool_call(&tc, &[external]);

        assert_eq!(request.name, ToolName::ListFiles);
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.target.as_deref(), Some("."));
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_update_plan_normalizes_boolean_status_flags() {
        // Both malformed shapes DeepSeek emits in the wild: flags without status
        // and flags redundant with status. Normalized output must pass the same
        // schema validation that runs before tool execution.
        let tc = make_tc(
            "update_plan",
            r#"{"plan":[{"completed":true,"step":"a"},{"in_progress":true,"status":"in_progress","step":"b"}]}"#,
        );
        let req = parse_tool_call(&tc, &[]);

        let raw = req.raw_arguments.as_deref().unwrap();
        let value: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(value["plan"][0]["status"], "completed");
        assert!(value["plan"][0].get("completed").is_none());
        assert_eq!(value["plan"][1]["status"], "in_progress");
        assert!(value["plan"][1].get("in_progress").is_none());

        let reg = registry::tool_registry_with_mcp_and_external(None, &[]);
        registry::validate_tool_request(&reg, &req).expect("normalized args must validate");
    }

    #[test]
    fn parse_update_goal_normalizes_status_aliases_and_flags() {
        let completed = make_tc("update_goal", r#"{"status":"completed","reason":"done"}"#);
        let complete_flag = make_tc("update_goal", r#"{"complete":true,"reason":"done"}"#);
        let blocked_flag = make_tc("update_goal", r#"{"blocked":true,"reason":"stuck"}"#);

        let completed = parse_tool_call(&completed, &[]);
        let complete_flag = parse_tool_call(&complete_flag, &[]);
        let blocked_flag = parse_tool_call(&blocked_flag, &[]);

        assert_eq!(
            serde_json::from_str::<Value>(completed.raw_arguments.as_deref().unwrap()).unwrap()["status"],
            "complete"
        );
        assert_eq!(
            serde_json::from_str::<Value>(complete_flag.raw_arguments.as_deref().unwrap()).unwrap()
                ["status"],
            "complete"
        );
        assert_eq!(
            serde_json::from_str::<Value>(blocked_flag.raw_arguments.as_deref().unwrap()).unwrap()
                ["status"],
            "blocked"
        );
    }

    #[test]
    fn parse_update_plan_leaves_clean_arguments_untouched() {
        let clean = r#"{"explanation":"x","plan":[{"step":"a","status":"pending"}]}"#;
        let tc = make_tc("update_plan", clean);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.raw_arguments.as_deref(), Some(clean));
    }

    #[test]
    fn strict_tools_apply_only_on_beta_endpoint_to_allowlisted_tools() {
        let tools = deepseek_tools_schema_with_mcp_and_external(None, &[]);

        assert!(strict_tools_for_endpoint(&tools, "https://api.deepseek.com").is_none());
        assert!(strict_tools_for_endpoint(&tools, DEFAULT_BASE_URL).is_none());

        let strict = strict_tools_for_endpoint(&tools, "https://api.deepseek.com/beta")
            .expect("beta endpoint must produce a strict tool list");
        let update_plan = strict
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("update_plan")
            })
            .expect("update_plan present");
        assert_eq!(
            update_plan.pointer("/function/strict"),
            Some(&Value::Bool(true))
        );
        // Strict mode demands every property listed in required.
        assert_eq!(
            update_plan.pointer("/function/parameters/required"),
            Some(&serde_json::json!(["explanation", "plan"]))
        );

        let glob = strict
            .iter()
            .find(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some("glob"))
            .expect("glob present");
        assert_eq!(glob.pointer("/function/strict"), Some(&Value::Bool(true)));
        assert!(glob.pointer("/function/parameters/oneOf").is_none());
        assert!(glob.pointer("/function/parameters/anyOf").is_some());

        let update_goal_schema =
            crate::tool_schema::deepseek_goal_tools_schema_with_mcp_and_external(None, &[]);
        let goal_strict =
            strict_tools_for_endpoint(&update_goal_schema, "https://api.deepseek.com/beta")
                .expect("goal schema must produce strict tools");
        let update_goal = goal_strict
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("update_goal")
            })
            .expect("update_goal present");
        assert_eq!(
            update_goal.pointer("/function/strict"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            update_goal.pointer("/function/parameters/required"),
            Some(&serde_json::json!(["reason", "status"]))
        );
        assert_eq!(
            update_goal.pointer("/function/parameters/properties/reason/anyOf/1/type"),
            Some(&Value::String("null".to_string()))
        );

        let bash = strict
            .iter()
            .find(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some("bash"))
            .expect("bash present");
        assert!(
            bash.pointer("/function/strict").is_none(),
            "non-allowlisted tools must stay non-strict"
        );

        // The original list must stay untouched for the fallback retry.
        let original_update_plan = tools
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("update_plan")
            })
            .expect("update_plan present");
        assert!(original_update_plan.pointer("/function/strict").is_none());
    }

    #[test]
    fn strict_rejection_detection_matches_retry_helper_error_strings() {
        assert!(is_strict_schema_rejection(
            "request error (400 Bad Request): invalid tools"
        ));
        assert!(is_strict_schema_rejection(
            "request error: HTTP status client error (400 Bad Request) for url (https://api.deepseek.com/beta/chat/completions)"
        ));
        assert!(!is_strict_schema_rejection(
            "max retries exceeded (last status: 500 Internal Server Error)"
        ));
        assert!(!is_strict_schema_rejection(
            "request failed after 3 attempts: connection refused"
        ));
    }

    #[test]
    fn parse_glob_with_pattern_and_path() {
        let tc = make_tc("glob", r#"{"pattern":"**/*.rs","path":"src"}"#);
        let req = parse_tool_call(&tc, &[]);
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
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::Glob);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("."));
        assert_eq!(req.raw_arguments.as_deref(), Some(r#"{"pattern":"*.rs"}"#));
    }

    #[test]
    fn parse_grep() {
        let tc = make_tc("grep", r#"{"pattern":"fn main","path":"src"}"#);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::Grep);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("fn main"));
    }

    #[test]
    fn parse_bash() {
        let tc = make_tc("bash", r#"{"command":"cargo test"}"#);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::Bash);
        assert_eq!(req.action, ActionKind::Shell);
        assert_eq!(req.target.as_deref(), Some("cargo test"));
    }

    #[test]
    fn parse_external_bash_collision_preserves_builtin_registry_precedence() {
        let external = orca_core::external_config::ExternalToolConfig {
            name: "bash".to_string(),
            description: "Shadow the built-in shell tool".to_string(),
            action_kind: ActionKind::Network,
            command: "echo shadowed".to_string(),
            schema: serde_json::json!({}),
        };
        let raw_arguments = r#"{"command":"cargo test -p orca-provider"}"#;
        let tc = make_tc("bash", raw_arguments);
        let request = parse_tool_call(&tc, &[external]);

        assert_eq!(request.id, "call_123");
        assert_eq!(request.name, ToolName::Bash);
        assert_eq!(request.action, ActionKind::Shell);
        assert_eq!(
            request.target.as_deref(),
            Some("cargo test -p orca-provider")
        );
        assert_eq!(request.raw_arguments.as_deref(), Some(raw_arguments));
    }

    #[test]
    fn parse_edit() {
        let tc = make_tc("edit", r#"{"path":"foo.rs","old_text":"a","new_text":"b"}"#);
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::Edit);
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.target.as_deref(), Some("foo.rs"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_git_status() {
        let tc = make_tc("git_status", r#"{}"#);
        let req = parse_tool_call(&tc, &[]);
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
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::Subagent);
        assert_eq!(req.action, ActionKind::Agent);
        assert_eq!(req.target.as_deref(), Some("inspect repo"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_mcp_tool() {
        let tc = make_tc("mcp__demo__search", r#"{"query":"orca"}"#);
        let req = parse_tool_call(&tc, &[]);
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
        let req = parse_tool_call(&tc, &[]);
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
        let req = parse_tool_call(&tc, &[]);
        assert_eq!(req.name, ToolName::UpdatePlan);
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.target.as_deref(), Some("2 items"));
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_unknown_tool_preserves_call_for_model_correction() {
        let tc = make_tc("wc -l", r#"{}"#);
        let request = parse_tool_call(&tc, &[]);

        assert_eq!(request.name, ToolName::External("wc -l".to_string()));
        assert_ne!(request.name, ToolName::Bash);
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.target.as_deref(), Some("wc -l"));
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_unresolved_namespaced_tool_stays_external() {
        let tc = make_tc("wc__lines", r#"{}"#);
        let request = parse_tool_call(&tc, &[]);

        assert_eq!(request.name, ToolName::External("wc__lines".to_string()));
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.target.as_deref(), Some("wc__lines"));
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_configured_namespaced_external_tool_stays_external() {
        let external = orca_core::external_config::ExternalToolConfig {
            name: "acme__deploy".to_string(),
            description: "Deploy through Acme".to_string(),
            action_kind: ActionKind::Shell,
            command: "acme deploy".to_string(),
            schema: serde_json::json!({}),
        };
        let tc = make_tc("acme__deploy", r#"{}"#);
        let request = parse_tool_call(&tc, &[external]);

        assert_eq!(request.name, ToolName::External("acme__deploy".to_string()));
        assert_eq!(request.action, ActionKind::Shell);
        assert_eq!(request.target.as_deref(), Some("acme__deploy"));
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_invalid_json_preserves_known_tool_call() {
        let tc = make_tc("write_file", r#"{"path":"note.txt","content":"partial"#);
        let request = parse_tool_call(&tc, &[]);

        assert_eq!(request.name, ToolName::WriteFile);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments, Some(tc.function.arguments));
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
    fn api_replay_omits_stale_reasoning_content() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(
            Some("done".to_string()),
            Some("private thinking".to_string()),
            vec![],
        );
        conv.add_user("next".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert!(messages.iter().all(|m| m.reasoning_content.is_none()));
    }

    #[test]
    fn api_messages_drop_reasoning_only_assistant() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(None, Some("private thinking".to_string()), vec![]);
        conv.add_user("second".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert_eq!(
            messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "user"]
        );
    }

    #[test]
    fn api_replay_preserves_reasoning_content_for_tool_call_turns() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(
            None,
            Some("tool reasoning".to_string()),
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());

        let messages = conversation_to_api_messages(&conv);
        let assistant = messages
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant replay");

        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("tool reasoning")
        );
    }

    #[test]
    fn api_replay_does_not_send_reasoning_omitted_placeholder() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(
            None,
            Some("(reasoning omitted)".to_string()),
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert!(messages.iter().all(|m| m.reasoning_content.is_none()));
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

    #[test]
    fn chat_request_serializes_reasoning_effort() {
        let request = ChatRequest {
            model: "deepseek-v4-pro".to_string(),
            messages: Vec::new(),
            stream: true,
            stream_options: None,
            tools: None,
            max_tokens: Some(DEFAULT_CHAT_MAX_TOKENS),
            reasoning_effort: Some(orca_core::config::ReasoningEffort::Max),
        };

        let json = serde_json::to_value(request).expect("serialize request");

        assert_eq!(json["reasoning_effort"], "max");
        assert_eq!(json["max_tokens"], DEFAULT_CHAT_MAX_TOKENS);
    }

    #[test]
    fn request_chat_retries_once_after_empty_response() {
        let (base_url, bodies) = spawn_two_response_server(
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_cache_hit_tokens":7}}"#,
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":13,"completion_tokens":5,"prompt_cache_hit_tokens":9}}"#,
        );
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = request_chat(&conversation, &config).expect("retry succeeds");

        assert_eq!(response.assistant_content.as_deref(), Some("ok"));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 24,
                output_tokens: 8,
                cache_tokens: 16,
            })
        );
        let bodies = bodies.lock().expect("lock captured bodies");
        assert_eq!(bodies.len(), 2);
        let first: Value = serde_json::from_str(&bodies[0]).expect("first request json");
        let retry: Value = serde_json::from_str(&bodies[1]).expect("retry request json");
        assert_eq!(first["max_tokens"], DEFAULT_CHAT_MAX_TOKENS);
        assert_eq!(retry["max_tokens"], DEFAULT_CHAT_MAX_TOKENS);
        assert_eq!(
            first["messages"].as_array().expect("first messages").len(),
            1
        );
        assert_eq!(
            retry["messages"].as_array().expect("retry messages").len(),
            1
        );
        assert_eq!(
            retry["messages"][0]["content"],
            format!("hello\n\n{EMPTY_RESPONSE_RECOVERY_PROMPT}")
        );
        assert_eq!(conversation.messages.len(), 1);
    }

    #[test]
    fn non_streaming_reasoning_only_response_is_rejected() {
        let reasoning_only = r#"{"choices":[{"message":{"content":null,"reasoning_content":"thinking"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2,"prompt_cache_hit_tokens":5}}"#;
        let (base_url, bodies) =
            spawn_response_sequence_server(vec![reasoning_only; EMPTY_RESPONSE_RETRIES + 1]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let error = request_chat(&conversation, &config).expect_err("reasoning-only is invalid");

        assert_eq!(error.message, EMPTY_RESPONSE_ERROR);
        assert_eq!(
            error.usage,
            Some(Usage {
                input_tokens: 14,
                output_tokens: 4,
                cache_tokens: 10,
            })
        );
        assert_eq!(
            bodies.lock().expect("lock captured bodies").len(),
            EMPTY_RESPONSE_RETRIES + 1
        );
    }

    #[test]
    fn non_streaming_facade_preserves_usage_when_recovery_also_fails() {
        let first = r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"prompt_cache_hit_tokens":2}}"#;
        let second = r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"prompt_cache_hit_tokens":4}}"#;
        let (base_url, bodies) = spawn_two_response_server(first, second);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = crate::call(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
        );

        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::Error(message)]
                if message == "DeepSeek provider error: response did not contain choices"
        ));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 8,
                output_tokens: 3,
                cache_tokens: 6,
            })
        );
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 2);
    }

    #[test]
    fn non_streaming_unknown_tool_is_returned_for_model_correction() {
        let unknown = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_wc","function":{"name":"wc -l","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
        let (base_url, bodies) = spawn_two_response_server(unknown, unknown);
        let mut conversation = Conversation::new();
        conversation.add_user("count lines".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = request_chat(&conversation, &config)
            .expect("unknown tool should remain a corrective tool turn");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert!(
            response
                .steps
                .iter()
                .all(|step| !matches!(step, ProviderStep::Error(_)))
        );
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_wc"
                    && request.name == ToolName::External("wc -l".to_string())
                    && request.name != ToolName::Bash
                    && request.action == ActionKind::Read
                    && request.target.as_deref() == Some("wc -l")
                    && request.raw_arguments.as_deref() == Some("{}")
        ));
        assert_eq!(response.tool_calls[0].id, "call_wc");
        assert_eq!(response.tool_calls[0].function_name, "wc -l");
        assert_eq!(response.tool_calls[0].arguments, "{}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_reasoning_only_response_is_rejected() {
        let reasoning_only = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n\
                              data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                              data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"prompt_cache_hit_tokens\":7}}\n\n\
                              data: [DONE]\n\n";
        let (base_url, bodies) = spawn_two_streaming_response_server(reasoning_only);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let error = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect_err("reasoning-only is invalid");

        assert_eq!(error.message, EMPTY_RESPONSE_ERROR);
        assert_eq!(
            error.usage,
            Some(Usage {
                input_tokens: 22,
                output_tokens: 6,
                cache_tokens: 14,
            })
        );
        assert_eq!(
            bodies.lock().expect("lock captured bodies").len(),
            EMPTY_RESPONSE_RETRIES + 1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_facade_preserves_usage_when_recovery_also_fails() {
        let first = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":13,\"completion_tokens\":4,\"prompt_cache_hit_tokens\":8}}\n\n\
                     data: [DONE]\n\n";
        let second = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":5,\"prompt_cache_hit_tokens\":9}}\n\n\
                      data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![first, second]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut emitted = Vec::new();

        let response = crate::call_streaming_async(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
            &cancel,
            |step| emitted.push(step.clone()),
        )
        .await;

        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::Error(message)]
                if message == "DeepSeek provider error: response did not contain content or tool calls"
        ));
        assert!(matches!(
            emitted.as_slice(),
            [ProviderStep::Error(message)]
                if message == "DeepSeek provider error: response did not contain content or tool calls"
        ));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 30,
                output_tokens: 9,
                cache_tokens: 17,
            })
        );
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_empty_response_retry_adds_recovery_instruction() {
        let reasoning_only = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"first attempt thinking\"},\"finish_reason\":null}]}\n\n\
                              data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                              data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":4,\"prompt_cache_hit_tokens\":12}}\n\n\
                              data: [DONE]\n\n";
        let recovered = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"retry thinking\"},\"finish_reason\":null}]}\n\n\
                         data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":null}]}\n\n\
                         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":19,\"completion_tokens\":6,\"prompt_cache_hit_tokens\":14}}\n\n\
                         data: [DONE]\n\n";
        let (base_url, bodies) =
            spawn_streaming_response_sequence_server(vec![reasoning_only, recovered]);
        let conversation = incident_plan_boundary_conversation();
        let original_messages = serde_json::to_value(conversation_to_api_messages(&conversation))
            .expect("serialize original messages");
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut emitted = Vec::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |step| {
            emitted.push(step.clone())
        })
        .await
        .expect("recovery response succeeds");

        assert_eq!(response.assistant_content.as_deref(), Some("recovered"));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 36,
                output_tokens: 10,
                cache_tokens: 26,
            })
        );
        assert!(emitted.iter().any(
            |step| matches!(step, ProviderStep::ReasoningDelta(text) if text == "first attempt thinking")
        ));
        assert!(!emitted.iter().any(
            |step| matches!(step, ProviderStep::ReasoningDelta(text) if text == "retry thinking")
        ));
        assert!(
            emitted.iter().any(
                |step| matches!(step, ProviderStep::MessageDelta(text) if text == "recovered")
            )
        );
        let bodies = bodies.lock().expect("lock captured bodies");
        assert_eq!(bodies.len(), 2);
        let first: Value = serde_json::from_str(&bodies[0]).expect("first request json");
        let retry: Value = serde_json::from_str(&bodies[1]).expect("retry request json");
        assert_eq!(
            first["messages"].as_array().expect("first messages").len(),
            3
        );
        assert_eq!(
            retry["messages"].as_array().expect("retry messages").len(),
            4
        );
        assert_eq!(
            first["messages"][1]["reasoning_content"],
            "The migration is complete; update the plan and report."
        );
        assert_eq!(retry["messages"][2]["role"], "tool");
        assert!(!bodies[1].contains("first attempt thinking"));
        assert_eq!(
            retry["messages"][3]["content"],
            EMPTY_RESPONSE_RECOVERY_PROMPT
        );
        assert_eq!(
            serde_json::to_value(conversation_to_api_messages(&conversation))
                .expect("serialize unchanged messages"),
            original_messages
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_invalid_tool_arguments_are_returned_for_tool_failure() {
        let incomplete = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_incomplete\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\",\\\"content\\\":\\\"partial\"}}]},\"finish_reason\":null}]}\n\n\
                          data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                          data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![incomplete]);
        let mut conversation = Conversation::new();
        conversation.add_user("write the file".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("invalid tool arguments should remain a tool call");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_incomplete"
                    && request.target.is_none()
                    && request.raw_arguments.as_deref()
                        == Some("{\"path\":\"src/main.rs\",\"content\":\"partial")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_unknown_tool_is_returned_for_model_correction() {
        let unknown = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_wc\",\"function\":{\"name\":\"wc -l\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
                       data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                       data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![unknown]);
        let mut conversation = Conversation::new();
        conversation.add_user("count lines".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("unknown tool should remain a corrective tool turn");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert!(
            response
                .steps
                .iter()
                .all(|step| !matches!(step, ProviderStep::Error(_)))
        );
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_wc"
                    && request.name == ToolName::External("wc -l".to_string())
                    && request.target.as_deref() == Some("wc -l")
        ));
        assert_eq!(response.tool_calls[0].function_name, "wc -l");
        assert_eq!(response.tool_calls[0].arguments, "{}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_premature_eof_without_visible_delta_retries_once() {
        let premature = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":0,\"total_tokens\":1}}\n\n";
        let complete = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_complete\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\",\\\"content\\\":\\\"done\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";
        let (base_url, bodies) =
            spawn_streaming_response_sequence_server(vec![premature, complete]);
        let mut conversation = Conversation::new();
        conversation.add_user("write the file".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("premature stream should retry once");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 2);
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_complete" && request.target.as_deref() == Some("src/main.rs")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_integrity_error_after_visible_delta_does_not_retry() {
        let premature = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let replacement = "data: {\"choices\":[{\"delta\":{\"content\":\"replacement\"},\"finish_reason\":\"stop\"}]}\n\n\
                           data: [DONE]\n\n";
        let (base_url, bodies) =
            spawn_streaming_response_sequence_server(vec![premature, replacement]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut deltas = Vec::new();

        let error = request_chat_streaming(&conversation, &config, &cancel, &mut |step| {
            if let ProviderStep::MessageDelta(text) = step {
                deltas.push(text.clone());
            }
        })
        .await
        .expect_err("a visible partial response must not be replayed transparently");

        assert_eq!(error.message, "stream ended before terminal marker");
        assert_eq!(error.usage, None);
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert_eq!(deltas, vec!["partial"]);
    }

    #[test]
    fn synchronous_facade_cancellation_does_not_deliver_prefetched_deltas() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind facade stream server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept facade stream request");
            let _ = read_http_request_body(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"second\"},\"finish_reason\":null}]}\n\n",
                )
                .expect("write facade stream response");
            stream.flush().expect("flush facade stream response");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set facade peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read facade client close: {error}"),
            };
            closed_tx.send(closed).expect("report facade close");
        });
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_from_callback = cancel.clone();
        let mut deltas = Vec::new();
        let started = Instant::now();

        let _response = crate::call_streaming(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                if let ProviderStep::MessageDelta(text) = step {
                    deltas.push(text.clone());
                    if deltas.len() == 1 {
                        std::thread::sleep(Duration::from_millis(50));
                        cancel_from_callback.cancel();
                    }
                }
            },
        );
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for facade peer close result");

        server.join().expect("facade stream server");
        assert_eq!(deltas, vec!["first"]);
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled facade returned after {elapsed:?}"
        );
        assert!(connection_closed, "facade cancellation must close the peer");
    }

    #[test]
    fn synchronous_facade_cancellation_stops_remaining_same_frame_callbacks() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind facade stream server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept facade stream request");
            let _ = read_http_request_body(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"first\",\"content\":\"second\"},\"finish_reason\":null}]}\n\n",
                )
                .expect("write facade stream response");
            stream.flush().expect("flush facade stream response");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set facade peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read facade client close: {error}"),
            };
            closed_tx.send(closed).expect("report facade close");
        });
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_from_callback = cancel.clone();
        let mut deltas = Vec::new();
        let started = Instant::now();

        let _response = crate::call_streaming(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                let text = match step {
                    ProviderStep::ReasoningDelta(text) | ProviderStep::MessageDelta(text) => text,
                    _ => return,
                };
                deltas.push(text.clone());
                if deltas.len() == 1 {
                    cancel_from_callback.cancel();
                }
            },
        );
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for facade peer close result");

        server.join().expect("facade stream server");
        assert_eq!(deltas, vec!["first"]);
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled facade returned after {elapsed:?}"
        );
        assert!(connection_closed, "facade cancellation must close the peer");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_streaming_body_closes_in_flight_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled stream server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (headers_tx, headers_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled stream request");
            let _ = read_http_request_body(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .expect("write stream headers");
            stream.flush().expect("flush stream headers");
            headers_tx.send(()).expect("announce stream headers");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read stalled stream close: {error}"),
            };
            closed_tx.send(closed).expect("report stream close");
        });
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_after_headers = cancel.clone();
        let canceller = std::thread::spawn(move || {
            headers_rx.recv().expect("wait for stream headers");
            std::thread::sleep(Duration::from_millis(100));
            cancel_after_headers.cancel();
        });

        let started = Instant::now();
        let result = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {}).await;
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for stalled stream close result");

        canceller.join().expect("stalled stream canceller");
        server.join().expect("stalled stream server");
        let error = result.unwrap_err();
        assert_eq!(error.message, "cancelled");
        assert_eq!(error.usage, None);
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled stream returned after {elapsed:?}"
        );
        assert!(
            connection_closed,
            "cancelled stream left the response body owned by a detached reader"
        );
    }

    #[test]
    fn deepseek_tools_are_capped_at_api_limit() {
        let tools = (0..(DEEPSEEK_MAX_TOOLS + 5))
            .map(|index| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("tool_{index}"),
                        "description": "test",
                        "parameters": {"type": "object", "properties": {}, "additionalProperties": false}
                    }
                })
            })
            .collect::<Vec<_>>();

        let capped = cap_tools_for_deepseek(tools);

        assert_eq!(capped.len(), DEEPSEEK_MAX_TOOLS);
        assert_eq!(
            capped[DEEPSEEK_MAX_TOOLS - 1]["function"]["name"],
            "tool_127"
        );
    }
}
