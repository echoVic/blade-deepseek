use std::io::{self, Write};

use crate::config::OutputFormat;
use crate::event_schema::{EventEnvelope, EventType};

pub struct EventSink<W: Write> {
    writer: W,
    format: OutputFormat,
}

impl<W: Write> EventSink<W> {
    pub fn new(writer: W, format: OutputFormat) -> Self {
        Self { writer, format }
    }

    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn emit(&mut self, event: &EventEnvelope) -> io::Result<()> {
        match self.format {
            OutputFormat::Jsonl => {
                serde_json::to_writer(&mut self.writer, event)?;
                writeln!(self.writer)?;
            }
            OutputFormat::Text => self.emit_text(event)?,
        }

        self.writer.flush()
    }

    fn emit_text(&mut self, event: &EventEnvelope) -> io::Result<()> {
        match event.event_type {
            EventType::SessionStarted => writeln!(self.writer, "session started"),
            EventType::TurnStarted => writeln!(self.writer, "turn started"),
            EventType::AssistantReasoningDelta => {
                writeln!(
                    self.writer,
                    "thinking: {}",
                    event.payload["text"].as_str().unwrap_or("")
                )
            }
            EventType::AssistantMessageDelta => {
                writeln!(
                    self.writer,
                    "assistant: {}",
                    event.payload["text"].as_str().unwrap_or("")
                )
            }
            EventType::ProviderReplayUpdated => writeln!(self.writer, "provider replay updated"),
            EventType::ModelRouted => {
                let actual = event.payload["actual_model"].as_str().unwrap_or("unknown");
                let reason = event.payload["reason"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "model routed: {actual} ({reason})")
            }
            EventType::UsageUpdated => {
                let total = event.payload["total_tokens"].as_u64().unwrap_or(0);
                let cost = event.payload["estimated_cost_usd"].as_f64().unwrap_or(0.0);
                writeln!(self.writer, "usage: {total} tokens (${cost:.6})")
            }
            EventType::ApprovalRequested => writeln!(self.writer, "approval requested"),
            EventType::ApprovalResolved => writeln!(self.writer, "approval resolved"),
            EventType::ToolCallProgress => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let bytes = event.payload["arguments_bytes"].as_u64().unwrap_or(0);
                writeln!(
                    self.writer,
                    "tool receiving arguments: {name} ({bytes} bytes)"
                )
            }
            EventType::ToolCallRequested => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                writeln!(self.writer, "tool requested: {name}")
            }
            EventType::ToolCallCompleted => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let status = event.payload["status"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "tool completed: {name} ({status})")
            }
            EventType::PlanUpdated => {
                if let Some(explanation) = event.payload["explanation"].as_str() {
                    writeln!(self.writer, "{explanation}")?;
                }
                if let Some(plan) = event.payload["plan"].as_array() {
                    for item in plan {
                        let step = item["step"].as_str().unwrap_or("");
                        let icon = match item["status"].as_str().unwrap_or("") {
                            "completed" => "\u{2713}",
                            "in_progress" => "\u{2192}",
                            "pending" => "\u{2022}",
                            _ => "\u{00b7}",
                        };
                        writeln!(self.writer, "  {icon} {step}")?;
                    }
                }
                Ok(())
            }
            EventType::SubagentStarted => {
                let description = event.payload["description"].as_str().unwrap_or("subagent");
                writeln!(self.writer, "subagent started: {description}")
            }
            EventType::SubagentCompleted => {
                let description = event.payload["description"].as_str().unwrap_or("subagent");
                let status = event.payload["status"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "subagent completed: {description} ({status})")
            }
            EventType::WorkflowStarted => {
                let workflow_name = event.payload["workflowName"].as_str().unwrap_or("workflow");
                writeln!(self.writer, "workflow started: {workflow_name}")
            }
            EventType::WorkflowResumed => writeln!(self.writer, "workflow resumed"),
            EventType::WorkflowPhaseStarted => writeln!(self.writer, "workflow phase started"),
            EventType::WorkflowPhaseCompleted => {
                writeln!(self.writer, "workflow phase completed")
            }
            EventType::WorkflowAgentStarted => writeln!(self.writer, "workflow agent started"),
            EventType::WorkflowAgentCached => writeln!(self.writer, "workflow agent cached"),
            EventType::WorkflowAgentCompleted => {
                writeln!(self.writer, "workflow agent completed")
            }
            EventType::WorkflowAgentFailed => writeln!(self.writer, "workflow agent failed"),
            EventType::WorkflowPaused => writeln!(self.writer, "workflow paused"),
            EventType::WorkflowStopped => writeln!(self.writer, "workflow stopped"),
            EventType::WorkflowCompleted => writeln!(self.writer, "workflow completed"),
            EventType::WorkflowFailed => writeln!(self.writer, "workflow failed"),
            EventType::WorkflowResultAvailable => {
                writeln!(self.writer, "workflow result available")
            }
            EventType::WorkflowTasksUpdated => writeln!(self.writer, "workflow tasks updated"),
            EventType::VerificationStarted => writeln!(self.writer, "verification started"),
            EventType::VerificationCompleted => writeln!(self.writer, "verification completed"),
            EventType::Error => writeln!(
                self.writer,
                "error: {}",
                event.payload["message"].as_str().unwrap_or("unknown")
            ),
            EventType::SessionCompleted => {
                writeln!(
                    self.writer,
                    "status: {}",
                    event.payload["status"].as_str().unwrap_or("unknown")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_schema::EventFactory;

    #[test]
    fn jsonl_format_writes_one_line_per_event() {
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Jsonl);
        let mut f = EventFactory::new("run-1".to_string());

        sink.emit(&f.error("test error")).unwrap();
        sink.emit(&f.assistant_message_delta("hello")).unwrap();

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["payload"]["message"], "test error");

        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["type"], "assistant.message.delta");
        assert_eq!(parsed["payload"]["text"], "hello");
    }

    #[test]
    fn text_format_writes_human_readable() {
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Text);
        let mut f = EventFactory::new("run-1".to_string());

        sink.emit(&f.error("something broke")).unwrap();
        sink.emit(&f.assistant_message_delta("hi")).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("error: something broke"));
        assert!(output.contains("assistant: hi"));
    }
}
