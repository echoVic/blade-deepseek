use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

use super::shortcuts::{self, ShortcutScope};
use super::types::{AppState, AppStatus, ChatMessage};

pub fn render(frame: &mut Frame, state: &mut AppState, textarea: &TextArea) {
    if state.status == AppStatus::Setup {
        render_setup(frame, state, textarea);
        return;
    }
    if state.status == AppStatus::SessionPicker {
        render_session_picker(frame, state);
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_messages(frame, chunks[0], state);
    render_input(frame, chunks[1], textarea);
    render_status(frame, chunks[2], state);

    if state.status == AppStatus::WaitingApproval {
        render_approval_dialog(frame, state);
    }

    if state.show_shortcuts {
        render_shortcuts(frame, state);
    }
}

fn render_session_picker(frame: &mut Frame, state: &mut AppState) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Resume Conversation ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Enter resume · n new session · Esc quit",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));

    for (index, session) in state.session_picker_sessions.iter().enumerate() {
        let selected = index == state.session_picker_selected;
        let marker = if selected { "> " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(session.title.clone(), style),
            Span::styled(
                format!(
                    "  {}  {}",
                    session.updated_at.format("%Y-%m-%d %H:%M"),
                    session.provider
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn render_messages(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let lines = build_message_lines(state);

    // Account for block borders: 1 left + 1 right
    let content_width = area.width.saturating_sub(2) as usize;
    let visible_height = area.height.saturating_sub(2);

    // Calculate total visual lines after wrapping
    let total_visual: u16 = lines
        .iter()
        .map(|line| wrapped_line_count(line, content_width) as u16)
        .sum();

    state.total_lines = total_visual;
    state.visible_height = visible_height;

    let scroll = if state.auto_scroll {
        let max_scroll = total_visual.saturating_sub(visible_height);
        state.scroll_offset = max_scroll;
        max_scroll
    } else {
        let max_scroll = total_visual.saturating_sub(visible_height);
        state.scroll_offset = state.scroll_offset.min(max_scroll);
        state.scroll_offset
    };

    let block = Block::default().borders(Borders::ALL).title(" Orca ");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

fn wrapped_line_count(line: &Line, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let line_width: usize = line
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if line_width == 0 {
        return 1;
    }
    (line_width + width - 1) / width
}

fn build_message_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for msg in &state.messages {
        match msg {
            ChatMessage::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Blue)),
                    Span::styled(text.clone(), Style::default().fg(Color::Blue)),
                ]));
                lines.push(Line::from(""));
            }
            ChatMessage::Reasoning(text) => {
                let prefix = Span::styled(
                    "[thinking] ",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                );
                let truncated = truncate_lines(text, 3);
                lines.push(Line::from(vec![
                    prefix,
                    Span::styled(
                        truncated,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
            ChatMessage::Assistant(text) => {
                let md_lines = render_markdown(text);
                for line in md_lines {
                    lines.push(line);
                }
                lines.push(Line::from(""));
            }
            ChatMessage::ToolCall {
                name,
                target,
                status,
                output,
            } => {
                let icon = match status.as_str() {
                    "completed" => "✓",
                    "running" => "⟳",
                    "denied" => "✗",
                    "failed" => "✗",
                    _ => "·",
                };
                let color = match status.as_str() {
                    "completed" => Color::Green,
                    "running" => Color::Yellow,
                    "denied" | "failed" => Color::Red,
                    _ => Color::Gray,
                };
                let target_str = target
                    .as_deref()
                    .map(|t| format!(": {t}"))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {icon} {name}{target_str}"),
                        Style::default().fg(color),
                    ),
                    Span::styled(format!(" ({status})"), Style::default().fg(Color::DarkGray)),
                ]));
                if let Some(out) = output {
                    let preview = truncate_lines(out, 2);
                    lines.push(Line::from(Span::styled(
                        format!("    {preview}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            ChatMessage::Subagent {
                description,
                status,
                output,
                error,
                ..
            } => {
                append_subagent_lines(&mut lines, description, status, output, error);
            }
            ChatMessage::Error(text) => {
                lines.push(Line::from(Span::styled(
                    format!("ERROR: {text}"),
                    Style::default().fg(Color::Red),
                )));
                lines.push(Line::from(""));
            }
            ChatMessage::System(text) => {
                lines.push(Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    lines
}

fn append_subagent_lines(
    lines: &mut Vec<Line<'static>>,
    description: &str,
    status: &str,
    output: &Option<String>,
    error: &Option<String>,
) {
    let (label, color) = match status {
        "success" | "completed" => ("done", Color::Green),
        "running" => ("running", Color::Cyan),
        "failed" => ("failed", Color::Red),
        other => (other, Color::Gray),
    };

    lines.push(Line::from(vec![
        Span::styled("  ┌─ delegated task", Style::default().fg(Color::Cyan)),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(label.to_string(), Style::default().fg(color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  │ ", Style::default().fg(Color::Cyan)),
        Span::styled(description.to_string(), Style::default().fg(Color::White)),
    ]));

    match (status, output, error) {
        ("running", _, _) => {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    "working in a child context",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        (_, _, Some(err)) => {
            lines.push(Line::from(vec![
                Span::styled("  │ error: ", Style::default().fg(Color::Red)),
                Span::styled(truncate_lines(err, 3), Style::default().fg(Color::Red)),
            ]));
        }
        (_, Some(out), _) => {
            lines.push(Line::from(vec![
                Span::styled("  │ result: ", Style::default().fg(Color::Green)),
                Span::styled(truncate_lines(out, 3), Style::default().fg(Color::DarkGray)),
            ]));
        }
        _ => {}
    }

    lines.push(Line::from(Span::styled(
        "  └─ returned to main agent",
        Style::default().fg(Color::DarkGray),
    )));
}

fn render_input(frame: &mut Frame, area: Rect, textarea: &TextArea) {
    frame.render_widget(textarea, area);
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let (status_text, color) = match &state.status {
        AppStatus::Setup => ("● setup", Color::Cyan),
        AppStatus::SessionPicker => ("● sessions", Color::Cyan),
        AppStatus::Idle => ("● idle", Color::Green),
        AppStatus::Running => ("● running", Color::Yellow),
        AppStatus::WaitingApproval => ("● approval", Color::Magenta),
    };

    let scroll_hint = if !state.auto_scroll {
        format!(
            " | scroll: {}/{}",
            state.scroll_offset,
            state.total_lines.saturating_sub(state.visible_height)
        )
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(format!(" {status_text}"), Style::default().fg(color)),
        Span::styled(scroll_hint, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" | model: {}", state.model_name),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            " | F1/ctrl+k shortcuts",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

fn render_shortcuts(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let width = 58u16.min(area.width.saturating_sub(4));
    let max_height = area.height.saturating_sub(4);
    let scopes = active_shortcut_scopes(state);
    let lines = shortcuts::shortcut_lines(&scopes);
    let height = ((lines.len() as u16) + 2).min(max_height).max(3);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Shortcuts ")
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, popup_area);
}

fn active_shortcut_scopes(state: &AppState) -> Vec<ShortcutScope> {
    match state.status {
        AppStatus::Idle => vec![ShortcutScope::Global, ShortcutScope::Idle],
        AppStatus::Running => vec![ShortcutScope::Global, ShortcutScope::Running],
        AppStatus::WaitingApproval => vec![ShortcutScope::Global, ShortcutScope::Approval],
        AppStatus::Setup | AppStatus::SessionPicker => vec![ShortcutScope::Global],
    }
}

fn render_approval_dialog(frame: &mut Frame, state: &AppState) {
    let Some(dialog) = &state.approval_dialog else {
        return;
    };

    let area = frame.area();
    let width = 44u16.min(area.width.saturating_sub(4));
    let height = 10u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let target_str = dialog.target.as_deref().unwrap_or("(none)");

    let allow_style = if dialog.selected == 0 {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let deny_style = if dialog.selected == 1 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let allow_prefix = if dialog.selected == 0 { "▸ " } else { "  " };
    let deny_prefix = if dialog.selected == 1 { "▸ " } else { "  " };

    let content = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Tool: ", Style::default().fg(Color::DarkGray)),
            Span::styled(dialog.tool.clone(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("  Target: ", Style::default().fg(Color::DarkGray)),
            Span::styled(target_str.to_string(), Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(Span::styled(format!("{allow_prefix}Allow"), allow_style)),
        Line::from(Span::styled(format!("{deny_prefix}Deny"), deny_style)),
        Line::from(""),
        Line::from(Span::styled(
            "  ↑↓ select, Enter confirm",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Approval Required ")
        .border_style(Style::default().fg(Color::Magenta));

    let paragraph = Paragraph::new(content).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_setup(frame: &mut Frame, state: &AppState, textarea: &TextArea) {
    let area = frame.area();

    match state.setup_step {
        0 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 16u16.min(area.height.saturating_sub(2));
            let x = (area.width.saturating_sub(width)) / 2;
            let y = (area.height.saturating_sub(height)) / 2;
            let popup_area = Rect::new(x, y, width, height);

            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "   ___                ",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    "  / _ \\ _ __ ___ __ _ ",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    " | | | | '__/ __/ _` |",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    " | |_| | | | (_| (_| |",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    "  \\___/|_|  \\___\\__,_|",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  A DeepSeek-native coding agent",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Let's get you set up!",
                    Style::default().fg(Color::Green),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press Enter to continue...",
                    Style::default().fg(Color::DarkGray),
                )),
            ];

            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Welcome ")
                .border_style(Style::default().fg(Color::Cyan));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
        }
        1 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 14u16.min(area.height.saturating_sub(2));
            let x = (area.width.saturating_sub(width)) / 2;
            let y = (area.height.saturating_sub(height)) / 2;
            let popup_area = Rect::new(x, y, width, height);

            let inner =
                Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).split(Rect::new(
                    popup_area.x + 1,
                    popup_area.y + 1,
                    popup_area.width.saturating_sub(2),
                    popup_area.height.saturating_sub(2),
                ));

            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Step 1: API Key",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Orca needs a DeepSeek API key to function.",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  https://platform.deepseek.com/api_keys",
                    Style::default().fg(Color::Blue),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Paste below and press Enter:",
                    Style::default().fg(Color::DarkGray),
                )),
            ];

            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Setup ")
                .border_style(Style::default().fg(Color::Cyan));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
            frame.render_widget(textarea, inner[1]);
        }
        2 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 12u16.min(area.height.saturating_sub(2));
            let x = (area.width.saturating_sub(width)) / 2;
            let y = (area.height.saturating_sub(height)) / 2;
            let popup_area = Rect::new(x, y, width, height);

            let content = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  ✓ API key saved successfully!",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Saved to: ~/.orca/auth.json",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  You're all set! Orca is ready to use.",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press Enter to start...",
                    Style::default().fg(Color::DarkGray),
                )),
            ];

            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Setup Complete ")
                .border_style(Style::default().fg(Color::Green));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
        }
        _ => {}
    }
}

fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(input, opts);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default().fg(Color::White)];
    let mut in_code_block = false;
    let mut list_depth: u16 = 0;

    // Table buffering state
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();

    for event in parser {
        // When inside a table, buffer content instead of rendering immediately
        if in_table {
            match event {
                Event::Start(Tag::TableHead) => {}
                Event::Start(Tag::TableRow) => {}
                Event::Start(Tag::TableCell) => {
                    current_cell.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    current_row.push(std::mem::take(&mut current_cell));
                }
                Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                    table_rows.push(std::mem::take(&mut current_row));
                }
                Event::End(TagEnd::Table) => {
                    render_table(&table_rows, &mut lines);
                    table_rows.clear();
                    in_table = false;
                }
                Event::Text(text) => {
                    current_cell.push_str(&text);
                }
                Event::Code(code) => {
                    current_cell.push('`');
                    current_cell.push_str(&code);
                    current_cell.push('`');
                }
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Table(_alignments)) => {
                flush_line(&mut current_spans, &mut lines);
                in_table = true;
                table_rows.clear();
            }
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let color = match level {
                        pulldown_cmark::HeadingLevel::H1 => Color::Cyan,
                        pulldown_cmark::HeadingLevel::H2 => Color::Green,
                        _ => Color::Yellow,
                    };
                    style_stack.push(Style::default().fg(color).add_modifier(Modifier::BOLD));
                }
                Tag::Strong => {
                    let base = *style_stack.last().unwrap_or(&Style::default());
                    style_stack.push(base.add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    let base = *style_stack.last().unwrap_or(&Style::default());
                    style_stack.push(base.add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(_) => {
                    flush_line(&mut current_spans, &mut lines);
                    in_code_block = true;
                }
                Tag::List(_) => {
                    list_depth += 1;
                }
                Tag::Item => {
                    let indent = "  ".repeat(list_depth.saturating_sub(1) as usize);
                    current_spans.push(Span::styled(
                        format!("{indent}• "),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Tag::BlockQuote(_) => {
                    current_spans.push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
                    let base = *style_stack.last().unwrap_or(&Style::default());
                    style_stack.push(base.fg(Color::Gray));
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                }
                TagEnd::Paragraph => {
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                }
                TagEnd::Item => {
                    flush_line(&mut current_spans, &mut lines);
                }
                TagEnd::BlockQuote(_) => {
                    style_stack.pop();
                    flush_line(&mut current_spans, &mut lines);
                }
                _ => {}
            },
            Event::Text(text) => {
                let style = if in_code_block {
                    Style::default().fg(Color::Gray)
                } else {
                    *style_stack.last().unwrap_or(&Style::default())
                };
                if in_code_block {
                    for code_line in text.lines() {
                        current_spans.push(Span::styled(format!("  {code_line}"), style));
                        flush_line(&mut current_spans, &mut lines);
                    }
                } else {
                    current_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!("`{code}`"),
                    Style::default().fg(Color::Magenta),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            _ => {}
        }
    }

    flush_line(&mut current_spans, &mut lines);
    lines
}

fn render_table(rows: &[Vec<String>], lines: &mut Vec<Line<'static>>) {
    if rows.is_empty() {
        return;
    }

    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return;
    }

    // Calculate column widths (minimum 3 chars per column)
    let mut col_widths: Vec<usize> = vec![3; num_cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }

    let border_style = Style::default().fg(Color::DarkGray);
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(Color::White);

    // Top border: ┌───┬───┐
    let top = format_table_border(&col_widths, '┌', '┬', '┐', '─');
    lines.push(Line::from(Span::styled(top, border_style)));

    for (row_idx, row) in rows.iter().enumerate() {
        // Data row: │ x │ y │
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("│", border_style));
        for (i, width) in col_widths.iter().enumerate() {
            let content = row.get(i).map(|s| s.as_str()).unwrap_or("");
            let display_width = UnicodeWidthStr::width(content);
            let padding = width.saturating_sub(display_width);
            let style = if row_idx == 0 {
                header_style
            } else {
                cell_style
            };
            spans.push(Span::styled(format!(" {content}"), style));
            spans.push(Span::styled(format!("{} ", " ".repeat(padding)), style));
            spans.push(Span::styled("│", border_style));
        }
        lines.push(Line::from(spans));

        // After header row: ├───┼───┤
        if row_idx == 0 {
            let sep = format_table_border(&col_widths, '├', '┼', '┤', '─');
            lines.push(Line::from(Span::styled(sep, border_style)));
        }
    }

    // Bottom border: └───┴───┘
    let bottom = format_table_border(&col_widths, '└', '┴', '┘', '─');
    lines.push(Line::from(Span::styled(bottom, border_style)));
    lines.push(Line::from(""));
}

fn format_table_border(
    col_widths: &[usize],
    left: char,
    mid: char,
    right: char,
    fill: char,
) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, &w) in col_widths.iter().enumerate() {
        // +2 for the padding spaces around content
        for _ in 0..w + 2 {
            s.push(fill);
        }
        if i < col_widths.len() - 1 {
            s.push(mid);
        }
    }
    s.push(right);
    s
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn truncate_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        lines.join(" ")
    } else {
        let joined: String = lines[..max_lines].join(" ");
        format!("{joined}...")
    }
}
