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
}
