use orca_core::approval_rules::{PermissionRule, PermissionRules};
use orca_core::approval_types::{ActionKind, ApprovalMode, Decision};
use orca_core::config::ActivePermissionProfile;
use orca_core::conversation::{Message, RawToolCall};
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_runtime::history::{
    SessionStore, SortDirection, ThreadListFilters, ThreadMetadataPatch, ThreadRelationFilter,
    ThreadSortKey, ThreadStore, TurnItemsView,
};
use tempfile::tempdir;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn session_store_thread_store_appends_live_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(
                home,
                "mock",
                Some("deepseek-v4-flash".to_string()),
                "thread store prompt",
            )
            .expect("create live thread");

        assert!(!thread.thread_id().is_empty());

        thread
            .append_items(&[
                Message::User {
                    content: "thread store prompt".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("thread store response".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append thread items");
        thread.complete("success").expect("complete thread");

        let transcript = store
            .load_session(thread.thread_id())
            .expect("thread id loads transcript");
        assert_eq!(transcript.meta.session_id, thread.thread_id());
        assert_eq!(transcript.meta.title, "thread store prompt");
        assert!(transcript.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == "thread store prompt")
        }));
        assert!(transcript.messages.iter().any(|message| {
            matches!(message, Message::Assistant { content: Some(content), .. } if content == "thread store response")
        }));
    });
}

#[test]
fn session_store_persists_thread_permission_profile() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread_with_permissions(
                home,
                "mock",
                Some("deepseek-v4-flash".to_string()),
                "permission profile prompt",
                Some(ActivePermissionProfile::new(
                    "locked-down",
                    Some(":workspace"),
                )),
                ApprovalMode::Plan,
                PermissionRules {
                    rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
                },
                Vec::new(),
            )
            .expect("create live thread with permissions");
        let thread_id = thread.thread_id().to_string();
        thread.complete("success").expect("complete thread");

        let transcript = store.load_session(&thread_id).expect("load thread");
        assert_eq!(
            transcript.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
        assert_eq!(transcript.meta.approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(transcript.meta.permission_rules.rules.len(), 1);
        assert_eq!(transcript.meta.permission_rules.rules[0].tool, "bash");

        let listed = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads");
        assert_eq!(listed.data[0].approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(
            listed.data[0].active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
        assert_eq!(listed.data[0].permission_rule_count, 1);
    });
}

#[test]
fn session_store_thread_store_updates_metadata_by_thread_id() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "old title")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread.complete("success").expect("complete thread");

        store
            .update_thread_metadata(
                &thread_id,
                ThreadMetadataPatch {
                    title: Some("new title".to_string()),
                    ..ThreadMetadataPatch::default()
                },
            )
            .expect("update metadata");

        let transcript = store.load_session(&thread_id).expect("load updated thread");
        assert_eq!(transcript.meta.title, "new title");
        let summary = store
            .list_sessions(1)
            .expect("list sessions")
            .into_iter()
            .find(|summary| summary.session_id == thread_id)
            .expect("thread summary");
        assert_eq!(summary.title, "new title");
    });
}

#[test]
fn session_store_thread_store_updates_permission_metadata_by_thread_id() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread_with_permissions(
                home,
                "mock",
                None,
                "old permissions",
                None,
                ApprovalMode::Plan,
                PermissionRules {
                    rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
                },
                Vec::new(),
            )
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread.complete("success").expect("complete thread");

        store
            .update_thread_metadata(
                &thread_id,
                ThreadMetadataPatch {
                    active_permission_profile: Some(ActivePermissionProfile::new(
                        "workspace-plus",
                        Some(":workspace"),
                    )),
                    approval_mode: Some(ApprovalMode::AutoEdit),
                    permission_rules: Some(PermissionRules {
                        rules: vec![PermissionRule::new(
                            "bash",
                            "cargo test *",
                            Decision::Prompt,
                        )],
                    }),
                    ..ThreadMetadataPatch::default()
                },
            )
            .expect("update permission metadata");

        let transcript = store.load_session(&thread_id).expect("load updated thread");
        assert_eq!(
            transcript.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace")
            ))
        );
        assert_eq!(transcript.meta.approval_mode, Some(ApprovalMode::AutoEdit));
        assert_eq!(
            transcript.meta.permission_rules.rules[0].pattern,
            "cargo test *"
        );
        assert_eq!(
            transcript.meta.permission_rules.rules[0].decision,
            Decision::Prompt
        );
        let summary = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads")
            .data
            .into_iter()
            .find(|summary| summary.thread_id == thread_id)
            .expect("thread summary");
        assert_eq!(summary.approval_mode, Some(ApprovalMode::AutoEdit));
        assert_eq!(
            summary.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace")
            ))
        );
        assert_eq!(summary.permission_rule_count, 1);
    });
}

#[test]
fn session_store_paginates_thread_summaries_and_search_hits() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut first = store
            .create_live_thread(home, "mock", None, "first paginated thread")
            .expect("create first thread");
        first
            .append_items(&[Message::User {
                content: "shared search needle first".to_string(),
                pinned: false,
            }])
            .expect("append first");
        let first_id = first.thread_id().to_string();
        first.complete("success").expect("complete first");
        std::thread::sleep(std::time::Duration::from_millis(5));

        let mut second = store
            .create_live_thread(home, "mock", None, "second paginated thread")
            .expect("create second thread");
        second
            .append_items(&[Message::User {
                content: "shared search needle second".to_string(),
                pinned: false,
            }])
            .expect("append second");
        let second_id = second.thread_id().to_string();
        second.complete("success").expect("complete second");

        let first_page = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("first list page");
        assert_eq!(first_page.data.len(), 1);
        let first_list_id = first_page.data[0].thread_id.clone();
        assert!(first_list_id == first_id || first_list_id == second_id);
        assert_eq!(first_page.next_cursor.as_deref(), Some("1"));

        let second_page = store
            .list_threads(
                first_page.next_cursor.as_deref(),
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("second list page");
        assert_eq!(second_page.data.len(), 1);
        let second_list_id = second_page.data[0].thread_id.clone();
        assert!(second_list_id == first_id || second_list_id == second_id);
        assert_ne!(first_list_id, second_list_id);
        assert_eq!(second_page.next_cursor, None);
        assert_eq!(second_page.backwards_cursor.as_deref(), Some("1"));

        let asc_page = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("ascending list page");
        assert_eq!(asc_page.data.len(), 1);
        assert_eq!(asc_page.data[0].thread_id, first_id);

        let created_desc_page = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::CreatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("created desc list page");
        assert_eq!(created_desc_page.data[0].thread_id, second_id);

        let filtered_page = store
            .list_threads(
                None,
                10,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                Some("second paginated"),
            )
            .expect("filtered list page");
        assert_eq!(filtered_page.data.len(), 1);
        assert_eq!(filtered_page.data[0].thread_id, second_id);
        assert_eq!(filtered_page.next_cursor, None);

        let search_page = store
            .search_threads(
                "shared search needle",
                None,
                1,
                false,
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
            )
            .expect("first search page");
        assert_eq!(search_page.data.len(), 1);
        let first_search_id = search_page.data[0].thread.thread_id.clone();
        assert!(first_search_id == first_id || first_search_id == second_id);
        assert_eq!(search_page.next_cursor.as_deref(), Some("1"));

        let search_page_2 = store
            .search_threads(
                "shared search needle",
                search_page.next_cursor.as_deref(),
                1,
                false,
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
            )
            .expect("second search page");
        assert_eq!(search_page_2.data.len(), 1);
        let second_search_id = search_page_2.data[0].thread.thread_id.clone();
        assert!(second_search_id == first_id || second_search_id == second_id);
        assert_ne!(first_search_id, second_search_id);
        assert_eq!(search_page_2.next_cursor, None);
    });
}

#[test]
fn session_store_filters_thread_list_by_metadata_archival_and_relation() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let alpha_cwd = home.join("alpha");
        let beta_cwd = home.join("beta");
        std::fs::create_dir_all(&alpha_cwd).expect("alpha cwd");
        std::fs::create_dir_all(&beta_cwd).expect("beta cwd");

        let mut parent = store
            .create_live_thread(
                &alpha_cwd,
                "deepseek",
                Some("deepseek-v4-flash".to_string()),
                "parent relation thread",
            )
            .expect("create parent");
        let parent_id = parent.thread_id().to_string();
        parent.complete("success").expect("complete parent");

        let direct_child_meta = store.create_fork_meta(
            &alpha_cwd,
            "deepseek",
            Some("deepseek-reasoner".to_string()),
            "direct child relation thread",
            parent_id.clone(),
        );
        let direct_child_id = direct_child_meta.session_id.clone();
        let mut direct_child = store
            .start_writer_from_meta(direct_child_meta)
            .expect("direct child writer");
        direct_child
            .complete("success")
            .expect("complete direct child");

        let grandchild_meta = store.create_fork_meta(
            &beta_cwd,
            "openai",
            Some("gpt-5".to_string()),
            "grandchild relation thread",
            direct_child_id.clone(),
        );
        let grandchild_id = grandchild_meta.session_id.clone();
        let mut grandchild = store
            .start_writer_from_meta(grandchild_meta)
            .expect("grandchild writer");
        grandchild.complete("success").expect("complete grandchild");

        let archived_meta = store.create_meta(
            &beta_cwd,
            "deepseek",
            Some("deepseek-v4-flash".to_string()),
            "archived beta thread",
        );
        let archived_id = archived_meta.session_id.clone();
        let mut archived = store
            .start_writer_from_meta(archived_meta)
            .expect("archived writer");
        archived.complete("success").expect("complete archived");
        store
            .archive_session(&archived_id)
            .expect("archive beta thread");

        let alpha_only = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    cwd_filters: vec![alpha_cwd.display().to_string()],
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("alpha cwd list");
        assert_eq!(
            alpha_only
                .data
                .iter()
                .map(|thread| thread.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec![parent_id.as_str(), direct_child_id.as_str()]
        );

        let deepseek_flash = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    model_providers: Some(vec!["deepseek".to_string()]),
                    model_names: Some(vec!["deepseek-v4-flash".to_string()]),
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("deepseek flash list");
        assert_eq!(deepseek_flash.data.len(), 1);
        assert_eq!(deepseek_flash.data[0].thread_id, parent_id);

        let archived_only = store
            .list_threads(
                None,
                10,
                ThreadListFilters::archived(),
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("archived list");
        assert_eq!(archived_only.data.len(), 1);
        assert_eq!(archived_only.data[0].thread_id, archived_id);
        assert!(archived_only.data[0].archived);

        let direct_children = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    relation: Some(ThreadRelationFilter::DirectChildrenOf(parent_id.clone())),
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("direct children list");
        assert_eq!(direct_children.data.len(), 1);
        assert_eq!(direct_children.data[0].thread_id, direct_child_id);

        let descendants = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    relation: Some(ThreadRelationFilter::DescendantsOf(parent_id)),
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("descendants list");
        assert_eq!(
            descendants
                .data
                .iter()
                .map(|thread| thread.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec![direct_child_id.as_str(), grandchild_id.as_str()]
        );
    });
}

#[test]
fn session_store_projects_thread_turns_and_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "turn projection user".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("turn projection assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append projection items");
        thread.complete("success").expect("complete thread");

        let turns = store
            .list_thread_turns(
                &thread_id,
                None,
                10,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .expect("list thread turns");
        assert_eq!(turns.data.len(), 1);
        assert_eq!(turns.data[0].items_view, TurnItemsView::Full);
        assert_eq!(turns.data[0].turn_id, "turn-1");
        assert_eq!(turns.data[0].index, 0);
        assert_eq!(turns.data[0].role, "user");
        assert_eq!(turns.data[0].items.len(), 2);
        assert_eq!(turns.data[0].items[0]["content"], "turn projection user");
        assert_eq!(
            turns.data[0].items[1]["content"],
            "turn projection assistant"
        );

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        assert_eq!(items.data.len(), 2);
        assert_eq!(items.data[0].item_id, "item-1");
        assert_eq!(items.data[0].turn_id, "turn-1");
        assert_eq!(items.data[0].item["content"], "turn projection user");
        assert_eq!(items.data[1].item_id, "item-2");
        assert_eq!(items.data[1].turn_id, "turn-1");
        assert_eq!(items.data[1].item["content"], "turn projection assistant");

        let filtered = store
            .list_thread_items(&thread_id, Some("turn-1"), None, 10, SortDirection::Asc)
            .expect("list filtered thread items");
        assert_eq!(filtered.data.len(), 2);
        assert_eq!(filtered.data[1].turn_id, "turn-1");
        assert_eq!(
            filtered.data[1].item["content"],
            "turn projection assistant"
        );
    });
}

#[test]
fn session_store_projects_mcp_tool_calls_as_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "mcp projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "call mcp search".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "mcp-call-1".to_string(),
                        function_name: "mcp__local__search".to_string(),
                        arguments: r#"{"query":"orca"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "mcp-call-1".to_string(),
                    content: r#"{"content":[{"type":"text","text":"found"}],"structuredContent":{"count":1},"_meta":{"source":"test"}}"#.to_string(),
                    pinned: false,
                },
            ])
            .expect("append mcp projection items");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let mcp_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "mcp-call-1")
            .expect("projected mcp item");
        assert_eq!(mcp_item.item["type"], "mcpToolCall");
        assert_eq!(mcp_item.item["server"], "local");
        assert_eq!(mcp_item.item["tool"], "search");
        assert_eq!(mcp_item.item["status"], "completed");
        assert_eq!(mcp_item.item["arguments"]["query"], "orca");
        assert_eq!(mcp_item.item["result"]["content"][0]["text"], "found");
        assert_eq!(mcp_item.item["result"]["structuredContent"]["count"], 1);
        assert_eq!(mcp_item.item["result"]["_meta"]["source"], "test");
        assert!(mcp_item.item["error"].is_null());
    });
}

#[test]
fn session_store_preserves_failed_mcp_tool_metadata_in_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "failed mcp projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "search failed".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "mcp-call-1".to_string(),
                        function_name: "mcp__local__search".to_string(),
                        arguments: r#"{"query":"orca"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "mcp-call-1".to_string(),
                    content:
                        r#"{"status":"failed","error":"MCP request timed out","exit_code":124}"#
                            .to_string(),
                    pinned: false,
                },
            ])
            .expect("append failed mcp projection items");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let mcp_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "mcp-call-1")
            .expect("projected mcp item");
        assert_eq!(mcp_item.item["type"], "mcpToolCall");
        assert_eq!(mcp_item.item["status"], "failed");
        assert!(mcp_item.item["result"].is_null());
        assert_eq!(mcp_item.item["error"]["message"], "MCP request timed out");
        assert_eq!(mcp_item.item["error"]["exitCode"], 124);
    });
}

#[test]
fn session_store_projects_error_prefixed_mcp_tool_content_as_failed_item() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "error prefixed mcp thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "slow mcp".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "mcp-call-error".to_string(),
                        function_name: "mcp__slow__wait".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "mcp-call-error".to_string(),
                    content: "ERROR: MCP request 'tools/call' timed out after 100ms".to_string(),
                    pinned: false,
                },
            ])
            .expect("append error-prefixed mcp projection items");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let mcp_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "mcp-call-error")
            .expect("projected mcp item");
        assert_eq!(mcp_item.item["type"], "mcpToolCall");
        assert_eq!(mcp_item.item["status"], "failed");
        assert!(mcp_item.item["result"].is_null());
        assert_eq!(
            mcp_item.item["error"]["message"],
            "MCP request 'tools/call' timed out after 100ms"
        );
    });
}

#[test]
fn session_store_projects_external_tool_calls_as_dynamic_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "external projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "deploy staging".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "external-call-1".to_string(),
                        function_name: "deploy".to_string(),
                        arguments: r#"{"env":"staging"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "external-call-1".to_string(),
                    content: "deployed staging".to_string(),
                    pinned: false,
                },
            ])
            .expect("append external projection items");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let external_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "external-call-1")
            .expect("projected external item");
        assert_eq!(external_item.item["type"], "dynamicToolCall");
        assert!(external_item.item["namespace"].is_null());
        assert_eq!(external_item.item["tool"], "deploy");
        assert_eq!(external_item.item["status"], "completed");
        assert_eq!(external_item.item["arguments"]["env"], "staging");
        assert_eq!(external_item.item["success"], true);
        assert_eq!(external_item.item["contentItems"][0]["type"], "text");
        assert_eq!(
            external_item.item["contentItems"][0]["text"],
            "deployed staging"
        );
        assert!(external_item.item["error"].is_null());
    });
}

#[test]
fn session_store_preserves_failed_external_tool_metadata_in_dynamic_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "failed external projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "deploy staging".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "external-call-1".to_string(),
                        function_name: "deploy".to_string(),
                        arguments: r#"{"env":"staging"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "external-call-1".to_string(),
                    content: r#"{"status":"failed","error":"deploy failed","exit_code":42}"#
                        .to_string(),
                    pinned: false,
                },
            ])
            .expect("append failed external projection items");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let external_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "external-call-1")
            .expect("projected external item");
        assert_eq!(external_item.item["type"], "dynamicToolCall");
        assert_eq!(external_item.item["status"], "failed");
        assert_eq!(external_item.item["success"], false);
        assert_eq!(external_item.item["error"]["message"], "deploy failed");
        assert_eq!(external_item.item["error"]["exitCode"], 42);
        assert!(external_item.item["contentItems"].is_null());
    });
}

#[test]
fn session_store_preserves_denied_external_tool_metadata_in_dynamic_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "denied external projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "deploy production".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "external-denied-1".to_string(),
                        function_name: "deploy".to_string(),
                        arguments: r#"{"env":"production"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append denied external projection items");

        let request = ToolRequest {
            id: "external-denied-1".to_string(),
            name: ToolName::External("deploy".to_string()),
            action: ActionKind::Write,
            target: Some("production".to_string()),
            raw_arguments: Some(r#"{"env":"production"}"#.to_string()),
        };
        let result = ToolResult::denied(&request, "policy denied deploy");
        thread
            .writer_mut()
            .append_tool_result_message(&result, String::new(), false)
            .expect("append denied external tool result");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let external_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "external-denied-1")
            .expect("projected external item");
        assert_eq!(external_item.item["type"], "dynamicToolCall");
        assert_eq!(external_item.item["status"], "denied");
        assert_eq!(external_item.item["success"], false);
        assert_eq!(
            external_item.item["error"]["message"],
            "policy denied deploy"
        );
        assert!(external_item.item["contentItems"].is_null());
    });
}

#[test]
fn session_store_preserves_truncated_tool_metadata_in_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "truncated projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "run verbose command".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "bash-call-1".to_string(),
                        function_name: "bash".to_string(),
                        arguments: r#"{"command":"printf lots"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append tool call");

        let request = ToolRequest {
            id: "bash-call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf lots".to_string()),
            raw_arguments: Some(r#"{"command":"printf lots"}"#.to_string()),
        };
        let result = ToolResult::completed(&request, "truncated visible output".to_string(), true);
        thread
            .writer_mut()
            .append_tool_result_message(&result, "truncated visible output".to_string(), false)
            .expect("append truncated tool result");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let tool_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "bash-call-1")
            .expect("projected tool item");
        assert_eq!(tool_item.item["type"], "commandExecution");
        assert_eq!(tool_item.item["status"], "completed");
        assert_eq!(
            tool_item.item["aggregatedOutput"],
            "truncated visible output"
        );
        assert!(tool_item.item.get("result").is_none());
        assert_eq!(tool_item.item["truncated"], true);
    });
}

#[test]
fn session_store_projects_builtin_read_tool_as_dynamic_thread_item() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "read projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "read readme".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "read-call-1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: r#"{"path":"README.md"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "read-call-1".to_string(),
                    content: "readme contents".to_string(),
                    pinned: false,
                },
            ])
            .expect("append read projection items");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let read_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "read-call-1")
            .expect("projected read item");
        assert_eq!(read_item.item["type"], "dynamicToolCall");
        assert_eq!(read_item.item["tool"], "read_file");
        assert_eq!(read_item.item["status"], "completed");
        assert_eq!(read_item.item["arguments"]["path"], "README.md");
        assert_eq!(read_item.item["success"], true);
        assert_eq!(read_item.item["contentItems"][0]["type"], "text");
        assert_eq!(read_item.item["contentItems"][0]["text"], "readme contents");
        assert!(read_item.item["error"].is_null());
    });
}

#[test]
fn session_store_projects_multiple_user_turns_with_stable_item_ids() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "multi turn projection")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "first user".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("first assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
                Message::User {
                    content: "second user".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("second assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append multi-turn items");
        thread.complete("success").expect("complete thread");

        let turns = store
            .list_thread_turns(
                &thread_id,
                None,
                10,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .expect("list thread turns");
        assert_eq!(turns.data.len(), 2);
        assert_eq!(turns.data[0].turn_id, "turn-1");
        assert_eq!(turns.data[0].items.len(), 2);
        assert_eq!(turns.data[1].turn_id, "turn-2");
        assert_eq!(turns.data[1].items.len(), 2);
        assert_eq!(turns.data[1].items[0]["content"], "second user");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list all items");
        assert_eq!(
            items
                .data
                .iter()
                .map(|item| (item.item_id.as_str(), item.turn_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("item-1", "turn-1"),
                ("item-2", "turn-1"),
                ("item-3", "turn-2"),
                ("item-4", "turn-2"),
            ]
        );

        let second_turn_items = store
            .list_thread_items(&thread_id, Some("turn-2"), None, 10, SortDirection::Asc)
            .expect("list second turn items");
        assert_eq!(second_turn_items.data.len(), 2);
        assert_eq!(second_turn_items.data[0].item_id, "item-3");
        assert_eq!(second_turn_items.data[0].item["content"], "second user");
        assert_eq!(second_turn_items.data[1].item_id, "item-4");
        assert_eq!(
            second_turn_items.data[1].item["content"],
            "second assistant"
        );
    });
}

#[test]
fn session_store_paginates_thread_turns_and_items_with_cursors() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "paginated projection")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread
            .append_items(&[
                Message::User {
                    content: "first user".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("first assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
                Message::User {
                    content: "second user".to_string(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("second assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append paginated items");
        thread.complete("success").expect("complete thread");

        let first_turn_page = store
            .list_thread_turns(&thread_id, None, 1, SortDirection::Asc, TurnItemsView::Full)
            .expect("first turn page");
        assert_eq!(first_turn_page.data.len(), 1);
        assert_eq!(first_turn_page.data[0].turn_id, "turn-1");
        assert_eq!(first_turn_page.next_cursor.as_deref(), Some("1"));
        assert_eq!(first_turn_page.backwards_cursor.as_deref(), Some("0"));

        let second_turn_page = store
            .list_thread_turns(
                &thread_id,
                first_turn_page.next_cursor.as_deref(),
                1,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .expect("second turn page");
        assert_eq!(second_turn_page.data.len(), 1);
        assert_eq!(second_turn_page.data[0].turn_id, "turn-2");
        assert_eq!(second_turn_page.next_cursor, None);
        assert_eq!(second_turn_page.backwards_cursor.as_deref(), Some("1"));

        let first_item_page = store
            .list_thread_items(&thread_id, None, None, 2, SortDirection::Asc)
            .expect("first item page");
        assert_eq!(first_item_page.data.len(), 2);
        assert_eq!(first_item_page.data[0].item_id, "item-1");
        assert_eq!(first_item_page.next_cursor.as_deref(), Some("2"));

        let second_item_page = store
            .list_thread_items(
                &thread_id,
                None,
                first_item_page.next_cursor.as_deref(),
                2,
                SortDirection::Asc,
            )
            .expect("second item page");
        assert_eq!(second_item_page.data.len(), 2);
        assert_eq!(second_item_page.data[0].item_id, "item-3");
        assert_eq!(second_item_page.next_cursor, None);
        assert_eq!(second_item_page.backwards_cursor.as_deref(), Some("2"));

        let latest_turn_page = store
            .list_thread_turns(
                &thread_id,
                None,
                1,
                SortDirection::Desc,
                TurnItemsView::Full,
            )
            .expect("latest turn page");
        assert_eq!(latest_turn_page.data.len(), 1);
        assert_eq!(latest_turn_page.data[0].turn_id, "turn-2");
        assert_eq!(latest_turn_page.next_cursor.as_deref(), Some("1"));

        let unloaded_turn_page = store
            .list_thread_turns(
                &thread_id,
                None,
                10,
                SortDirection::Asc,
                TurnItemsView::NotLoaded,
            )
            .expect("unloaded turn page");
        assert_eq!(unloaded_turn_page.data.len(), 2);
        assert_eq!(
            unloaded_turn_page.data[0].items_view,
            TurnItemsView::NotLoaded
        );
        assert!(unloaded_turn_page.data[0].items.is_empty());

        let latest_item_page = store
            .list_thread_items(&thread_id, None, None, 1, SortDirection::Desc)
            .expect("latest item page");
        assert_eq!(latest_item_page.data.len(), 1);
        assert_eq!(latest_item_page.data[0].item_id, "item-4");
        assert_eq!(latest_item_page.data[0].item["content"], "second assistant");
    });
}

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let home = tempdir().expect("temp home");
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
    }
    let result = f(home.path());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    result
}
