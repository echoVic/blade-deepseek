pub use crate::history::{
    LiveThread, SessionStore, SortDirection, StoredThreadItem, StoredThreadItemPage,
    StoredThreadProjection, StoredThreadSearchHit, StoredThreadSearchPage, StoredThreadSummary,
    StoredThreadSummaryPage, StoredThreadTurn, StoredThreadTurnPage, ThreadListFilters,
    ThreadMetadataPatch, ThreadRelationFilter, ThreadSortKey, ThreadStore, TurnItemsView,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history;

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
}
