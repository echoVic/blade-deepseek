use std::io;

use orca_core::workflow_types::{WorkflowEvidenceBundle, WorkflowRunStatus};
use serde::{Deserialize, Serialize};

use super::state::WorkflowStateStore;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowVerificationStatus {
    Proven,
    NotProven,
    Failed,
    CompletedWithFailures,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowVerificationReport {
    pub status: WorkflowVerificationStatus,
    pub run_id: String,
    pub evidence_status: WorkflowRunStatus,
    pub failure_count: u32,
    pub mailbox_present: bool,
    pub task_lists_present: bool,
    pub transcript_count: u32,
    pub missing_transcript_count: u32,
}

pub fn verify_workflow_run(
    store: &WorkflowStateStore,
    run_id: &str,
) -> io::Result<WorkflowVerificationReport> {
    let evidence = store.load_evidence_bundle(run_id)?;
    let mailbox_present = artifact_is_readable(&store.mailbox_path(run_id));
    let task_lists_present = artifact_is_readable(&store.task_lists_path(run_id));
    let (transcript_count, missing_transcript_count) = transcript_counts(&evidence);
    let failure_count = evidence.failures.len() as u32;
    let status = verification_status(&evidence, failure_count, missing_transcript_count);

    Ok(WorkflowVerificationReport {
        status,
        run_id: run_id.to_string(),
        evidence_status: evidence.status,
        failure_count,
        mailbox_present,
        task_lists_present,
        transcript_count,
        missing_transcript_count,
    })
}

fn artifact_is_readable(path: &std::path::Path) -> bool {
    path.exists() && std::fs::read_to_string(path).is_ok()
}

fn transcript_counts(evidence: &WorkflowEvidenceBundle) -> (u32, u32) {
    let mut present = 0u32;
    let mut missing = 0u32;
    for agent in &evidence.agents {
        let Some(path) = agent.transcript_path.as_deref() else {
            missing += 1;
            continue;
        };
        if artifact_is_readable(std::path::Path::new(path)) {
            present += 1;
        } else {
            missing += 1;
        }
    }
    (present, missing)
}

fn verification_status(
    evidence: &WorkflowEvidenceBundle,
    failure_count: u32,
    missing_transcript_count: u32,
) -> WorkflowVerificationStatus {
    match evidence.status {
        WorkflowRunStatus::Failed | WorkflowRunStatus::Cancelled | WorkflowRunStatus::Stopped => {
            WorkflowVerificationStatus::Failed
        }
        WorkflowRunStatus::Completed if failure_count > 0 => {
            WorkflowVerificationStatus::CompletedWithFailures
        }
        WorkflowRunStatus::Completed if missing_transcript_count > 0 => {
            WorkflowVerificationStatus::NotProven
        }
        WorkflowRunStatus::Completed => WorkflowVerificationStatus::Proven,
        WorkflowRunStatus::Queued
        | WorkflowRunStatus::Running
        | WorkflowRunStatus::Paused
        | WorkflowRunStatus::Stopping
        | WorkflowRunStatus::AsyncLaunched => WorkflowVerificationStatus::NotProven,
    }
}
