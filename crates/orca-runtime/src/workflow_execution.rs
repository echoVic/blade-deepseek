use std::io;
use std::path::Path;

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
    WorkflowBackgroundLaunch, WorkflowDraftStore, WorkflowLaunchRequest, WorkflowRunner,
};

#[derive(Debug)]
pub(crate) struct BackgroundWorkflowRun {
    task_id: String,
    run_id: String,
    workflow_name: String,
    task: crate::lifecycle::RuntimeTaskLifecycle,
    handle: WorkflowBackgroundLaunch,
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
                let completed_task = workflow.task.with_status(RuntimeTaskStatus::Succeeded);
                let completed_event = completed_task.attach_to_event(events.workflow_completed(
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                ));
                sink.emit(&completed_event)?;
                let result_event =
                    completed_task.attach_to_event(events.workflow_result_available(
                        &workflow.task_id,
                        &workflow.run_id,
                        &workflow.workflow_name,
                        None,
                        "completed",
                        &result.status_line,
                    ));
                sink.emit(&result_event)?;
            }
            Ok(Err(error)) => {
                let failed_task = workflow.task.with_status(RuntimeTaskStatus::Failed);
                let event = failed_task.attach_to_event(events.workflow_failed(
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                    None,
                    &error.to_string(),
                ));
                sink.emit(&event)?;
            }
            Err(_) => {
                let failed_task = workflow.task.with_status(RuntimeTaskStatus::Failed);
                let event = failed_task.attach_to_event(events.workflow_failed(
                    &workflow.task_id,
                    &workflow.run_id,
                    &workflow.workflow_name,
                    None,
                    "workflow thread panicked",
                ));
                sink.emit(&event)?;
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
