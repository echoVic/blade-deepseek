use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType, WorkflowTaskProgress};
use orca_core::tool_types::ToolName;
use orca_core::workflow_types::{WorkflowInput, WorkflowOutput, WorkflowRunStatus};

#[test]
fn workflow_input_accepts_official_fields() {
    let input: WorkflowInput = serde_json::from_value(serde_json::json!({
        "script": "export const meta = { name: 'audit', description: 'Audit code', phases: [] };",
        "name": "audit",
        "description": "ignored",
        "title": "ignored",
        "args": { "paths": ["src"] },
        "draftId": "workflow-draft-1",
        "scriptPath": "/tmp/workflow.js",
        "resumeFromRunId": "workflow-run-1"
    }))
    .unwrap();

    assert!(input.script.unwrap().contains("export const meta"));
    assert_eq!(input.name.as_deref(), Some("audit"));
    assert_eq!(input.args.unwrap()["paths"][0], "src");
    assert_eq!(input.draft_id.as_deref(), Some("workflow-draft-1"));
    assert_eq!(input.script_path.as_deref(), Some("/tmp/workflow.js"));
    assert_eq!(input.resume_from_run_id.as_deref(), Some("workflow-run-1"));
}

#[test]
fn workflow_output_serializes_claude_compatible_shape() {
    let output = WorkflowOutput {
        status: "async_launched".to_string(),
        task_id: "task-1".to_string(),
        task_type: Some("local_workflow".to_string()),
        workflow_name: Some("audit".to_string()),
        run_id: Some("workflow-run-1".to_string()),
        summary: Some("Workflow launched".to_string()),
        transcript_dir: Some("/tmp/transcripts".to_string()),
        script_path: Some("/tmp/script.js".to_string()),
        session_url: None,
    };

    let value = serde_json::to_value(output).unwrap();
    assert_eq!(value["status"], "async_launched");
    assert_eq!(value["taskId"], "task-1");
    assert_eq!(value["taskType"], "local_workflow");
    assert_eq!(value["workflowName"], "audit");
    assert_eq!(value["runId"], "workflow-run-1");
    assert_eq!(value["scriptPath"], "/tmp/script.js");
    assert!(value.get("sessionUrl").is_none());
}

#[test]
fn workflow_tool_name_round_trips() {
    assert_eq!(ToolName::Workflow.as_str(), "Workflow");
    assert_eq!(ToolName::from_str("Workflow"), Some(ToolName::Workflow));
    assert_eq!(ToolName::from_str("workflow"), Some(ToolName::Workflow));
    assert_eq!(ToolName::WorkflowDraft.as_str(), "WorkflowDraft");
    assert_eq!(
        ToolName::from_str("WorkflowDraft"),
        Some(ToolName::WorkflowDraft)
    );
    assert_eq!(
        ToolName::from_str("workflow_draft"),
        Some(ToolName::WorkflowDraft)
    );
    assert_eq!(
        ToolName::WorkflowDraftAction.as_str(),
        "WorkflowDraftAction"
    );
    assert_eq!(
        ToolName::from_str("WorkflowDraftAction"),
        Some(ToolName::WorkflowDraftAction)
    );
    assert_eq!(
        ToolName::from_str("workflow_draft_action"),
        Some(ToolName::WorkflowDraftAction)
    );
}

#[test]
fn background_task_summary_matches_sdk_names() {
    let summary = BackgroundTaskSummary {
        id: "task-1".to_string(),
        task_type: TaskType::Workflow,
        status: TaskStatus::Running,
        description: "Audit codebase".to_string(),
        command: None,
        agent_type: None,
        server: None,
        tool: None,
        name: Some("audit".to_string()),
        workflow_run_id: Some("workflow-run-1".to_string()),
        created_at_ms: 1_000,
        started_at_ms: Some(1_000),
        completed_at_ms: None,
        phase_count: Some(2),
        workflow_progress: Some(WorkflowTaskProgress {
            total_agents: 5,
            running_agents: 1,
            completed_agents: 3,
            failed_agents: 1,
            completed_phases: 1,
            running_phases: 1,
            failed_phases: 0,
        }),
        workflow_phases: Vec::new(),
        workflow_agents: Vec::new(),
        usage: None,
    };

    let value = serde_json::to_value(summary).unwrap();
    assert_eq!(value["type"], "workflow");
    assert_eq!(value["status"], "running");
    assert_eq!(value["name"], "audit");
    assert_eq!(value["workflowRunId"], "workflow-run-1");
    assert_eq!(value["phaseCount"], 2);
    assert_eq!(value["workflowProgress"]["totalAgents"], 5);
    assert_eq!(value["workflowProgress"]["completedAgents"], 3);
    assert_eq!(value["workflowProgress"]["failedAgents"], 1);
    assert!(value.get("workflowPhases").is_none());
    assert!(value.get("usage").is_none());
}

#[test]
fn workflow_status_serializes_snake_case() {
    assert_eq!(
        serde_json::to_value(WorkflowRunStatus::AsyncLaunched).unwrap(),
        "async_launched"
    );
}
