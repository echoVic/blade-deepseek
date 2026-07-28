use std::io::Write;

use orca_core::conversation::Message;

#[derive(Clone, Debug)]
pub enum HistoryCommandRequest {
    List { limit: usize, all: bool },
    Show { session: String },
    Archive { session: String },
    Delete { session: String },
    Rename { session: String, title: String },
    Search { query: String, all: bool },
    Compress { session: String },
}

pub fn run(request: HistoryCommandRequest) -> i32 {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with_writers(request, &mut stdout, &mut stderr)
}

pub fn run_with_writers(
    request: HistoryCommandRequest,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    match request {
        HistoryCommandRequest::List { limit, all } => {
            match crate::history::list_sessions_with_archived(limit, all) {
                Ok(sessions) => {
                    for session in sessions {
                        let model = session.model.as_deref().unwrap_or("-");
                        let state = if session.archived {
                            "archived"
                        } else {
                            "active"
                        };
                        if writeln!(
                            stdout,
                            "{}\t{}\t{}\t{}\t{}\t{}",
                            session.session_id,
                            session.updated_at.to_rfc3339(),
                            state,
                            session.provider,
                            model,
                            session.title
                        )
                        .is_err()
                        {
                            return 1;
                        }
                    }
                    0
                }
                Err(error) => history_error(stderr, "list", error),
            }
        }
        HistoryCommandRequest::Show { session } => match crate::history::load_session(&session) {
            Ok(transcript) => {
                let result = (|| -> std::io::Result<()> {
                    writeln!(stdout, "Session: {}", transcript.meta.session_id)?;
                    writeln!(stdout, "Title: {}", transcript.meta.title)?;
                    writeln!(
                        stdout,
                        "Created: {}",
                        transcript.meta.created_at.to_rfc3339()
                    )?;
                    writeln!(stdout, "Provider: {}", transcript.meta.provider)?;
                    writeln!(
                        stdout,
                        "Model: {}",
                        transcript.meta.model.as_deref().unwrap_or("-")
                    )?;
                    if let Some(parent_id) = &transcript.meta.parent_id {
                        writeln!(stdout, "Parent: {parent_id}")?;
                    }
                    writeln!(stdout, "Forked: {}", transcript.meta.forked)?;
                    if !transcript.compactions.is_empty() {
                        writeln!(stdout, "Compactions: {}", transcript.compactions.len())?;
                        for compaction in &transcript.compactions {
                            writeln!(
                                stdout,
                                "  {} {} -> {} messages",
                                compaction.collapsed_at.to_rfc3339(),
                                compaction.before_messages,
                                compaction.after_messages
                            )?;
                        }
                    }
                    if !transcript.summaries.is_empty() {
                        writeln!(stdout, "Summaries: {}", transcript.summaries.len())?;
                        for summary in &transcript.summaries {
                            writeln!(
                                stdout,
                                "  {} {} -> {} messages: {}",
                                summary.summarized_at.to_rfc3339(),
                                summary.before_messages,
                                summary.after_messages,
                                summary.summary.lines().next().unwrap_or_default()
                            )?;
                        }
                    }
                    if let Some(usage) = transcript.usage {
                        writeln!(
                            stdout,
                            "Usage: input={} output={} cache={} total={}",
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cache_tokens,
                            usage.total_tokens()
                        )?;
                        writeln!(stdout, "Estimated cost: ${:.6}", usage.estimated_cost_usd)?;
                    }
                    writeln!(stdout, "CWD: {}", transcript.meta.cwd)?;
                    writeln!(stdout, "Path: {}", transcript.path.display())?;
                    writeln!(stdout)?;
                    for message in transcript.messages {
                        write_message(stdout, message)?;
                    }
                    Ok(())
                })();
                if result.is_ok() { 0 } else { 1 }
            }
            Err(error) => history_error(stderr, "show", error),
        },
        HistoryCommandRequest::Archive { session } => {
            match crate::history::archive_session(&session) {
                Ok(path) => write_success(stdout, format_args!("archived {}", path.display())),
                Err(error) => history_error(stderr, "archive", error),
            }
        }
        HistoryCommandRequest::Delete { session } => match crate::history::delete_session(&session)
        {
            Ok(path) => write_success(stdout, format_args!("deleted {}", path.display())),
            Err(error) => history_error(stderr, "delete", error),
        },
        HistoryCommandRequest::Rename { session, title } => {
            match crate::history::rename_session(&session, &title) {
                Ok(path) => write_success(stdout, format_args!("renamed {}", path.display())),
                Err(error) => history_error(stderr, "rename", error),
            }
        }
        HistoryCommandRequest::Search { query, all } => {
            match crate::history::search_sessions(&query, all) {
                Ok(hits) => {
                    for hit in hits {
                        let state = if hit.archived { "archived" } else { "active" };
                        if writeln!(
                            stdout,
                            "{}\t{}\t{}\t{}:{}\t{}",
                            hit.session_id,
                            state,
                            hit.title,
                            hit.path.display(),
                            hit.line_number,
                            hit.line
                        )
                        .is_err()
                        {
                            return 1;
                        }
                    }
                    0
                }
                Err(error) => history_error(stderr, "search", error),
            }
        }
        HistoryCommandRequest::Compress { session } => {
            match crate::history::compress_session(&session) {
                Ok(path) => write_success(stdout, format_args!("compressed {}", path.display())),
                Err(error) => history_error(stderr, "compress", error),
            }
        }
    }
}

fn write_success(writer: &mut impl Write, args: std::fmt::Arguments<'_>) -> i32 {
    if writeln!(writer, "{args}").is_ok() {
        0
    } else {
        1
    }
}

fn history_error(writer: &mut impl Write, action: &str, error: impl std::fmt::Display) -> i32 {
    let _ = writeln!(writer, "orca: failed to {action} history: {error}");
    1
}

fn write_message(writer: &mut impl Write, message: Message) -> std::io::Result<()> {
    match message {
        Message::System { content, .. } => writeln!(writer, "[system]\n{}\n", content.trim()),
        Message::User { content, .. } => writeln!(writer, "[user]\n{}\n", content.trim()),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            writeln!(writer, "[assistant]")?;
            if let Some(reasoning) = reasoning_content.filter(|text| !text.trim().is_empty()) {
                writeln!(writer, "reasoning: {}", reasoning.trim())?;
            }
            if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
                writeln!(writer, "{}", content.trim())?;
            }
            for tool_call in tool_calls {
                writeln!(
                    writer,
                    "tool_call {} {} {}",
                    tool_call.id, tool_call.function_name, tool_call.arguments
                )?;
            }
            writeln!(writer)
        }
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => {
            writeln!(writer, "[tool {tool_call_id}]\n{}\n", content.trim())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_formatter_preserves_roles_and_tool_identity() {
        let mut output = Vec::new();
        write_message(
            &mut output,
            Message::Tool {
                tool_call_id: "call-1".to_string(),
                content: "result".to_string(),
                terminal: None,
                pinned: false,
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "[tool call-1]\nresult\n\n"
        );
    }
}
