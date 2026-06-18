use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::provider::conversation::{Conversation, Message, RawToolCall};
use crate::runtime::cost::UsageTotals;
use crate::tools::update_plan::{PlanItem, PlanStatus};

const ORCA_HOME_ENV: &str = "ORCA_HOME";
const SESSION_SCHEMA_VERSION: u32 = 1;

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

#[derive(Clone, Debug)]
pub struct SessionWriter {
    path: PathBuf,
}

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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
enum SessionRecord {
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
enum StoredMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl From<&Message> for StoredMessage {
    fn from(message: &Message) -> Self {
        match message {
            Message::System(content) => Self::System {
                content: content.clone(),
            },
            Message::User(content) => Self::User {
                content: content.clone(),
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => Self::Assistant {
                content: content.clone(),
                reasoning_content: reasoning_content.clone(),
                tool_calls: tool_calls.clone(),
            },
            Message::Tool {
                tool_call_id,
                content,
            } => Self::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
            },
        }
    }
}

impl From<StoredMessage> for Message {
    fn from(message: StoredMessage) -> Self {
        match message {
            StoredMessage::System { content } => Self::System(content),
            StoredMessage::User { content } => Self::User(content),
            StoredMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
            },
            StoredMessage::Tool {
                tool_call_id,
                content,
            } => Self::Tool {
                tool_call_id,
                content,
            },
        }
    }
}

impl SessionWriter {
    pub fn start(
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::start_from_meta(create_meta(cwd, provider, model, prompt))
    }

    pub fn start_from_meta(meta: SessionMeta) -> io::Result<Self> {
        let path = session_path(&meta.session_id, meta.created_at)?;
        write_record(&path, &SessionRecord::Meta(meta))?;
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
    let mut records = read_records(&path)?;
    let mut renamed = false;
    for record in &mut records {
        if let SessionRecord::Meta(meta) = record {
            meta.title = title.to_string();
            renamed = true;
            break;
        }
    }
    if !renamed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("missing session metadata in {}", path.display()),
        ));
    }
    rewrite_records(&path, &records)?;
    Ok(path)
}

pub fn resume_conversation(transcript: &SessionTranscript, system_prompt: String) -> Conversation {
    let mut conversation = Conversation::new();
    conversation.add_system(system_prompt);
    for message in transcript
        .messages
        .iter()
        .filter(|message| !matches!(message, Message::System(_)))
    {
        conversation.messages.push(message.clone());
    }
    conversation
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
    })
}

fn read_session_meta(path: &Path) -> io::Result<SessionMeta> {
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

fn read_transcript(path: &Path) -> io::Result<SessionTranscript> {
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

fn find_session_path(selector: &str, include_archived: bool) -> io::Result<Option<PathBuf>> {
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

fn resolve_session_path(selector: &str, include_archived: bool) -> io::Result<Option<PathBuf>> {
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

fn is_latest_selector(selector: &str) -> bool {
    matches!(selector, "latest" | "last")
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

fn collect_session_files(dir: &Path, on_file: &mut dyn FnMut(&Path)) -> io::Result<()> {
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

fn write_record(path: &Path, record: &SessionRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
    line.push('\n');

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    lock_file(&file)?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    unlock_file(&file)
}

fn read_records(path: &Path) -> io::Result<Vec<SessionRecord>> {
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

fn rewrite_records(path: &Path, records: &[SessionRecord]) -> io::Result<()> {
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

fn write_record_line(mut writer: impl Write, record: &SessionRecord) -> io::Result<()> {
    let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes())
}

fn temp_rewrite_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.jsonl");
    path.with_file_name(format!("{file_name}.tmp-{}", Uuid::new_v4()))
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

fn open_history_reader(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        let decoder = zstd::stream::read::Decoder::new(file)?;
        Ok(Box::new(BufReader::new(decoder)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

fn read_history_lines(path: &Path) -> io::Result<Vec<String>> {
    open_history_reader(path)?.lines().collect()
}

fn is_history_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")
}

#[cfg(unix)]
fn lock_file(file: &File) -> io::Result<()> {
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
fn unlock_file(file: &File) -> io::Result<()> {
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
fn lock_file(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn unlock_file(_file: &File) -> io::Result<()> {
    Ok(())
}

fn session_path(session_id: &str, timestamp: DateTime<Utc>) -> io::Result<PathBuf> {
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

fn sessions_dir() -> PathBuf {
    orca_home().join("sessions")
}

fn archive_dir() -> PathBuf {
    orca_home().join("archive")
}

fn orca_home() -> PathBuf {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .unwrap_or_else(|| std::env::temp_dir().join("orca"))
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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn title_from_prompt_normalizes_whitespace_and_truncates() {
        assert_eq!(title_from_prompt(" hello\nworld "), "hello world");
        assert_eq!(title_from_prompt("   "), "(empty prompt)");
        assert!(title_from_prompt(&"x".repeat(100)).ends_with("..."));
    }

    #[test]
    fn writer_persists_compaction_records() {
        let _guard = ENV_LOCK.lock().expect("env lock");
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
    fn plan_state_round_trips_through_session() {
        let _guard = ENV_LOCK.lock().expect("env lock");
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
                    PlanItem { step: "Step 1".to_string(), status: PlanStatus::Completed },
                    PlanItem { step: "Step 2".to_string(), status: PlanStatus::InProgress },
                    PlanItem { step: "Step 3".to_string(), status: PlanStatus::Pending },
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
        let _guard = ENV_LOCK.lock().expect("env lock");
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
                    PlanItem { step: "Done 1".to_string(), status: PlanStatus::Completed },
                    PlanItem { step: "Done 2".to_string(), status: PlanStatus::Completed },
                ],
            )?;
            let transcript = load_session("latest")?;
            assert!(transcript.plan.is_none(), "all-completed plan should restore as None");
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
        let _guard = ENV_LOCK.lock().expect("env lock");
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
            assert!(transcript.plan.is_none(), "empty plan should restore as None");
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
        let _guard = ENV_LOCK.lock().expect("env lock");
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os(ORCA_HOME_ENV);
        unsafe {
            std::env::set_var(ORCA_HOME_ENV, home.path());
        }

        let result = (|| {
            let cwd = std::env::current_dir()?;
            let _writer = SessionWriter::start(&cwd, "mock", None, "no plan")?;
            let transcript = load_session("latest")?;
            assert!(transcript.plan.is_none(), "no plan records means plan is None");
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
}
