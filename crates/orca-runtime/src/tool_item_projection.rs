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
}
