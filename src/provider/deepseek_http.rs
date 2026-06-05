use std::env;

use serde::{Deserialize, Serialize};

use crate::provider::{ProviderReplayState, ProviderStep};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-chat";

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
}

pub fn plan(prompt: &str) -> Vec<ProviderStep> {
    match request_chat(prompt) {
        Ok(steps) => steps,
        Err(error) => vec![ProviderStep::Error(format!(
            "DeepSeek provider error: {error}"
        ))],
    }
}

fn request_chat(prompt: &str) -> Result<Vec<ProviderStep>, String> {
    let api_key = env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY is required for --provider deepseek".to_string())?;
    let base_url = env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let model = env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let request = ChatRequest {
        model: &model,
        messages: vec![ChatMessage {
            role: "user",
            content: prompt,
        }],
        stream: false,
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

    let message = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "response did not contain choices".to_string())?
        .message;

    let mut steps = Vec::new();
    if let Some(reasoning_content) = message.reasoning_content.filter(|text| !text.is_empty()) {
        steps.push(ProviderStep::ReasoningDelta(reasoning_content.clone()));
        steps.push(ProviderStep::ReplayState(ProviderReplayState {
            provider: "deepseek",
            reasoning_content,
            tool_call_ids: Vec::new(),
        }));
    }

    if let Some(content) = message.content.filter(|text| !text.is_empty()) {
        steps.push(ProviderStep::MessageDelta(content));
    }

    if steps.is_empty() {
        return Err("response did not contain content".to_string());
    }

    Ok(steps)
}
