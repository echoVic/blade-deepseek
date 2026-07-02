use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;

use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::tool_types;
use orca_core::workflow_types::{WorkflowDraftActionOutput, WorkflowInput};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::{WorkflowDraftStore, WorkflowLaunchRequest, WorkflowRunner};
use serde::Deserialize;

use crate::agent_runner::{
    WorkflowNotificationPayload, send_workflow_notification_for_tui,
    send_workflow_tasks_updated_for_tui,
};
use crate::types::TuiEvent;

pub(crate) fn execute_workflow_draft_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    if !config.workflows.enabled {
        return tool_types::ToolResult::failed(request, "workflows are disabled", None);
    }
    let input = match parse_workflow_draft_input(request) {
        Ok(input) => input,
        Err(error) => return tool_types::ToolResult::invalid_input(request, error.to_string()),
    };
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = match draft_store.create_from_script(
        task_registry.session_id(),
        cwd,
        &input.script,
        config.workflows.max_concurrent_agents,
    ) {
        Ok(draft) => draft,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };
    match serde_json::to_string(&draft) {
        Ok(output) => tool_types::ToolResult::completed(request, output, false),
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

pub(crate) fn execute_workflow_draft_action_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    if !config.workflows.enabled {
        return tool_types::ToolResult::failed(request, "workflows are disabled", None);
    }
    let input = match parse_workflow_draft_action_input(request) {
        Ok(input) => input,
        Err(error) => return tool_types::ToolResult::invalid_input(request, error.to_string()),
    };
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = match draft_store.load(&input.draft_id) {
        Ok(draft) => draft,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };

    let output = match input.action.as_str() {
        "run" => {
            let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir);
            let launch =
                match runner.launch_background(WorkflowLaunchRequest::from(WorkflowInput {
                    draft_id: Some(input.draft_id.clone()),
                    args: input.args.clone(),
                    ..Default::default()
                })) {
                    Ok(launch) => launch,
                    Err(error) => {
                        return tool_types::ToolResult::failed(request, error.to_string(), None);
                    }
                };
            let task_id = launch.task_id.clone();
            let run_id = launch.run_id.clone();
            let workflow_name = launch.workflow_name.clone();
            let tool_use_id = request.id.clone();
            let task_id_for_notification = task_id.clone();
            let run_id_for_notification = run_id.clone();
            let tool_use_id_for_notification = tool_use_id.clone();
            let workflow_name_for_notification = workflow_name.clone();
            let mut task_events = EventFactory::new(run_id.clone());
            send_workflow_tasks_updated_for_tui(event_tx, &mut task_events, &task_registry.list());
            let notify_tx = event_tx.clone();
            let notify_registry = task_registry.clone();
            thread::spawn(move || {
                let mut events = EventFactory::new(run_id_for_notification.clone());
                while !launch.is_finished() {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    send_workflow_tasks_updated_for_tui(
                        &notify_tx,
                        &mut events,
                        &notify_registry.list(),
                    );
                }
                let (task_id, status, summary) = match launch.join() {
                    Ok(Ok(result)) => (result.task_id, "completed".to_string(), result.status_line),
                    Ok(Err(error)) => (
                        task_id_for_notification.clone(),
                        "failed".to_string(),
                        error.to_string(),
                    ),
                    Err(_) => (
                        task_id_for_notification,
                        "failed".to_string(),
                        "workflow thread panicked".to_string(),
                    ),
                };
                send_workflow_tasks_updated_for_tui(
                    &notify_tx,
                    &mut events,
                    &notify_registry.list(),
                );
                send_workflow_notification_for_tui(
                    &notify_tx,
                    &mut events,
                    WorkflowNotificationPayload {
                        task_id: &task_id,
                        run_id: &run_id_for_notification,
                        tool_use_id: &tool_use_id_for_notification,
                        workflow_name: &workflow_name_for_notification,
                        status: &status,
                        summary: &summary,
                    },
                );
            });
            WorkflowDraftActionOutput {
                status: "async_launched".to_string(),
                action: "run".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name,
                saved_path: None,
                task_id: Some(task_id),
                run_id: Some(run_id),
                script_path: Some(draft.script_path),
            }
        }
        "save" => {
            let workflow_dir = match input.scope.as_deref().unwrap_or("project") {
                "project" => cwd.join(".orca").join("workflows"),
                "user" => std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| cwd.to_path_buf())
                    .join(".orca")
                    .join("workflows"),
                other => {
                    return tool_types::ToolResult::invalid_input(
                        request,
                        format!("unsupported workflow draft save scope: {other}"),
                    );
                }
            };
            let saved_path = match draft_store.save_reusable(
                &input.draft_id,
                &workflow_dir,
                input.save_as.as_deref(),
            ) {
                Ok(path) => path,
                Err(error) => {
                    return tool_types::ToolResult::failed(request, error.to_string(), None);
                }
            };
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
        "edit" => {
            let Some(script) = input.script.as_deref() else {
                return tool_types::ToolResult::invalid_input(
                    request,
                    "workflow draft action edit requires script",
                );
            };
            let edited = match draft_store.edit_script(
                &input.draft_id,
                script,
                config.workflows.max_concurrent_agents,
            ) {
                Ok(edited) => edited,
                Err(error) => {
                    return tool_types::ToolResult::failed(request, error.to_string(), None);
                }
            };
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
        "cancel" => {
            if let Err(error) = draft_store.cancel(&input.draft_id) {
                return tool_types::ToolResult::failed(request, error.to_string(), None);
            }
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
            return tool_types::ToolResult::invalid_input(
                request,
                format!("unsupported workflow draft action: {other}"),
            );
        }
    };

    match serde_json::to_string(&output) {
        Ok(output) => tool_types::ToolResult::completed(request, output, false),
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

pub(crate) fn execute_workflow_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    if !config.workflows.enabled {
        return tool_types::ToolResult::failed(request, "workflows are disabled", None);
    }

    let input = match parse_workflow_input(request) {
        Ok(input) => input,
        Err(error) => return tool_types::ToolResult::invalid_input(request, error.to_string()),
    };
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir);
    let launch = match runner.launch_background(WorkflowLaunchRequest::from(input)) {
        Ok(launch) => launch,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };

    let task_id = launch.task_id.clone();
    let run_id = launch.run_id.clone();
    let workflow_name = launch.workflow_name.clone();
    let tool_use_id = request.id.clone();
    let output = match serde_json::to_string(&launch.output) {
        Ok(output) => output,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };
    let mut task_events = EventFactory::new(run_id.clone());
    send_workflow_tasks_updated_for_tui(event_tx, &mut task_events, &task_registry.list());

    let notify_tx = event_tx.clone();
    let notify_registry = task_registry.clone();
    thread::spawn(move || {
        let mut events = EventFactory::new(run_id.clone());
        while !launch.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(300));
            send_workflow_tasks_updated_for_tui(&notify_tx, &mut events, &notify_registry.list());
        }
        let (task_id, status, summary) = match launch.join() {
            Ok(Ok(result)) => (result.task_id, "completed".to_string(), result.status_line),
            Ok(Err(error)) => (task_id, "failed".to_string(), error.to_string()),
            Err(_) => (
                task_id,
                "failed".to_string(),
                "workflow thread panicked".to_string(),
            ),
        };
        send_workflow_tasks_updated_for_tui(&notify_tx, &mut events, &notify_registry.list());
        send_workflow_notification_for_tui(
            &notify_tx,
            &mut events,
            WorkflowNotificationPayload {
                task_id: &task_id,
                run_id: &run_id,
                tool_use_id: &tool_use_id,
                workflow_name: &workflow_name,
                status: &status,
                summary: &summary,
            },
        );
    });

    tool_types::ToolResult::completed(request, output, false)
}

fn parse_workflow_input(request: &tool_types::ToolRequest) -> std::io::Result<WorkflowInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDraftInput {
    script: String,
}

fn parse_workflow_draft_input(
    request: &tool_types::ToolRequest,
) -> std::io::Result<WorkflowDraftInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[derive(Deserialize)]
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
    request: &tool_types::ToolRequest,
) -> std::io::Result<WorkflowDraftActionInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}
