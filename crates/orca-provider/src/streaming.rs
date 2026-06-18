use std::io::{BufRead, BufReader, Read};

use serde::Deserialize;

use orca_core::cancel::CancelToken;
use orca_core::provider_types::Usage;

#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    pub usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChunkDelta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(alias = "reasoning")]
    pub reasoning_alias: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

impl ChunkDelta {
    pub fn reasoning(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning_alias.as_deref())
    }
}

#[derive(Debug, Deserialize)]
pub struct StreamToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub struct StreamFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug)]
pub struct ToolCallAccumulator {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Debug)]
pub struct StreamResult {
    pub finish_reason: Option<String>,
    pub reasoning: String,
    pub content: String,
    pub tool_calls: Vec<ToolCallAccumulator>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct StreamUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
}

impl From<StreamUsage> for Usage {
    fn from(usage: StreamUsage) -> Self {
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

pub enum StreamEvent<'a> {
    Reasoning(&'a str),
    Content(&'a str),
}

pub fn parse_sse_stream<R: Read>(
    reader: R,
    cancel: &CancelToken,
    mut on_delta: impl FnMut(StreamEvent),
) -> Result<StreamResult, String> {
    let buf_reader = BufReader::new(reader);
    let mut finish_reason: Option<String> = None;
    let mut reasoning_buf = String::new();
    let mut content_buf = String::new();
    let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();
    let mut usage = None;

    for line in buf_reader.lines() {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let line = line.map_err(|e| format!("stream read error: {e}"))?;
        let line = line.trim_end();

        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        if data == "[DONE]" {
            break;
        }

        let chunk: StreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        if let Some(chunk_usage) = chunk.usage {
            usage = Some(chunk_usage.into());
        }

        for choice in &chunk.choices {
            if let Some(ref reason) = choice.finish_reason {
                finish_reason = Some(reason.clone());
            }

            let delta = &choice.delta;

            if let Some(text) = delta.reasoning()
                && !text.is_empty()
            {
                reasoning_buf.push_str(text);
                on_delta(StreamEvent::Reasoning(text));
            }

            if let Some(ref text) = delta.content
                && !text.is_empty()
            {
                content_buf.push_str(text);
                on_delta(StreamEvent::Content(text));
            }

            if let Some(ref tcs) = delta.tool_calls {
                for tc_delta in tcs {
                    accumulate_tool_call(&mut tool_calls, tc_delta);
                }
            }
        }
    }

    Ok(StreamResult {
        finish_reason,
        reasoning: reasoning_buf,
        content: content_buf,
        tool_calls,
        usage,
    })
}

fn accumulate_tool_call(buf: &mut Vec<ToolCallAccumulator>, delta: &StreamToolCallDelta) {
    let idx = delta.index;
    while buf.len() <= idx {
        buf.push(ToolCallAccumulator {
            id: String::new(),
            function_name: String::new(),
            arguments: String::new(),
        });
    }
    if let Some(ref id) = delta.id {
        buf[idx].id.clone_from(id);
    }
    if let Some(ref func) = delta.function {
        if let Some(ref name) = func.name {
            buf[idx].function_name.push_str(name);
        }
        if let Some(ref args) = func.arguments {
            buf[idx].arguments.push_str(args);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_content_stream() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let mut content_parts = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::Content(text) = delta {
                content_parts.push(text.to_string());
            }
        })
        .unwrap();

        assert_eq!(result.content, "Hello world");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        assert_eq!(content_parts, vec!["Hello", " world"]);
    }

    #[test]
    fn parse_reasoning_and_content() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let mut reasoning_parts = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::Reasoning(text) = delta {
                reasoning_parts.push(text.to_string());
            }
        })
        .unwrap();

        assert_eq!(result.reasoning, "thinking...");
        assert_eq!(result.content, "answer");
        assert_eq!(reasoning_parts, vec!["thinking..."]);
    }

    #[test]
    fn parse_tool_calls_stream() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":null,\"arguments\":\"{\\\"path\\\"\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":null,\"arguments\":\": \\\"src/main.rs\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {}).unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].function_name, "read_file");
        assert_eq!(
            result.tool_calls[0].arguments,
            "{\"path\": \"src/main.rs\"}"
        );
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn cancel_interrupts_stream() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        cancel.cancel();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {});
        assert_eq!(result.unwrap_err(), "cancelled");
    }

    #[test]
    fn parse_usage_chunk() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n\
                        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":30,\"total_tokens\":150,\"prompt_cache_hit_tokens\":10}}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |_| {}).unwrap();

        let usage = result.usage.expect("usage");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.cache_tokens, 10);
    }
}
