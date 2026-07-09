use serde::Serialize;
use serde_json::{Value, json};

pub(crate) fn mcp_tool_parts(tool: &str) -> Option<(String, String)> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, local_tool) = rest.rsplit_once("__")?;
    Some((server.to_string(), local_tool.to_string()))
}

pub(crate) fn parse_json_or_null(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

pub(crate) fn mcp_result_from_content(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(value) if value.is_object() => json!({
            "content": value.get("content").cloned().unwrap_or_else(|| {
                json!([{ "type": "text", "text": content }])
            }),
            "structuredContent": value.get("structuredContent").cloned().unwrap_or(Value::Null),
            "_meta": value.get("_meta").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({
            "content": [{ "type": "text", "text": content }],
            "structuredContent": Value::Null,
            "_meta": Value::Null,
        }),
    }
}

pub(crate) fn mcp_tool_started_item(
    id: impl Into<String>,
    server: impl Into<String>,
    tool: impl Into<String>,
    arguments: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "mcpToolCall",
        "server": server.into(),
        "tool": tool.into(),
        "status": "in_progress",
        "arguments": arguments,
        "result": Value::Null,
        "error": Value::Null,
    })
}

pub(crate) fn dynamic_tool_started_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    arguments: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "dynamicToolCall",
        "namespace": Value::Null,
        "tool": tool.into(),
        "status": "in_progress",
        "arguments": arguments,
        "contentItems": Value::Null,
        "success": Value::Null,
        "error": Value::Null,
    })
}

pub(crate) fn mcp_tool_completed_item(
    id: impl Into<String>,
    server: impl Into<String>,
    tool: impl Into<String>,
    status: impl Into<String>,
    arguments: Value,
    result: Value,
    error: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "mcpToolCall",
        "server": server.into(),
        "tool": tool.into(),
        "status": status.into(),
        "arguments": arguments,
        "result": result,
        "error": error,
    })
}

pub(crate) fn dynamic_tool_completed_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    status: impl Into<String>,
    arguments: Value,
    content_items: Value,
    success: bool,
    error: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "dynamicToolCall",
        "namespace": Value::Null,
        "tool": tool.into(),
        "status": status.into(),
        "arguments": arguments,
        "contentItems": content_items,
        "success": success,
        "error": error,
    })
}

pub(crate) fn agent_message_item(id: impl Into<String>, text: impl Into<String>) -> Value {
    ProjectedTextThreadItem::agent_message(id, text).into_value()
}

pub(crate) fn plan_item(id: impl Into<String>, text: impl Into<String>) -> Value {
    ProjectedTextThreadItem::plan(id, text).into_value()
}

pub(crate) fn reasoning_item(id: impl Into<String>, summary: impl Into<String>) -> Value {
    ProjectedTextThreadItem::reasoning(id, summary).into_value()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ProjectedTextThreadItem {
    #[serde(rename = "agent_message")]
    AgentMessage { id: String, text: String },
    #[serde(rename = "plan")]
    Plan { id: String, text: String },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: String,
        content: String,
    },
}

impl ProjectedTextThreadItem {
    pub(crate) fn agent_message(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::AgentMessage {
            id: id.into(),
            text: text.into(),
        }
    }

    pub(crate) fn plan(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Plan {
            id: id.into(),
            text: text.into(),
        }
    }

    pub(crate) fn reasoning(id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::Reasoning {
            id: id.into(),
            summary: summary.into(),
            content: String::new(),
        }
    }

    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected text thread item serializes")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectedTextItemKind {
    AgentMessage,
    Plan,
    Reasoning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedTextItem {
    kind: ProjectedTextItemKind,
    id: &'static str,
    text: String,
}

impl ProjectedTextItem {
    pub(crate) fn new(kind: ProjectedTextItemKind) -> Self {
        Self {
            kind,
            id: kind.id(),
            text: String::new(),
        }
    }

    pub(crate) fn id(&self) -> &str {
        self.id
    }

    pub(crate) fn push_delta(&mut self, delta: &str) {
        self.text.push_str(delta);
    }

    pub(crate) fn started_item(&self) -> Value {
        self.kind.item(self.id, "")
    }

    pub(crate) fn completed_item(self) -> Value {
        self.kind.item(self.id, self.text)
    }
}

impl ProjectedTextItemKind {
    fn id(self) -> &'static str {
        match self {
            Self::AgentMessage => "item-agent-message-1",
            Self::Plan => "item-plan-1",
            Self::Reasoning => "item-reasoning-1",
        }
    }

    fn item(self, id: impl Into<String>, text: impl Into<String>) -> Value {
        match self {
            Self::AgentMessage => agent_message_item(id, text),
            Self::Plan => plan_item(id, text),
            Self::Reasoning => reasoning_item(id, text),
        }
    }
}

pub(crate) fn command_execution_started_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    command: Option<impl Into<String>>,
) -> Value {
    json!({
        "id": id.into(),
        "type": "commandExecution",
        "tool": tool.into(),
        "command": command.map(Into::into),
        "status": "in_progress",
    })
}

pub(crate) fn command_execution_completed_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    command: Option<impl Into<String>>,
    status: Value,
    aggregated_output: Value,
    error: Value,
    exit_code: Value,
    truncated: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "commandExecution",
        "tool": tool.into(),
        "command": command.map(Into::into),
        "status": status,
        "aggregatedOutput": aggregated_output,
        "error": error,
        "exitCode": exit_code,
        "truncated": truncated,
    })
}

pub(crate) fn persisted_command_execution_started_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    command: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "commandExecution",
        "tool": tool.into(),
        "command": command,
        "cwd": Value::Null,
        "processId": Value::Null,
        "source": Value::Null,
        "status": "in_progress",
        "commandActions": [],
        "aggregatedOutput": Value::Null,
        "error": Value::Null,
        "exitCode": Value::Null,
        "durationMs": Value::Null,
    })
}

pub(crate) fn persisted_command_execution_completed_item(
    started: &Value,
    status: Value,
    aggregated_output: Value,
    error: Value,
    truncated: Value,
) -> Value {
    let mut item = started.clone();
    item["status"] = status;
    item["aggregatedOutput"] = aggregated_output;
    item["error"] = error;
    if !truncated.is_null() {
        item["truncated"] = truncated;
    }
    item
}

pub(crate) fn file_change_started_item(
    id: impl Into<String>,
    path: Option<impl Into<String>>,
    kind: impl Into<String>,
    diff: Value,
) -> Value {
    file_change_item(id, "inProgress", path, kind, diff)
}

pub(crate) fn file_change_completed_item(
    id: impl Into<String>,
    path: Option<impl Into<String>>,
    kind: impl Into<String>,
    status: Value,
    diff: Value,
) -> Value {
    file_change_item(id, status, path, kind, diff)
}

pub(crate) fn persisted_file_change_started_item(
    tool_call_id: &str,
    tool: &str,
    arguments: &Value,
) -> Option<Value> {
    Some(file_change_started_item(
        format!("{tool_call_id}:file-change"),
        file_change_path(tool, arguments),
        file_change_kind(tool)?,
        Value::from(String::new()),
    ))
}

pub(crate) fn persisted_file_change_completed_item(started: &Value, status: Value) -> Value {
    let mut item = started.clone();
    item["status"] = status;
    item
}

pub(crate) fn complete_projected_tool_item(item: &mut Value, result: &Value) {
    let content = result["content"].as_str().unwrap_or_default();
    if let Some((status, failure)) = tool_failure_from_result(result)
        .or_else(|| parse_tool_failure_content(content).map(|failure| ("failed", failure)))
    {
        rebuild_completed_projected_tool_item(item, status, result, Value::Null, failure);
        return;
    }

    rebuild_completed_projected_tool_item(
        item,
        "completed",
        result,
        mcp_result_from_content(content),
        Value::Null,
    );
}

fn file_change_item(
    id: impl Into<String>,
    status: impl Into<Value>,
    path: Option<impl Into<String>>,
    kind: impl Into<String>,
    diff: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "fileChange",
        "status": status.into(),
        "changes": [{
            "path": path.map(Into::into),
            "kind": kind.into(),
            "diff": diff,
        }],
    })
}

fn file_change_kind(tool: &str) -> Option<&'static str> {
    match tool {
        "edit" => Some("edit"),
        "write_file" => Some("write"),
        _ => None,
    }
}

fn file_change_path(tool: &str, arguments: &Value) -> Option<String> {
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("target").and_then(Value::as_str))?
        .trim();
    if path.is_empty() {
        return None;
    }
    match tool {
        "edit" | "write_file" => Some(path.to_string()),
        _ => None,
    }
}

pub(crate) fn workflow_started_item(
    id: impl Into<String>,
    task_id: impl Into<String>,
    workflow_name: impl Into<String>,
    task: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "workflow",
        "workflowName": workflow_name.into(),
        "taskId": task_id.into(),
        "status": "running",
        "task": task,
    })
}

pub(crate) fn workflow_completed_item(
    id: impl Into<String>,
    task_id: impl Into<String>,
    workflow_name: impl Into<String>,
    status: impl Into<String>,
    result: Value,
    error: Value,
    task: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "workflow",
        "workflowName": workflow_name.into(),
        "taskId": task_id.into(),
        "status": status.into(),
        "result": result,
        "error": error,
        "task": task,
    })
}

pub(crate) fn tool_error_object(message: &str, exit_code: Option<i64>) -> Value {
    let mut error =
        serde_json::Map::from_iter([("message".to_string(), Value::from(message.to_string()))]);
    if let Some(exit_code) = exit_code {
        error.insert("exitCode".to_string(), Value::from(exit_code));
    }
    Value::Object(error)
}

pub(crate) fn tool_error_object_from_value(message: &str, value: &Value) -> Value {
    tool_error_object(
        message,
        value
            .get("exit_code")
            .and_then(Value::as_i64)
            .or_else(|| value.get("exitCode").and_then(Value::as_i64)),
    )
}

pub(crate) fn tool_status_is_completed(payload: &Value) -> bool {
    payload["status"].as_str() == Some("completed")
}

fn rebuild_completed_projected_tool_item(
    item: &mut Value,
    status: &str,
    result: &Value,
    mcp_result: Value,
    error: Value,
) {
    if item["type"] == "mcpToolCall" {
        *item = mcp_tool_completed_item(
            item["id"].as_str().unwrap_or_default(),
            item["server"].as_str().unwrap_or_default(),
            item["tool"].as_str().unwrap_or_default(),
            status,
            item["arguments"].clone(),
            mcp_result,
            error,
        );
        copy_truncated_metadata(item, result);
        return;
    }

    if item["type"] == "dynamicToolCall" {
        let content_items = if status == "completed" {
            json!([{
                "type": "text",
                "text": result["content"].as_str().unwrap_or_default(),
            }])
        } else {
            Value::Null
        };
        *item = dynamic_tool_completed_item(
            item["id"].as_str().unwrap_or_default(),
            item["tool"].as_str().unwrap_or_default(),
            status,
            item["arguments"].clone(),
            content_items,
            status == "completed",
            error,
        );
        copy_truncated_metadata(item, result);
        return;
    }

    if item["type"] == "commandExecution" {
        let content = result["content"].as_str().unwrap_or_default();
        *item = persisted_command_execution_completed_item(
            item,
            Value::from(status.to_string()),
            if status == "completed" {
                Value::from(content.to_string())
            } else {
                Value::Null
            },
            if status == "completed" {
                Value::Null
            } else {
                error
            },
            truncated_metadata(result),
        );
        return;
    }

    if item["type"] == "fileChange" {
        *item = persisted_file_change_completed_item(item, Value::from(status.to_string()));
        return;
    }

    let content = result["content"].as_str().unwrap_or_default();
    item["status"] = Value::from(status.to_string());
    copy_truncated_metadata(item, result);
    item["result"] = if status == "completed" {
        Value::from(content.to_string())
    } else {
        Value::Null
    };
    item["error"] = error;
}

fn truncated_metadata(result: &Value) -> Value {
    if result["truncated"].as_bool() == Some(true) {
        Value::from(true)
    } else {
        Value::Null
    }
}

fn copy_truncated_metadata(item: &mut Value, result: &Value) {
    if result["truncated"].as_bool() == Some(true) {
        item["truncated"] = Value::from(true);
    }
}

fn tool_failure_from_result(result: &Value) -> Option<(&'static str, Value)> {
    let status = match result["status"].as_str()? {
        "completed" => return None,
        "failed" => "failed",
        "denied" => "denied",
        "not_implemented" => "not_implemented",
        _ => "failed",
    };
    let message = result["error"]
        .as_str()
        .filter(|message| !message.is_empty())
        .or_else(|| {
            result["content"]
                .as_str()
                .filter(|message| !message.is_empty())
        })
        .unwrap_or("tool call failed");
    Some((status, tool_error_object_from_value(message, result)))
}

fn parse_tool_failure_content(content: &str) -> Option<Value> {
    if let Some(message) = content.strip_prefix("ERROR: ") {
        return Some(json!({ "message": message }));
    }

    let value = serde_json::from_str::<Value>(content).ok()?;
    if value.get("status").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    let message = value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("tool call failed");
    Some(tool_error_object_from_value(message, &value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_result_from_content_preserves_structured_payload_shape() {
        let result = mcp_result_from_content(
            r#"{"content":[{"type":"text","text":"ok"}],"structuredContent":{"answer":42},"_meta":{"trace":"abc"}}"#,
        );

        assert_eq!(result["content"][0]["text"], "ok");
        assert_eq!(result["structuredContent"]["answer"], 42);
        assert_eq!(result["_meta"]["trace"], "abc");
    }

    #[test]
    fn tool_error_object_uses_camel_case_exit_code() {
        let error = tool_error_object("failed", Some(42));

        assert_eq!(error["message"], "failed");
        assert_eq!(error["exitCode"], 42);
        assert!(error.get("exit_code").is_none());
    }

    #[test]
    fn tool_error_object_from_value_normalizes_exit_code_field_names() {
        let snake_case = tool_error_object_from_value(
            "failed",
            &json!({
                "exit_code": 17,
            }),
        );
        let camel_case = tool_error_object_from_value(
            "failed",
            &json!({
                "exitCode": 18,
            }),
        );

        assert_eq!(snake_case["exitCode"], 17);
        assert!(snake_case.get("exit_code").is_none());
        assert_eq!(camel_case["exitCode"], 18);
    }

    #[test]
    fn tool_status_is_completed_only_accepts_completed_status() {
        assert!(tool_status_is_completed(&json!({ "status": "completed" })));
        assert!(!tool_status_is_completed(&json!({ "status": "failed" })));
        assert!(!tool_status_is_completed(&json!({ "status": Value::Null })));
    }

    #[test]
    fn mcp_tool_started_item_projects_codex_style_shape() {
        let item = mcp_tool_started_item("call-1", "server", "search", json!({ "q": "orca" }));

        assert_eq!(item["id"], "call-1");
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["server"], "server");
        assert_eq!(item["tool"], "search");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["arguments"]["q"], "orca");
        assert!(item["result"].is_null());
        assert!(item["error"].is_null());
    }

    #[test]
    fn dynamic_tool_started_item_projects_codex_style_shape() {
        let item = dynamic_tool_started_item("call-2", "web_search", json!({ "query": "orca" }));

        assert_eq!(item["id"], "call-2");
        assert_eq!(item["type"], "dynamicToolCall");
        assert!(item["namespace"].is_null());
        assert_eq!(item["tool"], "web_search");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["arguments"]["query"], "orca");
        assert!(item["contentItems"].is_null());
        assert!(item["success"].is_null());
        assert!(item["error"].is_null());
    }

    #[test]
    fn mcp_tool_completed_item_projects_success_shape() {
        let item = mcp_tool_completed_item(
            "call-3",
            "server",
            "search",
            "completed",
            json!({ "q": "orca" }),
            mcp_result_from_content(
                r#"{"content":[{"type":"text","text":"found"}],"structuredContent":{"count":1},"_meta":{"source":"test"}}"#,
            ),
            Value::Null,
        );

        assert_eq!(item["id"], "call-3");
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["server"], "server");
        assert_eq!(item["tool"], "search");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["arguments"]["q"], "orca");
        assert_eq!(item["result"]["content"][0]["text"], "found");
        assert_eq!(item["result"]["structuredContent"]["count"], 1);
        assert_eq!(item["result"]["_meta"]["source"], "test");
        assert!(item["error"].is_null());
    }

    #[test]
    fn mcp_tool_completed_item_projects_failure_shape() {
        let item = mcp_tool_completed_item(
            "call-4",
            "server",
            "search",
            "failed",
            json!({ "q": "orca" }),
            Value::Null,
            tool_error_object("timeout", Some(124)),
        );

        assert_eq!(item["id"], "call-4");
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["status"], "failed");
        assert_eq!(item["arguments"]["q"], "orca");
        assert!(item["result"].is_null());
        assert_eq!(item["error"]["message"], "timeout");
        assert_eq!(item["error"]["exitCode"], 124);
    }

    #[test]
    fn dynamic_tool_completed_item_projects_success_shape() {
        let item = dynamic_tool_completed_item(
            "call-5",
            "deploy",
            "completed",
            json!({ "env": "staging" }),
            json!([{ "type": "text", "text": "deployed" }]),
            true,
            Value::Null,
        );

        assert_eq!(item["id"], "call-5");
        assert_eq!(item["type"], "dynamicToolCall");
        assert!(item["namespace"].is_null());
        assert_eq!(item["tool"], "deploy");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["arguments"]["env"], "staging");
        assert_eq!(item["contentItems"][0]["text"], "deployed");
        assert_eq!(item["success"], true);
        assert!(item["error"].is_null());
    }

    #[test]
    fn dynamic_tool_completed_item_projects_failure_shape() {
        let item = dynamic_tool_completed_item(
            "call-6",
            "deploy",
            "denied",
            json!({ "env": "production" }),
            Value::Null,
            false,
            tool_error_object("policy denied", None),
        );

        assert_eq!(item["id"], "call-6");
        assert_eq!(item["type"], "dynamicToolCall");
        assert_eq!(item["status"], "denied");
        assert_eq!(item["arguments"]["env"], "production");
        assert!(item["contentItems"].is_null());
        assert_eq!(item["success"], false);
        assert_eq!(item["error"]["message"], "policy denied");
    }

    #[test]
    fn file_change_started_item_projects_codex_style_shape() {
        let item = file_change_started_item(
            "call-7:file-change",
            Some("src/main.rs"),
            "edit",
            Value::from(""),
        );

        assert_eq!(item["id"], "call-7:file-change");
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["status"], "inProgress");
        assert_eq!(item["changes"][0]["path"], "src/main.rs");
        assert_eq!(item["changes"][0]["kind"], "edit");
        assert_eq!(item["changes"][0]["diff"], "");
        assert!(item.get("tool").is_none());
        assert!(item.get("error").is_none());
    }

    #[test]
    fn file_change_completed_item_projects_failure_shape() {
        let item = file_change_completed_item(
            "call-8:file-change",
            None::<String>,
            "write",
            Value::from("failed"),
            Value::from("diff"),
        );

        assert_eq!(item["id"], "call-8:file-change");
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["status"], "failed");
        assert!(item["changes"][0]["path"].is_null());
        assert_eq!(item["changes"][0]["kind"], "write");
        assert_eq!(item["changes"][0]["diff"], "diff");
        assert!(item.get("tool").is_none());
        assert!(item.get("output").is_none());
    }

    #[test]
    fn persisted_file_change_started_item_projects_edit_history_shape() {
        let item = persisted_file_change_started_item(
            "edit-call-1",
            "edit",
            &json!({
                "path": "src/lib.rs",
                "old_text": "before",
                "new_text": "after",
            }),
        )
        .expect("edit projects as fileChange");

        assert_eq!(item["id"], "edit-call-1:file-change");
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["status"], "inProgress");
        assert_eq!(item["changes"][0]["path"], "src/lib.rs");
        assert_eq!(item["changes"][0]["kind"], "edit");
        assert_eq!(item["changes"][0]["diff"], "");
        assert!(item.get("tool").is_none());
        assert!(item.get("output").is_none());
        assert!(item.get("error").is_none());
    }

    #[test]
    fn persisted_file_change_completed_item_preserves_file_change_metadata() {
        let started = persisted_file_change_started_item(
            "write-call-1",
            "write_file",
            &json!({
                "path": "notes/new.txt",
                "content": "hello",
            }),
        )
        .expect("write_file projects as fileChange");
        let completed = persisted_file_change_completed_item(&started, Value::from("completed"));

        assert_eq!(completed["id"], "write-call-1:file-change");
        assert_eq!(completed["type"], "fileChange");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["changes"][0]["path"], "notes/new.txt");
        assert_eq!(completed["changes"][0]["kind"], "write");
        assert_eq!(completed["changes"][0]["diff"], "");
        assert!(completed.get("tool").is_none());
        assert!(completed.get("output").is_none());
        assert!(completed.get("error").is_none());
    }

    #[test]
    fn complete_projected_tool_item_preserves_history_completion_shapes() {
        let mut command =
            persisted_command_execution_started_item("tool-6", "bash", Value::from("cargo test"));
        complete_projected_tool_item(
            &mut command,
            &json!({
                "content": "ok",
                "status": "completed",
                "truncated": true,
            }),
        );
        assert_eq!(command["type"], "commandExecution");
        assert_eq!(command["status"], "completed");
        assert_eq!(command["aggregatedOutput"], "ok");
        assert!(command["error"].is_null());
        assert_eq!(command["truncated"], true);

        let mut dynamic = dynamic_tool_started_item("call-9", "deploy", json!({ "env": "prod" }));
        complete_projected_tool_item(
            &mut dynamic,
            &json!({
                "content": "ERROR: blocked",
            }),
        );
        assert_eq!(dynamic["type"], "dynamicToolCall");
        assert_eq!(dynamic["status"], "failed");
        assert!(dynamic["contentItems"].is_null());
        assert_eq!(dynamic["success"], false);
        assert_eq!(dynamic["error"]["message"], "blocked");
    }

    #[test]
    fn workflow_started_item_projects_codex_style_shape() {
        let item = workflow_started_item(
            "workflow-run-1",
            "workflow-task-1",
            "audit",
            json!({ "kind": "workflow", "status": "running" }),
        );

        assert_eq!(item["id"], "workflow-run-1");
        assert_eq!(item["type"], "workflow");
        assert_eq!(item["workflowName"], "audit");
        assert_eq!(item["taskId"], "workflow-task-1");
        assert_eq!(item["status"], "running");
        assert_eq!(item["task"]["kind"], "workflow");
        assert!(item.get("result").is_none());
        assert!(item.get("error").is_none());
    }

    #[test]
    fn workflow_completed_item_projects_failure_shape() {
        let item = workflow_completed_item(
            "workflow-run-2",
            "workflow-task-2",
            "audit",
            "failed",
            Value::Null,
            json!({ "message": "boom" }),
            json!({ "kind": "workflow", "status": "failed" }),
        );

        assert_eq!(item["id"], "workflow-run-2");
        assert_eq!(item["type"], "workflow");
        assert_eq!(item["workflowName"], "audit");
        assert_eq!(item["taskId"], "workflow-task-2");
        assert_eq!(item["status"], "failed");
        assert!(item["result"].is_null());
        assert_eq!(item["error"]["message"], "boom");
        assert_eq!(item["task"]["status"], "failed");
    }

    #[test]
    fn agent_message_item_projects_text_lifecycle_shape() {
        let started = agent_message_item("item-agent-message-1", "");
        let completed = agent_message_item("item-agent-message-1", "hello");

        assert_eq!(started["id"], "item-agent-message-1");
        assert_eq!(started["type"], "agent_message");
        assert_eq!(started["text"], "");
        assert_eq!(completed["id"], "item-agent-message-1");
        assert_eq!(completed["type"], "agent_message");
        assert_eq!(completed["text"], "hello");
    }

    #[test]
    fn plan_item_projects_text_lifecycle_shape() {
        let started = plan_item("item-plan-1", "");
        let completed = plan_item("item-plan-1", "# Plan\n");

        assert_eq!(started["id"], "item-plan-1");
        assert_eq!(started["type"], "plan");
        assert_eq!(started["text"], "");
        assert_eq!(completed["id"], "item-plan-1");
        assert_eq!(completed["type"], "plan");
        assert_eq!(completed["text"], "# Plan\n");
    }

    #[test]
    fn reasoning_item_projects_summary_lifecycle_shape() {
        let started = reasoning_item("item-reasoning-1", "");
        let completed = reasoning_item("item-reasoning-1", "thinking");

        assert_eq!(started["id"], "item-reasoning-1");
        assert_eq!(started["type"], "reasoning");
        assert_eq!(started["summary"], "");
        assert_eq!(started["content"], "");
        assert_eq!(completed["id"], "item-reasoning-1");
        assert_eq!(completed["type"], "reasoning");
        assert_eq!(completed["summary"], "thinking");
        assert_eq!(completed["content"], "");
    }

    #[test]
    fn projected_text_thread_item_serializes_current_wire_shapes() {
        assert_eq!(
            ProjectedTextThreadItem::agent_message("item-agent-message-1", "hello").into_value(),
            agent_message_item("item-agent-message-1", "hello")
        );
        assert_eq!(
            ProjectedTextThreadItem::plan("item-plan-1", "1. inspect").into_value(),
            plan_item("item-plan-1", "1. inspect")
        );
        assert_eq!(
            ProjectedTextThreadItem::reasoning("item-reasoning-1", "thinking").into_value(),
            reasoning_item("item-reasoning-1", "thinking")
        );
    }

    #[test]
    fn projected_text_item_accumulates_agent_message_lifecycle_shape() {
        let mut item = ProjectedTextItem::new(ProjectedTextItemKind::AgentMessage);

        assert_eq!(item.id(), "item-agent-message-1");
        assert_eq!(item.started_item()["type"], "agent_message");
        assert_eq!(item.started_item()["text"], "");

        item.push_delta("hello ");
        item.push_delta("world");
        let completed = item.completed_item();

        assert_eq!(completed["id"], "item-agent-message-1");
        assert_eq!(completed["type"], "agent_message");
        assert_eq!(completed["text"], "hello world");
    }

    #[test]
    fn projected_text_item_accumulates_plan_and_reasoning_lifecycle_shapes() {
        let mut plan = ProjectedTextItem::new(ProjectedTextItemKind::Plan);
        plan.push_delta("1. inspect\n");
        assert_eq!(plan.id(), "item-plan-1");
        assert_eq!(plan.started_item()["type"], "plan");
        assert_eq!(plan.completed_item()["text"], "1. inspect\n");

        let mut reasoning = ProjectedTextItem::new(ProjectedTextItemKind::Reasoning);
        reasoning.push_delta("thinking");
        assert_eq!(reasoning.id(), "item-reasoning-1");
        assert_eq!(reasoning.started_item()["type"], "reasoning");
        let completed = reasoning.completed_item();
        assert_eq!(completed["summary"], "thinking");
        assert_eq!(completed["content"], "");
    }

    #[test]
    fn command_execution_started_item_projects_runtime_tool_shape() {
        let item = command_execution_started_item("tool-1", "bash", Some("cargo test"));

        assert_eq!(item["id"], "tool-1");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert_eq!(item["command"], "cargo test");
        assert_eq!(item["status"], "in_progress");
        assert!(item.get("aggregatedOutput").is_none());
        assert!(item.get("exitCode").is_none());
    }

    #[test]
    fn command_execution_completed_item_projects_success_shape() {
        let item = command_execution_completed_item(
            "tool-2",
            "bash",
            Some("cargo test"),
            Value::from("completed"),
            Value::from("ok"),
            Value::Null,
            Value::from(0),
            Value::Null,
        );

        assert_eq!(item["id"], "tool-2");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert_eq!(item["command"], "cargo test");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["aggregatedOutput"], "ok");
        assert!(item.get("output").is_none());
        assert!(item["error"].is_null());
        assert_eq!(item["exitCode"], 0);
        assert!(item["truncated"].is_null());
    }

    #[test]
    fn command_execution_completed_item_projects_failure_diagnostics() {
        let item = command_execution_completed_item(
            "tool-3",
            "bash",
            None::<String>,
            Value::from("failed"),
            Value::from("test failure details"),
            Value::from("command failed"),
            Value::from(101),
            Value::from(true),
        );

        assert_eq!(item["id"], "tool-3");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert!(item["command"].is_null());
        assert_eq!(item["status"], "failed");
        assert_eq!(item["aggregatedOutput"], "test failure details");
        assert_eq!(item["error"], "command failed");
        assert_eq!(item["exitCode"], 101);
        assert_eq!(item["truncated"], true);
    }

    #[test]
    fn persisted_command_execution_started_item_keeps_history_shape() {
        let item = persisted_command_execution_started_item("tool-4", "bash", Value::from("ls"));

        assert_eq!(item["id"], "tool-4");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert_eq!(item["command"], "ls");
        assert!(item["cwd"].is_null());
        assert!(item["processId"].is_null());
        assert!(item["source"].is_null());
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["commandActions"], json!([]));
        assert!(item["aggregatedOutput"].is_null());
        assert!(item["error"].is_null());
        assert!(item["exitCode"].is_null());
        assert!(item["durationMs"].is_null());
    }

    #[test]
    fn persisted_command_execution_completed_item_preserves_history_metadata() {
        let started =
            persisted_command_execution_started_item("tool-5", "bash", Value::from("cargo test"));
        let completed = persisted_command_execution_completed_item(
            &started,
            Value::from("completed"),
            Value::from("ok"),
            Value::Null,
            Value::from(true),
        );

        assert_eq!(completed["id"], "tool-5");
        assert_eq!(completed["type"], "commandExecution");
        assert_eq!(completed["tool"], "bash");
        assert_eq!(completed["command"], "cargo test");
        assert!(completed["cwd"].is_null());
        assert!(completed["processId"].is_null());
        assert!(completed["source"].is_null());
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["commandActions"], json!([]));
        assert_eq!(completed["aggregatedOutput"], "ok");
        assert!(completed["error"].is_null());
        assert!(completed["exitCode"].is_null());
        assert!(completed["durationMs"].is_null());
        assert_eq!(completed["truncated"], true);
        assert!(completed.get("result").is_none());
        assert!(completed.get("output").is_none());
    }
}
