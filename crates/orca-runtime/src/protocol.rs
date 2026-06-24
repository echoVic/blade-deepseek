use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    pub id: Value,
    pub op: ClientOp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientOp {
    Submit { prompt: String },
}

#[derive(Debug, PartialEq)]
pub struct DecodeError {
    pub id: Value,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct WireSubmission {
    id: Value,
    op: String,
    prompt: Option<String>,
}

impl Submission {
    pub fn decode(line: &str) -> Result<Self, DecodeError> {
        let wire = serde_json::from_str::<WireSubmission>(line).map_err(|error| DecodeError {
            id: Value::Null,
            message: format!("invalid request: {error}"),
        })?;
        match wire.op.as_str() {
            "submit" => Ok(Self {
                id: wire.id,
                op: ClientOp::Submit {
                    prompt: wire.prompt.unwrap_or_default(),
                },
            }),
            op => Err(DecodeError {
                id: wire.id,
                message: format!("unsupported op: {op}"),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ServerEventEnvelope {
    pub id: Value,
    #[serde(flatten)]
    pub event: ServerEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    TurnStarted {
        turn: Value,
    },
    ReasoningDelta {
        text: Value,
    },
    MessageDelta {
        text: Value,
    },
    ToolRequested {
        tool: Value,
        target: Value,
    },
    ToolCompleted {
        tool: Value,
        status: Value,
    },
    WorkflowStarted {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        #[serde(rename = "workflowName")]
        workflow_name: Value,
    },
    WorkflowResultAvailable {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        result: Value,
    },
    WorkflowCompleted {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        #[serde(rename = "workflowName")]
        workflow_name: Value,
    },
    WorkflowFailed {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        error: Value,
    },
    Error {
        message: String,
    },
    TurnCompleted {
        status: Value,
    },
}

impl ServerEvent {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

pub fn map_runtime_event_line(line: &str) -> Option<ServerEvent> {
    let event: Value = serde_json::from_str(line).ok()?;
    let payload = &event["payload"];
    match event["type"].as_str()? {
        "turn.started" => Some(ServerEvent::TurnStarted {
            turn: payload["turn"].clone(),
        }),
        "assistant.reasoning.delta" => Some(ServerEvent::ReasoningDelta {
            text: payload["text"].clone(),
        }),
        "assistant.message.delta" => Some(ServerEvent::MessageDelta {
            text: payload["text"].clone(),
        }),
        "tool.call.requested" => Some(ServerEvent::ToolRequested {
            tool: payload["name"].clone(),
            target: payload["target"].clone(),
        }),
        "tool.call.completed" => Some(ServerEvent::ToolCompleted {
            tool: payload["name"].clone(),
            status: payload["status"].clone(),
        }),
        "workflow.started" => Some(ServerEvent::WorkflowStarted {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            workflow_name: payload["workflowName"].clone(),
        }),
        "workflow.result.available" => Some(ServerEvent::WorkflowResultAvailable {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            result: payload["result"].clone(),
        }),
        "workflow.completed" => Some(ServerEvent::WorkflowCompleted {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            workflow_name: payload["workflowName"].clone(),
        }),
        "workflow.failed" => Some(ServerEvent::WorkflowFailed {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            error: payload["error"].clone(),
        }),
        "error" => Some(ServerEvent::Error {
            message: payload["message"].as_str().unwrap_or_default().to_string(),
        }),
        "session.completed" => Some(ServerEvent::TurnCompleted {
            status: payload["status"].clone(),
        }),
        _ => None,
    }
}

pub fn write_server_event<W: Write>(
    writer: &mut W,
    id: &Value,
    event: ServerEvent,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *writer,
        &ServerEventEnvelope {
            id: id.clone(),
            event,
        },
    )?;
    writeln!(writer)?;
    writer.flush()
}

pub fn legacy_json_event(id: Value, event: ServerEvent) -> Value {
    serde_json::to_value(ServerEventEnvelope { id, event }).unwrap_or_else(|error| {
        json!({
            "id": null,
            "event": "error",
            "message": format!("failed to encode protocol event: {error}")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_decodes_submit_wire_shape() {
        let submission =
            Submission::decode(r#"{"id":1,"op":"submit","prompt":"hello"}"#).expect("submission");

        assert_eq!(submission.id, Value::from(1));
        assert_eq!(
            submission.op,
            ClientOp::Submit {
                prompt: "hello".to_string()
            }
        );
    }

    #[test]
    fn submission_keeps_request_id_for_unsupported_ops() {
        let error = Submission::decode(r#"{"id":"req-1","op":"interrupt"}"#).expect_err("error");

        assert_eq!(error.id, Value::from("req-1"));
        assert_eq!(error.message, "unsupported op: interrupt");
    }

    #[test]
    fn server_event_serializes_legacy_flat_shape() {
        let value = legacy_json_event(
            Value::from(7),
            ServerEvent::ToolCompleted {
                tool: Value::from("read_file"),
                status: Value::from("completed"),
            },
        );

        assert_eq!(value["id"], 7);
        assert_eq!(value["event"], "tool_completed");
        assert_eq!(value["tool"], "read_file");
        assert_eq!(value["status"], "completed");
        assert!(value.get("type").is_none());
    }

    #[test]
    fn maps_runtime_session_completed_event() {
        let event = map_runtime_event_line(
            r#"{"type":"session.completed","payload":{"status":"success"}}"#,
        )
        .expect("event");

        assert_eq!(
            event,
            ServerEvent::TurnCompleted {
                status: Value::from("success")
            }
        );
    }
}
