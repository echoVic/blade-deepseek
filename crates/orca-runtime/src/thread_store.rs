use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::history::{self, CompactionRecord, ContextSummaryRecord};
use crate::tool_item_projection::{
    dynamic_tool_completed_item, dynamic_tool_started_item, mcp_result_from_content,
    mcp_tool_completed_item, mcp_tool_parts, mcp_tool_started_item, parse_json_or_null,
    persisted_command_execution_completed_item, persisted_command_execution_started_item,
    persisted_file_change_completed_item, persisted_file_change_started_item,
    tool_error_object_from_value,
};
use chrono::{DateTime, Utc};
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{ActivePermissionProfile, AdditionalWorkingDirectory};
use orca_core::conversation::{Conversation, Message, RawToolCall, SummaryState};
use orca_core::cost_types::UsageTotals;
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_core::tool_types::{ToolResult, ToolStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) const ORCA_HOME_ENV: &str = "ORCA_HOME";

#[derive(Clone, Debug, Default)]
pub struct JsonlThreadStore;

pub type SessionStore = JsonlThreadStore;

impl JsonlThreadStore {
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

    pub fn search_sessions(
        &self,
        query: &str,
        include_archived: bool,
    ) -> io::Result<Vec<SearchHit>> {
        search_sessions(query, include_archived)
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionMeta {
    pub schema_version: u32,
    pub session_id: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub forked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    #[serde(default)]
    pub runtime_workspace_roots: Vec<PathBuf>,
    #[serde(default)]
    pub permission_rules: PermissionRules,
    #[serde(default)]
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub path: PathBuf,
    pub archived: bool,
    pub parent_id: Option<String>,
    pub forked: bool,
    pub approval_mode: Option<ApprovalMode>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub permission_rule_count: usize,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
}

#[derive(Clone, Debug)]
pub struct SessionTranscript {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    pub compactions: Vec<CompactionRecord>,
    pub summaries: Vec<ContextSummaryRecord>,
    pub usage: Option<UsageTotals>,
    pub plan: Option<(Option<String>, Vec<PlanItem>)>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum SessionRecord {
    #[serde(rename = "session.meta")]
    Meta(SessionMeta),
    #[serde(rename = "conversation.message")]
    Message { message: StoredMessage },
    #[serde(rename = "session.completed")]
    Completed {
        status: String,
        completed_at: DateTime<Utc>,
    },
    #[serde(rename = "context.collapsed")]
    ContextCollapsed(CompactionRecord),
    #[serde(rename = "context.summary")]
    ContextSummary(ContextSummaryRecord),
    #[serde(rename = "session.usage")]
    Usage(UsageTotals),
    #[serde(rename = "plan.state")]
    PlanState {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "role")]
pub(crate) enum StoredMessage {
    System {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    User {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
        #[serde(default)]
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ToolStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
        #[serde(default)]
        pinned: bool,
    },
}

impl From<&Message> for StoredMessage {
    fn from(message: &Message) -> Self {
        match message {
            Message::System { content, pinned } => Self::System {
                content: content.clone(),
                pinned: *pinned,
            },
            Message::User { content, pinned } => Self::User {
                content: content.clone(),
                pinned: *pinned,
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Self::Assistant {
                content: content.clone(),
                reasoning_content: reasoning_content.clone(),
                tool_calls: tool_calls.clone(),
                pinned: *pinned,
            },
            Message::Tool {
                tool_call_id,
                content,
                pinned,
            } => Self::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                status: None,
                error: None,
                exit_code: None,
                truncated: false,
                pinned: *pinned,
            },
        }
    }
}

impl From<StoredMessage> for Message {
    fn from(message: StoredMessage) -> Self {
        match message {
            StoredMessage::System { content, pinned } => Self::System { content, pinned },
            StoredMessage::User { content, pinned } => Self::User { content, pinned },
            StoredMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            },
            StoredMessage::Tool {
                tool_call_id,
                content,
                status: _,
                error: _,
                exit_code: _,
                truncated: _,
                pinned,
            } => Self::Tool {
                tool_call_id,
                content,
                pinned,
            },
        }
    }
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
    JsonlThreadStore::new().update_thread_metadata(
        &meta.session_id,
        ThreadMetadataPatch {
            title: Some(title.to_string()),
            ..ThreadMetadataPatch::default()
        },
    )?;
    Ok(path)
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

fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn write_record(path: &Path, record: &SessionRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    lock_file(&file)?;
    write_record_line(&mut file, record)?;
    file.flush()?;
    unlock_file(&file)
}

pub(crate) fn write_record_line(mut writer: impl Write, record: &SessionRecord) -> io::Result<()> {
    let redacted = redact_session_record(record);
    let mut line = serde_json::to_string(&redacted).map_err(io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes())
}

pub(crate) fn read_records(path: &Path) -> io::Result<Vec<SessionRecord>> {
    let lines = read_history_lines(path)?;
    let mut records = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if i == lines.len() - 1 => break,
            Err(_) => continue,
        }
    }
    Ok(records)
}

pub(crate) fn rewrite_records(path: &Path, records: &[SessionRecord]) -> io::Result<()> {
    let lock = OpenOptions::new().read(true).write(true).open(path)?;
    lock_file(&lock)?;

    let result = (|| {
        let temp_path = temp_rewrite_path(path);
        {
            let temp = File::create(&temp_path)?;
            if let Err(error) = write_records_to(temp, path, records) {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
        }
        if let Err(error) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        Ok(())
    })();

    let unlock_result = unlock_file(&lock);
    result.and(unlock_result)
}

fn write_records_to(file: File, target_path: &Path, records: &[SessionRecord]) -> io::Result<()> {
    if target_path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        let mut encoder = zstd::stream::write::Encoder::new(file, 3)?;
        for record in records {
            write_record_line(&mut encoder, record)?;
        }
        encoder.finish()?;
    } else {
        let mut writer = io::BufWriter::new(file);
        for record in records {
            write_record_line(&mut writer, record)?;
        }
        writer.flush()?;
    }
    Ok(())
}

fn temp_rewrite_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.jsonl");
    path.with_file_name(format!("{file_name}.tmp-{}", Uuid::new_v4()))
}

pub(crate) fn read_history_lines(path: &Path) -> io::Result<Vec<String>> {
    open_history_reader(path)?.lines().collect()
}

pub(crate) fn open_history_reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        let decoder = zstd::stream::read::Decoder::new(file)?;
        return Ok(Box::new(BufReader::new(decoder)));
    }
    Ok(Box::new(BufReader::new(file)))
}

pub(crate) fn read_session_meta(path: &Path) -> io::Result<SessionMeta> {
    let reader = open_history_reader(path)?;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(SessionRecord::Meta(meta)) = serde_json::from_str::<SessionRecord>(&line) {
            return Ok(meta);
        }
        break;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("missing session metadata in {}", path.display()),
    ))
}

pub(crate) fn read_transcript(path: &Path) -> io::Result<SessionTranscript> {
    let records = read_records(path)?;
    let mut meta = None;
    let mut messages = Vec::new();
    let mut compactions = Vec::new();
    let mut summaries = Vec::new();
    let mut usage = None;
    let mut last_plan: Option<(Option<String>, Vec<PlanItem>)> = None;

    for record in records {
        match record {
            SessionRecord::Meta(m) => meta = Some(m),
            SessionRecord::Message { message } => messages.push(message.into()),
            SessionRecord::Completed { .. } => {}
            SessionRecord::ContextCollapsed(record) => compactions.push(record),
            SessionRecord::ContextSummary(record) => summaries.push(record),
            SessionRecord::Usage(record) => usage = Some(record),
            SessionRecord::PlanState { explanation, plan } => {
                let all_done = !plan.is_empty()
                    && plan.iter().all(|item| item.status == PlanStatus::Completed);
                if plan.is_empty() || all_done {
                    last_plan = None;
                } else {
                    last_plan = Some((explanation, plan));
                }
            }
        }
    }

    let meta = meta.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("missing session metadata in {}", path.display()),
        )
    })?;

    Ok(SessionTranscript {
        meta,
        messages,
        compactions,
        summaries,
        usage,
        plan: last_plan,
        path: path.to_path_buf(),
    })
}

pub(crate) fn load_thread_records(
    thread_id: &str,
) -> io::Result<(SessionMeta, Vec<StoredMessage>)> {
    let path = find_session_path(thread_id, true)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no saved session matches '{thread_id}'"),
        )
    })?;
    let records = read_records(&path)?;
    let mut meta = None;
    let mut messages = Vec::new();
    for record in records {
        match record {
            SessionRecord::Meta(record_meta) => meta = Some(record_meta),
            SessionRecord::Message { message } => messages.push(message),
            _ => {}
        }
    }
    let meta = meta.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session '{thread_id}' is missing metadata"),
        )
    })?;
    Ok((meta, messages))
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

pub(crate) fn summarize_session_with_archive_flag(
    path: &Path,
    archived: bool,
) -> io::Result<SessionSummary> {
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

pub(crate) fn find_session_path(
    selector: &str,
    include_archived: bool,
) -> io::Result<Option<PathBuf>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    collect_matching_paths(&sessions_dir(), selector, &mut candidates)?;
    if include_archived {
        collect_matching_paths(&archive_dir(), selector, &mut candidates)?;
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    candidates.sort_by(|a, b| b.cmp(a));
    Ok(Some(candidates.into_iter().next().unwrap()))
}

pub(crate) fn resolve_session_path(
    selector: &str,
    include_archived: bool,
) -> io::Result<Option<PathBuf>> {
    if is_latest_selector(selector) {
        return Ok(list_sessions_with_archived(1, include_archived)?
            .into_iter()
            .next()
            .map(|session| session.path));
    }
    find_session_path(selector, include_archived)
}

fn collect_matching_paths(
    root: &Path,
    selector: &str,
    candidates: &mut Vec<PathBuf>,
) -> io::Result<()> {
    if is_latest_selector(selector) {
        return Ok(());
    }
    if !root.exists() {
        return Ok(());
    }
    collect_session_files(root, &mut |path| {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        if file_name.contains(selector) {
            candidates.push(path.to_path_buf());
        }
    })
}

pub(crate) fn is_latest_selector(selector: &str) -> bool {
    matches!(selector, "latest" | "last")
}

pub(crate) fn collect_session_files(dir: &Path, on_file: &mut dyn FnMut(&Path)) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_session_files(&path, on_file)?;
        } else if is_history_file(&path) {
            on_file(&path);
        }
    }
    Ok(())
}

fn is_history_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")
}

pub(crate) fn sessions_dir() -> PathBuf {
    orca_home().join("sessions")
}

pub(crate) fn archive_dir() -> PathBuf {
    orca_home().join("archive")
}

fn orca_home() -> PathBuf {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .unwrap_or_else(|| std::env::temp_dir().join("orca"))
}

fn redact_session_record(record: &SessionRecord) -> SessionRecord {
    let mut redacted = record.clone();
    match &mut redacted {
        SessionRecord::Meta(meta) => {
            redact_string_in_place(&mut meta.cwd);
            redact_string_in_place(&mut meta.provider);
            if let Some(model) = &mut meta.model {
                redact_string_in_place(model);
            }
            redact_string_in_place(&mut meta.title);
            if let Some(parent_id) = &mut meta.parent_id {
                redact_string_in_place(parent_id);
            }
        }
        SessionRecord::Message { message } => redact_stored_message(message),
        SessionRecord::Completed { status, .. } => redact_string_in_place(status),
        SessionRecord::ContextCollapsed(_) => {}
        SessionRecord::ContextSummary(record) => {
            redact_string_in_place(&mut record.summary);
            if let Some(summary_state) = &mut record.summary_state {
                if let Some(baseline) = &mut summary_state.baseline {
                    redact_string_in_place(baseline);
                }
                for delta in &mut summary_state.deltas {
                    redact_string_in_place(delta);
                }
            }
        }
        SessionRecord::Usage(_) => {}
        SessionRecord::PlanState { explanation, plan } => {
            if let Some(explanation) = explanation {
                redact_string_in_place(explanation);
            }
            for item in plan {
                redact_string_in_place(&mut item.step);
            }
        }
    }
    redacted
}

fn redact_stored_message(message: &mut StoredMessage) {
    match message {
        StoredMessage::System { content, .. }
        | StoredMessage::User { content, .. }
        | StoredMessage::Tool { content, .. } => redact_string_in_place(content),
        StoredMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            if let Some(content) = content {
                redact_string_in_place(content);
            }
            if let Some(reasoning_content) = reasoning_content {
                redact_string_in_place(reasoning_content);
            }
            for tool_call in tool_calls {
                redact_string_in_place(&mut tool_call.arguments);
            }
        }
    }
}

fn redact_string_in_place(value: &mut String) {
    *value = redact_sensitive_text(value);
}

fn redact_sensitive_text(value: &str) -> String {
    redact_standalone_secret_tokens(&redact_keyed_secret_values(value))
}

fn redact_keyed_secret_values(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'=' && bytes[index] != b':' {
            index += 1;
            continue;
        }

        let key_start = key_start_before_delimiter(bytes, index);
        let key = trim_key_candidate(&value[key_start..index]);
        if !is_sensitive_key(key) {
            index += 1;
            continue;
        }

        let mut value_start = index + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        let quote = (value_start < bytes.len()
            && (bytes[value_start] == b'"' || bytes[value_start] == b'\''))
            .then_some(bytes[value_start]);
        let content_start = if quote.is_some() {
            value_start + 1
        } else {
            value_start
        };
        let content_end = if let Some(quote) = quote {
            quoted_value_end(bytes, content_start, quote)
        } else {
            unquoted_value_end(bytes, content_start)
        };
        if content_start == content_end {
            index += 1;
            continue;
        }

        output.push_str(&value[cursor..content_start]);
        output.push_str("<redacted>");
        if quote.is_some() && content_end < bytes.len() {
            output.push(bytes[content_end] as char);
            cursor = content_end + 1;
            index = cursor;
        } else {
            cursor = content_end;
            index = cursor;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

fn key_start_before_delimiter(bytes: &[u8], delimiter_index: usize) -> usize {
    let mut start = delimiter_index;
    while start > 0 {
        let previous = bytes[start - 1];
        if previous.is_ascii_whitespace() || matches!(previous, b'{' | b'[' | b',' | b';' | b'(') {
            break;
        }
        start -= 1;
    }
    start
}

fn trim_key_candidate(key: &str) -> &str {
    key.trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '[' | ','))
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("authorization")
}

fn quoted_value_end(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return index;
        }
        index += 1;
    }
    index
}

fn unquoted_value_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !matches!(bytes[index], b',' | b'}' | b']' | b';')
    {
        index += 1;
    }
    index
}

fn redact_standalone_secret_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut token = String::new();
    for ch in value.chars() {
        if is_secret_token_boundary(ch) {
            push_redacted_token(&mut output, &mut token);
            output.push(ch);
        } else {
            token.push(ch);
        }
    }
    push_redacted_token(&mut output, &mut token);
    output
}

fn is_secret_token_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '}' | '[' | ']' | ',' | ':')
}

fn push_redacted_token(output: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    if looks_like_standalone_secret(token) {
        output.push_str("<redacted>");
    } else {
        output.push_str(token);
    }
    token.clear();
}

fn looks_like_standalone_secret(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '.' | ';' | ')' | '(' | '<' | '>' | '`' | '"' | '\'' | '='
        )
    });
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("sk-")
        || (trimmed.len() >= 16
            && lower.contains("secret")
            && (lower.contains("token")
                || lower.contains("password")
                || lower.contains("key")
                || lower.contains("test")))
}

pub(crate) fn lock_file(file: &File) -> io::Result<()> {
    lock_file_impl(file)
}

pub(crate) fn unlock_file(file: &File) -> io::Result<()> {
    unlock_file_impl(file)
}

#[cfg(unix)]
fn lock_file_impl(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe {
        if flock(file.as_raw_fd(), LOCK_EX) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(unix)]
fn unlock_file_impl(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe {
        if flock(file.as_raw_fd(), LOCK_UN) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(unix)]
const LOCK_EX: i32 = 2;
#[cfg(unix)]
const LOCK_UN: i32 = 8;

#[cfg(not(unix))]
fn lock_file_impl(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn unlock_file_impl(_file: &File) -> io::Result<()> {
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SessionWriter {
    path: PathBuf,
}

impl SessionWriter {
    pub fn start(
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::start_from_meta(history::create_meta(cwd, provider, model, prompt))
    }

    pub fn start_from_meta(meta: SessionMeta) -> io::Result<Self> {
        let path = history::session_path(&meta.session_id, meta.created_at)?;
        write_record(&path, &SessionRecord::Meta(meta))?;
        Ok(Self { path })
    }

    pub fn append_to_existing(path: PathBuf) -> io::Result<Self> {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("history file not found: {}", path.display()),
            ));
        }
        Ok(Self { path })
    }

    pub fn append_message(&mut self, message: &Message) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::Message {
                message: StoredMessage::from(message),
            },
        )
    }

    pub fn append_tool_result_message(
        &mut self,
        result: &ToolResult,
        content: String,
        pinned: bool,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::Message {
                message: StoredMessage::Tool {
                    tool_call_id: result.id.clone(),
                    content,
                    status: Some(result.status),
                    error: result.error.clone(),
                    exit_code: result.exit_code,
                    truncated: result.truncated,
                    pinned,
                },
            },
        )
    }

    pub fn complete(&mut self, status: &str) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::Completed {
                status: status.to_string(),
                completed_at: Utc::now(),
            },
        )
    }

    pub fn append_compaction(
        &mut self,
        before_messages: usize,
        after_messages: usize,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::ContextCollapsed(CompactionRecord {
                collapsed_at: Utc::now(),
                before_messages,
                after_messages,
            }),
        )
    }

    pub fn append_summary(
        &mut self,
        before_messages: usize,
        after_messages: usize,
        summary: impl Into<String>,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::ContextSummary(ContextSummaryRecord {
                summarized_at: Utc::now(),
                before_messages,
                after_messages,
                summary: summary.into(),
                summary_state: None,
            }),
        )
    }

    pub fn append_summary_state(
        &mut self,
        before_messages: usize,
        after_messages: usize,
        summary: impl Into<String>,
        summary_state: &SummaryState,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::ContextSummary(ContextSummaryRecord {
                summarized_at: Utc::now(),
                before_messages,
                after_messages,
                summary: summary.into(),
                summary_state: Some(summary_state.clone()),
            }),
        )
    }

    pub fn append_usage(&mut self, usage: UsageTotals) -> io::Result<()> {
        write_record(&self.path, &SessionRecord::Usage(usage))
    }

    pub fn append_plan_state(
        &mut self,
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    ) -> io::Result<()> {
        write_record(&self.path, &SessionRecord::PlanState { explanation, plan })
    }
}

#[derive(Clone, Debug)]
pub struct LiveThread {
    pub(crate) thread_id: String,
    pub(crate) writer: SessionWriter,
}

impl LiveThread {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn append_items(&mut self, messages: &[Message]) -> io::Result<()> {
        for message in messages {
            self.writer.append_message(message)?;
        }
        Ok(())
    }

    pub fn complete(&mut self, status: &str) -> io::Result<()> {
        self.writer.complete(status)
    }

    pub fn writer_mut(&mut self) -> &mut SessionWriter {
        &mut self.writer
    }

    pub fn into_writer(self) -> SessionWriter {
        self.writer
    }

    pub fn into_thread_id_and_writer(self) -> (String, SessionWriter) {
        (self.thread_id, self.writer)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadMetadataPatch {
    pub title: Option<String>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub approval_mode: Option<ApprovalMode>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: Option<PermissionRules>,
    pub additional_working_directories: Option<Vec<AdditionalWorkingDirectory>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadProjection {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub message_count: usize,
    pub messages: Vec<Value>,
    pub turns: Vec<StoredThreadTurn>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSearchHit {
    pub thread: StoredThreadSummary,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadTurn {
    pub thread_id: String,
    pub turn_id: String,
    pub index: usize,
    pub role: String,
    pub items_view: TurnItemsView,
    pub items: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadItem {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub index: usize,
    pub item: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadTurnPage {
    pub data: Vec<StoredThreadTurn>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadItemPage {
    pub data: Vec<StoredThreadItem>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSummaryPage {
    pub data: Vec<StoredThreadSummary>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSearchPage {
    pub data: Vec<StoredThreadSearchHit>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadSortKey {
    CreatedAt,
    UpdatedAt,
    RecencyAt,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadListFilters {
    pub archived: bool,
    pub model_providers: Option<Vec<String>>,
    pub model_names: Option<Vec<String>>,
    pub cwd_filters: Vec<String>,
    pub relation: Option<ThreadRelationFilter>,
}

impl ThreadListFilters {
    pub fn active() -> Self {
        Self::default()
    }

    pub fn archived() -> Self {
        Self {
            archived: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadRelationFilter {
    DirectChildrenOf(String),
    DescendantsOf(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnItemsView {
    NotLoaded,
    Summary,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSummary {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    pub parent_id: Option<String>,
    pub forked: bool,
    pub approval_mode: Option<ApprovalMode>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub permission_rule_count: usize,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
}

pub trait ThreadStore {
    fn create_live_thread(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<LiveThread>;

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
    ) -> io::Result<LiveThread>;

    fn update_thread_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<SessionSummary>;

    fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection>;

    fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage>;

    fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage>;

    fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage>;

    fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage>;
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

pub(crate) fn thread_summary_matches(summary: &StoredThreadSummary, search_term: &str) -> bool {
    summary.title.contains(search_term)
        || summary.cwd.contains(search_term)
        || summary.provider.contains(search_term)
        || summary
            .model
            .as_deref()
            .is_some_and(|model| model.contains(search_term))
}

pub(crate) fn thread_summary_matches_filters(
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

pub(crate) fn sort_thread_summaries(
    summaries: &mut [StoredThreadSummary],
    sort_key: ThreadSortKey,
) {
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

pub(crate) fn sort_thread_search_hits(hits: &mut [StoredThreadSearchHit], sort_key: ThreadSortKey) {
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

pub(crate) fn page_vec<T>(
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
