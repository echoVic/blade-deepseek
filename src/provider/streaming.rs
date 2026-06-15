use std::io::{BufRead, BufReader, Read};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    pub choices: Vec<StreamChoice>,
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

pub struct StreamResult {
    pub finish_reason: Option<String>,
    pub reasoning: String,
    pub content: String,
    pub tool_calls: Vec<ToolCallAccumulator>,
}

pub enum StreamEvent<'a> {
    Reasoning(&'a str),
    Content(&'a str),
}

pub fn parse_sse_stream<R: Read>(
    reader: R,
    mut on_delta: impl FnMut(StreamEvent),
) -> Result<StreamResult, String> {
    let buf_reader = BufReader::new(reader);
    let mut finish_reason: Option<String> = None;
    let mut reasoning_buf = String::new();
    let mut content_buf = String::new();
    let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();

    for line in buf_reader.lines() {
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

        for choice in &chunk.choices {
            if let Some(ref reason) = choice.finish_reason {
                finish_reason = Some(reason.clone());
            }

            let delta = &choice.delta;

            if let Some(text) = delta.reasoning() {
                if !text.is_empty() {
                    reasoning_buf.push_str(text);
                    on_delta(StreamEvent::Reasoning(text));
                }
            }

            if let Some(ref text) = delta.content {
                if !text.is_empty() {
                    content_buf.push_str(text);
                    on_delta(StreamEvent::Content(text));
                }
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

        let mut content_parts = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), |delta| {
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

        let mut reasoning_parts = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), |delta| {
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

        let result = parse_sse_stream(sse_data.as_bytes(), |_| {}).unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].function_name, "read_file");
        assert_eq!(result.tool_calls[0].arguments, "{\"path\": \"src/main.rs\"}");
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
    }
}
