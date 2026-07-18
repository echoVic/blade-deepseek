//! Projection of internal [`EventEnvelope`] events onto ACP `session/update`
//! notifications. Pure functions only — no runtime state — so the mapping is
//! unit-testable in isolation.

use agent_client_protocol::{
    ContentBlock, ContentChunk, Diff, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use orca_core::event_schema::{EventEnvelope, EventType, RunStatus};
use serde_json::Value;

/// Maps a single runtime event to an ACP session update, or `None` when the
/// event has no client-facing projection in this version.
pub fn event_to_session_update(event: &EventEnvelope) -> Option<SessionUpdate> {
    let payload = &event.payload;
    match event.event_type {
        EventType::AssistantMessageDelta => {
            let text = payload["text"].as_str().unwrap_or_default();
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from(text.to_string()),
            )))
        }
        EventType::AssistantReasoningDelta => {
            let text = payload["text"].as_str().unwrap_or_default();
            Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                ContentBlock::from(text.to_string()),
            )))
        }
        EventType::ToolCallRequested => Some(tool_call_requested(payload)),
        EventType::ToolCallCompleted => Some(tool_call_completed(payload)),
        EventType::PlanUpdated => Some(plan_updated(payload)),
        EventType::Error => {
            let message = payload["message"].as_str().unwrap_or("unknown error");
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from(format!("Error: {message}")),
            )))
        }
        _ => None,
    }
}

fn tool_call_requested(payload: &Value) -> SessionUpdate {
    let id = payload["id"].as_str().unwrap_or_default();
    let name = payload["name"].as_str().unwrap_or_default();
    let action = payload["action"].as_str();
    let target = payload["target"].as_str();
    let title = match target {
        Some(target) if !target.is_empty() => format!("{name}: {target}"),
        _ => name.to_string(),
    };
    let raw_input = payload["raw_arguments"]
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    SessionUpdate::ToolCall(
        ToolCall::new(id.to_string(), title)
            .kind(tool_kind(name, action))
            .status(ToolCallStatus::InProgress)
            .raw_input(raw_input),
    )
}

fn tool_call_completed(payload: &Value) -> SessionUpdate {
    let id = payload["id"].as_str().unwrap_or_default();
    let status = tool_status(payload["status"].as_str().unwrap_or_default());
    let mut content: Vec<ToolCallContent> = Vec::new();

    let output = payload["output"].as_str().unwrap_or_default();
    let error = payload["error"].as_str().unwrap_or_default();
    let body = if !error.is_empty() {
        error
    } else {
        output
    };
    if !body.is_empty() {
        content.push(ToolCallContent::from(body.to_string()));
    }

    if let Some(diff_text) = payload["diff"].as_str() {
        let path = payload["target"]
            .as_str()
            .or_else(|| payload["name"].as_str())
            .unwrap_or("change")
            .to_string();
        content.push(ToolCallContent::from(Diff::new(path, diff_text.to_string())));
    }

    let mut fields = ToolCallUpdateFields::new().status(status);
    if !content.is_empty() {
        fields = fields.content(content);
    }
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.to_string(), fields))
}

fn plan_updated(payload: &Value) -> SessionUpdate {
    let entries = payload["plan"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let step = item["step"].as_str().unwrap_or_default().to_string();
                    let status = map_plan_status(item["status"].as_str().unwrap_or_default());
                    PlanEntry::new(step, PlanEntryPriority::Medium, status)
                })
                .collect()
        })
        .unwrap_or_default();
    SessionUpdate::Plan(Plan::new(entries))
}

/// Chooses an ACP tool kind from the internal tool name, falling back on the
/// coarse action classification when the specific name is unknown.
fn tool_kind(name: &str, action: Option<&str>) -> ToolKind {
    match name {
        "read_file" | "list_files" | "read_mcp_resource" | "list_mcp_resources"
        | "list_mcp_resource_templates" | "read_skill" | "list_skills" => ToolKind::Read,
        "glob" | "grep" | "web_search" => ToolKind::Search,
        "bash" | "git_status" => ToolKind::Execute,
        "edit" | "write_file" => ToolKind::Edit,
        "update_plan" => ToolKind::Think,
        _ => match action {
            Some("read") => ToolKind::Read,
            Some("write") => ToolKind::Edit,
            Some("network") => ToolKind::Fetch,
            Some("shell") => ToolKind::Execute,
            _ => ToolKind::Other,
        },
    }
}

/// Collapses the internal tool status onto the narrower ACP status set.
fn tool_status(status: &str) -> ToolCallStatus {
    match status {
        "completed" => ToolCallStatus::Completed,
        _ => ToolCallStatus::Failed,
    }
}

fn map_plan_status(status: &str) -> PlanEntryStatus {
    match status {
        "in_progress" => PlanEntryStatus::InProgress,
        "completed" => PlanEntryStatus::Completed,
        _ => PlanEntryStatus::Pending,
    }
}

/// Maps a terminal run status onto the ACP prompt stop reason.
pub fn run_status_to_stop_reason(status: RunStatus) -> StopReason {
    match status {
        RunStatus::Cancelled => StopReason::Cancelled,
        RunStatus::BudgetExhausted => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(event_type: EventType, payload: Value) -> EventEnvelope {
        EventEnvelope {
            version: "1".to_string(),
            run_id: "run".to_string(),
            seq: 0,
            timestamp_ms: 0,
            event_type,
            payload,
        }
    }

    #[test]
    fn message_delta_maps_to_agent_message_chunk() {
        let event = envelope(EventType::AssistantMessageDelta, json!({ "text": "hello" }));
        match event_to_session_update(&event) {
            Some(SessionUpdate::AgentMessageChunk(chunk)) => match chunk.content {
                ContentBlock::Text(text) => assert_eq!(text.text, "hello"),
                other => panic!("unexpected content block: {other:?}"),
            },
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn reasoning_delta_maps_to_agent_thought_chunk() {
        let event = envelope(
            EventType::AssistantReasoningDelta,
            json!({ "text": "thinking" }),
        );
        assert!(matches!(
            event_to_session_update(&event),
            Some(SessionUpdate::AgentThoughtChunk(_))
        ));
    }

    #[test]
    fn tool_call_requested_maps_to_tool_call() {
        let event = envelope(
            EventType::ToolCallRequested,
            json!({
                "id": "tc-1",
                "name": "read_file",
                "action": "read",
                "target": "src/main.rs",
                "raw_arguments": "{\"path\":\"src/main.rs\"}"
            }),
        );
        match event_to_session_update(&event) {
            Some(SessionUpdate::ToolCall(call)) => {
                assert_eq!(call.title, "read_file: src/main.rs");
                assert_eq!(call.kind, ToolKind::Read);
                assert_eq!(call.status, ToolCallStatus::InProgress);
                assert_eq!(call.raw_input, Some(json!({ "path": "src/main.rs" })));
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn tool_call_completed_maps_to_update_with_content() {
        let event = envelope(
            EventType::ToolCallCompleted,
            json!({
                "id": "tc-1",
                "name": "read_file",
                "status": "completed",
                "output": "file body",
                "error": null
            }),
        );
        match event_to_session_update(&event) {
            Some(SessionUpdate::ToolCallUpdate(update)) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
                assert!(update.fields.content.is_some());
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn tool_call_completed_failed_status_collapses() {
        let event = envelope(
            EventType::ToolCallCompleted,
            json!({
                "id": "tc-1",
                "name": "bash",
                "status": "denied",
                "error": "not allowed"
            }),
        );
        match event_to_session_update(&event) {
            Some(SessionUpdate::ToolCallUpdate(update)) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn tool_call_completed_with_diff_adds_diff_content() {
        let event = envelope(
            EventType::ToolCallCompleted,
            json!({
                "id": "tc-1",
                "name": "edit",
                "status": "completed",
                "output": "",
                "target": "src/lib.rs",
                "diff": "@@ -1 +1 @@\n-a\n+b\n"
            }),
        );
        match event_to_session_update(&event) {
            Some(SessionUpdate::ToolCallUpdate(update)) => {
                let content = update.fields.content.expect("content present");
                assert!(
                    content
                        .iter()
                        .any(|item| matches!(item, ToolCallContent::Diff(_)))
                );
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn plan_updated_maps_to_plan() {
        let event = envelope(
            EventType::PlanUpdated,
            json!({
                "explanation": "doing things",
                "plan": [
                    { "step": "first", "status": "completed" },
                    { "step": "second", "status": "in_progress" },
                    { "step": "third", "status": "pending" }
                ]
            }),
        );
        match event_to_session_update(&event) {
            Some(SessionUpdate::Plan(plan)) => {
                assert_eq!(plan.entries.len(), 3);
                assert_eq!(plan.entries[0].status, PlanEntryStatus::Completed);
                assert_eq!(plan.entries[1].status, PlanEntryStatus::InProgress);
                assert_eq!(plan.entries[2].status, PlanEntryStatus::Pending);
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn error_event_maps_to_agent_message_chunk() {
        let event = envelope(EventType::Error, json!({ "message": "boom" }));
        match event_to_session_update(&event) {
            Some(SessionUpdate::AgentMessageChunk(chunk)) => match chunk.content {
                ContentBlock::Text(text) => assert_eq!(text.text, "Error: boom"),
                other => panic!("unexpected content block: {other:?}"),
            },
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn ignored_events_map_to_none() {
        assert!(
            event_to_session_update(&envelope(EventType::SessionCompleted, json!({}))).is_none()
        );
        assert!(event_to_session_update(&envelope(EventType::TurnStarted, json!({}))).is_none());
        assert!(event_to_session_update(&envelope(EventType::UsageUpdated, json!({}))).is_none());
    }

    #[test]
    fn tool_kind_mapping() {
        assert_eq!(tool_kind("grep", None), ToolKind::Search);
        assert_eq!(tool_kind("bash", None), ToolKind::Execute);
        assert_eq!(tool_kind("write_file", None), ToolKind::Edit);
        assert_eq!(tool_kind("unknown", Some("network")), ToolKind::Fetch);
        assert_eq!(tool_kind("unknown", None), ToolKind::Other);
    }

    #[test]
    fn run_status_stop_reason_mapping() {
        assert_eq!(
            run_status_to_stop_reason(RunStatus::Success),
            StopReason::EndTurn
        );
        assert_eq!(
            run_status_to_stop_reason(RunStatus::Cancelled),
            StopReason::Cancelled
        );
        assert_eq!(
            run_status_to_stop_reason(RunStatus::BudgetExhausted),
            StopReason::MaxTokens
        );
    }
}
