use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approval::policy::{ApprovalRequest, ApprovalResolution};
use crate::provider::ProviderReplayState;
use crate::tools::{ToolRequest, ToolResult};
use crate::verification::VerificationResult;

pub const EVENT_SCHEMA_VERSION: &str = "1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventEnvelope {
    pub version: &'static str,
    pub run_id: String,
    pub seq: u64,
    pub timestamp_ms: u128,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EventType {
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "assistant.reasoning.delta")]
    AssistantReasoningDelta,
    #[serde(rename = "assistant.message.delta")]
    AssistantMessageDelta,
    #[serde(rename = "provider.replay.updated")]
    ProviderReplayUpdated,
    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    #[serde(rename = "tool.call.requested")]
    ToolCallRequested,
    #[serde(rename = "tool.call.completed")]
    ToolCallCompleted,
    #[serde(rename = "verification.started")]
    VerificationStarted,
    #[serde(rename = "verification.completed")]
    VerificationCompleted,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "session.completed")]
    SessionCompleted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Failed,
    Cancelled,
    ApprovalRequired,
    VerificationFailed,
    BudgetExhausted,
}

impl RunStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failed => 1,
            Self::VerificationFailed => 2,
            Self::ApprovalRequired => 3,
            Self::BudgetExhausted => 4,
            Self::Cancelled => 130,
        }
    }
}

pub struct EventFactory {
    run_id: String,
    seq: u64,
}

impl EventFactory {
    pub fn new(run_id: String) -> Self {
        Self { run_id, seq: 0 }
    }

    pub fn session_started(
        &mut self,
        cwd: &str,
        approval_mode: &str,
        provider: &str,
        max_turns: Option<u32>,
        verifier: Option<&str>,
    ) -> EventEnvelope {
        self.make(
            EventType::SessionStarted,
            json!({
                "cwd": cwd,
                "approval_mode": approval_mode,
                "provider": provider,
                "max_turns": max_turns,
                "verifier": verifier
            }),
        )
    }

    pub fn turn_started(&mut self, turn: u32, prompt: Option<&str>) -> EventEnvelope {
        let mut payload = json!({ "turn": turn });
        if let Some(p) = prompt {
            payload["prompt"] = json!(p);
        }
        self.make(EventType::TurnStarted, payload)
    }

    pub fn assistant_reasoning_delta(&mut self, text: &str) -> EventEnvelope {
        self.make(
            EventType::AssistantReasoningDelta,
            json!({
                "text": text
            }),
        )
    }

    pub fn assistant_message_delta(&mut self, text: &str) -> EventEnvelope {
        self.make(
            EventType::AssistantMessageDelta,
            json!({
                "text": text
            }),
        )
    }

    pub fn provider_replay_updated(&mut self, replay: &ProviderReplayState) -> EventEnvelope {
        self.make(
            EventType::ProviderReplayUpdated,
            json!({
                "provider": replay.provider,
                "reasoning_content": replay.reasoning_content,
                "tool_call_ids": replay.tool_call_ids
            }),
        )
    }

    pub fn approval_requested(&mut self, request: &ApprovalRequest) -> EventEnvelope {
        self.make(
            EventType::ApprovalRequested,
            json!({
                "id": request.id,
                "action": request.action,
                "description": request.description
            }),
        )
    }

    pub fn approval_resolved(&mut self, resolution: &ApprovalResolution) -> EventEnvelope {
        self.make(
            EventType::ApprovalResolved,
            json!({
                "id": resolution.id,
                "decision": resolution.decision,
                "reason": resolution.reason
            }),
        )
    }

    pub fn tool_call_requested(&mut self, request: &ToolRequest) -> EventEnvelope {
        self.make(
            EventType::ToolCallRequested,
            json!({
                "id": request.id,
                "name": request.name,
                "action": request.action,
                "target": request.target
            }),
        )
    }

    pub fn tool_call_completed(&mut self, result: &ToolResult) -> EventEnvelope {
        self.make(
            EventType::ToolCallCompleted,
            json!({
                "id": result.id,
                "name": result.name,
                "status": result.status,
                "output": result.output,
                "error": result.error,
                "exit_code": result.exit_code,
                "truncated": result.truncated
            }),
        )
    }

    pub fn verification_started(&mut self, command: &str) -> EventEnvelope {
        self.make(
            EventType::VerificationStarted,
            json!({
                "command": command
            }),
        )
    }

    pub fn verification_completed(&mut self, result: &VerificationResult) -> EventEnvelope {
        self.make(
            EventType::VerificationCompleted,
            json!({
                "command": result.command,
                "success": result.success,
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr
            }),
        )
    }

    pub fn error(&mut self, message: &str) -> EventEnvelope {
        self.make(
            EventType::Error,
            json!({
                "message": message
            }),
        )
    }

    pub fn session_completed(&mut self, status: RunStatus) -> EventEnvelope {
        self.make(
            EventType::SessionCompleted,
            json!({
                "status": status
            }),
        )
    }

    fn make(&mut self, event_type: EventType, payload: Value) -> EventEnvelope {
        let envelope = EventEnvelope {
            version: EVENT_SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            seq: self.seq,
            timestamp_ms: timestamp_ms(),
            event_type,
            payload,
        };
        self.seq += 1;
        envelope
    }
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
