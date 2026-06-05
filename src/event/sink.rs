use std::io::{self, Write};

use crate::config::OutputFormat;
use crate::event::schema::{EventEnvelope, EventType};

pub struct EventSink<W: Write> {
    writer: W,
    format: OutputFormat,
}

impl<W: Write> EventSink<W> {
    pub fn new(writer: W, format: OutputFormat) -> Self {
        Self { writer, format }
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
            EventType::ApprovalRequested => writeln!(self.writer, "approval requested"),
            EventType::ApprovalResolved => writeln!(self.writer, "approval resolved"),
            EventType::ToolCallRequested => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                writeln!(self.writer, "tool requested: {name}")
            }
            EventType::ToolCallCompleted => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let status = event.payload["status"].as_str().unwrap_or("unknown");
                writeln!(self.writer, "tool completed: {name} ({status})")
            }
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
