use std::collections::HashMap;

use orca_core::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};
use serde_json::{Value, json};

use crate::protocol::{self, ServerEvent};
use crate::tool_item_projection::{
    ProjectedFileChangeItem, ProjectedTextItem, ProjectedTextItemKind, ProjectedToolCallCompletion,
    ProjectedToolCallItem, ProjectedWorkflowItem, mcp_result_from_content, mcp_tool_parts,
    parse_json_or_null, tool_error_object_from_value, tool_status_is_completed,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeEventProjector {
    agent_message: Option<ProjectedTextItem>,
    plan: Option<ProjectedTextItem>,
    plan_parser: ProposedPlanStreamParser,
    reasoning: Option<ProjectedTextItem>,
    tool_items: HashMap<String, ProjectedToolCallItem>,
    file_change_items: HashMap<String, ProjectedFileChangeItem>,
    workflow_items: HashMap<String, ProjectedWorkflowItem>,
}

impl RuntimeEventProjector {
    pub(crate) fn project_line(&mut self, line: &str) -> Vec<ServerEvent> {
        let runtime_event: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let event_type = runtime_event["type"].as_str().unwrap_or_default();
        let mut events = Vec::new();

        match event_type {
            "assistant.message.delta" => {
                let delta = runtime_event["payload"]["text"]
                    .as_str()
                    .unwrap_or_default();
                self.project_assistant_message_delta(delta, &mut events);
            }
            "assistant.reasoning.delta" => {
                let delta = runtime_event["payload"]["text"]
                    .as_str()
                    .unwrap_or_default();
                self.project_reasoning_delta(delta, &mut events);
            }
            "tool.call.requested" => {
                self.project_tool_item_started(&runtime_event, &mut events);
                self.project_file_change_item_started(&runtime_event, &mut events);
            }
            "workflow.started" => {
                self.project_workflow_item_started(&runtime_event, &mut events);
            }
            _ => {}
        }

        if let Some(event) = protocol::map_runtime_event_line(line) {
            events.push(event);
        }

        match event_type {
            "tool.call.completed" => {
                self.project_tool_item_completed(&runtime_event, &mut events);
                self.project_file_change_item_completed(&runtime_event, &mut events);
            }
            "workflow.completed" => {
                self.record_workflow_completed(&runtime_event);
            }
            "workflow.result.available" => {
                self.record_workflow_result(&runtime_event);
                self.project_workflow_item_completed(&runtime_event, "completed", &mut events);
            }
            "workflow.failed" => {
                self.project_workflow_item_completed(&runtime_event, "failed", &mut events);
            }
            "session.completed" => {
                self.flush_assistant_message_parser(&mut events);
                self.project_terminal_items(&mut events);
            }
            _ => {}
        }

        events
    }

    fn project_reasoning_delta(&mut self, delta: &str, events: &mut Vec<ServerEvent>) {
        if self.reasoning.is_none() {
            self.reasoning = Some(ProjectedTextItem::new(ProjectedTextItemKind::Reasoning));
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: self
                    .reasoning
                    .as_ref()
                    .expect("reasoning item")
                    .started_item(),
            });
        }
        if let Some(item) = &mut self.reasoning {
            item.push_delta(delta);
            events.push(ServerEvent::ItemReasoningDelta {
                item_id: Value::from(item.id().to_string()),
                delta: Value::from(delta.to_string()),
            });
        }
    }

    fn project_assistant_message_delta(&mut self, delta: &str, events: &mut Vec<ServerEvent>) {
        for segment in self.plan_parser.push(delta) {
            match segment {
                ProposedPlanSegment::Agent(text) => self.project_agent_message_delta(&text, events),
                ProposedPlanSegment::Plan(text) => self.project_plan_delta(&text, events),
            }
        }
    }

    fn flush_assistant_message_parser(&mut self, events: &mut Vec<ServerEvent>) {
        for segment in self.plan_parser.finish() {
            match segment {
                ProposedPlanSegment::Agent(text) => self.project_agent_message_delta(&text, events),
                ProposedPlanSegment::Plan(text) => self.project_plan_delta(&text, events),
            }
        }
    }

    fn project_agent_message_delta(&mut self, delta: &str, events: &mut Vec<ServerEvent>) {
        if delta.is_empty() {
            return;
        }
        if self.agent_message.is_none() {
            self.agent_message = Some(ProjectedTextItem::new(ProjectedTextItemKind::AgentMessage));
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: self
                    .agent_message
                    .as_ref()
                    .expect("agent message item")
                    .started_item(),
            });
        }
        if let Some(item) = &mut self.agent_message {
            item.push_delta(delta);
            events.push(ServerEvent::ItemMessageDelta {
                item_id: Value::from(item.id().to_string()),
                delta: Value::from(delta.to_string()),
            });
        }
    }

    fn project_plan_delta(&mut self, delta: &str, events: &mut Vec<ServerEvent>) {
        if delta.is_empty() {
            return;
        }
        if self.plan.is_none() {
            self.plan = Some(ProjectedTextItem::new(ProjectedTextItemKind::Plan));
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: self.plan.as_ref().expect("plan item").started_item(),
            });
        }
        if let Some(item) = &mut self.plan {
            item.push_delta(delta);
            events.push(ServerEvent::ItemPlanDelta {
                item_id: Value::from(item.id().to_string()),
                delta: Value::from(delta.to_string()),
            });
        }
    }

    fn project_terminal_items(&mut self, events: &mut Vec<ServerEvent>) {
        if let Some(item) = self.agent_message.take() {
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: item.completed_item(),
            });
        }
        if let Some(item) = self.plan.take() {
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: item.completed_item(),
            });
        }
        if let Some(item) = self.reasoning.take() {
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: item.completed_item(),
            });
        }
    }

    fn project_tool_item_started(&mut self, runtime_event: &Value, events: &mut Vec<ServerEvent>) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        if let Some((server, local_tool)) = mcp_tool_parts(&tool) {
            let item = ProjectedToolCallItem::mcp_tool(tool_id.clone(), server, local_tool);
            let started_item = item.started_item(tool_arguments(payload));
            self.tool_items.insert(tool_id.clone(), item);
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: started_item,
            });
            return;
        }
        if is_dynamic_tool(&tool) {
            let item = ProjectedToolCallItem::dynamic_tool(tool_id.clone(), tool);
            let started_item = item.started_item(tool_arguments(payload));
            self.tool_items.insert(tool_id.clone(), item);
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: started_item,
            });
            return;
        }
        let command = payload["target"].as_str().map(ToString::to_string);
        let item = ProjectedToolCallItem::command_execution(tool_id.clone(), tool, command.clone());
        let started_item = item.started_item(Value::Null);
        self.tool_items.insert(tool_id.clone(), item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: started_item,
        });
    }

    fn project_tool_item_completed(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let item = self.tool_items.remove(&tool_id).unwrap_or_else(|| {
            fallback_projected_tool_call_item(
                tool_id,
                payload["name"].as_str().unwrap_or_default(),
                payload["target"].as_str(),
            )
        });
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: item.completed_item(ProjectedToolCallCompletion {
                status: payload["status"].as_str().unwrap_or_default().to_string(),
                command_status: payload["status"].clone(),
                arguments: tool_arguments(payload),
                result: mcp_tool_result(payload),
                command_error: payload["error"].clone(),
                mcp_error: mcp_tool_error(payload),
                dynamic_error: dynamic_tool_error(payload),
                content_items: dynamic_tool_content_items(payload),
                success: payload["status"].as_str() == Some("completed"),
                aggregated_output: payload["output"].clone(),
                exit_code: payload["exit_code"].clone(),
                truncated: payload["truncated"].clone(),
            }),
        });
    }

    fn project_file_change_item_started(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        let Some(kind) = file_change_kind(&tool) else {
            return;
        };
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let target = payload["target"].as_str();
        let path = file_change_path(&tool, target);
        let item = ProjectedFileChangeItem::new(format!("{tool_id}:file-change"), path, kind);
        let started_item = item.started_item(file_change_diff());
        self.file_change_items.insert(tool_id, item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: started_item,
        });
    }

    fn project_file_change_item_completed(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let Some(item) = self.file_change_items.remove(&tool_id) else {
            return;
        };
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: item.completed_item(file_change_status(payload), file_change_diff()),
        });
    }

    fn project_workflow_item_started(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"]
            .as_str()
            .unwrap_or("workflow-run")
            .to_string();
        let task_id = payload["taskId"].as_str().unwrap_or_default().to_string();
        let workflow_name = payload["workflowName"]
            .as_str()
            .unwrap_or("workflow")
            .to_string();
        let item = ProjectedWorkflowItem::started(
            run_id.clone(),
            task_id,
            workflow_name,
            payload["task"].clone(),
        );
        let started_item = item.started_item();
        self.workflow_items.insert(run_id, item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: started_item,
        });
    }

    fn record_workflow_result(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.record_result(payload["result"].clone(), payload["task"].clone());
        }
    }

    fn record_workflow_completed(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.record_completed(payload["task"].clone());
        }
    }

    fn project_workflow_item_completed(
        &mut self,
        runtime_event: &Value,
        status: &str,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"]
            .as_str()
            .unwrap_or("workflow-run")
            .to_string();
        let fallback = ProjectedWorkflowItem::started(
            run_id.clone(),
            payload["taskId"].as_str().unwrap_or_default(),
            payload["workflowName"].as_str().unwrap_or("workflow"),
            payload["task"].clone(),
        );
        let mut item = self.workflow_items.remove(&run_id).unwrap_or(fallback);
        item.fill_task_if_missing(payload["task"].clone());
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: item.completed_item(status, payload["error"].clone()),
        });
    }
}

fn file_change_kind(tool: &str) -> Option<&'static str> {
    match tool {
        "edit" => Some("edit"),
        "write_file" => Some("write"),
        _ => None,
    }
}

fn file_change_path(tool: &str, target: Option<&str>) -> Option<String> {
    let target = target?.trim();
    if target.is_empty() {
        return None;
    }
    match tool {
        "edit" => Some(
            target
                .split_once("::")
                .map(|(path, _)| path)
                .unwrap_or(target)
                .trim()
                .to_string(),
        ),
        "write_file" => Some(target.to_string()),
        _ => None,
    }
}

fn file_change_status(payload: &Value) -> Value {
    match payload["status"].as_str() {
        Some("in_progress") => Value::from("inProgress"),
        Some(status) => Value::from(status.to_string()),
        None => Value::Null,
    }
}

fn file_change_diff() -> Value {
    Value::from(String::new())
}

fn is_dynamic_tool(tool: &str) -> bool {
    !is_builtin_tool(tool) && mcp_tool_parts(tool).is_none()
}

fn fallback_projected_tool_call_item(
    id: String,
    tool: &str,
    target: Option<&str>,
) -> ProjectedToolCallItem {
    if let Some((server, local_tool)) = mcp_tool_parts(tool) {
        return ProjectedToolCallItem::mcp_tool(id, server, local_tool);
    }
    if is_dynamic_tool(tool) {
        return ProjectedToolCallItem::dynamic_tool(id, tool.to_string());
    }
    ProjectedToolCallItem::command_execution(id, tool.to_string(), target.map(ToString::to_string))
}

fn is_builtin_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "list_files"
            | "glob"
            | "grep"
            | "bash"
            | "edit"
            | "write_file"
            | "git_status"
            | "subagent"
            | "subagent_status"
            | "task_list"
            | "task_stop"
            | "WorkflowDraft"
            | "workflow_draft"
            | "WorkflowDraftAction"
            | "workflow_draft_action"
            | "Workflow"
            | "workflow"
            | "workflow_send_message"
            | "workflow_read_messages"
            | "workflow_clear_messages"
            | "workflow_create_task_list"
            | "workflow_claim_task"
            | "workflow_complete_task"
            | "workflow_list_tasks"
            | "web_search"
            | "get_goal"
            | "create_goal"
            | "update_goal"
            | "update_plan"
            | "request_user_input"
            | "list_skills"
            | "read_skill"
    )
}

fn tool_arguments(payload: &Value) -> Value {
    payload["raw_arguments"]
        .as_str()
        .or_else(|| payload["target"].as_str())
        .map(parse_json_or_null)
        .unwrap_or(Value::Null)
}

fn mcp_tool_result(payload: &Value) -> Value {
    if !tool_status_is_completed(payload) || !payload["error"].is_null() {
        return Value::Null;
    }
    let Some(output) = payload["output"].as_str() else {
        return Value::Null;
    };
    mcp_result_from_content(output)
}

fn mcp_tool_error(payload: &Value) -> Value {
    if let Some(error) = payload["error"].as_str() {
        return tool_error_object_from_value(error, payload);
    }
    if payload["status"].as_str() == Some("failed") {
        if let Some(output) = payload["output"].as_str() {
            return tool_error_object_from_value(output, payload);
        }
        return tool_error_object_from_value("MCP tool call failed", payload);
    }
    Value::Null
}

fn dynamic_tool_content_items(payload: &Value) -> Value {
    if !tool_status_is_completed(payload) {
        return Value::Null;
    }
    match payload["output"].as_str() {
        Some(output) => json!([{ "type": "text", "text": output }]),
        None => Value::Null,
    }
}

fn dynamic_tool_error(payload: &Value) -> Value {
    match tool_error_detail(payload) {
        Value::String(message) => tool_error_object_from_value(&message, payload),
        Value::Null => Value::Null,
        other => other,
    }
}

fn tool_error_detail(payload: &Value) -> Value {
    if let Some(error) = payload["error"].as_str() {
        return Value::from(error.to_string());
    }
    if !tool_status_is_completed(payload) {
        if let Some(output) = payload["output"].as_str() {
            return Value::from(output.to_string());
        }
        return Value::from("tool call failed");
    }
    Value::Null
}
