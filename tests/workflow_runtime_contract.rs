use std::fs;
use std::thread;
use std::time::Duration;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::model::ModelSelection;
use orca_core::task_types::TaskStatus;
use orca_core::workflow_types::WorkflowRunStatus;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::state::{WorkflowStateStore, input_hash};
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
    let result = record.result.unwrap();
    assert!(result.contains("inspect repo"));
    assert!(
        !result.contains("Mock child agent completed prompt:"),
        "workflow runner should use the real child-agent executor path"
    );
    assert!(launched.output.script_path.unwrap().ends_with(".js"));
    assert!(
        launched
            .output
            .transcript_dir
            .unwrap()
            .contains("transcripts")
    );
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
    fs::write(
        cache_path,
        serde_json::to_string_pretty(&legacy_record).unwrap(),
    )
    .unwrap();

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
        "export const meta = { name: 'host-failure', description: 'Host failure test', phases: [] };\nthrow new Error('boom from workflow');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error.to_string().contains("boom from workflow"),
        "unexpected host error: {error}"
    );

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

#[test]
fn workflow_runner_marks_task_and_run_failed_on_child_agent_error() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'child-failure', description: 'Child failure test', phases: [] };\nawait agent('mock_fail');\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(error.to_string().contains("mock child failure requested"));

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert!(
        record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("mock child failure requested")
    );

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("mock child failure requested")
    );
}

#[test]
fn workflow_runner_retries_transient_child_agent_failure() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let flaky_prompt = format!("mock_flaky_once {}", uuid::Uuid::new_v4());
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        format!(
            "export const meta = {{ name: 'child-retry', description: 'Child retry test', phases: [] }};\nconst result = await agent({});\nexport default result.result;",
            serde_json::to_string(&flaky_prompt).unwrap()
        ),
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("Mock runtime completed"));

    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.total_agents, 2);
    assert_eq!(progress.running_agents, 0);
    assert_eq!(progress.completed_agents, 1);
    assert_eq!(progress.failed_agents, 1);
    assert_eq!(record.workflow_agents.len(), 1);
    assert_eq!(record.workflow_agents[0].call_path, "root:1");
    assert_eq!(
        record.workflow_agents[0].status,
        orca_core::workflow_types::WorkflowAgentStatus::Completed
    );
    assert_eq!(record.workflow_agents[0].attempt, 2);
    assert_eq!(record.workflow_agents[0].max_attempts, 2);
    assert_eq!(record.workflow_agents[0].previous_errors.len(), 1);

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.total_agent_count, 2);
    assert!(
        store
            .cached_agent_result(run_id, "root:1", &input_hash(&flaky_prompt, &json!({})))
            .unwrap()
            .is_some()
    );
    let cache_path = store.run_dir(run_id).join("agent-cache.json");
    let cache_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cache_path).unwrap()).unwrap();
    let cache_key = format!("root:1:{}", input_hash(&flaky_prompt, &json!({})));
    let record = &cache_json[cache_key];
    assert_eq!(record["attempt"], 2);
    assert_eq!(record["maxAttempts"], 2);
    assert_eq!(record["previousErrors"].as_array().unwrap().len(), 1);
}

#[test]
fn workflow_agent_summary_surfaces_token_usage() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'usage', description: 'Usage test', phases: [] };\n\
         export default await agent('mock_usage');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("usage workflow runs");

    let record = tasks.get(&launched.task_id).expect("task record");
    let usage = record.workflow_agents[0]
        .usage
        .expect("agent usage should be surfaced");
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.cache_tokens, 10);
}

#[test]
fn parallel_preserves_order_and_records_phase() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'parallel', description: 'Parallel test', phases: ['fanout'] };\nconst result = await phase('fanout', async () => parallel([agent('first'), agent('second')]));\nexport default result.map(item => item.prompt).join(',');",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    assert!(launched.summary.contains("first,second"));

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.total_agent_count, 2);
    assert_eq!(state.phases.len(), 1);
    let phase = &state.phases[0];
    assert_eq!(phase.name, "fanout");
    assert_eq!(phase.status, WorkflowRunStatus::Completed);
    assert_eq!(phase.agent_count, 2);
    assert!(phase.started_at_ms.is_some());
    assert!(phase.completed_at_ms.is_some());

    assert!(
        store
            .cached_agent_result(run_id, "fanout:1", &input_hash("first", &json!({})))
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .cached_agent_result(run_id, "fanout:2", &input_hash("second", &json!({})))
            .unwrap()
            .is_some()
    );
}

#[test]
fn workflow_runner_persists_marker_phase_for_following_agents() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'marker-runtime', description: 'Marker runtime test', phases: ['scan'] };\nphase('scan');\nawait agent('inspect repo');\nexport default 'done';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.phases.len(), 1);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].agent_count, 1);
}

#[test]
fn failing_phase_is_persisted_as_failed_and_completed() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'failing-phase', description: 'Failing phase test', phases: ['scan'] };\nawait phase('scan', async () => { await agent('inspect repo'); throw new Error('boom in phase'); });\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let err = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(err.to_string().contains("boom in phase"));

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases.len(), 1);
    let phase = &state.phases[0];
    assert_eq!(phase.name, "scan");
    assert_eq!(phase.status, WorkflowRunStatus::Failed);
    assert_eq!(phase.agent_count, 1);
    assert!(phase.started_at_ms.is_some());
    assert!(phase.completed_at_ms.is_some());
}

#[test]
fn workflow_summary_prefers_child_result_over_agent_prompt() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'summary', description: 'Summary test', phases: [] };\nexport default await agent('review this');",
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

    assert!(launched.summary.contains("review this"));
    assert_ne!(launched.summary, "review this");
}

#[test]
fn workflow_summary_uses_last_dsl_phase_result() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dsl-summary', description: 'DSL summary', phases: [{ name: 'roles', tasks: [{ prompt: 'role draft' }] }, { name: 'synthesis', tasks: [{ prompt: 'final plan' }] }] };",
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

    assert!(launched.summary.contains("final plan"));

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(temp.path().join("session").join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.total_agent_count, 2);
    assert_eq!(state.phases.len(), 2);
    assert_eq!(state.phases[0].name, "roles");
    assert_eq!(state.phases[0].agent_count, 1);
    assert_eq!(state.phases[1].name, "synthesis");
    assert_eq!(state.phases[1].agent_count, 1);
    assert!(
        state
            .final_summary
            .as_deref()
            .unwrap_or_default()
            .contains("final plan")
    );
}

#[test]
fn workflow_runner_streams_running_phase_progress_to_state() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'live-state', description: 'Live state test', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo'));\nexport default result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.hooks = vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "sleep 1.0".to_string(),
        tool: None,
    }];
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());
    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let run_id = launch.output.run_id.as_deref().expect("run id");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut live_state = None;
    while std::time::Instant::now() < deadline {
        let state = store.load_run(run_id).expect("run state");
        if state.status == WorkflowRunStatus::Running && state.total_agent_count == 1 {
            live_state = Some(state);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let live_state = live_state.expect("running state should include live phase progress");
    assert_eq!(live_state.phases.len(), 1);
    assert_eq!(live_state.phases[0].name, "scan");
    assert_eq!(live_state.phases[0].status, WorkflowRunStatus::Running);
    assert_eq!(live_state.phases[0].agent_count, 1);

    launch.join().unwrap().unwrap();
}

#[test]
fn workflow_runner_stops_when_control_file_is_requested() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'stop-control', description: 'Stop control test', phases: [] };\nawait agent('first');\nexport default await agent('second');",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.hooks = vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "sleep 1.0".to_string(),
        tool: None,
    }];
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();

    thread::sleep(Duration::from_millis(250));
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    store
        .request_stop(launch.output.run_id.as_deref().unwrap())
        .expect("request stop");

    let result = launch.join().unwrap().unwrap();

    assert_eq!(result.output.status, "stopped");
    let record = tasks.get(&result.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Stopped);
    let state = store
        .load_run(result.output.run_id.as_deref().unwrap())
        .expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Stopped);
}

#[test]
fn agent_cap_failure_is_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'cap', description: 'Cap test', phases: [] };\nfor (let i = 0; i < 1001; i++) await agent(`agent ${i}`);\nexport default 'unreachable';",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let err = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("maximum workflow agent count 1000 exceeded")
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
    assert_eq!(state.total_agent_count, 1000);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("maximum workflow agent count 1000 exceeded")
    );
}

fn mock_run_config(cwd: &std::path::Path) -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        model_runtime: Default::default(),
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
