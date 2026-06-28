use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approval_types::{ApprovalRequest, ApprovalResolution};
use crate::cost_types::UsageTotals;
use crate::model::ModelRouteDecision;
use crate::plan_types::UpdatePlanArgs;
use crate::provider_types::ProviderReplayState;
use crate::task_types::BackgroundTaskSummary;
use crate::tool_types::{ToolRequest, ToolResult};
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
    #[serde(rename = "usage.updated")]
    UsageUpdated,
    #[serde(rename = "model.routed")]
    ModelRouted,
    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    #[serde(rename = "tool.call.requested")]
    ToolCallRequested,
    #[serde(rename = "tool.call.completed")]
    ToolCallCompleted,
    #[serde(rename = "plan.updated")]
    PlanUpdated,
    #[serde(rename = "subagent.started")]
    SubagentStarted,
    #[serde(rename = "subagent.completed")]
    SubagentCompleted,
    #[serde(rename = "workflow.started")]
    WorkflowStarted,
    #[serde(rename = "workflow.resumed")]
    WorkflowResumed,
    #[serde(rename = "workflow.phase.started")]
    WorkflowPhaseStarted,
    #[serde(rename = "workflow.phase.completed")]
    WorkflowPhaseCompleted,
    #[serde(rename = "workflow.agent.started")]
    WorkflowAgentStarted,
    #[serde(rename = "workflow.agent.cached")]
    WorkflowAgentCached,
    #[serde(rename = "workflow.agent.completed")]
    WorkflowAgentCompleted,
    #[serde(rename = "workflow.agent.failed")]
    WorkflowAgentFailed,
    #[serde(rename = "workflow.paused")]
    WorkflowPaused,
    #[serde(rename = "workflow.stopped")]
    WorkflowStopped,
    #[serde(rename = "workflow.completed")]
    WorkflowCompleted,
    #[serde(rename = "workflow.failed")]
    WorkflowFailed,
    #[serde(rename = "workflow.result.available")]
    WorkflowResultAvailable,
    #[serde(rename = "workflow.tasks.updated")]
    WorkflowTasksUpdated,
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::ApprovalRequired => "approval_required",
            Self::VerificationFailed => "verification_failed",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }

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

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn session_started(
        &mut self,
        cwd: &str,
        approval_mode: &str,
        provider: &str,
        verifier: Option<&str>,
    ) -> EventEnvelope {
        self.make(
            EventType::SessionStarted,
            json!({
                "cwd": cwd,
                "approval_mode": approval_mode,
                "provider": provider,
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

    pub fn usage_updated(&mut self, usage: UsageTotals) -> EventEnvelope {
        self.make(
            EventType::UsageUpdated,
            json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_tokens": usage.cache_tokens,
                "total_tokens": usage.total_tokens(),
                "estimated_cost_usd": usage.estimated_cost_usd
            }),
        )
    }

    pub fn model_routed(&mut self, decision: &ModelRouteDecision) -> EventEnvelope {
        self.make(
            EventType::ModelRouted,
            json!({
                "requested_model": decision.requested_model,
                "actual_model": decision.actual_model,
                "reason": decision.reason
            }),
        )
    }

    pub fn approval_requested(&mut self, request: &ApprovalRequest) -> EventEnvelope {
        self.make(
            EventType::ApprovalRequested,
            json!({
                "id": request.id,
                "action": request.action,
                "description": request.description,
                "tool": request.tool,
                "target": request.target,
                "preview": request.preview
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
                "target": request.target,
                "raw_arguments": request.raw_arguments
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
                "truncated": result.truncated,
                "kind": result.kind
            }),
        )
    }

    pub fn plan_updated(&mut self, update: &UpdatePlanArgs) -> EventEnvelope {
        self.make(EventType::PlanUpdated, json!(update))
    }

    pub fn subagent_started(&mut self, id: &str, description: &str) -> EventEnvelope {
        self.make(
            EventType::SubagentStarted,
            json!({
                "id": id,
                "description": description
            }),
        )
    }

    pub fn subagent_completed(
        &mut self,
        id: &str,
        description: &str,
        status: RunStatus,
        output: Option<&str>,
        error: Option<&str>,
    ) -> EventEnvelope {
        self.make(
            EventType::SubagentCompleted,
            json!({
                "id": id,
                "description": description,
                "status": status,
                "output": output,
                "error": error
            }),
        )
    }

    pub fn workflow_started(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        phases: &[String],
    ) -> EventEnvelope {
        self.make(
            EventType::WorkflowStarted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "phases": phases
            }),
        )
    }

    pub fn workflow_agent_completed(
        &mut self,
        task_id: &str,
        run_id: &str,
        phase: &str,
        agent_id: &str,
        output: &str,
    ) -> EventEnvelope {
        self.make(
            EventType::WorkflowAgentCompleted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "phase": phase,
                "agentId": agent_id,
                "output": output
            }),
        )
    }

    pub fn workflow_completed(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
    ) -> EventEnvelope {
        self.make(
            EventType::WorkflowCompleted,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name
            }),
        )
    }

    pub fn workflow_result_available(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        tool_use_id: Option<&str>,
        status: &str,
        result: &str,
    ) -> EventEnvelope {
        self.make(
            EventType::WorkflowResultAvailable,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "toolUseId": tool_use_id,
                "status": status,
                "result": result
            }),
        )
    }

    pub fn workflow_failed(
        &mut self,
        task_id: &str,
        run_id: &str,
        workflow_name: &str,
        tool_use_id: Option<&str>,
        error: &str,
    ) -> EventEnvelope {
        self.make(
            EventType::WorkflowFailed,
            json!({
                "taskId": task_id,
                "runId": run_id,
                "workflowName": workflow_name,
                "toolUseId": tool_use_id,
                "status": "failed",
                "error": error
            }),
        )
    }

    pub fn workflow_tasks_updated(&mut self, tasks: &[BackgroundTaskSummary]) -> EventEnvelope {
        self.make(
            EventType::WorkflowTasksUpdated,
            json!({
                "tasks": tasks
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_types::{BackgroundTaskSummary, TaskStatus, TaskType, WorkflowTaskProgress};

    #[test]
    fn factory_increments_seq() {
        let mut f = EventFactory::new("run-1".to_string());
        let e0 = f.error("a");
        let e1 = f.error("b");
        let e2 = f.error("c");
        assert_eq!(e0.seq, 0);
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
    }

    #[test]
    fn factory_preserves_run_id() {
        let mut f = EventFactory::new("run-abc".to_string());
        let e = f.error("x");
        assert_eq!(e.run_id, "run-abc");
    }

    #[test]
    fn event_type_serializes_correctly() {
        let s = serde_json::to_string(&EventType::SessionStarted).unwrap();
        assert_eq!(s, "\"session.started\"");

        let s = serde_json::to_string(&EventType::ToolCallCompleted).unwrap();
        assert_eq!(s, "\"tool.call.completed\"");

        let s = serde_json::to_string(&EventType::PlanUpdated).unwrap();
        assert_eq!(s, "\"plan.updated\"");
    }

    #[test]
    fn run_status_exit_codes() {
        assert_eq!(RunStatus::Success.exit_code(), 0);
        assert_eq!(RunStatus::Failed.exit_code(), 1);
        assert_eq!(RunStatus::VerificationFailed.exit_code(), 2);
        assert_eq!(RunStatus::ApprovalRequired.exit_code(), 3);
        assert_eq!(RunStatus::BudgetExhausted.exit_code(), 4);
        assert_eq!(RunStatus::Cancelled.exit_code(), 130);
    }

    #[test]
    fn session_started_payload_structure() {
        let mut f = EventFactory::new("run-1".to_string());
        let e = f.session_started("/tmp", "read-only", "mock", None);
        assert_eq!(e.event_type, EventType::SessionStarted);
        assert_eq!(e.payload["cwd"], "/tmp");
        assert_eq!(e.payload["approval_mode"], "read-only");
        assert_eq!(e.payload["provider"], "mock");
        assert!(e.payload["verifier"].is_null());
    }

    #[test]
    fn turn_started_with_and_without_prompt() {
        let mut f = EventFactory::new("run-1".to_string());

        let e = f.turn_started(1, Some("hello"));
        assert_eq!(e.payload["turn"], 1);
        assert_eq!(e.payload["prompt"], "hello");

        let e = f.turn_started(2, None);
        assert_eq!(e.payload["turn"], 2);
        assert!(e.payload.get("prompt").is_none());
    }

    #[test]
    fn approval_requested_payload_includes_display_metadata() {
        let mut f = EventFactory::new("run-1".to_string());
        let request = ApprovalRequest {
            id: "approval-1".to_string(),
            action: crate::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };

        let event = f.approval_requested(&request);

        assert_eq!(event.event_type, EventType::ApprovalRequested);
        assert_eq!(event.payload["id"], "approval-1");
        assert_eq!(event.payload["action"], "shell");
        assert_eq!(event.payload["description"], "bash requested shell");
        assert_eq!(event.payload["tool"], "bash");
        assert_eq!(event.payload["target"], "echo hi");
        assert_eq!(event.payload["preview"], "$ echo hi");
    }

    #[test]
    fn workflow_result_payload_includes_tui_notification_metadata() {
        let mut f = EventFactory::new("run-1".to_string());

        let event = f.workflow_result_available(
            "task-1",
            "workflow-run-1",
            "mock-workflow",
            Some("tool-use-1"),
            "completed",
            "all phases passed",
        );

        assert_eq!(event.event_type, EventType::WorkflowResultAvailable);
        assert_eq!(event.payload["taskId"], "task-1");
        assert_eq!(event.payload["runId"], "workflow-run-1");
        assert_eq!(event.payload["workflowName"], "mock-workflow");
        assert_eq!(event.payload["toolUseId"], "tool-use-1");
        assert_eq!(event.payload["status"], "completed");
        assert_eq!(event.payload["result"], "all phases passed");
    }

    #[test]
    fn workflow_tasks_updated_payload_includes_task_summaries() {
        let mut f = EventFactory::new("run-1".to_string());
        let task = BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            description: "demo workflow".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("workflow".to_string()),
            name: Some("demo".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: Some(WorkflowTaskProgress {
                total_agents: 3,
                running_agents: 1,
                completed_agents: 2,
                failed_agents: 0,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            }),
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: Some("workflow.md".to_string()),
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
        };

        let event = f.workflow_tasks_updated(&[task]);

        assert_eq!(event.event_type, EventType::WorkflowTasksUpdated);
        assert_eq!(event.payload["tasks"][0]["id"], "task-1");
        assert_eq!(event.payload["tasks"][0]["type"], "workflow");
        assert_eq!(event.payload["tasks"][0]["status"], "running");
        assert_eq!(event.payload["tasks"][0]["workflowRunId"], "workflow-run-1");
        assert_eq!(
            event.payload["tasks"][0]["workflowProgress"]["completedAgents"],
            2
        );
    }

    #[test]
    fn event_envelope_serializes_to_valid_json() {
        let mut f = EventFactory::new("run-1".to_string());
        let e = f.assistant_message_delta("test text");
        let json = serde_json::to_string(&e).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "assistant.message.delta");
        assert_eq!(parsed["payload"]["text"], "test text");
        assert_eq!(parsed["seq"], 0);
        assert_eq!(parsed["version"], "1");
    }
}
