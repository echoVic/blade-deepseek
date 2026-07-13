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
    next_turn_id_for_messages,
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
    use orca_core::conversation::Message;

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
    fn thread_store_projects_next_turn_id() {
        let messages = vec![
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

        assert_eq!(next_turn_id_for_messages("thread-a", &messages), "turn-2");
    }
}
