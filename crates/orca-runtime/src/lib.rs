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
    fn runtime_turn_context_types_are_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        for type_name in [
            "RuntimeTurnConfig",
            "RuntimeTurnDeps",
            "RuntimeTurnState",
            "RuntimeTurnExecution",
        ] {
            assert!(
                !agent_loop_source.contains(&format!("struct {type_name}")),
                "agent_loop must not own runtime turn context type {type_name}"
            );
            assert!(
                !agent_loop_source.contains(&format!("impl<'a> {type_name}")),
                "agent_loop must not own runtime turn context behavior {type_name}"
            );
            assert!(
                lifecycle_source.contains(&format!("struct {type_name}")),
                "lifecycle must own runtime turn context type {type_name}"
            );
            assert!(
                lifecycle_source.contains(&format!("impl<'a> {type_name}")),
                "lifecycle must own runtime turn context behavior {type_name}"
            );
        }
    }

    #[test]
    fn thread_steer_handle_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct ThreadSteerHandle"),
            "agent_loop must not own the thread turn steer handle"
        );
        assert!(
            !agent_loop_source.contains("impl ThreadSteerHandle"),
            "agent_loop must not own thread turn steer handle behavior"
        );
        assert!(
            lifecycle_source.contains("struct ThreadSteerHandle"),
            "lifecycle must own the thread turn steer handle"
        );
        assert!(
            lifecycle_source.contains("impl ThreadSteerHandle"),
            "lifecycle must own thread turn steer handle behavior"
        );
    }

    #[test]
    fn runtime_steer_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeSteerStep"),
            "agent_loop must not own runtime steer step state"
        );
        assert!(
            !agent_loop_source.contains("impl RuntimeSteerStep"),
            "agent_loop must not own runtime steer step behavior"
        );
        assert!(
            lifecycle_source.contains("struct RuntimeSteerStep"),
            "lifecycle must own runtime steer step state"
        );
        assert!(
            lifecycle_source.contains("impl RuntimeSteerStep"),
            "lifecycle must own runtime steer step behavior"
        );
        assert!(
            !agent_loop_source.contains("for input in steer_handle.drain()"),
            "agent_loop must not directly drain steer inputs into conversation"
        );
    }

    #[test]
    fn agent_loop_context_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct AgentLoopContext"),
            "agent_loop must not own the runtime agent loop context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentLoopContext"),
            "agent_loop must not own runtime agent loop context behavior"
        );
        assert!(
            lifecycle_source.contains("struct AgentLoopContext"),
            "lifecycle must own the runtime agent loop context"
        );
        assert!(
            lifecycle_source.contains("impl<'a> AgentLoopContext"),
            "lifecycle must own runtime agent loop context behavior"
        );
    }

    #[test]
    fn agent_tool_policy_context_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("struct AgentToolPolicyContext"),
            "agent_loop must not own agent tool policy context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentToolPolicyContext"),
            "agent_loop must not own agent tool policy behavior"
        );
        assert!(
            tool_invocation_source.contains("struct AgentToolPolicyContext"),
            "tool_invocation must own agent tool policy context"
        );
        assert!(
            tool_invocation_source.contains("impl<'a> AgentToolPolicyContext"),
            "tool_invocation must own agent tool policy behavior"
        );
    }

    #[test]
    fn child_tool_policy_gate_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("fn child_tool_policy_failure"),
            "agent_loop must not own child tool policy gate behavior"
        );
        assert!(
            tool_invocation_source.contains("fn child_tool_policy_failure"),
            "tool_invocation must own child tool policy gate behavior"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn reject_disallowed_child_tool"),
            "tool_invocation must expose child tool policy gate to the agent loop"
        );
    }

    #[test]
    fn agent_conversation_context_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("struct AgentConversationContext"),
            "agent_loop must not own agent conversation context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentConversationContext"),
            "agent_loop must not own agent conversation context behavior"
        );
        assert!(
            session_source.contains("struct AgentConversationContext"),
            "session must own agent conversation context"
        );
        assert!(
            session_source.contains("impl<'a> AgentConversationContext"),
            "session must own agent conversation context behavior"
        );
    }

    #[test]
    fn runtime_compaction_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeCompactionStep"),
            "agent_loop must not own runtime compaction step state"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> RuntimeCompactionStep"),
            "agent_loop must not own runtime compaction step behavior"
        );
        assert!(
            lifecycle_source.contains("struct RuntimeCompactionStep"),
            "lifecycle must own runtime compaction step state"
        );
        assert!(
            lifecycle_source.contains("impl<'a")
                && lifecycle_source.contains("RuntimeCompactionStep<'a"),
            "lifecycle must own runtime compaction step behavior"
        );

        for marker in [
            "HookEvent::OnBudgetWarning",
            "HookEvent::PreCompact",
            "HookEvent::PostCompact",
            "compact_with_summary(",
            "append_compaction(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime compaction detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_provider_turn_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeProviderTurnStep"),
            "agent_loop must not own runtime provider turn step state"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> RuntimeProviderTurnStep"),
            "agent_loop must not own runtime provider turn step behavior"
        );
        assert!(
            lifecycle_source.contains("struct RuntimeProviderTurnStep"),
            "lifecycle must own runtime provider turn step state"
        );
        assert!(
            lifecycle_source.contains("impl RuntimeProviderTurnStep"),
            "lifecycle must own runtime provider turn step behavior"
        );

        for marker in [
            "assistant_reasoning_delta",
            "assistant_message_delta",
            "usage_updated",
            "append_usage(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider turn detail {marker}"
            );
        }
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
    fn jsonl_thread_store_impl_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("impl ThreadStore for JsonlThreadStore"),
            "history must not own the JSONL ThreadStore trait implementation"
        );
        assert!(
            thread_store_source.contains("impl ThreadStore for JsonlThreadStore"),
            "thread_store must own the JSONL ThreadStore trait implementation"
        );
    }

    #[test]
    fn thread_projection_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for (function_name, allows_generic) in [
            ("thread_summary_matches", false),
            ("thread_summary_matches_filters", false),
            ("sort_thread_summaries", false),
            ("sort_thread_search_hits", false),
            ("message_to_thread_json", false),
            ("stored_message_to_thread_json", false),
            ("messages_to_thread_turns", false),
            ("messages_to_thread_items", false),
            ("stored_messages_to_thread_turns", false),
            ("stored_messages_to_thread_items", false),
            ("page_thread_turns", false),
            ("page_thread_items", false),
            ("page_vec", true),
            ("next_turn_id_for_messages", false),
        ] {
            let plain_fn = format!("fn {function_name}(");
            let generic_fn = format!("fn {function_name}<");
            assert!(
                !history_source.contains(&plain_fn) && !history_source.contains(&generic_fn),
                "history must not own ThreadStore projection helper {function_name}"
            );
            let thread_store_owns_helper = thread_store_source.contains(&plain_fn)
                || (allows_generic && thread_store_source.contains(&generic_fn));
            assert!(
                thread_store_owns_helper,
                "thread_store must own ThreadStore projection helper {function_name}"
            );
            assert!(
                !thread_store_source.contains(&format!("crate::history::{function_name}(")),
                "thread_store must not bridge projection helper {function_name} through history"
            );
        }
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
    fn session_search_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        assert!(
            !history_source.contains("pub struct SearchHit"),
            "history must not own ThreadStore search result type"
        );
        assert!(
            thread_store_source.contains("pub struct SearchHit"),
            "thread_store must own ThreadStore search result type"
        );

        for function_name in [
            "search_sessions(",
            "search_roots_with_ripgrep(",
            "search_root_in_process(",
            "search_compressed_root(",
            "push_matching_lines(",
            "push_search_hit(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore search operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore search operation {function_name}"
            );
        }
    }

    #[test]
    fn session_mutation_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store.rs");

        for function_name in [
            "delete_session(",
            "archive_session(",
            "rename_session(",
            "compress_session(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore mutation operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore mutation operation {function_name}"
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
