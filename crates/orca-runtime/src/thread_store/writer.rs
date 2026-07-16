use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use orca_core::conversation::{Message, SummaryState};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventPublicationStore};
use orca_core::plan_types::{PlanItem, PlanStatus};
use orca_core::tool_types::ToolResult;
use uuid::Uuid;

use crate::history::{self, CompactionRecord, ContextSummaryRecord};

use super::types::{SessionMeta, SessionRecord, SessionTranscript, StoredMessage};

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
        match serde_json::from_str::<SessionRecord>(line) {
            Ok(record) => records.push(record),
            Err(error) if line_has_record_type(line, "event.semantic") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid semantic event record at {} line {}: {error}",
                        path.display(),
                        i + 1
                    ),
                ));
            }
            Err(error) if line_has_invalid_tool_terminal(line) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid session record at {} line {}: {error}",
                        path.display(),
                        i + 1
                    ),
                ));
            }
            Err(_) if i == lines.len() - 1 => break,
            Err(_) => continue,
        }
    }
    Ok(records)
}

fn line_has_record_type(line: &str, record_type: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| value["type"] == record_type)
}

fn line_has_invalid_tool_terminal(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if value["type"].as_str() != Some("conversation.message")
        || value["message"]["role"].as_str() != Some("tool")
    {
        return false;
    }
    super::types::validate_stored_tool_terminal_fields(&value["message"])
        .is_some_and(|result| result.is_err())
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
    let mut usage_baseline = UsageTotals::default();
    let mut foreground_usage = None;
    let mut background_usage = UsageTotals::default();
    let mut has_usage_baseline = false;
    let mut has_background_usage = false;
    let mut last_plan: Option<(Option<String>, Vec<PlanItem>)> = None;
    let mut completion_status = None;
    let mut completion_error = None;
    let mut next_event_seq = 0;
    let mut semantic_events = Vec::new();

    for record in records {
        match record {
            SessionRecord::Meta(m) => meta = Some(m),
            SessionRecord::Message { message } => messages.push(message.into()),
            SessionRecord::Completed { status, error, .. } => {
                completion_status = Some(status);
                completion_error = error;
            }
            SessionRecord::BackgroundTaskProviderResponse {
                usage: Some(record),
                ..
            } => {
                background_usage.input_tokens = background_usage
                    .input_tokens
                    .saturating_add(record.input_tokens);
                background_usage.output_tokens = background_usage
                    .output_tokens
                    .saturating_add(record.output_tokens);
                background_usage.cache_tokens = background_usage
                    .cache_tokens
                    .saturating_add(record.cache_tokens);
                background_usage.estimated_cost_usd += record.estimated_cost_usd;
                has_background_usage = true;
            }
            SessionRecord::BackgroundTaskProviderResponse { usage: None, .. } => {}
            SessionRecord::ContextCollapsed(record) => compactions.push(record),
            SessionRecord::ContextSummary(record) => summaries.push(record),
            SessionRecord::Usage(record) => foreground_usage = Some(record),
            SessionRecord::UsageBaseline(record) => {
                usage_baseline = record;
                foreground_usage = None;
                background_usage = UsageTotals::default();
                has_usage_baseline = true;
                has_background_usage = false;
            }
            SessionRecord::EventSequenceReserved { next_seq } => {
                next_event_seq = next_event_seq.max(next_seq);
            }
            SessionRecord::SemanticEvent { event } => semantic_events.push(event),
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
    let has_foreground_usage = foreground_usage.is_some();
    let mut aggregate_usage = usage_baseline;
    add_usage_totals(&mut aggregate_usage, foreground_usage.unwrap_or_default());
    add_usage_totals(&mut aggregate_usage, background_usage);
    let usage = (has_usage_baseline || has_foreground_usage || has_background_usage)
        .then_some(aggregate_usage);

    Ok(SessionTranscript {
        meta,
        messages,
        compactions,
        summaries,
        usage,
        plan: last_plan,
        completion_status,
        completion_error,
        next_event_seq,
        semantic_events,
        path: path.to_path_buf(),
    })
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
        SessionRecord::Completed { status, error, .. } => {
            redact_string_in_place(status);
            if let Some(error) = error {
                redact_string_in_place(error);
            }
        }
        SessionRecord::BackgroundTaskProviderResponse {
            task_id,
            status,
            error,
            ..
        } => {
            redact_string_in_place(task_id);
            redact_string_in_place(status);
            if let Some(error) = error {
                redact_string_in_place(error);
            }
        }
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
        SessionRecord::Usage(_)
        | SessionRecord::UsageBaseline(_)
        | SessionRecord::EventSequenceReserved { .. } => {}
        SessionRecord::SemanticEvent { event } => redact_json_value(&mut event.payload),
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

fn add_usage_totals(totals: &mut UsageTotals, usage: UsageTotals) {
    totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
    totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
    totals.cache_tokens = totals.cache_tokens.saturating_add(usage.cache_tokens);
    totals.estimated_cost_usd += usage.estimated_cost_usd;
}

fn redact_stored_message(message: &mut StoredMessage) {
    match message {
        StoredMessage::System { content, .. } | StoredMessage::User { content, .. } => {
            redact_string_in_place(content)
        }
        StoredMessage::Tool {
            content, terminal, ..
        } => {
            redact_string_in_place(content);
            if let Some(error) = terminal.error_mut() {
                redact_string_in_place(error);
            }
        }
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

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => redact_string_in_place(text),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_json_value(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redact_string_in_place(value: &mut String) {
    *value = redact_sensitive_text(value);
}

pub(crate) fn redact_sensitive_text(value: &str) -> String {
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

fn restore_plaintext_transcript(path: PathBuf) -> io::Result<PathBuf> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("zst") {
        return Ok(path);
    }
    let plain_path = path.with_extension("");
    let lock = OpenOptions::new().read(true).write(true).open(&path)?;
    lock_file(&lock)?;
    let result = (|| {
        let input = File::open(&path)?;
        let output = File::create(&plain_path)?;
        if let Err(error) = zstd::stream::copy_decode(input, output) {
            let _ = fs::remove_file(&plain_path);
            return Err(io::Error::other(error));
        }
        fs::remove_file(&path)?;
        Ok(plain_path)
    })();
    let unlock_result = unlock_file(&lock);
    match (result, unlock_result) {
        (Ok(path), Ok(())) => Ok(path),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
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
        // Appends write plaintext JSONL; raw bytes after a zstd frame would
        // make the whole transcript undecodable, so a compressed session must
        // be restored to plaintext before it can be continued.
        let path = restore_plaintext_transcript(path)?;
        append_usage_baseline(&path)?;
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
                    terminal: super::types::StoredToolTerminal::from_terminal(Some(
                        result.terminal(),
                    )),
                    pinned,
                },
            },
        )
    }

    pub fn complete(&mut self, status: &str) -> io::Result<()> {
        self.complete_with_error(status, None)
    }

    pub fn complete_with_error(&mut self, status: &str, error: Option<&str>) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::Completed {
                status: status.to_string(),
                completed_at: Utc::now(),
                error: error.map(str::to_string),
            },
        )
    }

    pub fn append_background_task_provider_response(
        &mut self,
        task_id: &str,
        status: &str,
        error: Option<&str>,
        usage: Option<UsageTotals>,
    ) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::BackgroundTaskProviderResponse {
                task_id: task_id.to_string(),
                status: status.to_string(),
                completed_at: Utc::now(),
                error: error.map(str::to_string),
                usage,
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

    fn reserve_event_sequence(&self, next_seq: u64) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::EventSequenceReserved { next_seq },
        )
    }

    fn append_semantic_event_record(&self, event: &EventEnvelope) -> io::Result<()> {
        write_record(
            &self.path,
            &SessionRecord::SemanticEvent {
                event: event.clone(),
            },
        )
    }

    pub fn append_plan_state(
        &mut self,
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    ) -> io::Result<()> {
        write_record(&self.path, &SessionRecord::PlanState { explanation, plan })
    }
}

impl EventPublicationStore for SessionWriter {
    fn reserve_through(&self, next_seq_exclusive: u64) -> io::Result<()> {
        self.reserve_event_sequence(next_seq_exclusive)
    }

    fn append_semantic_event(&self, event: &EventEnvelope) -> io::Result<()> {
        self.append_semantic_event_record(event)
    }
}

fn append_usage_baseline(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).append(true).open(path)?;
    lock_file(&file)?;
    let result = (|| {
        let Some(usage) = read_transcript(path)?.usage else {
            return Ok(());
        };
        write_record_line(&mut file, &SessionRecord::UsageBaseline(usage))?;
        file.flush()
    })();
    let unlock_result = unlock_file(&file);
    result.and(unlock_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;

    fn usage(input_tokens: u64, output_tokens: u64, cache_tokens: u64, cost: f64) -> UsageTotals {
        UsageTotals {
            input_tokens,
            output_tokens,
            cache_tokens,
            estimated_cost_usd: cost,
        }
    }

    fn assert_usage(actual: UsageTotals, expected: UsageTotals) {
        assert_eq!(actual.input_tokens, expected.input_tokens);
        assert_eq!(actual.output_tokens, expected.output_tokens);
        assert_eq!(actual.cache_tokens, expected.cache_tokens);
        assert!(
            (actual.estimated_cost_usd - expected.estimated_cost_usd).abs() < 1e-12,
            "expected cost {}, got {}",
            expected.estimated_cost_usd,
            actual.estimated_cost_usd
        );
    }

    fn new_transcript() -> (tempfile::TempDir, PathBuf, SessionWriter) {
        let directory = tempfile::tempdir().expect("temporary transcript directory");
        let path = directory.path().join("resume-usage.jsonl");
        let meta = SessionMeta {
            schema_version: 1,
            session_id: "resume-usage".to_string(),
            cwd: directory.path().display().to_string(),
            provider: "mock".to_string(),
            model: None,
            title: "resume usage".to_string(),
            created_at: Utc::now(),
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        };
        write_record(&path, &SessionRecord::Meta(meta)).expect("write metadata");
        let writer = SessionWriter { path: path.clone() };
        (directory, path, writer)
    }

    fn aggregate_usage(path: &Path) -> UsageTotals {
        read_transcript(path)
            .expect("read transcript")
            .usage
            .expect("aggregate usage")
    }

    #[test]
    fn transcript_reduces_event_sequence_reservations_to_exclusive_maximum() {
        let (_directory, path, writer) = new_transcript();

        writer.reserve_through(256).expect("reserve first block");
        writer.reserve_through(512).expect("reserve second block");
        writer
            .reserve_through(256)
            .expect("stale reservation remains readable");

        let transcript = read_transcript(&path).expect("read sequence reservation");
        assert_eq!(transcript.next_event_seq, 512);
        let records = read_records(&path).expect("read typed records");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, SessionRecord::EventSequenceReserved { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn semantic_event_round_trips_as_the_original_typed_envelope() {
        let (_directory, path, writer) = new_transcript();
        let event = EventEnvelope {
            version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
            run_id: "semantic-round-trip".to_string(),
            seq: 37,
            timestamp_ms: 1_234_567,
            event_type: orca_core::event_schema::EventType::ToolCallCompleted,
            payload: serde_json::json!({
                "id": "tool-1",
                "name": "shell",
                "status": "completed",
                "nested": { "preserved": true }
            }),
        };

        writer
            .append_semantic_event(&event)
            .expect("append semantic event");

        let raw = fs::read_to_string(&path).expect("read semantic JSONL");
        let semantic_line = raw
            .lines()
            .find(|line| line.contains("\"type\":\"event.semantic\""))
            .expect("semantic JSONL line");
        serde_json::from_str::<SessionRecord>(semantic_line).expect("parse typed semantic record");
        let transcript = read_transcript(&path).expect("read semantic transcript");
        assert_eq!(
            transcript.semantic_events.as_slice(),
            std::slice::from_ref(&event)
        );
        let records = read_records(&path).expect("read typed semantic record");
        assert!(records.iter().any(|record| {
            matches!(record, SessionRecord::SemanticEvent { event: stored } if stored == &event)
        }));
        let record = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| record["type"] == "event.semantic")
            .expect("semantic JSONL record");
        assert_eq!(record["event"], serde_json::to_value(event).unwrap());
    }

    #[test]
    fn semantic_event_payload_uses_recursive_history_redaction() {
        let (_directory, path, writer) = new_transcript();
        let event = EventEnvelope {
            version: orca_core::event_schema::EVENT_SCHEMA_VERSION.to_string(),
            run_id: "semantic-redaction".to_string(),
            seq: 0,
            timestamp_ms: 42,
            event_type: orca_core::event_schema::EventType::ToolCallRequested,
            payload: serde_json::json!({
                "raw_arguments": {
                    "authorization": "token=secret-test-value",
                    "nested": ["api_key=secret-test-key"]
                }
            }),
        };

        writer
            .append_semantic_event(&event)
            .expect("append redacted semantic event");

        let transcript = read_transcript(&path).expect("read redacted semantic transcript");
        let payload = &transcript.semantic_events[0].payload;
        assert_eq!(
            payload["raw_arguments"]["authorization"],
            "token=<redacted>"
        );
        assert_eq!(payload["raw_arguments"]["nested"][0], "api_key=<redacted>");
    }

    #[test]
    fn legacy_transcript_without_event_sequence_reservation_starts_at_zero() {
        let (_directory, path, _writer) = new_transcript();

        let transcript = read_transcript(&path).expect("read legacy transcript");

        assert_eq!(transcript.next_event_seq, 0);
        assert!(transcript.semantic_events.is_empty());
    }

    fn seed_foreground_and_background(writer: &mut SessionWriter) {
        writer
            .append_usage(usage(100, 20, 40, 0.10))
            .expect("write initial foreground usage");
        writer
            .append_background_task_provider_response(
                "background-1",
                "success",
                None,
                Some(usage(30, 10, 15, 0.05)),
            )
            .expect("write background usage");
    }

    #[test]
    fn legacy_transcript_without_baseline_keeps_background_usage() {
        let (_directory, path, mut writer) = new_transcript();
        seed_foreground_and_background(&mut writer);
        writer
            .append_usage(usage(140, 25, 50, 0.13))
            .expect("update foreground snapshot");

        assert_usage(aggregate_usage(&path), usage(170, 35, 65, 0.18));
    }

    #[test]
    fn usage_baseline_accumulates_later_background_delta() {
        let (_directory, path, mut initial) = new_transcript();
        seed_foreground_and_background(&mut initial);

        let mut resumed = SessionWriter::append_to_existing(path.clone()).expect("resume writer");
        resumed
            .append_background_task_provider_response(
                "background-2",
                "success",
                None,
                Some(usage(20, 5, 8, 0.03)),
            )
            .expect("write resumed background usage");

        assert_usage(aggregate_usage(&path), usage(150, 35, 63, 0.18));
    }

    #[test]
    fn usage_baseline_uses_latest_resumed_foreground_snapshot() {
        let (_directory, path, mut initial) = new_transcript();
        seed_foreground_and_background(&mut initial);

        let mut resumed = SessionWriter::append_to_existing(path.clone()).expect("resume writer");
        resumed
            .append_usage(usage(50, 8, 20, 0.04))
            .expect("write resumed foreground usage");
        resumed
            .append_usage(usage(80, 12, 30, 0.07))
            .expect("update resumed foreground snapshot");

        assert_usage(aggregate_usage(&path), usage(210, 42, 85, 0.22));
    }

    #[test]
    fn multiple_resumes_roll_forward_each_aggregate_baseline_once() {
        let (_directory, path, mut initial) = new_transcript();
        seed_foreground_and_background(&mut initial);

        let mut first_resume =
            SessionWriter::append_to_existing(path.clone()).expect("first resume writer");
        first_resume
            .append_usage(usage(80, 12, 30, 0.07))
            .expect("write first resumed foreground usage");

        let mut second_resume =
            SessionWriter::append_to_existing(path.clone()).expect("second resume writer");
        second_resume
            .append_usage(usage(20, 4, 7, 0.02))
            .expect("write second resumed foreground usage");

        assert_usage(aggregate_usage(&path), usage(230, 46, 92, 0.24));
        assert_eq!(
            read_records(&path)
                .expect("read records")
                .iter()
                .filter(|record| matches!(record, SessionRecord::UsageBaseline(_)))
                .count(),
            2
        );
    }

    #[test]
    fn read_records_rejects_conflicting_tool_terminal_record() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        let valid = serde_json::to_string(&SessionRecord::Message {
            message: StoredMessage::User {
                content: "valid".to_string(),
                pinned: false,
            },
        })
        .expect("serialize valid record");
        fs::write(
            path.path(),
            format!(
                "{valid}\n{{\"type\":\"conversation.message\",\"message\":{{\"role\":\"tool\",\"tool_call_id\":\"call-1\",\"content\":\"cancelled\",\"status\":\"cancelled\",\"kind\":\"runtime_error\"}}}}\n{valid}\n"
            ),
        )
        .expect("write transcript");

        let error = read_records(path.path()).expect_err("terminal conflict must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line 2"));
    }

    #[test]
    fn read_records_rejects_complete_invalid_semantic_event_record() {
        let (_directory, path, _writer) = new_transcript();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open semantic transcript");
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "event.semantic",
                "event": {
                    "version": "1",
                    "run_id": "invalid-semantic",
                    "seq": 0,
                    "timestamp_ms": "not-a-number",
                    "type": "error",
                    "payload": { "message": "invalid" }
                }
            })
        )
        .expect("write invalid semantic event");

        let error =
            read_records(&path).expect_err("known invalid semantic record must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("invalid semantic event record"));
        assert!(error.to_string().contains("line 2"));
    }

    #[test]
    fn read_records_ignores_only_truncated_final_record() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        let valid = serde_json::to_string(&SessionRecord::Message {
            message: StoredMessage::User {
                content: "valid".to_string(),
                pinned: false,
            },
        })
        .expect("serialize valid record");
        fs::write(
            path.path(),
            format!("{valid}\n{{\"type\":\"conversation.message\",\"message\":{{\"role\":\"tool\""),
        )
        .expect("write transcript");

        let records = read_records(path.path()).expect("truncated final record is recoverable");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn read_records_preserves_legacy_skip_for_unrelated_invalid_record() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        fs::write(
            path.path(),
            "{\"type\":\"conversation.message\",\"message\":{\"role\":\"tool\"\n",
        )
        .expect("write transcript");

        let records = read_records(path.path()).expect("legacy malformed tail is skipped");
        assert!(records.is_empty());
    }

    #[test]
    fn read_records_skips_terminal_shaped_record_with_unrelated_error() {
        let path = tempfile::NamedTempFile::new().expect("temp transcript");
        fs::write(
            path.path(),
            concat!(
                "{\"type\":\"conversation.message\",\"message\":{\"role\":\"tool\",\"content\":\"missing id\",\"status\":\"cancelled\",\"kind\":\"cancelled\"}}\n",
                "{\"type\":\"conversation.message\",\"message\":{\"role\":\"user\",\"content\":\"valid\"}}\n",
            ),
        )
        .expect("write transcript");

        let records = read_records(path.path()).expect("unrelated old bad record is skipped");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn read_records_rejects_invalid_terminal_metadata_beyond_status_kind_conflicts() {
        for invalid_fields in [
            "\"kind\":\"cancelled\"",
            "\"status\":\"cancelled\",\"kind\":\"cancelled\",\"terminal_source\":\"future\"",
            "\"status\":\"cancelled\",\"kind\":\"cancelled\",\"invocation_started\":42",
        ] {
            let path = tempfile::NamedTempFile::new().expect("temp transcript");
            fs::write(
                path.path(),
                format!(
                    "{{\"type\":\"conversation.message\",\"message\":{{\"role\":\"tool\",\"tool_call_id\":\"call-1\",\"content\":\"cancelled\",{invalid_fields}}}}}\n"
                ),
            )
            .expect("write transcript");

            let error = read_records(path.path()).expect_err("terminal metadata must fail closed");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }
}
