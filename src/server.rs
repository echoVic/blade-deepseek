use std::io::{self, BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{HistoryMode, OutputFormat, RunConfig};
use crate::runtime::controller;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub run_config: RunConfig,
}

#[derive(Debug, Deserialize)]
struct ProtocolRequest {
    id: Value,
    op: String,
    prompt: Option<String>,
}

pub fn run(config: ServerConfig) -> i32 {
    match run_with_io(config, io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: server error: {error}");
            1
        }
    }
}

fn run_with_io<R: BufRead, W: Write>(
    config: ServerConfig,
    mut reader: R,
    mut writer: W,
) -> io::Result<()> {
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            handle_line(&config, trimmed, &mut writer)?;
        }
        line.clear();
    }
    Ok(())
}

fn handle_line<W: Write>(config: &ServerConfig, line: &str, writer: &mut W) -> io::Result<()> {
    let request = match serde_json::from_str::<ProtocolRequest>(line) {
        Ok(request) => request,
        Err(error) => {
            write_protocol_event(
                writer,
                &Value::Null,
                json!({
                    "event": "error",
                    "message": format!("invalid request: {error}")
                }),
            )?;
            return Ok(());
        }
    };

    match request.op.as_str() {
        "submit" => run_submit(config, request, writer),
        op => write_protocol_event(
            writer,
            &request.id,
            json!({
                "event": "error",
                "message": format!("unsupported op: {op}")
            }),
        ),
    }
}

fn run_submit<W: Write>(
    config: &ServerConfig,
    request: ProtocolRequest,
    writer: &mut W,
) -> io::Result<()> {
    let mut run_config = config.run_config.clone();
    run_config.prompt = request.prompt.unwrap_or_default();
    run_config.output_format = OutputFormat::Jsonl;
    run_config.history_mode = HistoryMode::Disabled;
    run_config.show_session_picker = false;
    run_config.desktop_notifications = false;

    let mut event_output = Vec::new();
    let _exit_code = controller::run_to_writer(run_config, &mut event_output);

    for line in String::from_utf8_lossy(&event_output).lines() {
        if let Some(protocol_event) = map_runtime_event(line) {
            write_protocol_event(writer, &request.id, protocol_event)?;
        }
    }
    Ok(())
}

fn map_runtime_event(line: &str) -> Option<Value> {
    let event: Value = serde_json::from_str(line).ok()?;
    let payload = &event["payload"];
    match event["type"].as_str()? {
        "turn.started" => Some(json!({
            "event": "turn_started",
            "turn": payload["turn"]
        })),
        "assistant.reasoning.delta" => Some(json!({
            "event": "reasoning_delta",
            "text": payload["text"]
        })),
        "assistant.message.delta" => Some(json!({
            "event": "message_delta",
            "text": payload["text"]
        })),
        "tool.call.requested" => Some(json!({
            "event": "tool_requested",
            "tool": payload["name"],
            "target": payload["target"]
        })),
        "tool.call.completed" => Some(json!({
            "event": "tool_completed",
            "tool": payload["name"],
            "status": payload["status"]
        })),
        "error" => Some(json!({
            "event": "error",
            "message": payload["message"]
        })),
        "session.completed" => Some(json!({
            "event": "turn_completed",
            "status": payload["status"]
        })),
        _ => None,
    }
}

fn write_protocol_event<W: Write>(writer: &mut W, id: &Value, mut event: Value) -> io::Result<()> {
    event["id"] = id.clone();
    serde_json::to_writer(&mut *writer, &event)?;
    writeln!(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_runtime_tool_events_to_protocol_shape() {
        let mapped = map_runtime_event(
            r#"{"type":"tool.call.requested","payload":{"name":"read_file","target":"src/main.rs"}}"#,
        )
        .expect("mapped event");

        assert_eq!(mapped["event"], "tool_requested");
        assert_eq!(mapped["tool"], "read_file");
        assert_eq!(mapped["target"], "src/main.rs");
        assert!(mapped.get("type").is_none());
    }
}
