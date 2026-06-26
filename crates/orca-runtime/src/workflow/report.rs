use std::io;

use orca_core::workflow_types::{
    WorkflowAgentFailureKind, WorkflowAgentStatus, WorkflowEvidenceBundle,
    WorkflowEvidenceFailureKind, WorkflowRunStatus,
};
use serde_json::{Value, json};

use super::state::WorkflowStateStore;

#[derive(Clone, Debug)]
pub struct WorkflowEvidenceReport {
    pub markdown: String,
    pub json: Value,
}

pub fn render_report_for_run(
    store: &WorkflowStateStore,
    run_id: &str,
) -> io::Result<WorkflowEvidenceReport> {
    let bundle = store.load_evidence_bundle(run_id).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("no verified workflow evidence for run {run_id}: {error}"),
        )
    })?;
    Ok(WorkflowEvidenceReport {
        markdown: render_evidence_markdown(&bundle),
        json: render_evidence_json(&bundle),
    })
}

pub fn render_evidence_markdown(bundle: &WorkflowEvidenceBundle) -> String {
    let counts = agent_counts(bundle);
    let retry_count = bundle
        .agents
        .iter()
        .map(|agent| agent.previous_errors.len() as u32)
        .sum::<u32>();
    let token_total = bundle
        .agents
        .iter()
        .filter_map(|agent| agent.usage)
        .map(|usage| usage.total_tokens())
        .sum::<u64>();

    let mut markdown = String::new();
    markdown.push_str("# Workflow Evidence Report\n\n");
    markdown.push_str("| Field | Value |\n");
    markdown.push_str("| --- | --- |\n");
    markdown.push_str(&format!(
        "| Evidence version | {} |\n",
        bundle.evidence_version
    ));
    markdown.push_str(&format!("| Run id | {} |\n", bundle.run_id));
    markdown.push_str(&format!("| Task id | {} |\n", bundle.task_id));
    markdown.push_str(&format!("| Workflow | {} |\n", bundle.workflow_name));
    markdown.push_str(&format!("| Status | {} |\n", status_label(bundle.status)));
    markdown.push_str(&format!(
        "| Total agents | {} |\n",
        bundle.total_agent_count
    ));
    markdown.push_str(&format!(
        "| Max configured concurrent agents | {} |\n",
        bundle.max_configured_concurrent_agents
    ));
    markdown.push_str(&format!(
        "| Max observed concurrent agents | {} |\n",
        bundle.max_observed_concurrent_agents
    ));
    markdown.push_str(&format!("| Evidence agents | {} |\n", bundle.agents.len()));
    markdown.push_str(&format!("| Completed agents | {} |\n", counts.completed));
    markdown.push_str(&format!("| Cached agents | {} |\n", counts.cached));
    markdown.push_str(&format!("| Failed agents | {} |\n", counts.failed));
    markdown.push_str(&format!("| Cancelled agents | {} |\n", counts.cancelled));
    markdown.push_str(&format!("| Retry errors | {} |\n", retry_count));
    markdown.push_str(&format!("| Total tokens | {} |\n", token_total));
    markdown.push_str(&format!(
        "| Generated at ms | {} |\n",
        bundle.identity.generated_at_ms
    ));
    markdown.push_str(&format!(
        "| App version | {} |\n",
        bundle.identity.app_version
    ));
    if let Some(binary_path) = &bundle.identity.binary_path {
        markdown.push_str(&format!("| Binary path | {} |\n", binary_path));
    }
    if let Some(summary) = &bundle.final_summary {
        markdown.push_str(&format!("| Final summary | {} |\n", escape_table(summary)));
    }
    if let Some(error) = &bundle.error {
        markdown.push_str(&format!("| Error | {} |\n", escape_table(error)));
    }

    if !bundle.phases.is_empty() {
        markdown.push_str("\n## Phases\n\n");
        markdown.push_str("| Name | Status | Agents | Fallback | Error |\n");
        markdown.push_str("| --- | --- | ---: | --- | --- |\n");
        for phase in &bundle.phases {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                escape_table(&phase.name),
                status_label(phase.status),
                phase.agent_count,
                phase
                    .fallback
                    .as_deref()
                    .map(escape_table)
                    .unwrap_or_default(),
                phase.error.as_deref().map(escape_table).unwrap_or_default(),
            ));
        }
    }

    if !bundle.failures.is_empty() {
        markdown.push_str("\n## Failures\n\n");
        markdown.push_str(
            "| Kind | Scope | Phase | Call id | Retryable | Retry attempted | Message |\n",
        );
        markdown.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
        for failure in &bundle.failures {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                evidence_failure_kind_label(failure.kind),
                escape_table(&failure.scope),
                failure
                    .phase_name
                    .as_deref()
                    .map(escape_table)
                    .unwrap_or_default(),
                failure
                    .call_id
                    .as_deref()
                    .map(escape_table)
                    .unwrap_or_default(),
                failure
                    .retryable
                    .map(|retryable| retryable.to_string())
                    .unwrap_or_default(),
                failure.retry_attempted,
                failure
                    .message
                    .as_deref()
                    .map(escape_table)
                    .unwrap_or_default(),
            ));
        }
    }

    if !bundle.agents.is_empty() {
        markdown.push_str("\n## Agents\n\n");
        markdown.push_str(
            "| Call id | Path | Team | Status | Failure kind | Retryable | Retry attempted | Attempt | Transcript |\n",
        );
        markdown.push_str("| --- | --- | --- | --- | --- | --- | --- | ---: | --- |\n");
        for agent in &bundle.agents {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {}/{} | {} |\n",
                escape_table(&agent.call_id),
                escape_table(&agent.call_path),
                agent.team.as_deref().map(escape_table).unwrap_or_default(),
                agent_status_label(agent.status),
                agent
                    .failure_kind
                    .map(agent_failure_kind_label)
                    .unwrap_or_default(),
                agent
                    .retryable
                    .map(|retryable| retryable.to_string())
                    .unwrap_or_default(),
                agent.retry_attempted,
                agent.attempt,
                agent.max_attempts,
                agent
                    .transcript_path
                    .as_deref()
                    .map(escape_table)
                    .unwrap_or_default(),
            ));
        }
    }

    markdown
}

pub fn render_evidence_json(bundle: &WorkflowEvidenceBundle) -> Value {
    serde_json::to_value(bundle).unwrap_or_else(|_| {
        json!({
            "evidenceVersion": bundle.evidence_version,
            "runId": bundle.run_id,
            "totalAgentCount": bundle.total_agent_count,
            "status": status_label(bundle.status),
        })
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct AgentCounts {
    completed: u32,
    failed: u32,
    cancelled: u32,
    cached: u32,
}

fn agent_counts(bundle: &WorkflowEvidenceBundle) -> AgentCounts {
    let mut counts = AgentCounts::default();
    for agent in &bundle.agents {
        match agent.status {
            WorkflowAgentStatus::Completed => counts.completed += 1,
            WorkflowAgentStatus::Failed => counts.failed += 1,
            WorkflowAgentStatus::Cancelled => counts.cancelled += 1,
            WorkflowAgentStatus::Cached => counts.cached += 1,
            WorkflowAgentStatus::Pending | WorkflowAgentStatus::Running => {}
        }
    }
    counts
}

fn status_label(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Queued => "queued",
        WorkflowRunStatus::Running => "running",
        WorkflowRunStatus::Paused => "paused",
        WorkflowRunStatus::Stopping => "stopping",
        WorkflowRunStatus::Stopped => "stopped",
        WorkflowRunStatus::Completed => "completed",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
        WorkflowRunStatus::AsyncLaunched => "async_launched",
    }
}

fn agent_status_label(status: WorkflowAgentStatus) -> &'static str {
    match status {
        WorkflowAgentStatus::Pending => "pending",
        WorkflowAgentStatus::Running => "running",
        WorkflowAgentStatus::Cached => "cached",
        WorkflowAgentStatus::Completed => "completed",
        WorkflowAgentStatus::Failed => "failed",
        WorkflowAgentStatus::Cancelled => "cancelled",
    }
}

fn evidence_failure_kind_label(kind: WorkflowEvidenceFailureKind) -> &'static str {
    match kind {
        WorkflowEvidenceFailureKind::AgentFailed => "agent_failed",
        WorkflowEvidenceFailureKind::PhaseFailedContinue => "phase_failed_continue",
        WorkflowEvidenceFailureKind::PhaseFailedBlocked => "phase_failed_blocked",
        WorkflowEvidenceFailureKind::WorkflowFailed => "workflow_failed",
    }
}

fn agent_failure_kind_label(kind: WorkflowAgentFailureKind) -> &'static str {
    match kind {
        WorkflowAgentFailureKind::AgentFailed => "agent_failed",
        WorkflowAgentFailureKind::ToolFailure => "tool_failure",
        WorkflowAgentFailureKind::McpFailure => "mcp_failure",
        WorkflowAgentFailureKind::TokenBudget => "token_budget",
        WorkflowAgentFailureKind::SchemaValidation => "schema_validation",
    }
}

fn escape_table(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}
