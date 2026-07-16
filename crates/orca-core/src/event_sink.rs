use std::io::{self, Write};
use std::sync::Arc;

use crate::config::OutputFormat;
use crate::event_schema::{EventDraft, EventEnvelope, EventType};

pub trait EventObserver: Send + Sync {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()>;
}

impl<F> EventObserver for F
where
    F: Fn(&EventEnvelope) -> io::Result<()> + Send + Sync,
{
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        self(event)
    }
}

pub struct EventSink<W: Write> {
    writer: W,
    format: OutputFormat,
    observer: Option<Arc<dyn EventObserver>>,
}

impl<W: Write> EventSink<W> {
    pub fn new(writer: W, format: OutputFormat) -> Self {
        Self {
            writer,
            format,
            observer: None,
        }
    }

    pub fn with_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn with_optional_observer(mut self, observer: Option<Arc<dyn EventObserver>>) -> Self {
        self.observer = observer;
        self
    }

    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn emit(&mut self, event: EventDraft) -> io::Result<()> {
        event.publish(|event| {
            if let Some(observer) = &self.observer {
                observer.observe(event)?;
            }
            match self.format {
                OutputFormat::Jsonl => {
                    serde_json::to_writer(&mut self.writer, event)?;
                    writeln!(self.writer)?;
                }
                OutputFormat::Text => self.emit_text(event)?,
            }

            self.writer.flush()
        })
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
            EventType::ContextUpdated => {
                let used = event.payload["used_tokens"].as_u64().unwrap_or_default();
                let limit = event.payload["limit_tokens"].as_u64().unwrap_or_default();
                writeln!(self.writer, "context: {used}/{limit} tokens")
            }
            EventType::ContextCompactionStarted => {
                writeln!(self.writer, "context compaction started")
            }
            EventType::ContextCompacted => {
                let before = event.payload["before_messages"].as_u64().unwrap_or(0);
                let after = event.payload["after_messages"].as_u64().unwrap_or(0);
                writeln!(
                    self.writer,
                    "context compacted: {before} -> {after} messages"
                )
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
            EventType::ToolOutputDelta => write!(
                self.writer,
                "{}",
                event.payload["chunk"].as_str().unwrap_or_default()
            ),
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
            EventType::SubagentProgress => {
                let description = event.payload["description"].as_str().unwrap_or("subagent");
                let activity = event.payload["activity"].as_str().unwrap_or("running");
                writeln!(self.writer, "subagent progress: {description} ({activity})")
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
            EventType::TaskStatusUpdated => writeln!(self.writer, "task status updated"),
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

pub fn observe_event(observer: Option<&dyn EventObserver>, event: EventDraft) -> io::Result<()> {
    let Some(observer) = observer else {
        return Ok(());
    };
    event.publish(|event| observer.observe(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_schema::EventFactory;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn jsonl_format_writes_one_line_per_event() {
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Jsonl);
        let mut f = EventFactory::new("run-1".to_string());

        sink.emit(f.error("test error")).unwrap();
        sink.emit(f.assistant_message_delta("hello")).unwrap();

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

        sink.emit(f.error("something broke")).unwrap();
        sink.emit(f.assistant_message_delta("hi")).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("error: something broke"));
        assert!(output.contains("assistant: hi"));
    }

    #[test]
    fn observer_receives_the_same_typed_event_that_is_serialized() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_callback = Arc::clone(&observed);
        let mut buf = Vec::new();
        let mut sink = EventSink::new(&mut buf, OutputFormat::Jsonl).with_observer(Arc::new(
            move |event: &EventEnvelope| {
                observed_for_callback.lock().unwrap().push(event.clone());
                Ok(())
            },
        ));
        let mut events = EventFactory::new("typed-observer".to_string());
        let event = events.assistant_message_delta("hello");
        let expected_run_id = event.run_id.clone();
        let expected_event_type = event.event_type;
        let expected_payload = event.payload.clone();

        sink.emit(event).unwrap();

        let observed = observed.lock().unwrap();
        let observed = observed.first().expect("one observed event");
        assert_eq!(observed.run_id, expected_run_id);
        assert_eq!(observed.seq, 0);
        assert!(observed.timestamp_ms > 0);
        assert_eq!(observed.event_type, expected_event_type);
        assert_eq!(observed.payload, expected_payload);
        let serialized: serde_json::Value =
            serde_json::from_str(String::from_utf8(buf).unwrap().trim()).unwrap();
        assert_eq!(serialized, serde_json::to_value(observed).unwrap());
    }

    #[test]
    fn observer_failure_is_returned_to_the_operation() {
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl).with_observer(Arc::new(
            |_event: &EventEnvelope| Err(io::Error::other("observer rejected event")),
        ));
        let mut events = EventFactory::new("typed-observer-error".to_string());

        let error = sink.emit(events.error("boom")).unwrap_err();

        assert!(error.to_string().contains("observer rejected event"));
    }

    #[test]
    fn observer_failure_consumes_its_sequence_before_cleanup_event() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let calls = Arc::clone(&calls);
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(io::Error::other("observer rejected event"));
                }
                observed.lock().unwrap().push(event.clone());
                Ok(())
            })
        };
        let mut events = EventFactory::new("observer-cleanup".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl).with_observer(observer);

        assert!(sink.emit(events.error("failed delivery")).is_err());
        sink.emit(events.error("cleanup delivery")).unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].seq, 1);
        assert_eq!(observed[0].payload["message"], "cleanup delivery");
    }

    #[test]
    fn forked_factories_number_events_in_publication_order() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let mut first_factory = EventFactory::new("ordered-publication".to_string());
        let mut second_factory = first_factory.fork();
        let first = first_factory.error("constructed first");
        let second = second_factory.error("published first");

        let second_observer = Arc::clone(&observer);
        std::thread::spawn(move || {
            let mut sink =
                EventSink::new(io::sink(), OutputFormat::Jsonl).with_observer(second_observer);
            sink.emit(second).unwrap();
        })
        .join()
        .unwrap();
        EventSink::new(io::sink(), OutputFormat::Jsonl)
            .with_observer(observer)
            .emit(first)
            .unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(
            observed.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(observed[0].payload["message"], "published first");
        assert_eq!(observed[1].payload["message"], "constructed first");
    }

    #[test]
    fn unpublished_draft_does_not_create_a_sequence_gap() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = {
            let observed = Arc::clone(&observed);
            Arc::new(move |event: &EventEnvelope| {
                observed.lock().unwrap().push(event.clone());
                Ok(())
            }) as Arc<dyn EventObserver>
        };
        let mut events = EventFactory::new("draft-gap".to_string());
        drop(events.error("not published"));

        EventSink::new(io::sink(), OutputFormat::Jsonl)
            .with_observer(observer)
            .emit(events.error("published"))
            .unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].seq, 0);
        assert_eq!(observed[0].payload["message"], "published");
    }
}
