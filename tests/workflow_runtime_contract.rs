use std::fs;
use std::thread;
use std::time::Duration;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
    WorkflowTeamConfig,
};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::model::ModelSelection;
use orca_core::task_types::TaskStatus;
use orca_core::workflow_types::{WorkflowAgentStatus, WorkflowRunStatus};
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
fn workflow_resume_reuses_complex_fallback_agents_as_cached_rows() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'resume-stress', description: 'Resume stress test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('mock_fail'), { fallback: async ({ error }) => agent(`recover ${error.includes('mock child failure requested')}`) });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.prompt}`));\n\
         export default { scan: scan.prompt, review: review.result };",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());

    let first = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("first workflow completes with fallback");
    let first_run_id = first.output.run_id.clone().expect("first run id");
    let second = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(first_run_id),
        )
        .expect("resumed workflow completes with cached fallback agents");
    let second_run_id = second.output.run_id.clone().expect("second run id");
    let third = runner
        .launch(
            WorkflowLaunchRequest::from_script_path(script.display().to_string())
                .with_resume_from(second_run_id),
        )
        .expect("second-generation resume reuses cached rows");

    assert!(second.summary.contains("cached 2 agents"));
    assert!(second.summary.contains("review recovered=recover true"));
    assert!(third.summary.contains("cached 2 agents"));

    let record = tasks.get(&second.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.total_agents, 3);
    assert_eq!(progress.completed_agents, 2);
    assert_eq!(progress.failed_agents, 1);
    assert_eq!(progress.failed_phases, 1);
    assert_eq!(record.workflow_agents.len(), 3);

    let agent_statuses = record
        .workflow_agents
        .iter()
        .map(|agent| (agent.call_path.as_str(), agent.status))
        .collect::<Vec<_>>();
    assert!(agent_statuses.contains(&("scan:1", WorkflowAgentStatus::Failed)));
    assert!(agent_statuses.contains(&("scan:2", WorkflowAgentStatus::Cached)));
    assert!(agent_statuses.contains(&("review:3", WorkflowAgentStatus::Cached)));

    let run_id = second.output.run_id.as_deref().expect("resumed run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("resumed run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].fallback.as_deref(), Some("function"));
    assert_eq!(state.phases[0].agent_count, 2);
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
}

#[test]
fn workflow_child_agents_can_exchange_messages_with_mailbox_tools() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
         "export const meta = { name: 'child-mailbox', description: 'Child mailbox test', phases: ['scan', 'review'] };\n\
         await phase('scan', async () => agent('workflow_send_message findings scanner high'));\n\
         const review = await phase('review', async () => agent('workflow_read_messages findings'));\n\
         await phase('cleanup', async () => agent('workflow_clear_messages findings'));\n\
         const after = await phase('verify', async () => agent('workflow_read_messages findings'));\n\
         export default { before: review.result, after: after.result };",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.approval_mode = ApprovalMode::Suggest;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir);

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with child mailbox tools");

    assert!(launched.summary.contains("scanner"));
    assert!(launched.summary.contains("high"));
    assert!(launched.summary.contains("[]"));
    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.workflow_agents.len(), 4);
}

#[test]
fn workflow_child_agents_can_claim_and_complete_shared_tasks() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'child-tasks', description: 'Child task list test', phases: ['setup', 'work', 'verify'] };\n\
         await phase('setup', async () => agent('workflow_create_task_list audit api docs'));\n\
         await phase('work', async () => agent('workflow_claim_task audit worker-a'));\n\
         await phase('done', async () => agent('workflow_complete_task audit workflow-task-1 worker-a ok'));\n\
         const tasks = await phase('verify', async () => agent('workflow_list_tasks audit'));\n\
         export default tasks.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.approval_mode = ApprovalMode::Suggest;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir);

    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with child task-list tools");

    assert!(launched.summary.contains("workflow-task-1"));
    assert!(launched.summary.contains("completed"));
    assert!(launched.summary.contains("worker-a"));
    assert!(launched.summary.contains("api"));
    assert!(launched.summary.contains("ok"));
    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.workflow_agents.len(), 4);
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
fn workflow_agent_summary_surfaces_team_from_agent_options() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'teams', description: 'Team test', phases: [] };\n\
         await Promise.all([\n\
           agent('inspect api', { team: 'backend' }),\n\
           agent('inspect ui', { team: 'frontend' })\n\
         ]);\n\
         export default 'done';",
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
        .expect("team workflow runs");

    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.workflow_agents.len(), 2);
    let teams = record
        .workflow_agents
        .iter()
        .map(|agent| (agent.call_path.as_str(), agent.team.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(
        teams,
        vec![("root:1", Some("backend")), ("root:2", Some("frontend")),]
    );

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let summaries = store.agent_summaries(run_id).expect("agent summaries");
    assert_eq!(summaries[0].team.as_deref(), Some("backend"));
    assert_eq!(summaries[1].team.as_deref(), Some("frontend"));
}

#[test]
fn workflow_team_policy_enforces_team_token_budget() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'team-policy', description: 'Team policy test', phases: [] };\n\
         export default await agent('mock_usage', { team: 'backend' });",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.teams.insert(
        "backend".to_string(),
        WorkflowTeamConfig {
            max_agent_retries: Some(0),
            max_agent_tokens: Some(100),
            allowed_tools: None,
        },
    );
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("150 tokens exceeded per-agent token budget 100"),
        "team token budget should fail backend agent: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(agent.team.as_deref(), Some("backend"));
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
    assert_eq!(agent.attempt, 1);
    assert_eq!(agent.max_attempts, 1);
    assert_eq!(
        agent
            .usage
            .expect("usage should be preserved")
            .total_tokens(),
        150
    );
}

#[test]
fn workflow_team_policy_blocks_disallowed_tools() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'team-tools', description: 'Team tool policy test', phases: [] };\n\
         export default await agent('bash printf hi', { team: 'backend' });",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.teams.insert(
        "backend".to_string(),
        WorkflowTeamConfig {
            max_agent_retries: Some(0),
            max_agent_tokens: None,
            allowed_tools: Some(vec!["read_file".to_string()]),
        },
    );
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("workflow team 'backend' disallows tool 'bash'"),
        "team tool policy should fail backend agent: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(agent.team.as_deref(), Some("backend"));
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
}

#[test]
fn workflow_agent_token_budget_fails_agent_after_usage_exceeds_limit() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'usage-budget', description: 'Usage budget test', phases: [] };\n\
         export default await agent('mock_usage');",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_tokens = Some(100);
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let error = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("exceeded per-agent token budget"),
        "error should explain the budget failure: {error}"
    );
    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
    assert_eq!(agent.attempt, 1);
    assert_eq!(agent.max_attempts, 2);
    assert!(agent.previous_errors.is_empty());
    assert_eq!(
        agent
            .usage
            .expect("usage should be preserved")
            .total_tokens(),
        150
    );
    assert!(
        agent
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("150 tokens exceeded per-agent token budget 100")
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
            .contains("exceeded per-agent token budget")
    );
}

#[test]
fn workflow_agent_schema_failure_fails_agent_and_run() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'schema-failure', description: 'Schema failure test', phases: [] };\n\
         export default await agent('mock_usage', {\n\
           schema: {\n\
             type: 'object',\n\
             required: ['result'],\n\
             properties: { result: { type: 'object' } }\n\
           }\n\
         });",
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
        error
            .to_string()
            .contains("workflow agent output schema validation failed"),
        "error should explain schema validation failure: {error}"
    );
    assert!(
        error.to_string().contains("result"),
        "schema error should identify the failing property: {error}"
    );

    let task = tasks.list().into_iter().next().expect("workflow task");
    let record = tasks.get(&task.id).expect("task record");
    assert_eq!(record.status, TaskStatus::Failed);
    assert_eq!(record.workflow_agents.len(), 1);
    let agent = &record.workflow_agents[0];
    assert_eq!(
        agent.status,
        orca_core::workflow_types::WorkflowAgentStatus::Failed
    );
    assert!(
        agent
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("workflow agent output schema validation failed")
    );

    let run_id = record.workflow_run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Failed);
}

#[test]
fn workflow_agent_schema_accepts_matching_output() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'schema-success', description: 'Schema success test', phases: [] };\n\
         const result = await agent('mock_usage', {\n\
           schema: {\n\
             type: 'object',\n\
             required: ['prompt', 'result'],\n\
             properties: {\n\
               prompt: { type: 'string' },\n\
               result: { type: 'string' }\n\
             }\n\
           }\n\
         });\n\
         export default `${result.prompt}:${typeof result.result}`;",
    )
    .unwrap();

    let config = mock_run_config(temp.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), temp.path().join("session"));
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("schema-matching workflow runs");

    assert_eq!(launched.summary, "mock_usage:string");
    let record = tasks.get(&launched.task_id).expect("task record");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.workflow_agents.len(), 1);
    assert_eq!(
        record.workflow_agents[0].status,
        orca_core::workflow_types::WorkflowAgentStatus::Completed
    );
}

#[test]
fn workflow_agent_worktree_isolation_preserves_parent_checkout() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let repo = tempdir().unwrap();
    run_git(repo.path(), &["init"]);
    run_git(repo.path(), &["config", "user.email", "orca@example.test"]);
    run_git(repo.path(), &["config", "user.name", "Orca Test"]);
    fs::write(repo.path().join("file.txt"), "placeholder").unwrap();
    run_git(repo.path(), &["add", "file.txt"]);
    run_git(repo.path(), &["commit", "-m", "seed"]);

    let script = repo.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'workflow-worktree', description: 'Worktree test', phases: [] };\n\
         export default await agent('edit file.txt :: placeholder => child', { isolation: 'worktree' });",
    )
    .unwrap();

    let config = mock_run_config(repo.path());
    let tasks = TaskRegistry::new("session-1".to_string());
    let runner = WorkflowRunner::new(config, tasks.clone(), repo.path().join("session"));
    runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow runs");

    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "placeholder"
    );
    let changed_worktree = fs::read_dir(repo.path().join(".orca/worktrees"))
        .expect("worktree directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("file.txt").exists())
        .expect("dirty workflow worktree was preserved");
    assert_eq!(
        fs::read_to_string(changed_worktree.join("file.txt")).unwrap(),
        "child"
    );
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
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
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
fn phase_fallback_continue_records_failed_phase_and_runs_next_phase() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'phase-fallback', description: 'Phase fallback test', phases: ['scan', 'review'] };\n\
         await phase('scan', async () => agent('mock_fail'), { fallback: 'continue' });\n\
         const review = await phase('review', async () => agent('review anyway'));\n\
         export default review.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with fallback");

    assert!(launched.summary.contains("review anyway"));
    let record = tasks.get(&launched.task_id).expect("task record");
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.completed_phases, 1);
    assert_eq!(progress.failed_phases, 1);
    assert_eq!(progress.failed_agents, 1);

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases.len(), 2);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].agent_count, 1);
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
}

#[test]
fn phase_fallback_value_is_returned_and_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'phase-fallback-value', description: 'Phase fallback value test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('mock_fail'), { fallback: { value: { recovered: true, label: 'fallback-value' } } });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.recovered}`));\n\
         export default review.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with fallback value");

    assert!(launched.summary.contains("review recovered=true"));
    let record = tasks.get(&launched.task_id).expect("task record");
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.completed_phases, 1);
    assert_eq!(progress.failed_phases, 1);

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].fallback.as_deref(), Some("value"));
    assert!(
        state.phases[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("mock child failure requested")
    );
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
}

#[test]
fn phase_fallback_function_runs_recovery_agent_and_is_recorded() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'phase-fallback-function', description: 'Phase fallback function test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('mock_fail'), { fallback: async ({ error }) => agent(`recover ${error.includes('mock child failure requested')}`) });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.prompt}`));\n\
         export default review.result;",
    )
    .unwrap();

    let mut config = mock_run_config(temp.path());
    config.workflows.max_agent_retries = 0;
    let tasks = TaskRegistry::new("session-1".to_string());
    let session_dir = temp.path().join("session");
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launched = runner
        .launch(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .expect("workflow completes with fallback function");

    assert!(launched.summary.contains("review recovered=recover true"));
    let record = tasks.get(&launched.task_id).expect("task record");
    let progress = record.workflow_progress.expect("workflow progress");
    assert_eq!(progress.completed_phases, 1);
    assert_eq!(progress.failed_phases, 1);
    assert_eq!(progress.failed_agents, 1);

    let run_id = launched.output.run_id.as_deref().expect("run id");
    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let state = store.load_run(run_id).expect("run state");
    assert_eq!(state.status, WorkflowRunStatus::Completed);
    assert_eq!(state.phases[0].name, "scan");
    assert_eq!(state.phases[0].status, WorkflowRunStatus::Failed);
    assert_eq!(state.phases[0].fallback.as_deref(), Some("function"));
    assert_eq!(state.phases[0].agent_count, 2);
    assert_eq!(state.phases[1].name, "review");
    assert_eq!(state.phases[1].status, WorkflowRunStatus::Completed);
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
    let runner = WorkflowRunner::new(config, tasks.clone(), session_dir.clone());
    let launch = runner
        .launch_background(WorkflowLaunchRequest::from_script_path(
            script.display().to_string(),
        ))
        .unwrap();
    let task_id = launch.task_id.clone();

    let store = WorkflowStateStore::new(session_dir.join("workflow-runs"));
    let run_id = launch.output.run_id.as_deref().expect("run id");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut live_state = None;
    let mut live_task = None;
    while std::time::Instant::now() < deadline {
        let state = store.load_run(run_id).expect("run state");
        if state.status == WorkflowRunStatus::Running && state.total_agent_count == 1 {
            if let Some(task) = tasks.get(&task_id)
                && task.workflow_agents.iter().any(|agent| {
                    agent.call_path == "scan:1"
                        && agent.status == WorkflowAgentStatus::Running
                        && agent.started_at_ms.is_some()
                        && agent.completed_at_ms.is_none()
                })
            {
                live_state = Some(state);
                live_task = Some(task);
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }

    let live_state = live_state.expect("running state should include live phase progress");
    assert_eq!(live_state.phases.len(), 1);
    assert_eq!(live_state.phases[0].name, "scan");
    assert_eq!(live_state.phases[0].status, WorkflowRunStatus::Running);
    assert_eq!(live_state.phases[0].agent_count, 1);
    let live_task = live_task.expect("task summary should include running agent row");
    let live_agent = live_task
        .workflow_agents
        .iter()
        .find(|agent| agent.call_path == "scan:1")
        .expect("running agent summary");
    assert_eq!(live_agent.status, WorkflowAgentStatus::Running);
    assert!(live_agent.started_at_ms.is_some());
    assert_eq!(live_agent.completed_at_ms, None);

    launch.join().unwrap().unwrap();

    let completed_task = tasks.get(&task_id).expect("completed task");
    let completed_agent = completed_task
        .workflow_agents
        .iter()
        .find(|agent| agent.call_path == "scan:1")
        .expect("completed agent summary");
    assert_eq!(completed_agent.status, WorkflowAgentStatus::Completed);
    assert!(completed_agent.started_at_ms.is_some());
    assert!(completed_agent.completed_at_ms.is_some());
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

fn run_git(cwd: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
