use orca_core::conversation::{Message, RawToolCall};
use orca_core::tool_types::ToolStatus;
use serde_json::{Value, json};

use crate::tool_item_projection::{
    dynamic_tool_completed_item, dynamic_tool_started_item, mcp_result_from_content,
    mcp_tool_completed_item, mcp_tool_parts, mcp_tool_started_item, parse_json_or_null,
    persisted_command_execution_completed_item, persisted_command_execution_started_item,
    persisted_file_change_completed_item, persisted_file_change_started_item,
    tool_error_object_from_value,
};

use super::types::{
    SessionSummary, StoredMessage, StoredThreadItem, StoredThreadSummary, StoredThreadTurn,
    TurnItemsView,
};

pub(crate) fn message_to_thread_json(message: &Message) -> Value {
    match message {
        Message::System { content, .. } => json!({
            "role": "system",
            "content": content,
        }),
        Message::User { content, .. } => json!({
            "role": "user",
            "content": content,
        }),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => json!({
            "role": "assistant",
            "content": content,
            "reasoningContent": reasoning_content,
            "toolCalls": tool_calls,
        }),
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => json!({
            "role": "tool",
            "toolCallId": tool_call_id,
            "content": content,
        }),
    }
}

pub(crate) fn stored_message_to_thread_json(message: &StoredMessage) -> Value {
    match message {
        StoredMessage::System { content, .. } => json!({
            "role": "system",
            "content": content,
        }),
        StoredMessage::User { content, .. } => json!({
            "role": "user",
            "content": content,
        }),
        StoredMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => json!({
            "role": "assistant",
            "content": content,
            "reasoningContent": reasoning_content,
            "toolCalls": tool_calls,
        }),
        StoredMessage::Tool {
            tool_call_id,
            content,
            ..
        } => json!({
            "role": "tool",
            "toolCallId": tool_call_id,
            "content": content,
        }),
    }
}

pub(crate) fn messages_to_thread_turns(
    thread_id: &str,
    messages: &[Message],
    limit: usize,
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    group_messages_into_thread_turns(thread_id, messages, items_view)
        .into_iter()
        .take(limit)
        .collect()
}

pub(crate) fn messages_to_thread_items(
    thread_id: &str,
    messages: &[Message],
    turn_id: Option<&str>,
    limit: usize,
) -> Vec<StoredThreadItem> {
    group_messages_into_thread_turns(thread_id, messages, TurnItemsView::Full)
        .into_iter()
        .flat_map(|turn| {
            turn.items
                .into_iter()
                .map(move |item| (turn.turn_id.clone(), item))
        })
        .enumerate()
        .map(|(item_index, (item_turn_id, item))| StoredThreadItem {
            thread_id: thread_id.to_string(),
            turn_id: item_turn_id,
            item_id: item_id_for_index(item_index),
            index: item_index,
            item,
        })
        .filter(|item| turn_id.is_none_or(|requested| requested == item.turn_id))
        .take(limit)
        .collect()
}

pub(crate) fn stored_messages_to_thread_turns(
    thread_id: &str,
    messages: &[StoredMessage],
    limit: usize,
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    group_stored_messages_into_thread_turns(thread_id, messages, items_view)
        .into_iter()
        .take(limit)
        .collect()
}

pub(crate) fn stored_messages_to_thread_items(
    thread_id: &str,
    messages: &[StoredMessage],
    turn_id: Option<&str>,
    limit: usize,
) -> Vec<StoredThreadItem> {
    group_stored_messages_into_thread_turns(thread_id, messages, TurnItemsView::Full)
        .into_iter()
        .flat_map(|turn| {
            turn.items
                .into_iter()
                .map(move |item| (turn.turn_id.clone(), item))
        })
        .enumerate()
        .map(|(item_index, (item_turn_id, item))| StoredThreadItem {
            thread_id: thread_id.to_string(),
            turn_id: item_turn_id,
            item_id: item_id_for_index(item_index),
            index: item_index,
            item,
        })
        .filter(|item| turn_id.is_none_or(|requested| requested == item.turn_id))
        .take(limit)
        .collect()
}

fn group_messages_into_thread_turns(
    thread_id: &str,
    messages: &[Message],
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    let mut turns = Vec::new();
    for message in messages {
        if matches!(message, Message::System { .. }) {
            continue;
        }
        let items = message_to_thread_items_for_projection(message);
        let role = message_role(message).to_string();
        let starts_turn = turns.is_empty() || matches!(message, Message::User { .. });

        if starts_turn {
            let index = turns.len();
            turns.push(StoredThreadTurn {
                thread_id: thread_id.to_string(),
                turn_id: turn_id_for_index(index),
                index,
                role,
                items_view,
                items: items_for_view(items_view, items),
            });
        } else if let Some(turn) = turns.last_mut() {
            if turn.items_view != TurnItemsView::NotLoaded {
                merge_projected_items(&mut turn.items, items);
            }
        }
    }
    turns
}

fn group_stored_messages_into_thread_turns(
    thread_id: &str,
    messages: &[StoredMessage],
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    let mut turns = Vec::new();
    for message in messages {
        if matches!(message, StoredMessage::System { .. }) {
            continue;
        }
        let items = stored_message_to_thread_items_for_projection(message);
        let role = stored_message_role(message).to_string();
        let starts_turn = turns.is_empty() || matches!(message, StoredMessage::User { .. });

        if starts_turn {
            let index = turns.len();
            turns.push(StoredThreadTurn {
                thread_id: thread_id.to_string(),
                turn_id: turn_id_for_index(index),
                index,
                role,
                items_view,
                items: items_for_view(items_view, items),
            });
        } else if let Some(turn) = turns.last_mut()
            && turn.items_view != TurnItemsView::NotLoaded
        {
            merge_projected_items(&mut turn.items, items);
        }
    }
    turns
}

fn message_role(message: &Message) -> &'static str {
    match message {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
        Message::Tool { .. } => "tool",
    }
}

fn stored_message_role(message: &StoredMessage) -> &'static str {
    match message {
        StoredMessage::System { .. } => "system",
        StoredMessage::User { .. } => "user",
        StoredMessage::Assistant { .. } => "assistant",
        StoredMessage::Tool { .. } => "tool",
    }
}

fn message_to_thread_items_for_projection(message: &Message) -> Vec<Value> {
    match message {
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            let mut items = Vec::new();
            if content.is_some() || reasoning_content.is_some() || tool_calls.is_empty() {
                items.push(message_to_thread_json(message));
            }
            items.extend(tool_calls.iter().map(tool_call_to_thread_item));
            items
        }
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => vec![tool_result_to_thread_item(tool_call_id, content)],
        _ => vec![message_to_thread_json(message)],
    }
}

fn stored_message_to_thread_items_for_projection(message: &StoredMessage) -> Vec<Value> {
    match message {
        StoredMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            let mut items = Vec::new();
            if content.is_some() || reasoning_content.is_some() || tool_calls.is_empty() {
                items.push(stored_message_to_thread_json(message));
            }
            items.extend(tool_calls.iter().map(tool_call_to_thread_item));
            items
        }
        StoredMessage::Tool {
            tool_call_id,
            content,
            status,
            error,
            exit_code,
            truncated,
            ..
        } => vec![tool_result_to_thread_item_with_metadata(
            tool_call_id,
            content,
            *status,
            error.as_deref(),
            *exit_code,
            *truncated,
        )],
        _ => vec![stored_message_to_thread_json(message)],
    }
}

fn merge_projected_items(turn_items: &mut Vec<Value>, items: Vec<Value>) {
    for item in items {
        if item["type"] == "tool_result"
            && let Some(tool_call_id) = item["toolCallId"].as_str()
            && let Some(existing) = turn_items
                .iter_mut()
                .rev()
                .find(|candidate| projected_tool_item_matches_result(candidate, tool_call_id))
        {
            complete_tool_item(existing, &item);
            continue;
        }
        turn_items.push(item);
    }
}

fn projected_tool_item_matches_result(candidate: &Value, tool_call_id: &str) -> bool {
    candidate["id"].as_str() == Some(tool_call_id)
        || (candidate["type"] == "fileChange"
            && candidate["id"].as_str() == Some(&format!("{tool_call_id}:file-change")))
}

fn tool_call_to_thread_item(tool_call: &RawToolCall) -> Value {
    if let Some((server, tool)) = mcp_tool_parts(&tool_call.function_name) {
        mcp_tool_started_item(
            tool_call.id.clone(),
            server,
            tool,
            parse_json_or_null(&tool_call.arguments),
        )
    } else {
        let arguments = parse_json_or_null(&tool_call.arguments);
        if let Some(item) =
            persisted_file_change_started_item(&tool_call.id, &tool_call.function_name, &arguments)
        {
            return item;
        }
        if tool_call.function_name == "bash" {
            return command_execution_thread_item(tool_call);
        }
        dynamic_tool_started_item(
            tool_call.id.clone(),
            tool_call.function_name.clone(),
            arguments,
        )
    }
}

fn command_execution_thread_item(tool_call: &RawToolCall) -> Value {
    persisted_command_execution_started_item(
        tool_call.id.clone(),
        tool_call.function_name.clone(),
        command_from_tool_arguments(&tool_call.arguments),
    )
}

fn tool_result_to_thread_item(tool_call_id: &str, content: &str) -> Value {
    json!({
        "type": "tool_result",
        "toolCallId": tool_call_id,
        "content": content,
    })
}

fn tool_result_to_thread_item_with_metadata(
    tool_call_id: &str,
    content: &str,
    status: Option<ToolStatus>,
    error: Option<&str>,
    exit_code: Option<i32>,
    truncated: bool,
) -> Value {
    let mut item = tool_result_to_thread_item(tool_call_id, content);
    if let Some(status) = status {
        item["status"] = Value::from(status.as_str());
    }
    if let Some(error) = error {
        item["error"] = Value::from(error.to_string());
    }
    if let Some(exit_code) = exit_code {
        item["exitCode"] = Value::from(exit_code);
    }
    if truncated {
        item["truncated"] = Value::from(true);
    }
    item
}

fn complete_tool_item(item: &mut Value, result: &Value) {
    let content = result["content"].as_str().unwrap_or_default();
    if let Some((status, failure)) = tool_failure_from_result(result)
        .or_else(|| parse_tool_failure_content(content).map(|failure| ("failed", failure)))
    {
        complete_projected_tool_item(item, status, result, Value::Null, failure);
        return;
    }

    complete_projected_tool_item(
        item,
        "completed",
        result,
        mcp_result_from_content(content),
        Value::Null,
    );
}

fn complete_projected_tool_item(
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

fn command_from_tool_arguments(raw: &str) -> Value {
    parse_json_or_null(raw)
        .get("command")
        .and_then(Value::as_str)
        .map(|command| Value::from(command.to_string()))
        .unwrap_or(Value::Null)
}

fn items_for_view(items_view: TurnItemsView, items: Vec<Value>) -> Vec<Value> {
    match items_view {
        TurnItemsView::NotLoaded => Vec::new(),
        TurnItemsView::Summary | TurnItemsView::Full => items,
    }
}

fn turn_id_for_index(index: usize) -> String {
    format!("turn-{}", index + 1)
}

pub(crate) fn next_turn_id_for_messages(thread_id: &str, messages: &[Message]) -> String {
    let turn_count =
        group_messages_into_thread_turns(thread_id, messages, TurnItemsView::NotLoaded).len();
    turn_id_for_index(turn_count)
}

fn item_id_for_index(index: usize) -> String {
    format!("item-{}", index + 1)
}

impl From<SessionSummary> for StoredThreadSummary {
    fn from(summary: SessionSummary) -> Self {
        Self {
            thread_id: summary.session_id,
            title: summary.title,
            cwd: summary.cwd,
            provider: summary.provider,
            model: summary.model,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            archived: summary.archived,
            parent_id: summary.parent_id,
            forked: summary.forked,
            approval_mode: summary.approval_mode,
            active_permission_profile: summary.active_permission_profile,
            permission_rule_count: summary.permission_rule_count,
            runtime_workspace_roots: summary.runtime_workspace_roots,
            additional_working_directories: summary.additional_working_directories,
        }
    }
}
