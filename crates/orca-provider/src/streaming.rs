use std::io::{self, BufRead, BufReader, Read};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use orca_core::cancel::CancelToken;
use orca_core::provider_types::{ToolCallProgress, Usage};

const TOOL_CALL_PROGRESS_ARGUMENT_BYTES_STEP: usize = 8 * 1024;
const IDLE_READ_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    ToolCallProgress(ToolCallProgress),
}

pub fn parse_sse_stream_with_idle_timeout<R: Read + Send + 'static>(
    reader: R,
    cancel: &CancelToken,
    idle_timeout: Duration,
    on_delta: impl FnMut(StreamEvent),
) -> Result<StreamResult, String> {
    parse_sse_stream(
        IdleReadTimeoutReader::new(reader, idle_timeout, cancel.clone()),
        cancel,
        on_delta,
    )
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
    let mut tool_call_progress = ToolCallProgressTracker::default();
    let mut usage = None;

    for line in buf_reader.lines() {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let line = line.map_err(|e| {
            if cancel.is_cancelled() {
                "cancelled".to_string()
            } else {
                format!("stream read error: {e}")
            }
        })?;
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
                    if let Some(progress) =
                        tool_call_progress.progress_for_delta(&tool_calls, tc_delta.index)
                    {
                        on_delta(StreamEvent::ToolCallProgress(progress));
                    }
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

struct IdleReadTimeoutReader {
    request_tx: mpsc::Sender<usize>,
    response_rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    idle_timeout: Duration,
    cancel: CancelToken,
}

impl IdleReadTimeoutReader {
    fn new<R: Read + Send + 'static>(
        mut reader: R,
        idle_timeout: Duration,
        cancel: CancelToken,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<usize>();
        let (response_tx, response_rx) = mpsc::channel::<io::Result<Vec<u8>>>();

        // The helper thread can outlive this wrapper if the underlying blocking
        // read never returns after a stall. That is the deliberate tradeoff that
        // lets the main streaming path enforce an idle timeout and observe cancel.
        thread::spawn(move || {
            while let Ok(len) = request_rx.recv() {
                let mut buf = vec![0; len];
                let result = reader.read(&mut buf).map(|read| {
                    buf.truncate(read);
                    buf
                });
                if response_tx.send(result).is_err() {
                    break;
                }
            }
        });

        Self {
            request_tx,
            response_rx,
            idle_timeout,
            cancel,
        }
    }
}

impl Read for IdleReadTimeoutReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.request_tx
            .send(buf.len())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stream reader stopped"))?;
        let started = Instant::now();
        loop {
            if self.cancel.is_cancelled() {
                return Err(io::Error::other("cancelled"));
            }
            let elapsed = started.elapsed();
            if elapsed >= self.idle_timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("idle read timed out after {:?}", self.idle_timeout),
                ));
            }
            let wait = (self.idle_timeout - elapsed).min(IDLE_READ_CANCEL_POLL_INTERVAL);
            match self.response_rx.recv_timeout(wait) {
                Ok(Ok(bytes)) => {
                    let len = bytes.len();
                    buf[..len].copy_from_slice(&bytes);
                    return Ok(len);
                }
                Ok(Err(err)) => return Err(err),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream reader stopped",
                    ));
                }
            }
        }
    }
}

#[derive(Default)]
struct ToolCallProgressTracker {
    calls: Vec<ToolCallProgressState>,
}

#[derive(Default)]
struct ToolCallProgressState {
    last_emitted_arguments_bytes: Option<usize>,
}

impl ToolCallProgressTracker {
    fn progress_for_delta(
        &mut self,
        buf: &[ToolCallAccumulator],
        index: usize,
    ) -> Option<ToolCallProgress> {
        while self.calls.len() <= index {
            self.calls.push(ToolCallProgressState::default());
        }
        let current = buf.get(index)?;
        if current.id.is_empty() || current.function_name.is_empty() {
            return None;
        }
        let arguments_bytes = current.arguments.len();
        let state = self.calls.get_mut(index)?;
        let should_emit = match state.last_emitted_arguments_bytes {
            None => true,
            Some(last) => {
                arguments_bytes.saturating_sub(last) >= TOOL_CALL_PROGRESS_ARGUMENT_BYTES_STEP
            }
        };
        if !should_emit {
            return None;
        }
        state.last_emitted_arguments_bytes = Some(arguments_bytes);
        Some(tool_call_progress(current, arguments_bytes))
    }
}

fn tool_call_progress(current: &ToolCallAccumulator, arguments_bytes: usize) -> ToolCallProgress {
    ToolCallProgress {
        id: current.id.clone(),
        function_name: Some(current.function_name.clone()),
        arguments_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct BlockingReader {
        unblock_rx: mpsc::Receiver<()>,
    }

    impl Read for BlockingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            let _ = self.unblock_rx.recv();
            Ok(0)
        }
    }

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
    fn parse_tool_calls_stream_emits_argument_progress() {
        let large_chunk = "a".repeat(8 * 1024);
        let sse_data = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"function\":{{\"name\":\"write_file\",\"arguments\":\"\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":null,\"function\":{{\"name\":null,\"arguments\":\"abc\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":null,\"function\":{{\"name\":null,\"arguments\":\"{large_chunk}\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
             data: [DONE]\n\n"
        );

        let cancel = CancelToken::new();
        let mut progress = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::ToolCallProgress(update) = delta {
                progress.push((
                    update.id.clone(),
                    update.function_name.clone(),
                    update.arguments_bytes,
                ));
            }
        })
        .unwrap();

        assert_eq!(result.tool_calls[0].function_name, "write_file");
        assert_eq!(
            progress,
            vec![
                ("call_1".to_string(), Some("write_file".to_string()), 0),
                (
                    "call_1".to_string(),
                    Some("write_file".to_string()),
                    result.tool_calls[0].arguments.len()
                )
            ]
        );
        assert_eq!(result.tool_calls[0].arguments.len(), 3 + large_chunk.len());
    }

    #[test]
    fn parse_tool_calls_stream_waits_for_stable_id_before_progress() {
        let sse_data = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":\"write_file\",\"arguments\":\"abc\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":null,\"arguments\":\"def\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";

        let cancel = CancelToken::new();
        let mut progress = Vec::new();
        let result = parse_sse_stream(sse_data.as_bytes(), &cancel, |delta| {
            if let StreamEvent::ToolCallProgress(update) = delta {
                progress.push((update.id.clone(), update.arguments_bytes));
            }
        })
        .unwrap();

        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].arguments, "abcdef");
        assert_eq!(progress, vec![("call_1".to_string(), 6)]);
    }

    #[test]
    fn parse_stream_with_idle_timeout_fails_when_read_stalls() {
        let (unblock_tx, unblock_rx) = mpsc::channel();
        let cancel = CancelToken::new();

        let result = parse_sse_stream_with_idle_timeout(
            BlockingReader { unblock_rx },
            &cancel,
            Duration::from_millis(10),
            |_| {},
        );

        drop(unblock_tx);
        assert!(
            result
                .unwrap_err()
                .contains("stream read error: idle read timed out after 10ms")
        );
    }

    #[test]
    fn parse_stream_with_idle_timeout_observes_cancel_while_read_stalls() {
        let (_unblock_tx, unblock_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let parse_cancel = cancel.clone();
        let (result_tx, result_rx) = mpsc::channel();

        thread::spawn(move || {
            let result = parse_sse_stream_with_idle_timeout(
                BlockingReader { unblock_rx },
                &parse_cancel,
                Duration::from_secs(5),
                |_| {},
            );
            let _ = result_tx.send(result.map(|_| ()));
        });

        thread::sleep(Duration::from_millis(20));
        cancel.cancel();

        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel should interrupt a stalled read promptly");
        assert_eq!(result.unwrap_err(), "cancelled");
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
