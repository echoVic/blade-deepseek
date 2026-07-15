use crossbeam_channel::Sender;
use std::path::Path;
use std::thread;
use std::time::Duration;

use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::tool_types;
#[cfg(test)]
use orca_core::workflow_types::WorkflowDraftActionOutput;
use orca_core::workflow_types::WorkflowInput;
use orca_runtime::tasks::TaskRegistry;
#[cfg(test)]
use orca_runtime::workflow::WorkflowDraftStore;
use orca_runtime::workflow::{
    WorkflowBackgroundLaunch, WorkflowLaunchRequest, WorkflowLaunchResult, WorkflowRunner,
};
#[cfg(test)]
use serde::Deserialize;

use crate::agent_runner::{
    WorkflowNotificationPayload, send_task_status_updated_for_tui,
    send_workflow_notification_for_tui, send_workflow_tasks_updated_for_tui, task_summary_for_tui,
};
use crate::types::TuiEvent;

const WORKFLOW_STARTUP_HEALTH_CHECK_POLLS: usize = 2;
const WORKFLOW_STARTUP_HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(300);

fn send_workflow_task_status_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    registry: &TaskRegistry,
    task_id: &str,
) {
    if let Some(task) = task_summary_for_tui(registry, task_id) {
        send_task_status_updated_for_tui(event_tx, events, &task);
    }
}

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn execute_workflow_draft_action_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    execute_workflow_draft_action_for_tui_with_notifications(
        config,
        cwd,
        request,
        event_tx,
        event_tx,
        task_registry,
    )
}

#[cfg(test)]
pub(crate) fn execute_workflow_draft_action_for_tui_with_notifications(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    notification_event_tx: &Sender<TuiEvent>,
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
            let mut task_events = EventFactory::new(run_id.clone());
            send_workflow_task_status_for_tui(event_tx, &mut task_events, task_registry, &task_id);
            let launch = match wait_for_workflow_startup(launch) {
                WorkflowStartupStatus::StillRunning(launch) => launch,
                WorkflowStartupStatus::Completed(result) => {
                    send_workflow_task_status_for_tui(
                        event_tx,
                        &mut task_events,
                        task_registry,
                        &task_id,
                    );
                    return completed_workflow_draft_action_result(
                        request,
                        &input.draft_id,
                        &draft.script_path,
                        result,
                    );
                }
                WorkflowStartupStatus::Failed { error } => {
                    send_workflow_task_status_for_tui(
                        event_tx,
                        &mut task_events,
                        task_registry,
                        &task_id,
                    );
                    return tool_types::ToolResult::failed(request, error, None);
                }
            };
            let task_id_for_notification = task_id.clone();
            let run_id_for_notification = run_id.clone();
            let tool_use_id_for_notification = tool_use_id.clone();
            let workflow_name_for_notification = workflow_name.clone();
            let notify_tx = notification_event_tx.clone();
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
                send_workflow_task_status_for_tui(
                    &notify_tx,
                    &mut events,
                    &notify_registry,
                    &task_id,
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
    execute_workflow_for_tui_with_notifications(
        config,
        cwd,
        request,
        event_tx,
        event_tx,
        task_registry,
    )
}

pub(crate) fn execute_workflow_for_tui_with_notifications(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    notification_event_tx: &Sender<TuiEvent>,
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
    send_workflow_task_status_for_tui(event_tx, &mut task_events, task_registry, &task_id);
    let launch = match wait_for_workflow_startup(launch) {
        WorkflowStartupStatus::StillRunning(launch) => launch,
        WorkflowStartupStatus::Completed(result) => {
            send_workflow_task_status_for_tui(event_tx, &mut task_events, task_registry, &task_id);
            return completed_workflow_result(request, result);
        }
        WorkflowStartupStatus::Failed { error } => {
            send_workflow_task_status_for_tui(event_tx, &mut task_events, task_registry, &task_id);
            return tool_types::ToolResult::failed(request, error, None);
        }
    };

    let notify_tx = notification_event_tx.clone();
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
        send_workflow_task_status_for_tui(&notify_tx, &mut events, &notify_registry, &task_id);
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

#[cfg(test)]
fn completed_workflow_draft_action_result(
    request: &tool_types::ToolRequest,
    draft_id: &str,
    draft_script_path: &str,
    result: WorkflowLaunchResult,
) -> tool_types::ToolResult {
    let action_output = WorkflowDraftActionOutput {
        status: "completed".to_string(),
        action: "run".to_string(),
        draft_id: draft_id.to_string(),
        workflow_name: result
            .output
            .workflow_name
            .clone()
            .unwrap_or_else(|| "workflow".to_string()),
        saved_path: None,
        task_id: Some(result.task_id),
        run_id: result.output.run_id,
        script_path: result
            .output
            .script_path
            .or_else(|| Some(draft_script_path.to_string())),
    };
    match serde_json::to_string(&action_output) {
        Ok(output) => tool_types::ToolResult::completed(request, output, false),
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

fn completed_workflow_result(
    request: &tool_types::ToolRequest,
    result: WorkflowLaunchResult,
) -> tool_types::ToolResult {
    match serde_json::to_string(&result.output) {
        Ok(output) => tool_types::ToolResult::completed(request, output, false),
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

fn parse_workflow_input(request: &tool_types::ToolRequest) -> std::io::Result<WorkflowInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(test)]
struct WorkflowDraftInput {
    script: String,
}

#[cfg(test)]
fn parse_workflow_draft_input(
    request: &tool_types::ToolRequest,
) -> std::io::Result<WorkflowDraftInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg(test)]
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

#[cfg(test)]
fn parse_workflow_draft_action_input(
    request: &tool_types::ToolRequest,
) -> std::io::Result<WorkflowDraftActionInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;

    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use orca_core::model::ModelSelection;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
    use orca_runtime::tasks::TaskRegistry;
    use orca_runtime::workflow::host::WorkflowHost;

    use super::{
        execute_workflow_draft_action_for_tui, execute_workflow_draft_for_tui,
        execute_workflow_for_tui,
    };

    fn tool_request(id: &str, name: ToolName, raw_arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            id: id.to_string(),
            name,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
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
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn workflow_draft_action_run_reports_immediate_startup_failure() {
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("session-immediate-failure".to_string());
        let (event_tx, _event_rx) = mpsc::unbounded();
        let script = r#"
export const meta = {
  name: "bad-workflow",
  description: "Fails on load",
  phases: [{ name: "main", tasks: [{ prompt: "noop" }] }]
};
throw new Error("startup boom");
"#;

        let draft_result = execute_workflow_draft_for_tui(
            &config,
            temp.path(),
            &tool_request(
                "draft",
                ToolName::WorkflowDraft,
                serde_json::json!({ "script": script }),
            ),
            &registry,
        );
        assert_eq!(draft_result.status, ToolStatus::Completed);
        let draft_output = draft_result.output.as_deref().expect("draft output");
        let draft: serde_json::Value = serde_json::from_str(draft_output).unwrap();
        let draft_id = draft["draftId"].as_str().expect("draft id");

        let run_result = execute_workflow_draft_action_for_tui(
            &config,
            temp.path(),
            &tool_request(
                "run",
                ToolName::WorkflowDraftAction,
                serde_json::json!({ "draftId": draft_id, "action": "run" }),
            ),
            &event_tx,
            &registry,
        );

        assert_eq!(run_result.status, ToolStatus::Failed);
        assert!(
            run_result
                .output
                .as_deref()
                .is_some_and(|output| output.contains("startup boom"))
                || run_result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("startup boom")),
            "expected immediate failure details, got output={:?} error={:?}",
            run_result.output,
            run_result.error
        );
    }

    #[test]
    fn workflow_tool_reports_immediate_startup_failure() {
        if !WorkflowHost::node_available() {
            return;
        }

        let config = config();
        let temp = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("session-workflow-immediate-failure".to_string());
        let (event_tx, _event_rx) = mpsc::unbounded();
        let script = r#"
export const meta = {
  name: "bad-workflow",
  description: "Fails on load",
  phases: [{ name: "main", tasks: [{ prompt: "noop" }] }]
};
throw new Error("startup boom");
"#;

        let result = execute_workflow_for_tui(
            &config,
            temp.path(),
            &tool_request(
                "workflow",
                ToolName::Workflow,
                serde_json::json!({ "script": script }),
            ),
            &event_tx,
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .output
                .as_deref()
                .is_some_and(|output| output.contains("startup boom"))
                || result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("startup boom")),
            "expected immediate failure details, got output={:?} error={:?}",
            result.output,
            result.error
        );
    }
}
