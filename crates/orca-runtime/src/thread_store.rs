use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::history::{self, CompactionRecord, ContextSummaryRecord};
use chrono::{DateTime, Utc};
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{ActivePermissionProfile, AdditionalWorkingDirectory};
use orca_core::conversation::{Conversation, Message, RawToolCall, SummaryState};
use orca_core::cost_types::UsageTotals;
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_core::tool_types::{ToolResult, ToolStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

pub(crate) fn messages_to_thread_turns(
    thread_id: &str,
    messages: &[Message],
    limit: usize,
    items_view: TurnItemsView,
) -> Vec<StoredThreadTurn> {
    crate::history::messages_to_thread_turns(thread_id, messages, limit, items_view)
}

pub(crate) fn message_to_thread_json(message: &Message) -> serde_json::Value {
    crate::history::message_to_thread_json(message)
}

pub(crate) fn messages_to_thread_items(
    thread_id: &str,
    messages: &[Message],
    turn_id: Option<&str>,
    limit: usize,
) -> Vec<StoredThreadItem> {
    crate::history::messages_to_thread_items(thread_id, messages, turn_id, limit)
}

pub(crate) fn next_turn_id_for_messages(thread_id: &str, messages: &[Message]) -> String {
    crate::history::next_turn_id_for_messages(thread_id, messages)
}

pub(crate) fn resume_conversation(
    transcript: &SessionTranscript,
    system_prompt: String,
) -> Conversation {
    crate::history::resume_conversation(transcript, system_prompt)
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
