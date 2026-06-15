use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use tui_textarea::TextArea;

use super::types::{AppState, AppStatus, ChatMessage};

pub fn render(frame: &mut Frame, state: &AppState, textarea: &TextArea) {
    let chunks = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_messages(frame, chunks[0], state);
    render_input(frame, chunks[1], textarea);
    render_status(frame, chunks[2], state);
}

fn render_messages(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &state.messages {
        match msg {
            ChatMessage::User(text) => {
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Blue)),
                    Span::styled(text.as_str(), Style::default().fg(Color::Blue)),
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
                for line in text.lines() {
                    lines.push(Line::from(Span::raw(line.to_string())));
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
            ChatMessage::Error(text) => {
                lines.push(Line::from(Span::styled(
                    format!("ERROR: {text}"),
                    Style::default().fg(Color::Red),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    let visible_height = area.height.saturating_sub(2) as usize;
    let scroll = if lines.len() > visible_height {
        (lines.len() - visible_height) as u16
    } else {
        0
    };

    let block = Block::default().borders(Borders::ALL).title(" Orca ");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

fn render_input(frame: &mut Frame, area: Rect, textarea: &TextArea) {
    frame.render_widget(textarea, area);
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let (status_text, color) = match &state.status {
        AppStatus::Idle => ("● idle", Color::Green),
        AppStatus::Running => ("● running", Color::Yellow),
        AppStatus::WaitingApproval => ("● waiting approval [y/n]", Color::Magenta),
    };

    let approval_hint = if state.status == AppStatus::WaitingApproval {
        state
            .approval_info
            .as_deref()
            .map(|info| format!(" — {info}"))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(format!(" {status_text}"), Style::default().fg(color)),
        Span::styled(approval_hint, Style::default().fg(Color::Magenta)),
        Span::styled(
            format!(" | model: {}", state.model_name),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
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
