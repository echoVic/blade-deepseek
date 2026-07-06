use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::tool_types;
use orca_core::workflow_types::{WorkflowDraftActionOutput, WorkflowInput};

use crate::agent_child::ChildAgentExecutor;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::tasks::TaskRegistry;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow::{
    WorkflowBackgroundLaunch, WorkflowDraftStore, WorkflowLaunchRequest, WorkflowLaunchResult,
    WorkflowRunner,
};

const WORKFLOW_STARTUP_HEALTH_CHECK_POLLS: usize = 2;
const WORKFLOW_STARTUP_HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub(crate) struct BackgroundWorkflowRun {
    task_id: String,
    run_id: String,
    workflow_name: String,
    task: crate::lifecycle::RuntimeTaskLifecycle,
    handle: WorkflowBackgroundLaunch,
}

enum WorkflowStartupStatus {
    StillRunning(WorkflowBackgroundLaunch),
    Completed(WorkflowLaunchResult),
    Failed { error: String },
}

fn wait_for_workflow_startup(launch: WorkflowBackgroundLaunch) -> WorkflowStartupStatus {
    let mut launch = Some(launch);
    for _ in 0..WORKFLOW_STARTUP_HEALTH_CHECK_POLLS {
        if launch
            .as_ref()
            .is_some_and(WorkflowBackgroundLaunch::is_finished)
        {
            break;
        }
        thread::sleep(WORKFLOW_STARTUP_HEALTH_CHECK_INTERVAL);
    }

    let launch = launch.take().expect("launch present");
    if !launch.is_finished() {
        return WorkflowStartupStatus::StillRunning(launch);
    }

    match launch.join() {
        Ok(Ok(result)) => WorkflowStartupStatus::Completed(result),
        Ok(Err(error)) => WorkflowStartupStatus::Failed {
            error: error.to_string(),
        },
        Err(_) => WorkflowStartupStatus::Failed {
            error: "workflow thread panicked".to_string(),
        },
    }
}

fn emit_workflow_completed<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    task: &crate::lifecycle::RuntimeTaskLifecycle,
    task_id: &str,
    run_id: &str,
    workflow_name: &str,
    status_line: &str,
) -> io::Result<()> {
    let completed_task = task.with_status(RuntimeTaskStatus::Succeeded);
    let completed_event =
        completed_task.attach_to_event(events.workflow_completed(task_id, run_id, workflow_name));
    sink.emit(&completed_event)?;
    let result_event = completed_task.attach_to_event(events.workflow_result_available(
        task_id,
        run_id,
        workflow_name,
        None,
        "completed",
        status_line,
    ));
    sink.emit(&result_event)
}

fn emit_workflow_failed<W: io::Write>(
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    task: &crate::lifecycle::RuntimeTaskLifecycle,
    task_id: &str,
    run_id: &str,
    workflow_name: &str,
    error: &str,
) -> io::Result<()> {
    let failed_task = task.with_status(RuntimeTaskStatus::Failed);
    let event = failed_task.attach_to_event(events.workflow_failed(
        task_id,
        run_id,
        workflow_name,
        None,
        error,
    ));
    sink.emit(&event)
}

fn completed_workflow_result(
    tool_request: &tool_types::ToolRequest,
    result: WorkflowLaunchResult,
) -> io::Result<tool_types::ToolResult> {
    let output = serde_json::to_string(&result.output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(tool_types::ToolResult::completed(
        tool_request,
        output,
        false,
    ))
}

pub(crate) fn execute_workflow_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    child_executor: ChildAgentExecutor<SharedEventBuffer>,
) -> io::Result<tool_types::ToolResult> {
    if !config.workflows.enabled {
        return Ok(tool_types::ToolResult::failed(
            tool_request,
            "workflows are disabled",
            None,
        ));
    }

    let input = parse_workflow_input(tool_request)?;
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir)
        .with_child_executor(child_executor);
    let launch = runner.launch_background(WorkflowLaunchRequest::from(input))?;
    let task_id = launch.task_id.clone();
    let run_id = launch.run_id.clone();
    let workflow_name = launch.workflow_name.clone();
    let mut workflow_lifecycle = RuntimeSessionLifecycle::new(launch.run_id.clone());
    let workflow_task = workflow_lifecycle
        .start_task(RuntimeTaskKind::Workflow)
        .clone();
    if emit_deltas {
        let event = workflow_task.attach_to_event(events.workflow_started(
            &launch.task_id,
            &launch.run_id,
            &launch.workflow_name,
            &launch.phases,
        ));
        sink.emit(&event)?;
    }

    match wait_for_workflow_startup(launch) {
        WorkflowStartupStatus::StillRunning(launch) => {
            let output = serde_json::to_string(&launch.output)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            background_workflows.push(BackgroundWorkflowRun {
                task_id: launch.task_id.clone(),
                run_id: launch.run_id.clone(),
                workflow_name: launch.workflow_name.clone(),
                task: workflow_task,
                handle: launch,
            });
            Ok(tool_types::ToolResult::completed(
                tool_request,
                output,
                false,
            ))
        }
        WorkflowStartupStatus::Completed(result) => {
            if emit_deltas {
                emit_workflow_completed(
                    events,
                    sink,
                    &workflow_task,
                    &task_id,
                    &run_id,
                    &workflow_name,
                    &result.status_line,
                )?;
            }
            completed_workflow_result(tool_request, result)
        }
        WorkflowStartupStatus::Failed { error } => {
            if emit_deltas {
                emit_workflow_failed(
                    events,
                    sink,
                    &workflow_task,
                    &task_id,
                    &run_id,
                    &workflow_name,
                    &error,
                )?;
            }
            Ok(tool_types::ToolResult::failed(tool_request, error, None))
        }
    }
}

pub(crate) fn execute_workflow_draft_action_tool(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    emit_deltas: bool,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    child_executor: ChildAgentExecutor<SharedEventBuffer>,
) -> io::Result<tool_types::ToolResult> {
    if !config.workflows.enabled {
        return Ok(tool_types::ToolResult::failed(
            tool_request,
            "workflows are disabled",
            None,
        ));
    }

    let input = parse_workflow_draft_action_input(tool_request)?;
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = draft_store.load(&input.draft_id)?;

    let output = match input.action.as_str() {
        "run" => {
            let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir)
                .with_child_executor(child_executor);
            let launch = runner.launch_background(WorkflowLaunchRequest::from(WorkflowInput {
                draft_id: Some(input.draft_id.clone()),
                args: input.args.clone(),
                ..Default::default()
            }))?;
            let task_id = launch.task_id.clone();
            let run_id = launch.run_id.clone();
            let workflow_name = launch.workflow_name.clone();
            let mut workflow_lifecycle = RuntimeSessionLifecycle::new(launch.run_id.clone());
            let workflow_task = workflow_lifecycle
                .start_task(RuntimeTaskKind::Workflow)
                .clone();
            if emit_deltas {
                let event = workflow_task.attach_to_event(events.workflow_started(
                    &launch.task_id,
                    &launch.run_id,
                    &launch.workflow_name,
                    &launch.phases,
                ));
                sink.emit(&event)?;
            }
            match wait_for_workflow_startup(launch) {
                WorkflowStartupStatus::StillRunning(launch) => {
                    let action_output = WorkflowDraftActionOutput {
                        status: "async_launched".to_string(),
                        action: "run".to_string(),
                        draft_id: input.draft_id.clone(),
                        workflow_name: launch.workflow_name.clone(),
                        saved_path: None,
                        task_id: Some(launch.task_id.clone()),
                        run_id: Some(launch.run_id.clone()),
                        script_path: launch.output.script_path.clone(),
                    };
                    background_workflows.push(BackgroundWorkflowRun {
                        task_id: launch.task_id.clone(),
                        run_id: launch.run_id.clone(),
                        workflow_name: launch.workflow_name.clone(),
                        task: workflow_task,
                        handle: launch,
                    });
                    action_output
                }
                WorkflowStartupStatus::Completed(result) => {
                    if emit_deltas {
                        emit_workflow_completed(
                            events,
                            sink,
                            &workflow_task,
                            &task_id,
                            &run_id,
                            &workflow_name,
                            &result.status_line,
                        )?;
                    }
                    WorkflowDraftActionOutput {
                        status: "completed".to_string(),
                        action: "run".to_string(),
                        draft_id: input.draft_id.clone(),
                        workflow_name: result
                            .output
                            .workflow_name
                            .clone()
                            .unwrap_or_else(|| workflow_name.clone()),
                        saved_path: None,
                        task_id: Some(result.task_id),
                        run_id: result.output.run_id,
                        script_path: result
                            .output
                            .script_path
                            .or_else(|| Some(draft.script_path.clone())),
                    }
                }
                WorkflowStartupStatus::Failed { error } => {
                    if emit_deltas {
                        emit_workflow_failed(
                            events,
                            sink,
                            &workflow_task,
                            &task_id,
                            &run_id,
                            &workflow_name,
                            &error,
                        )?;
                    }
                    return Ok(tool_types::ToolResult::failed(tool_request, error, None));
                }
            }
        }
        "edit" => {
            let script = input.script.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workflow draft action edit requires script",
                )
            })?;
            let edited = draft_store.edit_script(
                &input.draft_id,
                script,
                config.workflows.max_concurrent_agents,
            )?;
            WorkflowDraftActionOutput {
                status: "edited".to_string(),
                action: "edit".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name: edited.name,
                saved_path: None,
                task_id: None,
                run_id: None,
                script_path: Some(edited.script_path),
            }
        }
        "save" => {
            let workflow_dir = match input.scope.as_deref().unwrap_or("project") {
                "project" => cwd.join(".orca").join("workflows"),
                "user" => dirs::home_dir()
                    .unwrap_or_else(|| cwd.to_path_buf())
                    .join(".orca")
                    .join("workflows"),
                other => {
                    return Ok(tool_types::ToolResult::invalid_input(
                        tool_request,
                        format!("unsupported workflow draft save scope: {other}"),
                    ));
                }
            };
            let saved_path = draft_store.save_reusable(
                &input.draft_id,
                &workflow_dir,
                input.save_as.as_deref(),
            )?;
            WorkflowDraftActionOutput {
                status: "saved".to_string(),
                action: "save".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name: draft.name,
                saved_path: Some(saved_path.display().to_string()),
                task_id: None,
                run_id: None,
                script_path: Some(draft.script_path),
            }
        }
        "cancel" => {
            draft_store.cancel(&input.draft_id)?;
            WorkflowDraftActionOutput {
                status: "cancelled".to_string(),
                action: "cancel".to_string(),
                draft_id: input.draft_id,
                workflow_name: draft.name,
                saved_path: None,
                task_id: None,
                run_id: None,
                script_path: None,
            }
        }
        other => {
            return Ok(tool_types::ToolResult::invalid_input(
                tool_request,
                format!("unsupported workflow draft action: {other}"),
            ));
        }
    };

    let output = serde_json::to_string(&output)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(tool_types::ToolResult::completed(
        tool_request,
        output,
        false,
    ))
}

pub(crate) fn observe_background_workflows(
    wait: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
) -> io::Result<()> {
    if !wait {
        return Ok(());
    }

    for workflow in background_workflows.drain(..) {
        match workflow.handle.join() {
            Ok(Ok(result)) => {
                emit_workflow_completed(
                    events,
                    sink,
                    &workflow.task,
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                    &result.status_line,
                )?;
            }
            Ok(Err(error)) => {
                emit_workflow_failed(
                    events,
                    sink,
                    &workflow.task,
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                    &error.to_string(),
                )?;
            }
            Err(_) => {
                emit_workflow_failed(
                    events,
                    sink,
                    &workflow.task,
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                    "workflow thread panicked",
                )?;
            }
        }
    }

    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDraftActionInput {
    draft_id: String,
    action: String,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    save_as: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

fn parse_workflow_draft_action_input(
    tool_request: &tool_types::ToolRequest,
) -> io::Result<WorkflowDraftActionInput> {
    let raw_arguments = tool_request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn parse_workflow_input(tool_request: &tool_types::ToolRequest) -> io::Result<WorkflowInput> {
    let raw_arguments = tool_request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ToolConfig, WorkflowConfig,
    };
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::model::ModelSelection;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::tasks::TaskRegistry;
    use crate::workflow::host::WorkflowHost;
    use crate::workflow::runner::SharedEventBuffer;

    use super::{
        BackgroundWorkflowRun, WorkflowDraftStore, execute_workflow_draft_action_tool,
        execute_workflow_tool,
    };

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn tool_request(id: &str, name: ToolName, raw_arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    fn unused_child_executor(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, SharedEventBuffer>,
        _cost: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("startup failure test must not execute child agents")
    }

    fn startup_failure_script() -> &'static str {
        r#"
throw new Error("startup boom");
export const meta = {
  name: "bad-workflow",
  description: "Fails on load",
  phases: [{ name: "main", tasks: [{ prompt: "noop" }] }]
};
"#
    }

    #[test]
    fn workflow_tool_reports_immediate_startup_failure() {
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("session-workflow-immediate-failure".to_string());
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::<BackgroundWorkflowRun>::new();
        let request = tool_request(
            "workflow",
            ToolName::Workflow,
            serde_json::json!({ "script": startup_failure_script() }),
        );

        let result = execute_workflow_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("startup boom")),
            "expected startup failure details, got {result:?}"
        );
        assert!(background_workflows.is_empty());
    }

    #[test]
    fn workflow_draft_action_run_reports_immediate_startup_failure() {
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("session-draft-immediate-failure".to_string());
        let session_dir = temp
            .path()
            .join(".orca")
            .join("workflow-sessions")
            .join(registry.session_id());
        let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
        let draft = draft_store
            .create_from_script(
                registry.session_id(),
                temp.path(),
                startup_failure_script(),
                config.workflows.max_concurrent_agents,
            )
            .unwrap();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut background_workflows = Vec::<BackgroundWorkflowRun>::new();
        let request = tool_request(
            "run",
            ToolName::WorkflowDraftAction,
            serde_json::json!({ "draftId": draft.draft_id, "action": "run" }),
        );

        let result = execute_workflow_draft_action_tool(
            &config,
            temp.path(),
            &mut events,
            &mut sink,
            &request,
            true,
            &registry,
            &mut background_workflows,
            unused_child_executor,
        )
        .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("startup boom")),
            "expected startup failure details, got {result:?}"
        );
        assert!(background_workflows.is_empty());
    }
}
