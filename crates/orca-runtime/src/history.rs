use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::thread_store::{
    SessionRecord, StoredMessage, archive_dir, collect_session_files, find_session_path,
    is_latest_selector, load_thread_records, lock_file, read_history_lines, read_records,
    read_session_meta, read_transcript, resolve_session_path, rewrite_records, sessions_dir,
    unlock_file,
};
use orca_core::config::{ActivePermissionProfile, AdditionalWorkingDirectory};
use orca_core::conversation::{
    Conversation, Message, RawToolCall, SummaryState, normalize_tool_boundaries,
};
use orca_core::tool_types::ToolStatus;
use orca_core::{approval_rules::PermissionRules, approval_types::ApprovalMode};

pub use crate::thread_store::{
    JsonlThreadStore, LiveThread, SessionMeta, SessionStore, SessionSummary, SessionTranscript,
    SessionWriter, SortDirection, StoredThreadItem, StoredThreadItemPage, StoredThreadProjection,
    StoredThreadSearchHit, StoredThreadSearchPage, StoredThreadSummary, StoredThreadSummaryPage,
    StoredThreadTurn, StoredThreadTurnPage, ThreadListFilters, ThreadMetadataPatch,
    ThreadRelationFilter, ThreadSortKey, ThreadStore, TurnItemsView,
};

const SESSION_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactionRecord {
    pub collapsed_at: DateTime<Utc>,
    pub before_messages: usize,
    pub after_messages: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContextSummaryRecord {
    pub summarized_at: DateTime<Utc>,
    pub before_messages: usize,
    pub after_messages: usize,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_state: Option<SummaryState>,
}

impl JsonlThreadStore {
    pub fn new() -> Self {
        Self
    }

    pub fn list_sessions(&self, limit: usize) -> io::Result<Vec<SessionSummary>> {
        list_sessions(limit)
    }

    pub fn list_sessions_with_archived(
        &self,
        limit: usize,
        include_archived: bool,
    ) -> io::Result<Vec<SessionSummary>> {
        list_sessions_with_archived(limit, include_archived)
    }

    pub fn load_session(&self, selector: &str) -> io::Result<SessionTranscript> {
        load_session(selector)
    }

    pub fn delete_session(&self, selector: &str) -> io::Result<PathBuf> {
        delete_session(selector)
    }

    pub fn archive_session(&self, selector: &str) -> io::Result<PathBuf> {
        archive_session(selector)
    }

    pub fn rename_session(&self, selector: &str, title: &str) -> io::Result<PathBuf> {
        rename_session(selector, title)
    }

    pub fn compress_session(&self, selector: &str) -> io::Result<PathBuf> {
        compress_session(selector)
    }

    pub fn search_sessions(
        &self,
        query: &str,
        include_archived: bool,
    ) -> io::Result<Vec<SearchHit>> {
        search_sessions(query, include_archived)
    }

    pub fn create_meta(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> SessionMeta {
        create_meta(cwd, provider, model, prompt)
    }

    pub fn create_meta_with_permissions(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_mode: ApprovalMode,
        permission_rules: PermissionRules,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
    ) -> SessionMeta {
        let mut meta = create_meta(cwd, provider, model, prompt);
        meta.active_permission_profile = active_permission_profile;
        meta.approval_mode = Some(approval_mode);
        meta.runtime_workspace_roots = vec![cwd.to_path_buf()];
        meta.permission_rules = permission_rules;
        meta.additional_working_directories = additional_working_directories;
        meta
    }

    pub fn create_fork_meta(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        parent_id: String,
    ) -> SessionMeta {
        create_fork_meta(cwd, provider, model, prompt, parent_id)
    }

    pub fn start_writer(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<SessionWriter> {
        SessionWriter::start(cwd, provider, model, prompt)
    }

    pub fn start_writer_from_meta(&self, meta: SessionMeta) -> io::Result<SessionWriter> {
        SessionWriter::start_from_meta(meta)
    }

    pub fn resume_conversation(
        &self,
        transcript: &SessionTranscript,
        system_prompt: String,
    ) -> Conversation {
        resume_conversation(transcript, system_prompt)
    }
}

impl ThreadStore for JsonlThreadStore {
    fn create_live_thread(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<LiveThread> {
        let meta = self.create_meta(cwd, provider, model, prompt);
        let thread_id = meta.session_id.clone();
        let writer = self.start_writer_from_meta(meta)?;
        Ok(LiveThread { thread_id, writer })
    }

    fn create_live_thread_with_permissions(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_mode: ApprovalMode,
        permission_rules: PermissionRules,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
    ) -> io::Result<LiveThread> {
        let meta = self.create_meta_with_permissions(
            cwd,
            provider,
            model,
            prompt,
            active_permission_profile,
            approval_mode,
            permission_rules,
            additional_working_directories,
        );
        let thread_id = meta.session_id.clone();
        let writer = self.start_writer_from_meta(meta)?;
        Ok(LiveThread { thread_id, writer })
    }

    fn update_thread_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<SessionSummary> {
        let path = find_session_path(thread_id, true)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no saved session matches '{thread_id}'"),
            )
        })?;
        let mut records = read_records(&path)?;
        let mut patched = false;
        for record in &mut records {
            if let SessionRecord::Meta(meta) = record {
                if let Some(title) = patch.title {
                    meta.title = title;
                    patched = true;
                }
                if let Some(approval_mode) = patch.approval_mode {
                    meta.approval_mode = Some(approval_mode);
                    patched = true;
                }
                if let Some(active_permission_profile) = patch.active_permission_profile {
                    meta.active_permission_profile = Some(active_permission_profile);
                    patched = true;
                }
                if let Some(runtime_workspace_roots) = patch.runtime_workspace_roots {
                    meta.runtime_workspace_roots = runtime_workspace_roots;
                    patched = true;
                }
                if let Some(permission_rules) = patch.permission_rules {
                    meta.permission_rules = permission_rules;
                    patched = true;
                }
                if let Some(additional_working_directories) = patch.additional_working_directories {
                    meta.additional_working_directories = additional_working_directories;
                    patched = true;
                }
                break;
            }
        }
        if !patched {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread metadata patch did not include any supported fields",
            ));
        }
        rewrite_records(&path, &records)?;
        summarize_session_with_archive_flag(&path, path.starts_with(archive_dir()))
    }

    fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection> {
        let (meta, stored_messages) = load_thread_records(thread_id)?;
        let projected_messages = if include_messages {
            stored_messages
                .iter()
                .map(stored_message_to_thread_json)
                .collect()
        } else {
            Vec::new()
        };
        let turns = if include_turns {
            stored_messages_to_thread_turns(
                &meta.session_id,
                &stored_messages,
                usize::MAX,
                TurnItemsView::Full,
            )
        } else {
            Vec::new()
        };
        Ok(StoredThreadProjection {
            thread_id: meta.session_id,
            title: meta.title,
            cwd: meta.cwd,
            runtime_workspace_roots: meta.runtime_workspace_roots,
            active_permission_profile: meta.active_permission_profile,
            additional_working_directories: meta.additional_working_directories,
            message_count: stored_messages.len(),
            messages: projected_messages,
            turns,
        })
    }

    fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage> {
        let mut summaries = self
            .list_sessions_with_archived(usize::MAX, filters.archived)?
            .into_iter()
            .map(StoredThreadSummary::from)
            .collect::<Vec<_>>();
        let all_summaries = summaries.clone();
        summaries
            .retain(|summary| thread_summary_matches_filters(summary, &filters, &all_summaries));
        if let Some(search_term) = search_term.filter(|term| !term.is_empty()) {
            summaries.retain(|summary| thread_summary_matches(summary, search_term));
        }
        sort_thread_summaries(&mut summaries, sort_key);
        if sort_direction == SortDirection::Asc {
            summaries.reverse();
        }
        let (data, next_cursor, backwards_cursor) = page_vec(summaries, cursor, limit);
        Ok(StoredThreadSummaryPage {
            data,
            next_cursor,
            backwards_cursor,
        })
    }

    fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage> {
        let mut hits = self
            .search_sessions(query, include_archived)?
            .into_iter()
            .map(|hit| {
                let archived = hit.archived;
                let snippet = hit.line.clone();
                summarize_session_with_archive_flag(&hit.path, archived).map(|summary| {
                    StoredThreadSearchHit {
                        thread: StoredThreadSummary::from(summary),
                        snippet,
                    }
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        sort_thread_search_hits(&mut hits, sort_key);
        if sort_direction == SortDirection::Asc {
            hits.reverse();
        }
        let (data, next_cursor, backwards_cursor) = page_vec(hits, cursor, limit);
        Ok(StoredThreadSearchPage {
            data,
            next_cursor,
            backwards_cursor,
        })
    }

    fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage> {
        let (meta, messages) = load_thread_records(thread_id)?;
        Ok(page_thread_turns(
            stored_messages_to_thread_turns(&meta.session_id, &messages, usize::MAX, items_view),
            cursor,
            limit,
            sort_direction,
        ))
    }

    fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage> {
        let (meta, messages) = load_thread_records(thread_id)?;
        Ok(page_thread_items(
            stored_messages_to_thread_items(&meta.session_id, &messages, turn_id, usize::MAX),
            cursor,
            limit,
            sort_direction,
        ))
    }
}

fn thread_summary_matches(summary: &StoredThreadSummary, search_term: &str) -> bool {
    summary.title.contains(search_term)
        || summary.cwd.contains(search_term)
        || summary.provider.contains(search_term)
        || summary
            .model
            .as_deref()
            .is_some_and(|model| model.contains(search_term))
}

fn thread_summary_matches_filters(
    summary: &StoredThreadSummary,
    filters: &ThreadListFilters,
    all_summaries: &[StoredThreadSummary],
) -> bool {
    if summary.archived != filters.archived {
        return false;
    }
    if let Some(model_providers) = &filters.model_providers {
        if !model_providers.is_empty()
            && !model_providers
                .iter()
                .any(|provider| provider == &summary.provider)
        {
            return false;
        }
    }
    if let Some(model_names) = &filters.model_names {
        if !model_names.is_empty()
            && !summary
                .model
                .as_ref()
                .is_some_and(|model| model_names.iter().any(|expected| expected == model))
        {
            return false;
        }
    }
    if !filters.cwd_filters.is_empty() && !filters.cwd_filters.iter().any(|cwd| cwd == &summary.cwd)
    {
        return false;
    }
    match &filters.relation {
        Some(ThreadRelationFilter::DirectChildrenOf(parent_id)) => {
            summary.parent_id.as_deref() == Some(parent_id.as_str())
        }
        Some(ThreadRelationFilter::DescendantsOf(ancestor_id)) => {
            thread_descends_from(summary, ancestor_id, all_summaries)
        }
        None => true,
    }
}

fn thread_descends_from(
    summary: &StoredThreadSummary,
    ancestor_id: &str,
    all_summaries: &[StoredThreadSummary],
) -> bool {
    let mut next_parent = summary.parent_id.as_deref();
    while let Some(parent_id) = next_parent {
        if parent_id == ancestor_id {
            return true;
        }
        next_parent = all_summaries
            .iter()
            .find(|candidate| candidate.thread_id == parent_id)
            .and_then(|candidate| candidate.parent_id.as_deref());
    }
    false
}

fn sort_thread_summaries(summaries: &mut [StoredThreadSummary], sort_key: ThreadSortKey) {
    summaries.sort_by(|a, b| match sort_key {
        ThreadSortKey::CreatedAt => b
            .created_at
            .cmp(&a.created_at)
            .then_with(|| b.updated_at.cmp(&a.updated_at)),
        ThreadSortKey::UpdatedAt | ThreadSortKey::RecencyAt => b
            .updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at)),
    });
}

fn sort_thread_search_hits(hits: &mut [StoredThreadSearchHit], sort_key: ThreadSortKey) {
    hits.sort_by(|a, b| match sort_key {
        ThreadSortKey::CreatedAt => b
            .thread
            .created_at
            .cmp(&a.thread.created_at)
            .then_with(|| b.thread.updated_at.cmp(&a.thread.updated_at)),
        ThreadSortKey::UpdatedAt | ThreadSortKey::RecencyAt => b
            .thread
            .updated_at
            .cmp(&a.thread.updated_at)
            .then_with(|| b.thread.created_at.cmp(&a.thread.created_at)),
    });
}

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

fn stored_message_to_thread_json(message: &StoredMessage) -> Value {
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

fn stored_messages_to_thread_turns(
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

fn stored_messages_to_thread_items(
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

pub(crate) fn page_thread_turns(
    mut turns: Vec<StoredThreadTurn>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
) -> StoredThreadTurnPage {
    if sort_direction == SortDirection::Desc {
        turns.reverse();
    }
    let (data, next_cursor, backwards_cursor) = page_vec(turns, cursor, limit);
    StoredThreadTurnPage {
        data,
        next_cursor,
        backwards_cursor,
    }
}

pub(crate) fn page_thread_items(
    mut items: Vec<StoredThreadItem>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
) -> StoredThreadItemPage {
    if sort_direction == SortDirection::Desc {
        items.reverse();
    }
    let (data, next_cursor, backwards_cursor) = page_vec(items, cursor, limit);
    StoredThreadItemPage {
        data,
        next_cursor,
        backwards_cursor,
    }
}

fn page_vec<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    limit: usize,
) -> (Vec<T>, Option<String>, Option<String>) {
    let start = cursor
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let page_size = limit.max(1);
    let end = start.saturating_add(page_size).min(items.len());
    let next_cursor = (end < items.len()).then(|| end.to_string());
    let backwards_cursor = (!items.is_empty()).then(|| start.to_string());
    let data = items.into_iter().skip(start).take(end - start).collect();
    (data, next_cursor, backwards_cursor)
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
                .find(|candidate| candidate["id"].as_str() == Some(tool_call_id))
        {
            complete_tool_item(existing, &item);
            continue;
        }
        turn_items.push(item);
    }
}

fn tool_call_to_thread_item(tool_call: &RawToolCall) -> Value {
    if let Some((server, tool)) = mcp_tool_parts(&tool_call.function_name) {
        json!({
            "id": tool_call.id,
            "type": "mcpToolCall",
            "server": server,
            "tool": tool,
            "status": "in_progress",
            "arguments": parse_json_or_null(&tool_call.arguments),
            "result": Value::Null,
            "error": Value::Null,
        })
    } else {
        if tool_call.function_name == "bash" {
            return command_execution_thread_item(tool_call);
        }
        json!({
            "id": tool_call.id,
            "type": "dynamicToolCall",
            "namespace": Value::Null,
            "tool": tool_call.function_name,
            "status": "in_progress",
            "arguments": parse_json_or_null(&tool_call.arguments),
            "contentItems": Value::Null,
            "success": Value::Null,
            "error": Value::Null,
        })
    }
}

fn command_execution_thread_item(tool_call: &RawToolCall) -> Value {
    json!({
        "id": tool_call.id,
        "type": "commandExecution",
        "tool": tool_call.function_name,
        "command": command_from_tool_arguments(&tool_call.arguments),
        "cwd": Value::Null,
        "processId": Value::Null,
        "source": Value::Null,
        "status": "in_progress",
        "commandActions": [],
        "aggregatedOutput": Value::Null,
        "error": Value::Null,
        "exitCode": Value::Null,
        "durationMs": Value::Null,
    })
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
        item["status"] = Value::from(status);
        copy_truncated_metadata(item, result);
        if item["type"] == "mcpToolCall" {
            item["result"] = Value::Null;
        } else if item["type"] == "dynamicToolCall" {
            item["contentItems"] = Value::Null;
            item["success"] = Value::from(false);
        } else {
            item["result"] = Value::Null;
        }
        item["error"] = failure;
        return;
    }

    item["status"] = Value::from("completed");
    copy_truncated_metadata(item, result);
    if item["type"] == "mcpToolCall" {
        item["result"] = mcp_result_from_content(content);
        item["error"] = Value::Null;
    } else if item["type"] == "dynamicToolCall" {
        item["contentItems"] = json!([{
            "type": "text",
            "text": content,
        }]);
        item["success"] = Value::from(true);
        item["error"] = Value::Null;
    } else if item["type"] == "commandExecution" {
        item["aggregatedOutput"] = Value::from(content.to_string());
        item["error"] = Value::Null;
    } else {
        item["result"] = Value::from(content.to_string());
        item["error"] = Value::Null;
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
    let mut error = json!({ "message": message });
    if let Some(exit_code) = result["exitCode"].as_i64() {
        error["exitCode"] = Value::from(exit_code);
    }
    Some((status, error))
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
    let mut error = json!({ "message": message });
    if let Some(exit_code) = value.get("exit_code").and_then(Value::as_i64) {
        error["exitCode"] = Value::from(exit_code);
    } else if let Some(exit_code) = value.get("exitCode").and_then(Value::as_i64) {
        error["exitCode"] = Value::from(exit_code);
    }
    Some(error)
}

fn mcp_tool_parts(tool: &str) -> Option<(String, String)> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, local_tool) = rest.rsplit_once("__")?;
    Some((server.to_string(), local_tool.to_string()))
}

fn parse_json_or_null(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

fn command_from_tool_arguments(raw: &str) -> Value {
    parse_json_or_null(raw)
        .get("command")
        .and_then(Value::as_str)
        .map(|command| Value::from(command.to_string()))
        .unwrap_or(Value::Null)
}

fn mcp_result_from_content(content: &str) -> Value {
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

pub fn list_sessions(limit: usize) -> io::Result<Vec<SessionSummary>> {
    list_sessions_with_archived(limit, false)
}

pub fn list_sessions_with_archived(
    limit: usize,
    include_archived: bool,
) -> io::Result<Vec<SessionSummary>> {
    let mut summaries = Vec::new();
    collect_summaries_from_root(&sessions_dir(), false, &mut summaries)?;
    if include_archived {
        collect_summaries_from_root(&archive_dir(), true, &mut summaries)?;
    }

    summaries.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    summaries.truncate(limit);
    Ok(summaries)
}

pub fn load_session(selector: &str) -> io::Result<SessionTranscript> {
    let path = if is_latest_selector(selector) {
        list_sessions(1)?
            .into_iter()
            .next()
            .map(|s| s.path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no saved sessions"))?
    } else {
        find_session_path(selector, true)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no saved session matches '{selector}'"),
            )
        })?
    };

    read_transcript(&path)
}

pub fn delete_session(selector: &str) -> io::Result<PathBuf> {
    let path = if is_latest_selector(selector) {
        list_sessions_with_archived(1, true)?
            .into_iter()
            .next()
            .map(|session| session.path)
    } else {
        find_session_path(selector, true)?
    }
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{selector}'"),
        )
    })?;
    fs::remove_file(&path)?;
    Ok(path)
}

pub fn archive_session(selector: &str) -> io::Result<PathBuf> {
    let path = if is_latest_selector(selector) {
        list_sessions(1)?
            .into_iter()
            .next()
            .map(|session| session.path)
    } else {
        find_session_path(selector, false)?
    }
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no active session matches '{selector}'"),
        )
    })?;
    let relative = path.strip_prefix(sessions_dir()).unwrap_or(&path);
    let archived_path = archive_dir().join(relative);
    if let Some(parent) = archived_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&path, &archived_path)?;
    Ok(archived_path)
}

pub fn rename_session(selector: &str, title: &str) -> io::Result<PathBuf> {
    let path = resolve_session_path(selector, true)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{selector}'"),
        )
    })?;
    let meta = read_session_meta(&path)?;
    SessionStore::new().update_thread_metadata(
        &meta.session_id,
        ThreadMetadataPatch {
            title: Some(title.to_string()),
            ..ThreadMetadataPatch::default()
        },
    )?;
    Ok(path)
}

pub fn resume_conversation(transcript: &SessionTranscript, system_prompt: String) -> Conversation {
    let mut conversation = Conversation::new();
    conversation.add_system(system_prompt);
    let mut restored_messages = replay_compactions_for_resume(
        &transcript.messages,
        &transcript.compactions,
        &transcript.summaries,
    )
    .into_iter()
    .filter(|message| !matches!(message, Message::System { .. }))
    .collect::<Vec<_>>();
    normalize_tool_boundaries(&mut restored_messages);
    for message in restored_messages.iter() {
        conversation.messages.push(message.clone());
    }
    if let Some(summary_state) = transcript
        .summaries
        .iter()
        .rev()
        .find_map(|record| record.summary_state.clone())
    {
        conversation.summary = summary_state;
        conversation.rolling_summary = transcript
            .summaries
            .last()
            .map(|record| record.summary.clone());
    } else if let Some(first_summary) = transcript.summaries.first() {
        conversation.summary.baseline = Some(first_summary.summary.clone());
        conversation.summary.deltas = transcript
            .summaries
            .iter()
            .skip(1)
            .map(|record| record.summary.clone())
            .collect();
        conversation.rolling_summary = transcript
            .summaries
            .last()
            .map(|record| record.summary.clone());
    }
    conversation
}

fn replay_compactions_for_resume(
    messages: &[Message],
    compactions: &[CompactionRecord],
    summaries: &[ContextSummaryRecord],
) -> Vec<Message> {
    let summarized_compactions: HashSet<(usize, usize)> = summaries
        .iter()
        .map(|record| (record.before_messages, record.after_messages))
        .collect();
    let mut restored = messages.to_vec();
    for compaction in compactions {
        let has_remote_summary = summarized_compactions
            .contains(&(compaction.before_messages, compaction.after_messages));
        restored = replay_compaction_for_resume(restored, compaction, has_remote_summary);
    }
    restored
}

fn replay_compaction_for_resume(
    messages: Vec<Message>,
    compaction: &CompactionRecord,
    has_remote_summary: bool,
) -> Vec<Message> {
    if compaction.before_messages == 0
        || compaction.after_messages >= compaction.before_messages
        || messages.len() < compaction.before_messages
    {
        return messages;
    }

    let prefix = &messages[..compaction.before_messages];
    let suffix = &messages[compaction.before_messages..];
    let system = prefix
        .iter()
        .find(|message| matches!(message, Message::System { .. }))
        .cloned();
    let pinned: Vec<Message> = prefix
        .iter()
        .filter(|message| !matches!(message, Message::System { .. }) && message.is_pinned())
        .cloned()
        .collect();

    let structural_messages = usize::from(system.is_some()) + usize::from(!has_remote_summary);
    let retained_non_system = compaction
        .after_messages
        .saturating_sub(structural_messages);
    let retained_tail = retained_non_system.saturating_sub(pinned.len());
    let mut tail: Vec<Message> = prefix
        .iter()
        .filter(|message| !matches!(message, Message::System { .. }) && !message.is_pinned())
        .rev()
        .take(retained_tail)
        .cloned()
        .collect();
    tail.reverse();

    let mut replayed = Vec::with_capacity(
        usize::from(system.is_some()) + pinned.len() + tail.len() + suffix.len(),
    );
    if let Some(system) = system {
        replayed.push(system);
    }
    replayed.extend(pinned);
    replayed.extend(tail);
    replayed.extend_from_slice(suffix);
    replayed
}

pub fn create_meta(cwd: &Path, provider: &str, model: Option<String>, prompt: &str) -> SessionMeta {
    let now = Utc::now();
    SessionMeta {
        schema_version: SESSION_SCHEMA_VERSION,
        session_id: Uuid::new_v4().to_string(),
        cwd: cwd.display().to_string(),
        provider: provider.to_string(),
        model,
        title: title_from_prompt(prompt),
        created_at: now,
        parent_id: None,
        forked: false,
        approval_mode: None,
        active_permission_profile: None,
        runtime_workspace_roots: vec![cwd.to_path_buf()],
        permission_rules: PermissionRules::default(),
        additional_working_directories: Vec::new(),
    }
}

pub fn create_fork_meta(
    cwd: &Path,
    provider: &str,
    model: Option<String>,
    prompt: &str,
    parent_id: String,
) -> SessionMeta {
    let mut meta = create_meta(cwd, provider, model, prompt);
    meta.parent_id = Some(parent_id);
    meta.forked = true;
    meta
}

fn summarize_session_with_archive_flag(path: &Path, archived: bool) -> io::Result<SessionSummary> {
    let meta = read_session_meta(path)?;
    let updated_at = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or(meta.created_at);

    Ok(SessionSummary {
        session_id: meta.session_id,
        title: meta.title,
        cwd: meta.cwd,
        provider: meta.provider,
        model: meta.model,
        created_at: meta.created_at,
        updated_at,
        path: path.to_path_buf(),
        archived,
        parent_id: meta.parent_id,
        forked: meta.forked,
        approval_mode: meta.approval_mode,
        active_permission_profile: meta.active_permission_profile,
        permission_rule_count: meta.permission_rules.rules.len(),
        runtime_workspace_roots: meta.runtime_workspace_roots,
        additional_working_directories: meta.additional_working_directories,
    })
}

fn collect_summaries_from_root(
    root: &Path,
    archived: bool,
    summaries: &mut Vec<SessionSummary>,
) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        if let Ok(summary) = summarize_session_with_archive_flag(path, archived) {
            summaries.push(summary);
        }
    })
}

pub fn compress_session(selector: &str) -> io::Result<PathBuf> {
    let path = resolve_session_path(selector, true)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{selector}'"),
        )
    })?;
    if path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        return Ok(path);
    }
    let compressed_path = path.with_extension("jsonl.zst");
    let lock = OpenOptions::new().read(true).write(true).open(&path)?;
    lock_file(&lock)?;
    let result = (|| {
        let input = File::open(&path)?;
        let output = File::create(&compressed_path)?;
        if let Err(error) = zstd::stream::copy_encode(input, output, 3) {
            let _ = fs::remove_file(&compressed_path);
            return Err(io::Error::other(error));
        }
        fs::remove_file(&path)?;
        Ok(compressed_path)
    })();
    let unlock_result = unlock_file(&lock);
    match (result, unlock_result) {
        (Ok(path), Ok(())) => Ok(path),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

pub fn search_sessions(query: &str, include_archived: bool) -> io::Result<Vec<SearchHit>> {
    let mut hits = Vec::new();
    let used_ripgrep = search_roots_with_ripgrep(query, include_archived, &mut hits)?;
    let mut seen: HashSet<(PathBuf, usize)> = hits
        .iter()
        .map(|hit| (hit.path.clone(), hit.line_number))
        .collect();

    if !used_ripgrep {
        search_root_in_process(&sessions_dir(), false, query, &mut hits, &mut seen)?;
    } else {
        search_compressed_root(&sessions_dir(), false, query, &mut hits, &mut seen)?;
    }
    if include_archived {
        if !used_ripgrep {
            search_root_in_process(&archive_dir(), true, query, &mut hits, &mut seen)?;
        } else {
            search_compressed_root(&archive_dir(), true, query, &mut hits, &mut seen)?;
        }
    }
    Ok(hits)
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub archived: bool,
    pub path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

fn search_roots_with_ripgrep(
    query: &str,
    include_archived: bool,
    hits: &mut Vec<SearchHit>,
) -> io::Result<bool> {
    let mut roots = Vec::new();
    if sessions_dir().exists() {
        roots.push(sessions_dir());
    }
    if include_archived && archive_dir().exists() {
        roots.push(archive_dir());
    }
    if roots.is_empty() {
        return Ok(true);
    }

    let output = match Command::new("rg")
        .arg("--json")
        .arg("--fixed-strings")
        .arg("--glob")
        .arg("*.jsonl")
        .arg(query)
        .args(&roots)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };

    if !output.status.success() && output.status.code() != Some(1) {
        return Ok(false);
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["type"].as_str() != Some("match") {
            continue;
        }
        let Some(path_text) = value["data"]["path"]["text"].as_str() else {
            continue;
        };
        let Some(line_number) = value["data"]["line_number"].as_u64() else {
            continue;
        };
        let Some(line_text) = value["data"]["lines"]["text"].as_str() else {
            continue;
        };
        let path = PathBuf::from(path_text);
        let archived = path.starts_with(archive_dir());
        push_search_hit(
            &path,
            archived,
            line_number as usize,
            line_text.trim_end_matches('\n').to_string(),
            hits,
        );
    }

    Ok(true)
}

fn search_root_in_process(
    root: &Path,
    archived: bool,
    query: &str,
    hits: &mut Vec<SearchHit>,
    seen: &mut HashSet<(PathBuf, usize)>,
) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        if let Ok(lines) = read_history_lines(path) {
            push_matching_lines(path, archived, query, &lines, hits, seen);
        }
    })
}

fn search_compressed_root(
    root: &Path,
    archived: bool,
    query: &str,
    hits: &mut Vec<SearchHit>,
    seen: &mut HashSet<(PathBuf, usize)>,
) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        if path.extension().and_then(|ext| ext.to_str()) != Some("zst") {
            return;
        }
        if let Ok(lines) = read_history_lines(path) {
            push_matching_lines(path, archived, query, &lines, hits, seen);
        }
    })
}

fn push_matching_lines(
    path: &Path,
    archived: bool,
    query: &str,
    lines: &[String],
    hits: &mut Vec<SearchHit>,
    seen: &mut HashSet<(PathBuf, usize)>,
) {
    for (index, line) in lines.iter().enumerate() {
        let line_number = index + 1;
        if line.contains(query) && seen.insert((path.to_path_buf(), line_number)) {
            push_search_hit(path, archived, line_number, line.clone(), hits);
        }
    }
}

fn push_search_hit(
    path: &Path,
    archived: bool,
    line_number: usize,
    line: String,
    hits: &mut Vec<SearchHit>,
) {
    if let Ok(meta) = read_session_meta(path) {
        hits.push(SearchHit {
            session_id: meta.session_id,
            title: meta.title,
            archived,
            path: path.to_path_buf(),
            line_number,
            line,
        });
    }
}

pub(crate) fn session_path(session_id: &str, timestamp: DateTime<Utc>) -> io::Result<PathBuf> {
    let dir = sessions_dir()
        .join(format!("{:04}", timestamp.year()))
        .join(format!("{:02}", timestamp.month()))
        .join(format!("{:02}", timestamp.day()));
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!(
        "session-{}-{}.jsonl",
        timestamp.format("%Y-%m-%dT%H-%M-%S"),
        session_id
    )))
}

fn title_from_prompt(prompt: &str) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "(empty prompt)".to_string();
    }
    const MAX_CHARS: usize = 80;
    let mut title: String = normalized.chars().take(MAX_CHARS).collect();
    if normalized.chars().count() > MAX_CHARS {
        title.push_str("...");
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_store::ORCA_HOME_ENV;
    use orca_core::plan_types::{PlanItem, PlanStatus};

    #[test]
    fn title_from_prompt_normalizes_whitespace_and_truncates() {
        assert_eq!(title_from_prompt(" hello\nworld "), "hello world");
        assert_eq!(title_from_prompt("   "), "(empty prompt)");
        assert!(title_from_prompt(&"x".repeat(100)).ends_with("..."));
    }

    #[test]
    fn writer_persists_compaction_records() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "compact me")?;
            writer.append_compaction(42, 7)?;
            writer.append_summary(42, 7, "important facts")?;
            let transcript = load_session("latest")?;
            assert_eq!(transcript.compactions.len(), 1);
            assert_eq!(transcript.compactions[0].before_messages, 42);
            assert_eq!(transcript.compactions[0].after_messages, 7);
            assert_eq!(transcript.summaries.len(), 1);
            assert_eq!(transcript.summaries[0].summary, "important facts");
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("compaction record persisted");
    }

    #[test]
    fn writer_round_trips_pinned_messages() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "remember")?;
            writer.append_message(&Message::pinned_user("pinned constraint".to_string()))?;
            let transcript = load_session("latest")?;
            assert_eq!(transcript.messages.len(), 1);
            assert!(transcript.messages[0].is_pinned());
            assert!(matches!(
                &transcript.messages[0],
                Message::User { content, .. } if content == "pinned constraint"
            ));
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("pinned message round-tripped");
    }

    #[test]
    fn writer_redacts_secrets_before_persisting_transcript() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let prompt_secret = "sk-test-redaction-title-1234567890";
            let env_secret = "sk-test-redaction-env-1234567890";
            let json_secret = "sk-test-redaction-json-1234567890";
            let password_secret = "super-secret-test-password";
            let tool_secret = "tool-token-test-secret";
            let mut writer = SessionWriter::start(
                &cwd,
                "mock",
                None,
                &format!("start ORCA_API_KEY={prompt_secret}"),
            )?;
            writer.append_message(&Message::user(format!(
                "please run ORCA_API_KEY={env_secret} and password={password_secret}"
            )))?;
            writer.append_message(&Message::Assistant {
                content: Some(format!(
                    "configured with {{\"DEEPSEEK_API_KEY\":\"{json_secret}\"}}"
                )),
                reasoning_content: Some(format!("reasoning token={tool_secret}")),
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "shell".to_string(),
                    arguments: format!("{{\"env\":{{\"API_TOKEN\":\"{tool_secret}\"}}}}"),
                }],
                pinned: false,
            })?;
            writer.append_message(&Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: format!("TOKEN={tool_secret}"),
                pinned: false,
            })?;
            writer.append_summary(3, 2, format!("summary kept {json_secret}"))?;
            writer.append_plan_state(
                Some(format!("plan with {env_secret}")),
                vec![PlanItem {
                    step: format!("step uses {password_secret}"),
                    status: PlanStatus::Pending,
                }],
            )?;

            let transcript = load_session("latest")?;
            let raw = fs::read_to_string(&transcript.path)?;
            for secret in [
                prompt_secret,
                env_secret,
                json_secret,
                password_secret,
                tool_secret,
            ] {
                assert!(
                    !raw.contains(secret),
                    "raw transcript leaked secret value {secret}"
                );
            }
            assert!(raw.contains("<redacted>"));
            assert!(raw.contains("please run"));
            assert!(raw.contains("configured with"));

            let rendered_loaded = transcript
                .messages
                .iter()
                .filter_map(Message::content_str)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!rendered_loaded.contains(env_secret));
            assert!(!rendered_loaded.contains(json_secret));
            assert!(rendered_loaded.contains("<redacted>"));

            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("session transcript secrets redacted");
    }

    #[test]
    fn plan_state_round_trips_through_session() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "plan test")?;
            writer.append_plan_state(
                Some("initial plan".to_string()),
                vec![
                    PlanItem {
                        step: "Step 1".to_string(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        step: "Step 2".to_string(),
                        status: PlanStatus::InProgress,
                    },
                    PlanItem {
                        step: "Step 3".to_string(),
                        status: PlanStatus::Pending,
                    },
                ],
            )?;
            let transcript = load_session("latest")?;
            let (explanation, plan) = transcript.plan.expect("plan should be present");
            assert_eq!(explanation.as_deref(), Some("initial plan"));
            assert_eq!(plan.len(), 3);
            assert_eq!(plan[0].step, "Step 1");
            assert_eq!(plan[1].status, PlanStatus::InProgress);
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("plan state round-tripped");
    }

    #[test]
    fn all_completed_plan_restores_as_none() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "done plan")?;
            writer.append_plan_state(
                None,
                vec![
                    PlanItem {
                        step: "Done 1".to_string(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        step: "Done 2".to_string(),
                        status: PlanStatus::Completed,
                    },
                ],
            )?;
            let transcript = load_session("latest")?;
            assert!(
                transcript.plan.is_none(),
                "all-completed plan should restore as None"
            );
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("all-completed plan cleared");
    }

    #[test]
    fn empty_plan_restores_as_none() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "empty plan")?;
            writer.append_plan_state(None, vec![])?;
            let transcript = load_session("latest")?;
            assert!(
                transcript.plan.is_none(),
                "empty plan should restore as None"
            );
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("empty plan cleared");
    }

    #[test]
    fn session_without_plan_loads_normally() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let _writer = SessionWriter::start(&cwd, "mock", None, "no plan")?;
            let transcript = load_session("latest")?;
            assert!(
                transcript.plan.is_none(),
                "no plan records means plan is None"
            );
            assert_eq!(transcript.meta.title, "no plan");
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("session without plan loaded");
    }

    #[test]
    fn resume_restores_rolling_summary_from_last_context_summary_record() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "rolling summary")?;
            writer.append_summary(10, 5, "first summary")?;
            writer.append_summary(20, 8, "updated rolling summary")?;
            let transcript = load_session("latest")?;

            let conv = resume_conversation(&transcript, "new system prompt".to_string());
            assert_eq!(
                conv.rolling_summary.as_deref(),
                Some("updated rolling summary"),
                "should restore the last summary as rolling_summary"
            );
            assert_eq!(
                conv.summary.baseline.as_deref(),
                Some("first summary"),
                "first summary record should remain the stable baseline"
            );
            assert_eq!(
                conv.summary.deltas,
                vec!["updated rolling summary".to_string()],
                "later summary records should resume as append-only deltas"
            );
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("rolling summary restored from history");
    }

    #[test]
    fn resume_without_summaries_has_no_rolling_summary() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let _writer = SessionWriter::start(&cwd, "mock", None, "no summaries")?;
            let transcript = load_session("latest")?;

            let conv = resume_conversation(&transcript, "sys".to_string());
            assert!(
                conv.rolling_summary.is_none(),
                "no summary records means no rolling_summary"
            );
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("no rolling summary without records");
    }

    #[test]
    fn resume_drops_incomplete_assistant_tool_call_turns() {
        let cwd = std::env::current_dir().unwrap();
        let transcript = SessionTranscript {
            meta: create_meta(&cwd, "mock", None, "bad tool boundary"),
            messages: vec![
                Message::user("start".to_string()),
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: "{\"path\":\"README.md\"}".to_string(),
                    }],
                    pinned: false,
                },
                Message::user("continue after failed turn".to_string()),
            ],
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: None,
            plan: None,
            path: cwd.join("bad-tool-boundary.jsonl"),
        };

        let conv = resume_conversation(&transcript, "sys".to_string());

        assert!(
            !conv.messages.iter().any(|message| matches!(
                message,
                Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()
            )),
            "resumed conversation must not contain assistant tool calls without tool results"
        );
        assert!(conv.messages.iter().any(|message| matches!(
            message,
            Message::User { content, .. } if content == "continue after failed turn"
        )));
    }

    #[test]
    fn resume_prefers_persisted_summary_state_over_legacy_summary_list() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "summary state")?;
            writer.append_summary(10, 5, "legacy baseline")?;
            writer.append_summary(20, 8, "legacy delta")?;
            writer.append_summary_state(
                30,
                9,
                "new delta",
                &SummaryState {
                    baseline: Some("rebuilt baseline".to_string()),
                    deltas: vec!["fresh delta".to_string()],
                },
            )?;
            let transcript = load_session("latest")?;

            let conv = resume_conversation(&transcript, "sys".to_string());
            assert_eq!(
                conv.summary.baseline.as_deref(),
                Some("rebuilt baseline"),
                "latest persisted summary_state should be exact resume source"
            );
            assert_eq!(conv.summary.deltas, vec!["fresh delta".to_string()]);
            assert_eq!(conv.rolling_summary.as_deref(), Some("new delta"));
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("summary state restored from history");
    }

    #[test]
    fn resume_replays_compaction_records_to_drop_collapsed_messages() {
        let _guard = TEST_ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let mut writer = SessionWriter::start(&cwd, "mock", None, "compacted resume")?;
            writer.append_message(&Message::system("old system".to_string()))?;
            writer.append_message(&Message::user("collapsed old user".repeat(100)))?;
            writer.append_message(&Message::Assistant {
                content: Some("collapsed old assistant".repeat(100)),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })?;
            writer.append_message(&Message::Assistant {
                content: Some("kept tail before compaction".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })?;
            writer.append_compaction(4, 2)?;
            writer.append_summary_state(
                4,
                2,
                "summary of collapsed old messages",
                &SummaryState {
                    baseline: Some("summary baseline".to_string()),
                    deltas: Vec::new(),
                },
            )?;
            writer.append_message(&Message::user("new prompt after compaction".to_string()))?;

            let transcript = load_session("latest")?;
            let conv = resume_conversation(&transcript, "fresh system".to_string());
            let rendered = conv
                .messages
                .iter()
                .filter_map(Message::content_str)
                .collect::<Vec<_>>()
                .join("\n");

            assert!(
                !rendered.contains("collapsed old user"),
                "collapsed pre-compaction user message should not re-enter resumed context"
            );
            assert!(
                !rendered.contains("collapsed old assistant"),
                "collapsed pre-compaction assistant message should not re-enter resumed context"
            );
            assert!(rendered.contains("kept tail before compaction"));
            assert!(rendered.contains("new prompt after compaction"));
            assert_eq!(conv.summary.baseline.as_deref(), Some("summary baseline"));
            Ok::<(), io::Error>(())
        })();

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var(ORCA_HOME_ENV, previous);
            } else {
                std::env::remove_var(ORCA_HOME_ENV);
            }
        }
        result.expect("compacted messages filtered on resume");
    }
}
