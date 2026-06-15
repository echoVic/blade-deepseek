use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tui_textarea::TextArea;

use super::types::{AppState, AppStatus, ChatMessage};

pub fn render(frame: &mut Frame, state: &mut AppState, textarea: &TextArea) {
    if state.status == AppStatus::Setup {
        render_setup(frame, state, textarea);
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
}

fn render_messages(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let lines = build_message_lines(state);

    let visible_height = area.height.saturating_sub(2);
    let total_lines = lines.len() as u16;

    state.total_lines = total_lines;
    state.visible_height = visible_height;

    let scroll = if state.auto_scroll {
        let max_scroll = total_lines.saturating_sub(visible_height);
        state.scroll_offset = max_scroll;
        max_scroll
    } else {
        let max_scroll = total_lines.saturating_sub(visible_height);
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
    ]);

    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
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
    let parser = Parser::new(input);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default().fg(Color::White)];
    let mut in_code_block = false;
    let mut list_depth: u16 = 0;

    for event in parser {
        match event {
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
