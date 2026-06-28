pub use crate::history::{
    JsonlThreadStore, LiveThread, SessionStore, SortDirection, StoredThreadItem,
    StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchHit, StoredThreadSearchPage,
    StoredThreadSummary, StoredThreadSummaryPage, StoredThreadTurn, StoredThreadTurnPage,
    ThreadListFilters, ThreadMetadataPatch, ThreadRelationFilter, ThreadSortKey, ThreadStore,
    TurnItemsView,
};
use orca_core::conversation::Message;

pub(crate) fn messages_to_thread_turns(
    thread_id: &str,
    messages: &[Message],
    limit: usize,
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    crate::history::messages_to_thread_turns(thread_id, messages, limit, items_view)
}

pub(crate) fn messages_to_thread_items(
    thread_id: &str,
    messages: &[Message],
    turn_id: Option<&str>,
    limit: usize,
) -> Vec<StoredThreadItem> {
    crate::history::messages_to_thread_items(thread_id, messages, turn_id, limit)
}

pub(crate) fn page_thread_turns(
    turns: Vec<StoredThreadTurn>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
) -> StoredThreadTurnPage {
    crate::history::page_thread_turns(turns, cursor, limit, sort_direction)
}

pub(crate) fn page_thread_items(
    items: Vec<StoredThreadItem>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
) -> StoredThreadItemPage {
    crate::history::page_thread_items(items, cursor, limit, sort_direction)
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
        let _guard = history::TEST_ENV_LOCK.lock().unwrap();
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
}
