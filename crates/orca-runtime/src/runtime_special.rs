use std::io;
use std::path::Path;

use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::extension::RuntimeExtensionStores;
use crate::lifecycle::{
    AllowRequestedPermissions, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimeSubagentStatusLookup, RuntimeToolActorContext, RuntimeUsageTotals, RuntimeWorkflowIpc,
};
use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};
use crate::runtime_state::RuntimeTurnReducer;
use crate::tasks::TaskRegistry;
use crate::workflow::WorkflowDraftStore;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimePermissionRequestArgs {
    #[serde(default)]
    reason: Option<String>,
    permissions: RequestPermissionProfile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSpecialToolDispatch {
    WorkflowDraft,
    WorkflowDraftAction,
    Workflow,
    Subagent,
    SubagentStatus,
    TaskList,
    TaskStop,
    RequestPermissions,
    RequestUserInput,
    WorkflowIpc,
    Normal,
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeWorkflowDraftRequest<'a> {
    pub workflows_enabled: bool,
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub max_concurrent_agents: usize,
}

impl RuntimeToolActorContext {
    pub fn classify_dispatch(&self, request: &ToolRequest) -> RuntimeSpecialToolDispatch {
        match request.name {
            ToolName::WorkflowDraft => RuntimeSpecialToolDispatch::WorkflowDraft,
            ToolName::WorkflowDraftAction => RuntimeSpecialToolDispatch::WorkflowDraftAction,
            ToolName::Workflow => RuntimeSpecialToolDispatch::Workflow,
            ToolName::Subagent => RuntimeSpecialToolDispatch::Subagent,
            ToolName::SubagentStatus => RuntimeSpecialToolDispatch::SubagentStatus,
            ToolName::TaskList => RuntimeSpecialToolDispatch::TaskList,
            ToolName::TaskStop => RuntimeSpecialToolDispatch::TaskStop,
            ToolName::RequestPermissions => RuntimeSpecialToolDispatch::RequestPermissions,
            ToolName::RequestUserInput => RuntimeSpecialToolDispatch::RequestUserInput,
            ToolName::WorkflowSendMessage
            | ToolName::WorkflowReadMessages
            | ToolName::WorkflowClearMessages
            | ToolName::WorkflowCreateTaskList
            | ToolName::WorkflowClaimTask
            | ToolName::WorkflowCompleteTask
            | ToolName::WorkflowListTasks => RuntimeSpecialToolDispatch::WorkflowIpc,
            _ => RuntimeSpecialToolDispatch::Normal,
        }
    }

    pub fn execute_request_permissions_tool(&mut self, request: &ToolRequest) -> ToolResult {
        self.execute_request_permissions_tool_with_handler(request, &AllowRequestedPermissions)
    }

    pub fn execute_request_permissions_tool_with_handler(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimePermissionRequestHandler,
    ) -> ToolResult {
        let args = match parse_runtime_permission_request(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let permission_request = RuntimePermissionRequest {
            id: request.id.clone(),
            reason: args.reason,
            permissions: args.permissions,
        };
        let reducer = RuntimeTurnReducer::from_extension_stores(RuntimeExtensionStores::new(
            &self.thread_extensions,
            &self.turn_extensions,
        ));
        let response = match reducer.request_permission(
            &mut self.permission_overlay,
            handler,
            permission_request.clone(),
        ) {
            Ok(response) => response,
            Err(error) => return ToolResult::failed(request, error.to_string(), None),
        };
        if response.decision == PermissionResponseDecision::Deny {
            return ToolResult::denied(request, "permission request denied".to_string());
        }
        let write_roots = response
            .permissions
            .file_system
            .as_ref()
            .and_then(|file_system| file_system.write.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();
        let read_roots = response
            .permissions
            .file_system
            .as_ref()
            .and_then(|file_system| file_system.read.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let write_roots_json = write_roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let network_enabled = response
            .permissions
            .network
            .as_ref()
            .and_then(|network| network.enabled);
        let network_domains = response
            .permissions
            .network
            .as_ref()
            .map(|network| network.domains.clone())
            .unwrap_or_default();
        let output = json!({
            "message": "Permissions granted for the current turn",
            "reason": permission_request.reason,
            "granted": {
                "fileSystem": {
                    "read": read_roots,
                    "write": write_roots_json,
                },
                "network": {
                    "enabled": network_enabled,
                    "domains": network_domains,
                },
            },
            "scope": response.scope,
            "persistent": response.scope == PermissionGrantScope::Session,
            "strictAutoReview": response.strict_auto_review,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_workflow_ipc_tool(
        &mut self,
        request: &ToolRequest,
        workflow_ipc: Option<&dyn RuntimeWorkflowIpc>,
    ) -> ToolResult {
        let Some(workflow_ipc) = workflow_ipc else {
            return ToolResult::failed(
                request,
                "workflow IPC tools are only available inside workflow child agents",
                None,
            );
        };
        let raw = request.raw_arguments.as_deref().unwrap_or("{}");
        let args: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::invalid_input(
                    request,
                    format!("arguments are not valid JSON: {error}"),
                );
            }
        };
        let result = match request.name {
            ToolName::WorkflowSendMessage => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                let message = args.get("message").cloned().unwrap_or(Value::Null);
                let from = args.get("from").and_then(Value::as_str);
                workflow_ipc.send_message(channel, from, message)
            }
            ToolName::WorkflowReadMessages => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                workflow_ipc.read_messages(channel)
            }
            ToolName::WorkflowClearMessages => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                workflow_ipc.clear_messages(channel)
            }
            ToolName::WorkflowCreateTaskList => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let items = match args.get("items").and_then(Value::as_array) {
                    Some(items) => items.clone(),
                    None => {
                        return ToolResult::invalid_input(
                            request,
                            "missing required array field: items",
                        );
                    }
                };
                workflow_ipc.create_task_list(name, items)
            }
            ToolName::WorkflowClaimTask => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let by = args.get("by").and_then(Value::as_str);
                workflow_ipc.claim_task(name, by)
            }
            ToolName::WorkflowCompleteTask => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let task_id = match required_string_arg(request, &args, "task_id") {
                    Ok(task_id) => task_id,
                    Err(result) => return result,
                };
                let result = args.get("result").cloned().unwrap_or(Value::Null);
                let by = args.get("by").and_then(Value::as_str);
                workflow_ipc.complete_task(name, task_id, result, by)
            }
            ToolName::WorkflowListTasks => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                workflow_ipc.list_tasks(name)
            }
            _ => unreachable!("workflow IPC tool dispatch guarded by caller"),
        };

        match result {
            Ok(value) => ToolResult::completed(request, value.to_string(), false),
            Err(error) => ToolResult::invalid_input(request, error),
        }
    }

    pub fn execute_subagent_status_tool(
        &mut self,
        request: &ToolRequest,
        lookup: &dyn RuntimeSubagentStatusLookup,
    ) -> ToolResult {
        let agent_id =
            extract_tool_string_field(request, "agent_id").or_else(|| request.target.clone());
        let Some(agent_id) = agent_id else {
            return ToolResult::invalid_input(request, "missing agent_id");
        };
        let Some(record) = lookup.subagent_status_record(&agent_id) else {
            return ToolResult::failed(request, format!("subagent '{agent_id}' not found"), None);
        };
        let (result_output, result_task) = record
            .output
            .as_deref()
            .map(unpack_async_subagent_result)
            .unwrap_or((None, None));
        let (error_output, error_task) = record
            .error
            .as_deref()
            .map(unpack_async_subagent_result)
            .unwrap_or((None, None));
        let output = json!({
            "agent_id": agent_id,
            "status": record.status,
            "description": record.description,
            "agent_type": record.agent_type,
            "created_at_ms": record.created_at_ms,
            "started_at_ms": record.started_at_ms,
            "completed_at_ms": record.completed_at_ms,
            "output": result_output,
            "error": error_output,
            "task": result_task.or(error_task),
            "usage": record.usage.map(runtime_usage_totals_json),
            "current_activity": record.subagent_current_activity,
            "turn": record.subagent_turn,
            "last_activity_at_ms": record.last_activity_at_ms,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_task_list_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let tasks = task_registry
            .list()
            .into_iter()
            .map(task_summary_json)
            .collect::<Vec<_>>();
        ToolResult::completed(request, json!({ "tasks": tasks }).to_string(), false)
    }

    pub fn execute_task_stop_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let args = match parse_tool_arguments(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let Some(task_id) = args
            .get("task_id")
            .and_then(Value::as_str)
            .or_else(|| args.get("shell_id").and_then(Value::as_str))
            .filter(|task_id| !task_id.trim().is_empty())
        else {
            return ToolResult::invalid_input(request, "missing required field: task_id");
        };
        let Some(record) = task_registry.get(task_id) else {
            return ToolResult::failed(request, format!("task '{task_id}' not found"), None);
        };
        if is_terminal_task_status(record.status) {
            return ToolResult::failed(
                request,
                format!(
                    "task is already {} and cannot be stopped",
                    task_status_label(record.status)
                ),
                None,
            );
        }
        if record.status == TaskStatus::ApprovalRequired {
            if let Err(error) = task_registry.stop(task_id, "Task stopped".to_string()) {
                return ToolResult::failed(request, error, None);
            }
        } else if let Err(error) = task_registry.request_stop(task_id) {
            return ToolResult::failed(request, error, None);
        }
        let output = json!({
            "message": if record.status == TaskStatus::ApprovalRequired {
                "Task stopped"
            } else {
                "Task stop requested"
            },
            "task_id": record.id,
            "task_type": task_type_label(record.task_type),
            "command": record.command,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_workflow_draft_tool(
        &mut self,
        request: &ToolRequest,
        draft_request: RuntimeWorkflowDraftRequest<'_>,
    ) -> io::Result<ToolResult> {
        if !draft_request.workflows_enabled {
            return Ok(ToolResult::failed(request, "workflows are disabled", None));
        }
        let script = workflow_draft_script_arg(request)?;
        let session_dir = draft_request
            .cwd
            .join(".orca")
            .join("workflow-sessions")
            .join(draft_request.session_id);
        let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
        let draft = draft_store.create_from_script(
            draft_request.session_id,
            draft_request.cwd,
            &script,
            draft_request.max_concurrent_agents,
        )?;
        let output = serde_json::to_string(&draft)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(ToolResult::completed(request, output, false))
    }
}

fn parse_runtime_permission_request(
    request: &ToolRequest,
) -> Result<RuntimePermissionRequestArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing request_permissions arguments JSON".to_string())?;
    let mut args: RuntimePermissionRequestArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid request_permissions arguments JSON: {error}"))?;
    args.permissions = args.permissions.normalize_file_system_entries();
    if args
        .reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err("missing required request_permissions argument: reason".to_string());
    }
    let file_system = args.permissions.file_system.as_ref();
    let has_file_system_request = file_system.is_some_and(|file_system| {
        file_system
            .read
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
            || file_system
                .write
                .as_ref()
                .is_some_and(|paths| !paths.is_empty())
    });
    let has_network_request = args
        .permissions
        .network
        .as_ref()
        .is_some_and(|network| network.enabled.is_some() || !network.domains.is_empty());
    if !has_file_system_request && !has_network_request {
        return Err("request_permissions requires at least one permission request".to_string());
    }
    Ok(args)
}

fn required_string_arg<'a>(
    request: &ToolRequest,
    args: &'a Value,
    field: &str,
) -> Result<&'a str, ToolResult> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        ToolResult::invalid_input(request, format!("missing required string field: {field}"))
    })
}

fn parse_tool_arguments(request: &ToolRequest) -> Result<Value, String> {
    serde_json::from_str(request.raw_arguments.as_deref().unwrap_or("{}"))
        .map_err(|error| format!("arguments are not valid JSON: {error}"))
}

fn task_summary_json(task: BackgroundTaskSummary) -> Value {
    json!({
        "id": task.id,
        "subject": task.description,
        "status": task_status_label(task.status),
        "owner": Value::Null,
        "blockedBy": [],
        "task_type": task_type_label(task.task_type),
        "isBackgrounded": task.is_backgrounded,
        "command": task.command,
        "tool": task.tool,
        "pendingToolCall": task.pending_tool_call,
    })
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::ApprovalRequired => "approval_required",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::MainSession => "main_session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

fn is_terminal_task_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Stopped | TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

fn extract_tool_string_field(request: &ToolRequest, field: &str) -> Option<String> {
    let raw = request.raw_arguments.as_deref()?;
    let value = serde_json::from_str::<Value>(raw).ok()?;
    value.get(field).and_then(Value::as_str).map(str::to_string)
}

fn runtime_usage_totals_json(usage: RuntimeUsageTotals) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_tokens": usage.cache_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "estimated_cost_usd": usage.estimated_cost_usd,
    })
}

fn unpack_async_subagent_result(raw: &str) -> (Option<Value>, Option<Value>) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return (Some(Value::String(raw.to_string())), None);
    };
    let Some(output) = value.get("output") else {
        return (Some(Value::String(raw.to_string())), None);
    };
    let task = value.get("task").cloned().filter(|task| !task.is_null());
    (Some(output.clone()), task)
}

fn workflow_draft_script_arg(request: &ToolRequest) -> io::Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WorkflowDraftInput {
        script: String,
    }

    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str::<WorkflowDraftInput>(raw_arguments)
        .map(|input| input.script)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::task_types::PendingToolCallSummary;
    use orca_core::tool_types::ToolStatus;

    #[test]
    fn task_summary_json_marks_backgrounded_main_sessions() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "long turn".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(2_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: Some("task_list".to_string()),
            pending_tool_call: Some(PendingToolCallSummary {
                id: "mock-tool-1".to_string(),
                name: "task_list".to_string(),
                action: ActionKind::Read,
                target: None,
                arguments: "{}".to_string(),
            }),
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
        };

        let summary = task_summary_json(task);

        assert_eq!(summary["task_type"], "main_session");
        assert_eq!(summary["status"], "approval_required");
        assert_eq!(summary["isBackgrounded"], true);
        assert_eq!(summary["tool"], "task_list");
        assert_eq!(summary["pendingToolCall"]["id"], "mock-tool-1");
        assert_eq!(summary["pendingToolCall"]["name"], "task_list");
        assert_eq!(summary["pendingToolCall"]["action"], "read");
        assert_eq!(summary["pendingToolCall"]["arguments"], "{}");
    }

    #[test]
    fn task_stop_stops_approval_required_background_main_session() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("waiting for approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_tool(
                &task.id,
                "approval_required".to_string(),
                Some("task_list".to_string()),
            )
            .unwrap();
        let request = ToolRequest {
            id: "call-stop".to_string(),
            name: ToolName::TaskStop,
            action: orca_core::approval_types::ActionKind::Write,
            target: None,
            raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
        };
        let mut context = RuntimeToolActorContext::new("test-run", 8);

        let result = context.execute_task_stop_tool(&request, &registry);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        let stopped = registry.get(&task.id).unwrap();
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert_eq!(stopped.result.as_deref(), Some("Task stopped"));
        assert_eq!(stopped.error, None);
    }
}
