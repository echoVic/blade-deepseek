use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cost_types::UsageTotals;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from_run_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOutput {
    pub status: String,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Cancelled,
    AsyncLaunched,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentStatus {
    Pending,
    Running,
    Cached,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhaseRecord {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub agent_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunState {
    pub run_id: String,
    pub task_id: String,
    pub session_id: String,
    pub cwd: String,
    pub workflow_name: String,
    pub meta: WorkflowMeta,
    pub script_digest: String,
    pub args_digest: String,
    pub status: WorkflowRunStatus,
    #[serde(default)]
    pub phases: Vec<WorkflowPhaseRecord>,
    pub total_agent_count: u32,
    pub final_summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceIdentity {
    pub app_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    pub generated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidencePhase {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub agent_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceAgent {
    pub call_id: String,
    pub call_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub input_hash: String,
    pub status: WorkflowAgentStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    #[serde(default)]
    pub previous_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvidenceBundle {
    pub evidence_version: u32,
    pub identity: WorkflowEvidenceIdentity,
    pub run_id: String,
    pub task_id: String,
    pub session_id: String,
    pub cwd: String,
    pub workflow_name: String,
    pub meta: WorkflowMeta,
    pub script_digest: String,
    pub args_digest: String,
    pub status: WorkflowRunStatus,
    #[serde(default)]
    pub phases: Vec<WorkflowEvidencePhase>,
    pub total_agent_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub agents: Vec<WorkflowEvidenceAgent>,
}
