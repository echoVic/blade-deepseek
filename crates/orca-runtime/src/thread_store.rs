mod live_thread;
mod local;
mod pagination;
mod projection;
mod types;
mod writer;

pub(crate) const ORCA_HOME_ENV: &str = "ORCA_HOME";

use orca_core::conversation::Conversation;

pub use live_thread::LiveThread;
pub(crate) use local::sessions_dir;
pub use local::{
    JsonlThreadStore, SearchHit, SessionStore, archive_session, compress_session, delete_session,
    list_sessions, list_sessions_with_archived, load_session, rename_session, search_sessions,
};
pub(crate) use pagination::{page_thread_items, page_thread_turns};
pub(crate) use projection::{
    message_to_thread_json, messages_to_thread_items, messages_to_thread_turns,
};
pub use types::{
    SessionMeta, SessionSummary, SessionTranscript, SortDirection, StoredThreadItem,
    StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchHit, StoredThreadSearchPage,
    StoredThreadSummary, StoredThreadSummaryPage, StoredThreadTurn, StoredThreadTurnPage,
    ThreadListFilters, ThreadMetadataPatch, ThreadRelationFilter, ThreadSortKey, ThreadStore,
    TurnItemsView,
};
pub use writer::SessionWriter;
pub(crate) use writer::redact_sensitive_text;

pub(crate) fn resume_conversation(
    transcript: &SessionTranscript,
    system_prompt: String,
) -> Conversation {
    crate::history::resume_conversation(transcript, system_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history;
    use orca_core::approval_types::ActionKind;
    use orca_core::conversation::{MISSING_TOOL_TERMINAL_ERROR, Message, RawToolCall};
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult, ToolTerminalSource};

    #[test]
    fn jsonl_thread_store_is_the_named_storage_backend() {
        fn assert_thread_store<T: ThreadStore>(store: &T) {
            let _ = store;
        }

        let store = JsonlThreadStore::new();
        assert_thread_store(&store);

        let legacy: SessionStore = store;
        assert_thread_store(&legacy);
    }

    #[test]
    fn session_store_boundary_creates_loadable_jsonl_thread() {
        let _guard = history::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let cwd = tempfile::tempdir().unwrap();

        let store = SessionStore::new();
        let thread = store
            .create_live_thread(cwd.path(), "deepseek", Some("model-a".to_string()), "hello")
            .unwrap();
        let thread_id = thread.thread_id().to_string();
        drop(thread);

        let loaded = store.load_session(&thread_id).unwrap();
        assert_eq!(loaded.meta.session_id, thread_id);
        assert_eq!(loaded.meta.provider, "deepseek");
        assert_eq!(loaded.meta.model.as_deref(), Some("model-a"));

        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    fn thread_store_projects_conversation_turns() {
        let messages = vec![
            Message::System {
                content: "system".to_string(),
                pinned: false,
            },
            Message::User {
                content: "hello".to_string(),
                pinned: false,
            },
            Message::Assistant {
                content: Some("hi".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            },
        ];

        let turns =
            messages_to_thread_turns("thread-a", &messages, usize::MAX, TurnItemsView::Full);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].thread_id, "thread-a");
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].items_view, TurnItemsView::Full);
        assert_eq!(turns[0].items.len(), 2);
    }

    #[test]
    fn thread_store_projects_messages_to_json_items() {
        let message = Message::User {
            content: "hello".to_string(),
            pinned: false,
        };

        let item = message_to_thread_json(&message);

        assert_eq!(item["role"], "user");
        assert_eq!(item["content"], "hello");
    }

    #[test]
    fn tool_terminal_metadata_projects_live_and_stored_items_identically() {
        let request = ToolRequest {
            id: "indeterminate-call".to_string(),
            name: ToolName::External("deploy".to_string()),
            action: ActionKind::Write,
            target: Some("production".to_string()),
            raw_arguments: Some(r#"{"env":"production"}"#.to_string()),
        };
        let result = ToolResult::indeterminate(&request, MISSING_TOOL_TERMINAL_ERROR)
            .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
        let messages = vec![
            Message::user("deploy production".to_string()),
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: request.id.clone(),
                    function_name: "deploy".to_string(),
                    arguments: request.raw_arguments.clone().unwrap(),
                }],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: request.id.clone(),
                content: format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
                terminal: Some(result.terminal().clone()),
                pinned: false,
            },
        ];
        let stored_messages = messages
            .iter()
            .map(types::StoredMessage::from)
            .collect::<Vec<_>>();

        let live_items = messages_to_thread_items("thread-a", &messages, None, usize::MAX);
        let stored_items = projection::stored_messages_to_thread_items(
            "thread-a",
            &stored_messages,
            None,
            usize::MAX,
        );

        assert_eq!(live_items, stored_items);
        let item = &stored_items
            .iter()
            .find(|item| item.item["id"] == request.id)
            .expect("projected tool item")
            .item;
        assert_eq!(item["status"], "indeterminate");
        assert_eq!(item["terminalSource"], "compatibility_repair");
        assert!(item.get("invocationStarted").is_none());
        assert_eq!(item["kind"], "indeterminate");
        assert_eq!(item["error"]["message"], MISSING_TOOL_TERMINAL_ERROR);
    }

    #[test]
    fn tool_terminal_metadata_survives_every_persisted_tool_item_shape() {
        for (tool_name, arguments, expected_type) in [
            (
                "bash",
                serde_json::json!({ "command": "deploy" }),
                "commandExecution",
            ),
            (
                "mcp__ops__deploy",
                serde_json::json!({ "env": "production" }),
                "mcpToolCall",
            ),
            (
                "deploy",
                serde_json::json!({ "env": "production" }),
                "dynamicToolCall",
            ),
            (
                "write_file",
                serde_json::json!({ "path": "release.txt", "content": "ready" }),
                "fileChange",
            ),
        ] {
            let request = ToolRequest {
                id: format!("{tool_name}-call"),
                name: ToolName::External(tool_name.to_string()),
                action: ActionKind::Write,
                target: None,
                raw_arguments: Some(arguments.to_string()),
            };
            let result = ToolResult::indeterminate(&request, MISSING_TOOL_TERMINAL_ERROR)
                .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
            let messages = vec![
                Message::user("run tool".to_string()),
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: request.id.clone(),
                        function_name: tool_name.to_string(),
                        arguments: arguments.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: request.id.clone(),
                    content: format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
                    terminal: Some(result.terminal().clone()),
                    pinned: false,
                },
            ];
            let stored_messages = messages
                .iter()
                .map(types::StoredMessage::from)
                .collect::<Vec<_>>();
            let live_items = messages_to_thread_items("thread-a", &messages, None, usize::MAX);
            let stored_items = projection::stored_messages_to_thread_items(
                "thread-a",
                &stored_messages,
                None,
                usize::MAX,
            );
            assert_eq!(live_items, stored_items, "tool {tool_name}");

            let item = stored_items
                .into_iter()
                .find(|item| {
                    item.item["id"] == request.id
                        || item.item["id"] == format!("{}:file-change", request.id)
                })
                .expect("projected tool item")
                .item;
            assert_eq!(item["type"], expected_type, "tool {tool_name}");
            assert_eq!(item["status"], "indeterminate", "tool {tool_name}");
            assert_eq!(
                item["terminalSource"], "compatibility_repair",
                "tool {tool_name}"
            );
            assert_eq!(item["kind"], "indeterminate", "tool {tool_name}");
            assert_eq!(
                item["error"]["message"], MISSING_TOOL_TERMINAL_ERROR,
                "tool {tool_name}"
            );
        }
    }

    #[test]
    fn stored_tool_terminal_rejects_conflicting_status_and_kind() {
        let conflicting = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": "cancelled",
            "status": "cancelled",
            "kind": "runtime_error"
        });

        assert!(serde_json::from_value::<types::StoredMessage>(conflicting).is_err());
    }
}
