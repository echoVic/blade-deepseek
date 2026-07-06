use std::collections::HashMap;

use serde_json::{Value, json};

use crate::protocol::{self, ServerEvent};
use crate::tool_item_projection::{
    agent_message_item, command_execution_completed_item, command_execution_started_item,
    dynamic_tool_completed_item, dynamic_tool_started_item, file_change_completed_item,
    file_change_started_item, mcp_result_from_content, mcp_tool_completed_item, mcp_tool_parts,
    mcp_tool_started_item, parse_json_or_null, plan_item, reasoning_item,
    tool_error_object_from_value, tool_status_is_completed, workflow_completed_item,
    workflow_started_item,
};

const PROPOSED_PLAN_OPEN: &str = "<proposed_plan>";
const PROPOSED_PLAN_CLOSE: &str = "</proposed_plan>";

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeEventProjector {
    agent_message: Option<AgentMessageItem>,
    plan: Option<PlanItem>,
    plan_parser: ProposedPlanStreamParser,
    reasoning: Option<ReasoningItem>,
    tool_items: HashMap<String, ToolCallItem>,
    file_change_items: HashMap<String, FileChangeItem>,
    workflow_items: HashMap<String, WorkflowItem>,
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
            self.reasoning = Some(ReasoningItem {
                id: "item-reasoning-1".to_string(),
                summary: String::new(),
            });
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: reasoning_item("item-reasoning-1", ""),
            });
        }
        if let Some(item) = &mut self.reasoning {
            item.summary.push_str(delta);
            events.push(ServerEvent::ItemReasoningDelta {
                item_id: Value::from(item.id.clone()),
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
            self.agent_message = Some(AgentMessageItem {
                id: "item-agent-message-1".to_string(),
                text: String::new(),
            });
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: agent_message_item("item-agent-message-1", ""),
            });
        }
        if let Some(item) = &mut self.agent_message {
            item.text.push_str(delta);
            events.push(ServerEvent::ItemMessageDelta {
                item_id: Value::from(item.id.clone()),
                delta: Value::from(delta.to_string()),
            });
        }
    }

    fn project_plan_delta(&mut self, delta: &str, events: &mut Vec<ServerEvent>) {
        if delta.is_empty() {
            return;
        }
        if self.plan.is_none() {
            self.plan = Some(PlanItem {
                id: "item-plan-1".to_string(),
                text: String::new(),
            });
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: plan_item("item-plan-1", ""),
            });
        }
        if let Some(item) = &mut self.plan {
            item.text.push_str(delta);
            events.push(ServerEvent::ItemPlanDelta {
                item_id: Value::from(item.id.clone()),
                delta: Value::from(delta.to_string()),
            });
        }
    }

    fn project_terminal_items(&mut self, events: &mut Vec<ServerEvent>) {
        if let Some(item) = self.agent_message.take() {
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: agent_message_item(item.id, item.text),
            });
        }
        if let Some(item) = self.plan.take() {
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: plan_item(item.id, item.text),
            });
        }
        if let Some(item) = self.reasoning.take() {
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: reasoning_item(item.id, item.summary),
            });
        }
    }

    fn project_tool_item_started(&mut self, runtime_event: &Value, events: &mut Vec<ServerEvent>) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        if let Some((server, local_tool)) = mcp_tool_parts(&tool) {
            let item = ToolCallItem {
                id: tool_id.clone(),
                tool: tool.clone(),
                command: None,
            };
            self.tool_items.insert(tool_id.clone(), item);
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: mcp_tool_started_item(tool_id, server, local_tool, tool_arguments(payload)),
            });
            return;
        }
        if is_dynamic_tool(&tool) {
            let item = ToolCallItem {
                id: tool_id.clone(),
                tool: tool.clone(),
                command: None,
            };
            self.tool_items.insert(tool_id.clone(), item);
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: dynamic_tool_started_item(tool_id, tool, tool_arguments(payload)),
            });
            return;
        }
        let command = payload["target"].as_str().map(ToString::to_string);
        let item = ToolCallItem {
            id: tool_id.clone(),
            tool: tool.clone(),
            command: command.clone(),
        };
        self.tool_items.insert(tool_id.clone(), item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: command_execution_started_item(tool_id, tool, command),
        });
    }

    fn project_tool_item_completed(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let item = self.tool_items.remove(&tool_id).unwrap_or(ToolCallItem {
            id: tool_id,
            tool: payload["name"].as_str().unwrap_or_default().to_string(),
            command: None,
        });
        if let Some((server, local_tool)) = mcp_tool_parts(&item.tool) {
            let status = payload["status"].as_str().unwrap_or_default().to_string();
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: mcp_tool_completed_item(
                    item.id,
                    server,
                    local_tool,
                    status,
                    tool_arguments(payload),
                    mcp_tool_result(payload),
                    mcp_tool_error(payload),
                ),
            });
            return;
        }
        if is_dynamic_tool(&item.tool) {
            let status = payload["status"].as_str().unwrap_or_default().to_string();
            events.push(ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: dynamic_tool_completed_item(
                    item.id,
                    item.tool,
                    status,
                    tool_arguments(payload),
                    dynamic_tool_content_items(payload),
                    payload["status"].as_str() == Some("completed"),
                    dynamic_tool_error(payload),
                ),
            });
            return;
        }
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: command_execution_completed_item(
                item.id,
                item.tool,
                item.command,
                payload["status"].clone(),
                payload["output"].clone(),
                payload["error"].clone(),
                payload["exit_code"].clone(),
                payload["truncated"].clone(),
            ),
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
        let item = FileChangeItem {
            id: format!("{tool_id}:file-change"),
            path: path.clone(),
            kind: kind.to_string(),
        };
        self.file_change_items.insert(tool_id, item.clone());
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: file_change_started_item(item.id, item.path, item.kind, file_change_diff()),
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
            item: file_change_completed_item(
                item.id,
                item.path,
                item.kind,
                file_change_status(payload),
                file_change_diff(),
            ),
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
        let item = WorkflowItem {
            id: run_id.clone(),
            task_id: task_id.clone(),
            workflow_name: workflow_name.clone(),
            task: payload["task"].clone(),
            status: "running".to_string(),
            result: Value::Null,
        };
        self.workflow_items.insert(run_id.clone(), item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: workflow_started_item(run_id, task_id, workflow_name, payload["task"].clone()),
        });
    }

    fn record_workflow_result(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.result = payload["result"].clone();
            item.task = payload["task"].clone();
            item.status = "completed".to_string();
        }
    }

    fn record_workflow_completed(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.task = payload["task"].clone();
            item.status = "completed".to_string();
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
        let fallback = WorkflowItem {
            id: run_id,
            task_id: payload["taskId"].as_str().unwrap_or_default().to_string(),
            workflow_name: payload["workflowName"]
                .as_str()
                .unwrap_or("workflow")
                .to_string(),
            task: payload["task"].clone(),
            status: status.to_string(),
            result: Value::Null,
        };
        let mut item = self.workflow_items.remove(&fallback.id).unwrap_or(fallback);
        if item.task.is_null() {
            item.task = payload["task"].clone();
        }
        item.status = status.to_string();
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: workflow_completed_item(
                item.id,
                item.task_id,
                item.workflow_name,
                item.status,
                item.result,
                payload["error"].clone(),
                item.task,
            ),
        });
    }
}

#[derive(Clone, Debug, Default)]
struct ProposedPlanStreamParser {
    buffer: String,
    in_plan: bool,
    plan_buffer: String,
    drop_leading_plan_newline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProposedPlanSegment {
    Agent(String),
    Plan(String),
}

#[derive(Clone, Debug)]
struct AgentMessageItem {
    id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct PlanItem {
    id: String,
    text: String,
}

#[derive(Clone, Debug)]
struct ReasoningItem {
    id: String,
    summary: String,
}

#[derive(Clone, Debug)]
struct ToolCallItem {
    id: String,
    tool: String,
    command: Option<String>,
}

#[derive(Clone, Debug)]
struct FileChangeItem {
    id: String,
    path: Option<String>,
    kind: String,
}

#[derive(Clone, Debug)]
struct WorkflowItem {
    id: String,
    task_id: String,
    workflow_name: String,
    task: Value,
    status: String,
    result: Value,
}

impl ProposedPlanStreamParser {
    fn push(&mut self, delta: &str) -> Vec<ProposedPlanSegment> {
        self.buffer.push_str(delta);
        self.drain(false)
    }

    fn finish(&mut self) -> Vec<ProposedPlanSegment> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<ProposedPlanSegment> {
        let mut out = Vec::new();
        loop {
            if self.in_plan {
                if let Some(end) = self.buffer.find(PROPOSED_PLAN_CLOSE) {
                    let plan_and_close: String = self
                        .buffer
                        .drain(..end + PROPOSED_PLAN_CLOSE.len())
                        .collect();
                    self.plan_buffer.push_str(&plan_and_close[..end]);
                    let text = self.normalize_plan_text();
                    if !text.is_empty() {
                        out.push(ProposedPlanSegment::Plan(text));
                    }
                    self.in_plan = false;
                    self.drop_leading_plan_newline = false;
                    continue;
                }
                if finish {
                    let text = format!("{PROPOSED_PLAN_OPEN}{}{}", self.plan_buffer, self.buffer);
                    self.plan_buffer.clear();
                    self.buffer.clear();
                    self.in_plan = false;
                    self.drop_leading_plan_newline = false;
                    if !text.is_empty() {
                        out.push(ProposedPlanSegment::Agent(text));
                    }
                } else if !self.buffer.is_empty() {
                    self.plan_buffer.push_str(&self.buffer);
                    self.buffer.clear();
                }
                break;
            }

            if let Some(start) = self.buffer.find(PROPOSED_PLAN_OPEN) {
                if start > 0 {
                    out.push(ProposedPlanSegment::Agent(self.buffer[..start].to_string()));
                }
                self.buffer.drain(..start + PROPOSED_PLAN_OPEN.len());
                self.in_plan = true;
                self.drop_leading_plan_newline = true;
                continue;
            }
            let keep = if finish {
                0
            } else {
                pending_open_tag_prefix_len(&self.buffer)
            };
            if self.buffer.len() > keep {
                let take = self.buffer.len() - keep;
                out.push(ProposedPlanSegment::Agent(
                    self.buffer.drain(..take).collect(),
                ));
            }
            break;
        }
        out
    }

    fn normalize_plan_text(&mut self) -> String {
        let mut text = std::mem::take(&mut self.plan_buffer);
        if self.drop_leading_plan_newline {
            if let Some(stripped) = text.strip_prefix('\n') {
                text = stripped.to_string();
            }
            self.drop_leading_plan_newline = false;
        }
        text
    }
}

fn pending_open_tag_prefix_len(text: &str) -> usize {
    let max = text.len().min(PROPOSED_PLAN_OPEN.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|&len| PROPOSED_PLAN_OPEN.starts_with(&text[text.len() - len..]))
        .unwrap_or(0)
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
