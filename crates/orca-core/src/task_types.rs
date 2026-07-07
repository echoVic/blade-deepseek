use serde::{Deserialize, Serialize};

use crate::cost_types::UsageTotals;
use crate::workflow_types::{WorkflowAgentStatus, WorkflowInput, WorkflowRunStatus};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    MainSession,
    Workflow,
    Subagent,
    Shell,
    Monitor,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskProgress {
    pub total_agents: u32,
    pub running_agents: u32,
    pub completed_agents: u32,
    pub failed_agents: u32,
    pub completed_phases: usize,
    pub running_phases: usize,
    pub failed_phases: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentTaskSummary {
    pub call_id: String,
    pub call_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub status: WorkflowAgentStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhaseTaskSummary {
    pub name: String,
    pub status: WorkflowRunStatus,
    pub agent_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskSummary {
    pub id: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub description: String,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_progress: Option<WorkflowTaskProgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_phases: Vec<WorkflowPhaseTaskSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_agents: Vec<WorkflowAgentTaskSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_launch_input: Option<WorkflowInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_final_summary: Option<String>,
    #[serde(default)]
    pub workflow_failure_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_current_activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<i64>,
}
