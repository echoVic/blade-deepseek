use std::io::{self, BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::controller;
use orca_core::config::{HistoryMode, OutputFormat, RunConfig};

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
    // Defensive: force JSONL output and disable history regardless of config file settings.
    run_config.output_format = OutputFormat::Jsonl;
    run_config.history_mode = HistoryMode::Disabled;
    run_config.show_session_picker = false;
    run_config.desktop_notifications = false;

    let mut streaming_writer = ServerWriter::new(request.id, writer);
    let _exit_code = controller::run_to_writer_with_options(
        run_config,
        &mut streaming_writer,
        controller::ControllerRunOptions {
            wait_for_background_workflows: false,
        },
    );
    streaming_writer.flush_remaining()
}

struct ServerWriter<'a, W: Write> {
    id: Value,
    inner: &'a mut W,
    buffer: Vec<u8>,
}

impl<'a, W: Write> ServerWriter<'a, W> {
    fn new(id: Value, inner: &'a mut W) -> Self {
        Self {
            id,
            inner,
            buffer: Vec::new(),
        }
    }

    fn flush_remaining(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            if let Some(event) = map_runtime_event(&line) {
                write_protocol_event(self.inner, &self.id, event)?;
            }
        }
        Ok(())
    }
}

impl<W: Write> Write for ServerWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buffer[..pos]).to_string();
            self.buffer.drain(..=pos);
            if let Some(event) = map_runtime_event(&line) {
                write_protocol_event(self.inner, &self.id, event)?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
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
        "workflow.started" => Some(json!({
            "event": "workflow_started",
            "taskId": payload["taskId"],
            "runId": payload["runId"],
            "workflowName": payload["workflowName"]
        })),
        "workflow.result.available" => Some(json!({
            "event": "workflow_result_available",
            "taskId": payload["taskId"],
            "runId": payload["runId"],
            "result": payload["result"]
        })),
        "workflow.completed" => Some(json!({
            "event": "workflow_completed",
            "taskId": payload["taskId"],
            "runId": payload["runId"],
            "workflowName": payload["workflowName"]
        })),
        "workflow.failed" => Some(json!({
            "event": "workflow_failed",
            "taskId": payload["taskId"],
            "runId": payload["runId"],
            "error": payload["error"]
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
    writeln!(writer)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use std::io::Cursor;

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

    #[test]
    fn maps_runtime_workflow_events_to_protocol_shape() {
        let mapped = map_runtime_event(
            r#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit"}}"#,
        )
        .expect("mapped event");

        assert_eq!(mapped["event"], "workflow_started");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["workflowName"], "audit");
    }

    #[test]
    fn maps_runtime_workflow_result_available_event_to_protocol_shape() {
        let mapped = map_runtime_event(
            r#"{"type":"workflow.result.available","payload":{"taskId":"task-1","runId":"workflow-run-1","result":"done"}}"#,
        )
        .expect("mapped event");

        assert_eq!(mapped["event"], "workflow_result_available");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["result"], "done");
    }

    #[test]
    fn maps_runtime_workflow_completed_event_to_protocol_shape() {
        let mapped = map_runtime_event(
            r#"{"type":"workflow.completed","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit"}}"#,
        )
        .expect("mapped event");

        assert_eq!(mapped["event"], "workflow_completed");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["workflowName"], "audit");
    }

    #[test]
    fn maps_runtime_workflow_failed_event_to_protocol_shape() {
        let mapped = map_runtime_event(
            r#"{"type":"workflow.failed","payload":{"taskId":"task-1","runId":"workflow-run-1","error":"boom"}}"#,
        )
        .expect("mapped event");

        assert_eq!(mapped["event"], "workflow_failed");
        assert_eq!(mapped["taskId"], "task-1");
        assert_eq!(mapped["runId"], "workflow-run-1");
        assert_eq!(mapped["error"], "boom");
    }

    #[test]
    fn server_writer_streams_events_as_lines_arrive() {
        let mut output = Vec::new();
        let id = Value::from(42);
        {
            let mut writer = ServerWriter::new(id, &mut output);
            writer
                .write_all(
                    b"{\"type\":\"assistant.message.delta\",\"payload\":{\"text\":\"hi\"}}\n",
                )
                .unwrap();
        }
        let line = String::from_utf8(output).unwrap();
        let event: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(event["id"], 42);
        assert_eq!(event["event"], "message_delta");
        assert_eq!(event["text"], "hi");
    }

    #[test]
    fn workflow_submit_does_not_wait_for_background_result() {
        let input = Cursor::new(br#"{"id":7,"op":"submit","prompt":"workflow inline"}"#.to_vec());
        let mut output = Vec::new();

        run_with_io(
            ServerConfig {
                run_config: test_run_config(),
            },
            input,
            &mut output,
        )
        .expect("server run");

        let events = parse_jsonl(&output);
        assert!(events.iter().all(|event| event["id"] == 7));
        assert!(events.iter().any(|event| {
            event["event"] == "tool_completed"
                && event["tool"] == "Workflow"
                && event["status"] == "completed"
        }));
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "workflow_started")
        );
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "turn_completed")
        );
        assert!(
            !events
                .iter()
                .any(|event| event["event"] == "workflow_result_available")
        );
        assert!(
            !events
                .iter()
                .any(|event| event["event"] == "workflow_completed")
        );
    }

    fn test_run_config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("model"),
            model_runtime: Default::default(),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: PermissionRules::default(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
            .collect()
    }
}
