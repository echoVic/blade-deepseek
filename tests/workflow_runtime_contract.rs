use std::fs;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::{WorkflowLaunchRequest, WorkflowRunner};
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
