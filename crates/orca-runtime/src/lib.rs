pub mod agent_child;
pub mod agent_common;
pub mod agent_loop;
pub mod approval_resolution;
pub mod controller;
pub mod cost;
pub mod goals;
pub mod history;
pub mod hooks;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod mentions;
pub mod notify;
pub mod protocol;
pub mod schema_validation;
pub mod server;
pub mod server_runtime;
pub mod session;
pub mod shell_session;
pub mod subagent;
pub mod subagent_execution;
pub mod tasks;
pub mod thread_store;
pub mod tool_execution;
pub mod tool_invocation;
pub mod update_check;
pub mod workflow;
pub mod workflow_execution;
pub mod worktree;

#[cfg(test)]
mod tests {
    use crate::thread_store::{SessionStore, ThreadStore};

    #[test]
    fn thread_store_module_exports_session_store_boundary() {
        fn assert_thread_store<T: ThreadStore>(store: &T) {
            let _ = store;
        }

        assert_thread_store(&SessionStore::new());
    }

    #[test]
    fn thread_store_trait_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub trait ThreadStore"),
            "history must not own the storage-neutral ThreadStore trait"
        );
        assert!(
            thread_store_source.contains("pub trait ThreadStore"),
            "thread_store must own the storage-neutral ThreadStore trait"
        );
    }

    #[test]
    fn jsonl_thread_store_type_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct JsonlThreadStore"),
            "history must not own the JSONL ThreadStore backend type"
        );
        assert!(
            !history_source.contains("pub type SessionStore"),
            "history must not own the SessionStore compatibility alias"
        );
        assert!(
            thread_store_source.contains("pub struct JsonlThreadStore"),
            "thread_store must own the JSONL ThreadStore backend type"
        );
        assert!(
            thread_store_source.contains("pub type SessionStore"),
            "thread_store must own the SessionStore compatibility alias"
        );
    }

    #[test]
    fn thread_store_api_types_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for type_name in [
            "StoredThreadProjection",
            "ThreadListFilters",
            "SortDirection",
            "TurnItemsView",
        ] {
            assert!(
                !history_source.contains(&format!("pub struct {type_name}"))
                    && !history_source.contains(&format!("pub enum {type_name}")),
                "history must not own ThreadStore API type {type_name}"
            );
            assert!(
                thread_store_source.contains(&format!("pub struct {type_name}"))
                    || thread_store_source.contains(&format!("pub enum {type_name}")),
                "thread_store must own ThreadStore API type {type_name}"
            );
        }
    }

    #[test]
    fn live_thread_handle_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct LiveThread"),
            "history must not own the live ThreadStore handle"
        );
        assert!(
            !history_source.contains("impl LiveThread"),
            "history must not own live ThreadStore handle behavior"
        );
        assert!(
            thread_store_source.contains("pub struct LiveThread"),
            "thread_store must own the live ThreadStore handle"
        );
        assert!(
            thread_store_source.contains("impl LiveThread"),
            "thread_store must own live ThreadStore handle behavior"
        );
    }

    #[test]
    fn session_meta_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct SessionMeta"),
            "history must not own ThreadStore session metadata"
        );
        assert!(
            thread_store_source.contains("pub struct SessionMeta"),
            "thread_store must own ThreadStore session metadata"
        );
    }

    #[test]
    fn session_summary_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct SessionSummary"),
            "history must not own ThreadStore session summary"
        );
        assert!(
            thread_store_source.contains("pub struct SessionSummary"),
            "thread_store must own ThreadStore session summary"
        );
    }

    #[test]
    fn session_transcript_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct SessionTranscript"),
            "history must not own ThreadStore session transcript"
        );
        assert!(
            thread_store_source.contains("pub struct SessionTranscript"),
            "thread_store must own ThreadStore session transcript"
        );
    }

    #[test]
    fn session_writer_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct SessionWriter"),
            "history must not own ThreadStore session writer"
        );
        assert!(
            !history_source.contains("impl SessionWriter"),
            "history must not own ThreadStore session writer behavior"
        );
        assert!(
            thread_store_source.contains("pub struct SessionWriter"),
            "thread_store must own ThreadStore session writer"
        );
        assert!(
            thread_store_source.contains("impl SessionWriter"),
            "thread_store must own ThreadStore session writer behavior"
        );
    }

    #[test]
    fn jsonl_record_types_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for type_name in ["SessionRecord", "StoredMessage"] {
            assert!(
                !history_source.contains(&format!("enum {type_name}")),
                "history must not own JSONL ThreadStore record type {type_name}"
            );
            assert!(
                thread_store_source.contains(&format!("enum {type_name}")),
                "thread_store must own JSONL ThreadStore record type {type_name}"
            );
        }
    }

    #[test]
    fn jsonl_append_writer_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for function_name in [
            "write_record(",
            "write_record_line(",
            "redact_session_record(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own JSONL append helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own JSONL append helper {function_name}"
            );
        }
    }

    #[test]
    fn jsonl_read_rewrite_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for function_name in ["read_records(", "rewrite_records(", "write_records_to("] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own JSONL read/rewrite helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own JSONL read/rewrite helper {function_name}"
            );
        }
    }

    #[test]
    fn session_read_models_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for function_name in ["read_session_meta(", "read_transcript("] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore session reader {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore session reader {function_name}"
            );
        }
    }

    #[test]
    fn thread_record_lookup_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for function_name in [
            "load_thread_records(",
            "find_session_path(",
            "collect_session_files(",
            "is_history_file(",
            "sessions_dir(",
            "archive_dir(",
            "orca_home(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore lookup helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore lookup helper {function_name}"
            );
        }
    }

    #[test]
    fn session_list_load_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for function_name in [
            "list_sessions(",
            "list_sessions_with_archived(",
            "load_session(",
            "summarize_session_with_archive_flag(",
            "collect_summaries_from_root(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore read operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore read operation {function_name}"
            );
        }
    }

    #[test]
    fn protocol_imports_thread_types_from_thread_store_boundary() {
        let protocol_source = include_str!("protocol.rs");

        assert!(
            !protocol_source.contains("use crate::history"),
            "protocol must import thread protocol types through thread_store"
        );
    }

    #[test]
    fn agent_loop_imports_session_types_from_thread_store_boundary() {
        let agent_loop_source = include_str!("agent_loop.rs");

        assert!(
            !agent_loop_source.contains("use crate::history"),
            "agent loop must import session transcript/writer types through thread_store"
        );
    }

    #[test]
    fn session_imports_session_types_from_thread_store_boundary() {
        let session_source = include_str!("session.rs");

        assert!(
            !session_source.contains("use crate::history::{self, SessionWriter};"),
            "session production code must import session transcript/writer types through thread_store"
        );
    }
}
