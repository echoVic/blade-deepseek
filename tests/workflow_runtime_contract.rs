use std::fs;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_core::task_types::TaskStatus;
use orca_core::workflow_types::WorkflowRunStatus;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::state::{input_hash, WorkflowStateStore};
use orca_runtime::workflow::{WorkflowLaunchRequest, WorkflowRunner};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn workflow_runner_executes_agent_and_writes_state() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'audit', description: 'Audit code', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo'));\nexport default result;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let record = tasks.get(&launched.task_id).unwrap();
    assert_eq!(record.status, orca_core::task_types::TaskStatus::Completed);
    assert!(record.result.unwrap().contains("inspect repo"));
    assert!(launched.output.script_path.unwrap().ends_with(".js"));
    assert!(launched
        .output
        .transcript_dir
        .unwrap()
        .contains("transcripts"));
}

#[test]
fn workflow_resume_uses_completed_agent_cache() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'cache', description: 'Cache test', phases: [] };\nconst result = await agent('inspect repo');\nexport default result;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(first.output.run_id.clone().unwrap()),
        )
        .unwrap();

    assert!(second.summary.contains("cached 1 agent"));
}

#[test]
fn workflow_resume_replays_legacy_object_cache_as_object_value() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'legacy-cache', description: 'Legacy cache test', phases: [] };\nconst result = await agent('inspect repo');\nexport default result.kind;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let resumed_run_id = first.output.run_id.clone().unwrap();
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let cache_path = store.run_dir(&resumed_run_id).join("agent-cache.json");
    let legacy_hash = input_hash("inspect repo", &json!({}));
    let legacy_record = json!({
        format!("root:1:{legacy_hash}"): {
            "call_path": "root:1",
            "input_hash": legacy_hash,
            "output": {
                "kind": "legacy-object"
            }
        }
    });
    fs::write(cache_path, serde_json::to_string_pretty(&legacy_record).unwrap()).unwrap();

    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(resumed_run_id),
        )
        .unwrap();

    let record = tasks.get(&second.task_id).unwrap();
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.result.as_deref(), Some("legacy-object"));
    assert!(second.summary.contains("cached 1 agent"));
}

#[test]
fn workflow_runner_marks_task_and_run_failed_on_host_error() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'host-failure', description: 'Host failure test', phases: [] };\nconsole.log('not-json');\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let error = runner
        .launch(WorkflowLaunchRequest::from(orca_core::workflow_types::WorkflowInput {
            script_path: Some(script.display().to_string()),
            args: Some(serde_json::json!({
                "__orcaHostTestMode": "emit_invalid_json"
            })),
            ..Default::default()
        }))
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert!(record.error.as_deref().is_some());

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert!(state.error.as_deref().is_some());
}

fn mock_run_config(cwd: &std::path::Path) -> RunConfig {
    RunConfig {
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        permission_rules: Default::default(),
        max_budget_usd: None,
        subagents: Default::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: Default::default(),
        vim_mode: false,
        update_check: false,
        desktop_notifications: false,
        auto_memory: false,
    }
}
