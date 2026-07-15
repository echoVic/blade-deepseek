use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph, Wrap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

use orca_core::approval_types::ApprovalMode;
use orca_core::task_types::{
    BackgroundTaskSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
};
use orca_core::workflow_types::{WorkflowAgentStatus, WorkflowRunStatus};
use orca_file_search::SearchPhase;
use orca_runtime::history::SessionSummary;

use crate::display_text::{compact_long_text, truncate_to_display_width};
use crate::shortcuts::{self, ShortcutScope};
use crate::theme::Theme;
use crate::transcript_view::viewport_paragraph;
use crate::types::{AppState, AppStatus, ApprovalOption, ChatMessage, PanelMode};

pub fn render(frame: &mut Frame, state: &mut AppState, textarea: &TextArea, theme: &Theme) {
    // Recomputed below when the widgets are actually shown; cleared here so
    // panel/status switches never leave stale mouse hit targets behind.
    state.jump_to_bottom_area = None;
    state.frame_area = Some(frame.area());
    state.input_area = None;
    if state.status == AppStatus::Setup {
        render_setup(frame, state, textarea, theme);
        return;
    }
    if state.status == AppStatus::SessionPicker {
        render_session_picker(frame, state, theme);
        return;
    }

    let input_height = if composer_visible(state) {
        composer_input_height(frame.area().width, textarea)
    } else {
        0
    };

    let plan_height = plan_panel_height(state);
    let goal_height: u16 = if state.current_goal.is_some() { 3 } else { 0 };
    // An activity indicator sits above the composer while the agent is working (or
    // waiting on the user), showing status + elapsed time. It takes two rows — a blank
    // spacer, then the text — so the transcript tail, the indicator, and the input box
    // don't sit flush against each other. Idle collapses it to zero height so a resting
    // session has no chrome noise there.
    let activity_height: u16 = if activity_line(state, theme).is_some() {
        2
    } else {
        0
    };

    let chunks = main_layout(
        frame.area(),
        goal_height,
        plan_height,
        activity_height,
        input_height,
    );

    if goal_height > 0 {
        render_goal_banner(frame, chunks[0], state, theme);
    }
    let compact_conversation_background = state.status == AppStatus::WaitingApproval;
    match state.panel_mode {
        PanelMode::Conversation => render_live_messages(frame, chunks[1], state, theme),
        PanelMode::Workflows => render_workflows_panel(frame, chunks[1], state, theme),
        PanelMode::Agents => render_agents_panel(frame, chunks[1], state, theme),
    }
    let _ = compact_conversation_background;
    if plan_height > 0 {
        render_plan_panel(frame, chunks[2], state, theme);
    }
    if activity_height > 0 {
        render_activity(frame, chunks[3], state, theme);
    }
    if composer_visible(state) {
        state.input_area = Some(chunks[4]);
        render_input(frame, chunks[4], textarea, state, theme);
    }
    render_status(frame, chunks[5], state, theme);

    if state.slash_menu.is_some() {
        render_slash_menu(frame, chunks[4], state, theme);
    }

    if state.mention.phase.is_some() && state.slash_menu.is_none() {
        render_mention_candidates(frame, chunks[4], state, theme);
    }

    if state.status == AppStatus::WaitingApproval {
        render_approval_dialog(frame, state, theme);
    }

    if state.show_shortcuts {
        render_shortcuts(frame, state, theme);
    }
}

fn main_layout(
    area: Rect,
    goal_height: u16,
    plan_height: u16,
    activity_height: u16,
    input_height: u16,
) -> std::rc::Rc<[Rect]> {
    // The fixed chrome (goal banner, plan, activity line, input box, status line) MUST keep
    // its height so the input box stays pinned at the bottom; the message transcript takes
    // whatever is left. In ratatui 0.29 `Min` has the HIGHEST solver priority and `Fill` the
    // LOWEST, so giving the transcript `Min(5)` made it steal rows from the `Length` chrome
    // when the transcript overflowed — the input box got squeezed off-screen and the
    // auto-scrolled tail landed behind it. `Fill(1)` makes the transcript yield instead.
    Layout::vertical([
        Constraint::Length(goal_height),
        Constraint::Fill(1),
        Constraint::Length(plan_height),
        Constraint::Length(activity_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(area)
}

/// A `width`×`height` rect centered inside `area`, clamped so it never extends past
/// `area`'s bounds.
///
/// Floating popups (setup, approval dialog, shortcuts, panel overlays) are positioned by
/// centering within `frame.area()`. Under the inline viewport, `frame.area()` does NOT start
/// at `(0, 0)` — its origin is wherever the viewport is anchored (e.g. `y: 31`). Computing the
/// offset as `(area.height - height) / 2` alone yields a coordinate relative to `(0, 0)`, so
/// the popup lands *above* the viewport's buffer and `Buffer::index_of` panics with "index
/// outside of buffer". Adding `area.x`/`area.y` keeps the popup inside the actual buffer; the
/// final `clamp`/`min` guarantees it stays in bounds even when `width`/`height` exceed `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn composer_visible(state: &AppState) -> bool {
    !matches!(state.status, AppStatus::WaitingApproval)
}

fn render_goal_banner(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    use orca_core::goal_types::{
        ThreadGoalStatus, format_goal_elapsed_seconds, format_tokens_compact, goal_status_label,
    };

    let Some(goal) = &state.current_goal else {
        return;
    };

    let status_color = match goal.status {
        ThreadGoalStatus::Active => theme.success,
        ThreadGoalStatus::Paused => theme.warning,
        ThreadGoalStatus::Blocked => theme.error,
        ThreadGoalStatus::UsageLimited | ThreadGoalStatus::BudgetLimited => theme.warning,
        ThreadGoalStatus::Complete => theme.success,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" ⌖ Goal ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut metadata_spans = vec![Span::styled(
        format!("● {}", goal_status_label(goal.status)),
        Style::default().fg(status_color),
    )];
    if goal.time_used_seconds > 0 {
        metadata_spans.push(Span::styled(
            format!(
                "  · {}",
                format_goal_elapsed_seconds(goal.time_used_seconds)
            ),
            Style::default().fg(theme.muted),
        ));
    }
    if goal.tokens_used > 0 {
        metadata_spans.push(Span::styled(
            format!("  · {} tok", format_tokens_compact(goal.tokens_used)),
            Style::default().fg(theme.muted),
        ));
    }
    if goal.status.should_continue() {
        metadata_spans.push(Span::styled(
            "  · auto-continue",
            Style::default().fg(theme.muted),
        ));
    }

    let metadata_width = metadata_spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let separator_width = 2usize;
    let objective_width = (inner.width as usize)
        .saturating_sub(metadata_width)
        .saturating_sub(separator_width);
    let objective = goal
        .objective
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let objective = truncate_to_display_width(&objective, objective_width);
    let has_objective = !objective.is_empty();

    let mut spans = vec![Span::styled(
        objective,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    )];
    if has_objective {
        spans.push(Span::raw("  "));
    }
    spans.append(&mut metadata_spans);

    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, inner);
}

fn render_session_picker(frame: &mut Frame, state: &mut AppState, theme: &Theme) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Resume Conversation ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let filtered = state.filtered_session_indices();
    let total = state.session_picker_sessions.len();

    let mut lines = Vec::new();

    // Search field: live query + match count.
    let query_display = if state.session_picker_query.is_empty() {
        Span::styled("type to filter…", Style::default().fg(theme.muted))
    } else {
        Span::styled(
            state.session_picker_query.clone(),
            Style::default().fg(theme.text),
        )
    };
    lines.push(Line::from(vec![
        Span::styled("⌕ ", Style::default().fg(theme.border)),
        query_display,
        Span::styled(
            format!("    {}/{} matches", filtered.len(), total),
            Style::default().fg(theme.muted),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "↑↓ select · Enter resume · Backspace edit · Esc quit",
        Style::default().fg(theme.muted),
    )));
    lines.push(Line::from(""));

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No sessions match this filter.",
            Style::default().fg(theme.muted),
        )));
    }

    let needle = state.session_picker_query.to_lowercase();
    for &index in &filtered {
        let session = &state.session_picker_sessions[index];
        let selected = index == state.session_picker_selected;
        let marker = if selected { "> " } else { "  " };
        let base = if selected {
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        let mut spans = vec![Span::styled(marker, base)];
        // Highlight the matched substring inside the title.
        spans.extend(highlight_match(&session.title, &needle, base, theme));
        spans.push(Span::styled(
            format!(
                "  {}  {}",
                session.updated_at.format("%Y-%m-%d %H:%M"),
                session.provider
            ),
            Style::default().fg(theme.muted),
        ));
        lines.push(Line::from(spans));

        if let Some(metadata) = session_permission_metadata_label(session) {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(metadata, Style::default().fg(theme.muted)),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn session_permission_metadata_label(session: &SessionSummary) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(profile) = &session.active_permission_profile {
        parts.push(format!("profile {}", profile.id));
    }
    if session.permission_rule_count > 0 {
        parts.push(format!(
            "{} rule{}",
            session.permission_rule_count,
            if session.permission_rule_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if !session.additional_working_directories.is_empty() {
        let labels = session
            .additional_working_directories
            .iter()
            .map(|entry| {
                format!(
                    "{} {}",
                    entry.source,
                    workspace_relative_path_label(&entry.path, &session.runtime_workspace_roots)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("dirs {labels}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("  "))
    }
}

fn workspace_relative_path_label(path: &Path, runtime_workspace_roots: &[PathBuf]) -> String {
    let Some(root) = runtime_workspace_roots
        .iter()
        .filter(|root| path == root.as_path() || path.starts_with(root))
        .max_by_key(|root| root.components().count())
    else {
        return path.display().to_string();
    };

    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ":workspace_roots".to_string(),
        Ok(relative) => format!(":workspace_roots/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Split `text` into styled spans, highlighting the first case-insensitive
/// occurrence of `needle` with the theme warning color. Empty needle returns
/// the whole text in `base` style.
fn highlight_match(text: &str, needle: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    if needle.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let lower = text.to_lowercase();
    let Some(start) = lower.find(needle) else {
        return vec![Span::styled(text.to_string(), base)];
    };
    let end = start + needle.len();
    let hl = base.fg(theme.warning).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    if start > 0 {
        spans.push(Span::styled(text[..start].to_string(), base));
    }
    spans.push(Span::styled(text[start..end].to_string(), hl));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_string(), base));
    }
    spans
}

/// Render the transcript messages into `area` with no border. While `auto_scroll` is on
/// the newest content is pinned to the bottom of `area`; once the user scrolls up
/// (PageUp, k/j, etc.) `auto_scroll` clears and the pane honours `scroll_offset`.
/// Overlay the mouse selection on materialized transcript rows. The render
/// caches stay selection-agnostic so highlighting never invalidates them.
fn apply_selection_overlay(
    lines: Vec<ratatui::text::Line<'static>>,
    selection: Option<crate::selection::TranscriptSelection>,
    base_row: usize,
    theme: &Theme,
) -> Vec<ratatui::text::Line<'static>> {
    let Some(selection) = selection else {
        return lines;
    };
    lines
        .into_iter()
        .enumerate()
        .map(
            |(index, line)| match selection.cols_on_row(base_row + index) {
                Some((col_start, col_end)) => crate::selection::apply_selection_to_line(
                    line,
                    col_start,
                    col_end,
                    theme.selection_bg,
                ),
                None => line,
            },
        )
        .collect()
}

pub(crate) fn render_live_messages(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    theme: &Theme,
) {
    let width = area.width.max(1) as usize;
    let visible_height = area.height as usize;
    state.reconcile_message_tracking();
    state.transcript_area = Some(area);

    if state.messages.is_empty() {
        // The welcome screen renders through its own cache so its text is
        // selectable and copyable exactly like transcript content.
        let lines = build_welcome_lines(state, theme);
        let welcome_message = [ChatMessage::System(String::new())];
        // Sentinel revision: never collides with allocated ones, and the
        // explicit invalidate below forces a rebuild whenever we redraw.
        let welcome_revisions = [u64::MAX];
        state.welcome_render_cache.invalidate(0);
        state.welcome_render_cache.prepare(
            &welcome_message,
            &welcome_revisions,
            width,
            theme,
            state.tick,
            false,
            |_, _, _, _, _| lines.clone(),
        );
        let requested_scroll = if state.auto_scroll {
            usize::MAX
        } else {
            state.scroll_offset
        };
        let viewport = state
            .welcome_render_cache
            .viewport(0, requested_scroll, visible_height);
        state.total_lines = viewport.total_height;
        state.visible_height = visible_height;
        state.scroll_offset = viewport.scroll_offset;
        state.viewport_base_row = viewport.base_row;
        let lines =
            apply_selection_overlay(viewport.lines, state.selection, viewport.base_row, theme);
        frame.render_widget(viewport_paragraph(lines), area);
        return;
    }

    let requested_scroll = if state.auto_scroll {
        usize::MAX
    } else {
        state.scroll_offset
    };
    let live_start = state.flushed_count.min(state.messages.len());
    state.transcript_render_cache.prepare(
        &state.messages,
        &state.message_revisions,
        width,
        theme,
        state.tick,
        false,
        |message, theme, width, tick, force_expand| {
            build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
    let viewport =
        state
            .transcript_render_cache
            .viewport(live_start, requested_scroll, visible_height);
    state.total_lines = viewport.total_height;
    state.visible_height = visible_height;
    state.scroll_offset = viewport.scroll_offset;
    state.viewport_base_row = viewport.base_row;

    // Overlay the mouse selection on the materialized rows; the render cache
    // itself stays selection-agnostic so highlighting never invalidates it.
    let lines = apply_selection_overlay(viewport.lines, state.selection, viewport.base_row, theme);

    frame.render_widget(viewport_paragraph(lines), area);

    // Floating "jump to bottom" pill, shown while the user has scrolled away
    // from the tail (auto-follow disarmed). While detached it doubles as an
    // unread indicator: messages landing below bump `unseen_messages`.
    // Clicking it re-arms follow and clears the count.
    if !state.auto_scroll && viewport.total_height > visible_height && area.height > 0 {
        let label = match state.unseen_messages {
            0 => " Jump to bottom (click) ↓ ".to_string(),
            1 => " 1 new message (click) ↓ ".to_string(),
            count => format!(" {count} new messages (click) ↓ "),
        };
        let pill_width = UnicodeWidthStr::width(label.as_str()) as u16;
        if area.width >= pill_width {
            let pill = Rect {
                x: area.x + (area.width - pill_width) / 2,
                y: area.y + area.height - 1,
                width: pill_width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().bg(theme.selection_bg).fg(theme.text),
                )),
                pill,
            );
            state.jump_to_bottom_area = Some(pill);
        }
    }
}

fn render_workflows_panel(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Tasks ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tasks = state.workflow_panel.tasks.iter().collect::<Vec<_>>();

    if tasks.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                " No background tasks available in this view yet.",
                Style::default().fg(theme.muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    // One hint row + one header row + task rows. The selected workflow expands into
    // phase and per-agent rows so the panel can act as a lightweight dashboard.
    let hint_h: u16 = 1;
    let header_h: u16 = 1;
    let row_h: u16 = 2;
    let mut constraints = vec![Constraint::Length(hint_h), Constraint::Length(header_h)];
    constraints.extend(tasks.iter().enumerate().map(|(index, task)| {
        let detail_rows = if index == state.workflow_panel.selected {
            workflow_metadata_row_count(task)
                + workflow_phase_detail_rows(task).len() as u16
                + task.workflow_agents.len() as u16
        } else {
            0
        };
        Constraint::Length(row_h.saturating_add(detail_rows))
    }));
    constraints.push(Constraint::Min(0));
    let rows = Layout::vertical(constraints).split(inner);

    let selected_task = state
        .workflow_panel
        .tasks
        .get(state.workflow_panel.selected);
    frame.render_widget(
        Paragraph::new(workflow_panel_action_hint(selected_task, theme)),
        rows[0],
    );

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Name", Style::default().fg(theme.muted)),
        Span::styled("   Type", Style::default().fg(theme.muted)),
        Span::styled("       Status", Style::default().fg(theme.muted)),
        Span::styled("      Detail", Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(header, rows[1]);

    for (index, task) in tasks.iter().enumerate() {
        let row_area = rows[index + 2];
        let selected = index == state.workflow_panel.selected;
        let marker = if selected { ">" } else { " " };
        let name = task.name.as_deref().unwrap_or(task.description.as_str());
        let task_type = task_type_label(task);
        let status = task_status_label(task.status);
        let status_color = task_status_color(task.status, theme);
        let detail = task_detail_label(task);
        let name_style = if selected {
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        // Split the row into a label line, a gauge line, and optional agent rows.
        let mut row_constraints = vec![Constraint::Length(1), Constraint::Length(1)];
        let metadata_rows = if selected {
            workflow_metadata_rows(task, theme)
        } else {
            Vec::new()
        };
        if selected {
            row_constraints.extend(metadata_rows.iter().map(|_| Constraint::Length(1)));
            row_constraints.extend(
                workflow_phase_detail_rows(task)
                    .iter()
                    .map(|_| Constraint::Length(1)),
            );
            row_constraints.extend(task.workflow_agents.iter().map(|_| Constraint::Length(1)));
        }
        let parts = Layout::vertical(row_constraints).split(row_area);

        let label = Paragraph::new(Line::from(vec![
            Span::styled(format!("{marker} {name}"), name_style),
            Span::styled("  ", Style::default()),
            Span::styled(task_type, Style::default().fg(theme.muted)),
            Span::styled("  ", Style::default()),
            Span::styled(status.to_string(), Style::default().fg(status_color)),
            Span::styled(format!("  {detail}"), Style::default().fg(theme.muted)),
        ]));
        frame.render_widget(label, parts[0]);

        // Gauge ratio reflects lifecycle, not fabricated progress: terminal
        // states fill the bar, queued/paused stay empty, and a running task
        // shows a tick-driven activity pulse. The status word stays in the
        // label so a moving bar can't be misread as a real percentage.
        let ratio = workflow_gauge_ratio(task.status, state.tick);
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(status_color).bg(theme.muted))
            .ratio(ratio)
            .label(Span::styled(
                workflow_gauge_label(task.status),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ));
        frame.render_widget(gauge, parts[1]);

        if selected {
            let phase_rows = workflow_phase_detail_rows(task);
            for (metadata_index, line) in metadata_rows.iter().enumerate() {
                frame.render_widget(Paragraph::new(line.clone()), parts[metadata_index + 2]);
            }
            let detail_offset = metadata_rows.len() + 2;
            for (phase_index, phase) in phase_rows.iter().enumerate() {
                let line = Paragraph::new(workflow_phase_row_label(phase, theme));
                frame.render_widget(line, parts[detail_offset + phase_index]);
            }
            for (agent_index, agent) in task.workflow_agents.iter().enumerate() {
                let line = Paragraph::new(agent_row_label(agent, theme));
                frame.render_widget(line, parts[detail_offset + phase_rows.len() + agent_index]);
            }
        }
    }
}

fn workflow_panel_action_hint<'a>(
    selected_task: Option<&BackgroundTaskSummary>,
    theme: &Theme,
) -> Line<'a> {
    let mut spans = vec![Span::styled(" ↑↓ select", Style::default().fg(theme.muted))];
    if selected_task.is_some_and(is_approval_actionable_task) {
        spans.push(Span::styled(
            " · Enter approve",
            Style::default().fg(theme.muted),
        ));
    }
    if selected_task.is_some_and(is_stoppable_task) {
        spans.push(Span::styled(" · s stop", Style::default().fg(theme.muted)));
    }
    if selected_task.is_some_and(is_foregroundable_task) {
        spans.push(Span::styled(
            " · f foreground",
            Style::default().fg(theme.muted),
        ));
    }
    spans.push(Span::styled(
        " · Esc close",
        Style::default().fg(theme.muted),
    ));
    Line::from(spans)
}

fn is_approval_actionable_task(task: &BackgroundTaskSummary) -> bool {
    task.task_type == TaskType::MainSession
        && task.status == TaskStatus::ApprovalRequired
        && task.is_backgrounded
        && task.pending_tool_call.is_some()
}

fn is_stoppable_task(task: &BackgroundTaskSummary) -> bool {
    !matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Stopped
    )
}

fn is_foregroundable_task(task: &BackgroundTaskSummary) -> bool {
    task.task_type == TaskType::MainSession
        && task.status == TaskStatus::Running
        && task.is_backgrounded
}

fn render_agents_panel(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Agents ")
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = state
        .workflow_panel
        .tasks
        .iter()
        .flat_map(|task| {
            let workflow_name = task.name.as_deref().unwrap_or(task.description.as_str());
            task.workflow_agents
                .iter()
                .map(move |agent| (workflow_name, agent))
        })
        .collect::<Vec<_>>();

    if rows.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                " No workflow agents available yet.",
                Style::default().fg(theme.muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    }

    let mut constraints = vec![Constraint::Length(1)];
    constraints.extend(rows.iter().map(|_| Constraint::Length(1)));
    constraints.push(Constraint::Min(0));
    let areas = Layout::vertical(constraints).split(inner);
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" Workflow", Style::default().fg(theme.muted)),
        Span::styled("   Agent", Style::default().fg(theme.muted)),
        Span::styled("      Status", Style::default().fg(theme.muted)),
        Span::styled("      Detail", Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(header, areas[0]);

    for (index, (workflow_name, agent)) in rows.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(agent_dashboard_row_label(workflow_name, agent, theme)),
            areas[index + 1],
        );
    }
}

fn workflow_phase_detail_rows(
    task: &BackgroundTaskSummary,
) -> Vec<&orca_core::task_types::WorkflowPhaseTaskSummary> {
    task.workflow_phases
        .iter()
        .filter(|phase| {
            phase.error.is_some()
                || phase.fallback.is_some()
                || matches!(
                    phase.status,
                    WorkflowRunStatus::Failed
                        | WorkflowRunStatus::Cancelled
                        | WorkflowRunStatus::Stopped
                )
        })
        .collect()
}

const TASK_DETAIL_MAX_LINES: usize = 3;
const TASK_DETAIL_MAX_CHARS: usize = 120;

fn workflow_metadata_row_count(task: &BackgroundTaskSummary) -> u16 {
    u16::from(task.workflow_run_id.is_some())
        + u16::from(task.workflow_script_path.is_some())
        + u16::from(task.workflow_launch_input.is_some())
        + u16::from(task.workflow_failure_count > 0)
        + u16::from(task.workflow_final_summary.is_some())
        + task
            .result
            .as_deref()
            .map(task_detail_line_count)
            .unwrap_or_default() as u16
        + task
            .error
            .as_deref()
            .map(task_detail_line_count)
            .unwrap_or_default() as u16
}

fn workflow_metadata_rows<'a>(task: &BackgroundTaskSummary, theme: &Theme) -> Vec<Line<'a>> {
    let mut rows = Vec::new();
    if let Some(run_id) = &task.workflow_run_id {
        rows.push(Line::from(vec![
            Span::styled("    run ", Style::default().fg(theme.muted)),
            Span::styled(run_id.clone(), Style::default().fg(theme.text)),
        ]));
    }
    if let Some(script_path) = &task.workflow_script_path {
        rows.push(Line::from(vec![
            Span::styled("    script ", Style::default().fg(theme.muted)),
            Span::styled(script_path.clone(), Style::default().fg(theme.text)),
        ]));
    }
    if let Some(launch_input) = &task.workflow_launch_input {
        rows.push(Line::from(vec![
            Span::styled("    launch ", Style::default().fg(theme.muted)),
            Span::styled(
                workflow_launch_input_label(launch_input),
                Style::default().fg(theme.text),
            ),
        ]));
    }
    if task.workflow_failure_count > 0 {
        rows.push(Line::from(vec![
            Span::styled("    failures ", Style::default().fg(theme.muted)),
            Span::styled(
                task.workflow_failure_count.to_string(),
                Style::default().fg(theme.error),
            ),
        ]));
    }
    if let Some(summary) = &task.workflow_final_summary {
        rows.push(Line::from(vec![
            Span::styled("    final ", Style::default().fg(theme.muted)),
            Span::styled(summary.clone(), Style::default().fg(theme.text)),
        ]));
    }
    if let Some(result) = &task.result {
        rows.extend(task_detail_text_rows(
            "result",
            result,
            Style::default().fg(theme.success),
            Style::default().fg(theme.text),
        ));
    }
    if let Some(error) = &task.error {
        rows.extend(task_detail_text_rows(
            "error",
            error,
            Style::default().fg(theme.error),
            Style::default().fg(theme.error),
        ));
    }
    rows
}

fn task_detail_line_count(text: &str) -> usize {
    text.lines().count().max(1).min(TASK_DETAIL_MAX_LINES)
}

fn task_detail_text_rows<'a>(
    label: &str,
    text: &str,
    label_style: Style,
    text_style: Style,
) -> Vec<Line<'a>> {
    let mut lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push("");
    }
    let truncated = lines.len() > TASK_DETAIL_MAX_LINES;
    lines.truncate(TASK_DETAIL_MAX_LINES);

    let prefix = format!("    {label} ");
    let continuation_prefix = " ".repeat(prefix.chars().count());
    let last_index = lines.len().saturating_sub(1);

    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix_text = if index == 0 {
                prefix.clone()
            } else {
                continuation_prefix.clone()
            };
            let mut detail = clamp_label(line, TASK_DETAIL_MAX_CHARS);
            if truncated && index == last_index && !detail.ends_with('…') {
                detail.push('…');
            }
            Line::from(vec![
                Span::styled(prefix_text, label_style),
                Span::styled(detail, text_style),
            ])
        })
        .collect()
}

fn workflow_launch_input_label(input: &orca_core::workflow_types::WorkflowInput) -> String {
    let mut parts = Vec::new();
    if let Some(draft_id) = input.draft_id.as_deref() {
        parts.push(format!("draftId={draft_id}"));
    }
    if let Some(name) = input.name.as_deref() {
        parts.push(format!("name={name}"));
    }
    if let Some(script_path) = input.script_path.as_deref() {
        parts.push(format!("scriptPath={script_path}"));
    }
    if let Some(resume_from) = input.resume_from_run_id.as_deref() {
        parts.push(format!("resumeFrom={resume_from}"));
    }
    if let Some(args) = &input.args {
        parts.push(format!("args={args}"));
    }
    if parts.is_empty() {
        "inline script".to_string()
    } else {
        parts.join(" ")
    }
}

/// Truthful gauge fill for a workflow lifecycle state.
///
/// We don't have a completed-phase count in the task model, so we never
/// invent a percentage. Terminal states fill the bar; queued/paused are
/// empty; running animates a bounded pulse from the UI tick.
fn workflow_gauge_ratio(status: TaskStatus, tick: u64) -> f64 {
    match status {
        TaskStatus::Completed => 1.0,
        TaskStatus::Failed | TaskStatus::Cancelled => 1.0,
        TaskStatus::Queued
        | TaskStatus::Paused
        | TaskStatus::Stopped
        | TaskStatus::ApprovalRequired => 0.0,
        TaskStatus::Running | TaskStatus::Stopping => {
            // Triangle wave in [0.15, 0.85] so the bar visibly breathes.
            let period = 20u64;
            let phase = (tick % period) as f64 / period as f64;
            let tri = if phase < 0.5 {
                phase * 2.0
            } else {
                2.0 - phase * 2.0
            };
            0.15 + tri * 0.7
        }
    }
}

fn workflow_gauge_label(status: TaskStatus) -> String {
    match status {
        TaskStatus::Completed => "done".to_string(),
        TaskStatus::Failed => "failed".to_string(),
        TaskStatus::ApprovalRequired => "approval required".to_string(),
        TaskStatus::Cancelled => "cancelled".to_string(),
        TaskStatus::Queued => "queued".to_string(),
        TaskStatus::Paused => "paused".to_string(),
        TaskStatus::Stopped => "stopped".to_string(),
        TaskStatus::Running => "running…".to_string(),
        TaskStatus::Stopping => "stopping…".to_string(),
    }
}

fn task_type_label(task: &BackgroundTaskSummary) -> &'static str {
    match task.task_type {
        TaskType::MainSession => "session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

fn task_detail_label(task: &BackgroundTaskSummary) -> String {
    match task.task_type {
        TaskType::Workflow => workflow_progress_label(task),
        TaskType::Subagent => subagent_progress_label(task),
        TaskType::MainSession
            if task.is_backgrounded && task.status == TaskStatus::ApprovalRequired =>
        {
            if let Some(tool) = task.tool.as_deref() {
                return format!("waiting on {tool} • backgrounded • {}", elapsed_label(task));
            }
            format!("backgrounded • {}", elapsed_label(task))
        }
        TaskType::MainSession if task.is_backgrounded => {
            format!("backgrounded • {}", elapsed_label(task))
        }
        TaskType::MainSession | TaskType::Shell | TaskType::Monitor => elapsed_label(task),
    }
}

fn workflow_progress_label(task: &BackgroundTaskSummary) -> String {
    let total_phases = task.phase_count.unwrap_or_default();
    let Some(progress) = task.workflow_progress else {
        return match task.phase_count {
            Some(count) => format!("{count} phases"),
            None => "phases -".to_string(),
        };
    };

    let mut parts = vec![format!(
        "agents {}/{}",
        progress.completed_agents, progress.total_agents
    )];
    if progress.running_agents > 0 {
        parts.push(format!("running {}", progress.running_agents));
    }
    if progress.failed_agents > 0 {
        parts.push(format!("failed {}", progress.failed_agents));
    }

    let phase_total = if total_phases == 0 {
        progress
            .completed_phases
            .saturating_add(progress.running_phases)
            .saturating_add(progress.failed_phases)
    } else {
        total_phases
    };
    parts.push(format!(
        "phases {}/{}",
        progress.completed_phases, phase_total
    ));
    parts.join(", ")
}

fn agent_row_label<'a>(agent: &WorkflowAgentTaskSummary, theme: &Theme) -> Line<'a> {
    let status = workflow_agent_status_label(agent.status);
    let status_color = workflow_agent_status_color(agent.status, theme);
    let attempt = format!("attempt {}/{}", agent.attempt, agent.max_attempts);
    let retry = if agent.previous_errors.is_empty() {
        "retry errors 0".to_string()
    } else {
        format!("retry errors {}", agent.previous_errors.len())
    };
    let team = agent
        .team
        .as_deref()
        .map(|team| format!("  team {team}"))
        .unwrap_or_default();
    let elapsed = agent_elapsed_label(agent)
        .map(|elapsed| format!("  {elapsed}"))
        .unwrap_or_default();
    let usage = agent
        .usage
        .map(|usage| {
            format!(
                "  {} tok ${:.6}",
                usage.total_tokens(),
                usage.estimated_cost_usd
            )
        })
        .unwrap_or_default();
    let error = agent
        .error
        .as_deref()
        .or_else(|| agent.previous_errors.last().map(String::as_str));
    let detail = error.map(|error| format!("  {error}")).unwrap_or_default();

    Line::from(vec![
        Span::styled("    ", Style::default()),
        Span::styled(agent.call_path.clone(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled(team, Style::default().fg(theme.muted)),
        Span::styled(format!("  {attempt}"), Style::default().fg(theme.muted)),
        Span::styled(format!("  {retry}"), Style::default().fg(theme.muted)),
        Span::styled(elapsed, Style::default().fg(theme.muted)),
        Span::styled(usage, Style::default().fg(theme.muted)),
        Span::styled(detail, Style::default().fg(theme.error)),
    ])
}

fn agent_dashboard_row_label<'a>(
    workflow_name: &str,
    agent: &WorkflowAgentTaskSummary,
    theme: &Theme,
) -> Line<'a> {
    let status = workflow_agent_status_label(agent.status);
    let status_color = workflow_agent_status_color(agent.status, theme);
    let attempt = format!("attempt {}/{}", agent.attempt, agent.max_attempts);
    let team = agent
        .team
        .as_deref()
        .map(|team| format!("  team {team}"))
        .unwrap_or_default();
    let elapsed = agent_elapsed_label(agent)
        .map(|elapsed| format!("  {elapsed}"))
        .unwrap_or_default();
    let usage = agent
        .usage
        .map(|usage| {
            format!(
                "  {} tok ${:.6}",
                usage.total_tokens(),
                usage.estimated_cost_usd
            )
        })
        .unwrap_or_default();
    let retry = if agent.previous_errors.is_empty() {
        String::new()
    } else {
        format!("  retry errors {}", agent.previous_errors.len())
    };
    let error = agent
        .error
        .as_deref()
        .or_else(|| agent.previous_errors.last().map(String::as_str))
        .map(|error| format!("  {error}"))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(workflow_name.to_string(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(agent.call_path.clone(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(status, Style::default().fg(status_color)),
        Span::styled(team, Style::default().fg(theme.muted)),
        Span::styled(format!("  {attempt}"), Style::default().fg(theme.muted)),
        Span::styled(elapsed, Style::default().fg(theme.muted)),
        Span::styled(usage, Style::default().fg(theme.muted)),
        Span::styled(retry, Style::default().fg(theme.muted)),
        Span::styled(error, Style::default().fg(theme.error)),
    ])
}

fn workflow_phase_row_label<'a>(
    phase: &orca_core::task_types::WorkflowPhaseTaskSummary,
    theme: &Theme,
) -> Line<'a> {
    let status = task_status_from_workflow_status(phase.status);
    let status_color = task_status_color(status, theme);
    let fallback = phase
        .fallback
        .as_deref()
        .map(|fallback| format!("  fallback {fallback}"))
        .unwrap_or_default();
    let error = phase
        .error
        .as_deref()
        .map(|error| format!("  {error}"))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled("    phase ", Style::default().fg(theme.muted)),
        Span::styled(phase.name.clone(), Style::default().fg(theme.text)),
        Span::styled("  ", Style::default()),
        Span::styled(
            workflow_run_status_label(phase.status),
            Style::default().fg(status_color),
        ),
        Span::styled(
            format!("  agents {}", phase.agent_count),
            Style::default().fg(theme.muted),
        ),
        Span::styled(fallback, Style::default().fg(theme.muted)),
        Span::styled(error, Style::default().fg(theme.error)),
    ])
}

fn workflow_run_status_label(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Queued => "queued",
        WorkflowRunStatus::Running => "running",
        WorkflowRunStatus::AsyncLaunched => "async",
        WorkflowRunStatus::Paused => "paused",
        WorkflowRunStatus::Stopping => "stopping",
        WorkflowRunStatus::Stopped => "stopped",
        WorkflowRunStatus::Completed => "completed",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
    }
}

fn task_status_from_workflow_status(status: WorkflowRunStatus) -> TaskStatus {
    match status {
        WorkflowRunStatus::Queued => TaskStatus::Queued,
        WorkflowRunStatus::Running | WorkflowRunStatus::AsyncLaunched => TaskStatus::Running,
        WorkflowRunStatus::Paused => TaskStatus::Paused,
        WorkflowRunStatus::Stopping => TaskStatus::Stopping,
        WorkflowRunStatus::Stopped => TaskStatus::Stopped,
        WorkflowRunStatus::Completed => TaskStatus::Completed,
        WorkflowRunStatus::Failed => TaskStatus::Failed,
        WorkflowRunStatus::Cancelled => TaskStatus::Cancelled,
    }
}

fn workflow_agent_status_label(status: WorkflowAgentStatus) -> &'static str {
    match status {
        WorkflowAgentStatus::Pending => "pending",
        WorkflowAgentStatus::Running => "running",
        WorkflowAgentStatus::Cached => "cached",
        WorkflowAgentStatus::Completed => "completed",
        WorkflowAgentStatus::Failed => "failed",
        WorkflowAgentStatus::Cancelled => "cancelled",
    }
}

fn workflow_agent_status_color(status: WorkflowAgentStatus, theme: &Theme) -> Color {
    match status {
        WorkflowAgentStatus::Completed | WorkflowAgentStatus::Cached => theme.success,
        WorkflowAgentStatus::Failed | WorkflowAgentStatus::Cancelled => theme.error,
        WorkflowAgentStatus::Running => theme.warning,
        WorkflowAgentStatus::Pending => theme.muted,
    }
}

fn subagent_progress_label(task: &BackgroundTaskSummary) -> String {
    let mut parts = Vec::new();
    if let Some(agent_type) = task.agent_type.as_deref() {
        parts.push(agent_type.to_string());
    }
    if let Some(turn) = task.subagent_turn {
        parts.push(format!("turn {turn}"));
    }
    parts.push(elapsed_label(task));
    if let Some(usage) = task.usage {
        parts.push(format!(
            "{} tok ${:.6}",
            usage.total_tokens(),
            usage.estimated_cost_usd
        ));
    }
    // The activity carries a tool target of arbitrary length (often a full
    // shell command), so it is clamped and rendered last: when the row
    // truncates, the fixed-width fields stay visible.
    if let Some(activity) = task.subagent_current_activity.as_deref() {
        parts.push(clamp_label(activity, 32));
    }
    parts.join(", ")
}

fn clamp_label(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let clamped: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{clamped}…")
}

fn elapsed_label(task: &BackgroundTaskSummary) -> String {
    let Some(started_at_ms) = task.started_at_ms else {
        return "not started".to_string();
    };
    let end_ms = task.completed_at_ms.unwrap_or_else(current_time_ms);
    let elapsed_ms = end_ms.saturating_sub(started_at_ms);
    format!(
        "elapsed {}",
        format_elapsed_compact((elapsed_ms / 1000) as u64)
    )
}

fn agent_elapsed_label(agent: &WorkflowAgentTaskSummary) -> Option<String> {
    let started_at_ms = agent.started_at_ms?;
    let end_ms = agent.completed_at_ms.unwrap_or_else(current_time_ms);
    let elapsed_ms = end_ms.saturating_sub(started_at_ms);
    Some(format!(
        "elapsed {}",
        format_elapsed_compact((elapsed_ms / 1000) as u64)
    ))
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn build_welcome_lines<'a>(state: &AppState, theme: &Theme) -> Vec<Line<'a>> {
    let cyan = Style::default().fg(theme.border);
    let text = Style::default().fg(theme.text);
    let muted = Style::default().fg(theme.muted);

    vec![
        Line::from(""),
        Line::from(Span::styled("   ___                ", cyan)),
        Line::from(Span::styled("  / _ \\ _ __ ___ __ _ ", cyan)),
        Line::from(Span::styled(" | | | | '__/ __/ _` |", cyan)),
        Line::from(Span::styled(" | |_| | | | (_| (_| |", cyan)),
        Line::from(vec![
            Span::styled("  \\___/|_|  \\___\\__,_|", cyan),
            Span::styled(format!("  v{}", state.app_version), muted),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  model:      ", muted),
            Span::styled(state.model_name.clone(), text),
        ]),
        Line::from(vec![
            Span::styled("  directory:  ", muted),
            Span::styled(state.cwd.clone(), text),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Tips", Style::default().fg(theme.success))),
        Line::from(Span::styled(
            "  • Enter to send, Alt+Enter (or Shift+Enter) for newline",
            muted,
        )),
        Line::from(Span::styled(
            "  • / commands, @ to mention files, $ to invoke skills",
            muted,
        )),
        Line::from(Span::styled(
            "  • /model to switch model, /compact to compress context",
            muted,
        )),
        Line::from(Span::styled(
            "  • Ctrl+K or F1 for keyboard shortcuts",
            muted,
        )),
        Line::from(""),
    ]
}

/// Render the lines for a contiguous slice of messages. Used both to flush a settled
/// prefix into the terminal scrollback and to draw the live bottom pane, so the two
/// surfaces stay pixel-identical.
///
/// `force_expand` overrides each tool/subagent's collapsed view and renders its full
/// output. The flush path sets this so a completed tool's output is committed to the
/// immutable scrollback in full — once flushed it can never be re-expanded, so we must
/// not freeze a truncated view. The live pane passes `false` and honours the per-message
/// `expanded` flag that `e` toggles.
pub(crate) fn build_lines_for_messages(
    messages: &[ChatMessage],
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for msg in messages {
        append_message_lines(&mut lines, msg, theme, width, tick, force_expand);
    }
    lines
}

/// Append the rendered lines for a single chat message. Pure with respect to global
/// state: the only dynamic input is `tick`, which drives the running-tool spinner.
fn append_message_lines(
    lines: &mut Vec<Line<'static>>,
    msg: &ChatMessage,
    theme: &Theme,
    width: usize,
    tick: u64,
    force_expand: bool,
) {
    match msg {
        ChatMessage::User(text) => {
            let text = compact_long_text(text, width.saturating_sub(2).max(1), 3);
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(theme.user)),
                Span::styled(text, Style::default().fg(theme.user)),
            ]));
            lines.push(Line::from(""));
        }
        ChatMessage::Reasoning(text) => {
            let prefix = Span::styled(
                "[thinking] ",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::ITALIC),
            );
            let truncated = truncate_lines(text, 3);
            lines.push(Line::from(vec![
                prefix,
                Span::styled(
                    truncated,
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        ChatMessage::Assistant(text) => {
            let md_lines = render_markdown(text, width);
            for line in md_lines {
                lines.push(line);
            }
            lines.push(Line::from(""));
        }
        ChatMessage::ProposedPlan(text) => {
            append_proposed_plan_lines(lines, text, width, theme);
        }
        ChatMessage::ToolCall {
            name,
            target,
            status,
            output,
            diff,
            kind,
            expanded,
            ..
        } => {
            let neutral_completed =
                status == "completed" && matches!(kind.as_deref(), Some("empty" | "no_matches"));
            let icon = match status.as_str() {
                "completed" => "✓",
                "running" | "receiving" => spinner_frame(tick),
                "denied" => "✗",
                "failed" => "✗",
                "cancelled" => "×",
                "indeterminate" => "?",
                _ => "·",
            };
            let color = match status.as_str() {
                "completed" if neutral_completed => theme.muted,
                "completed" => theme.success,
                "running" | "receiving" => theme.warning,
                "denied" | "failed" => theme.error,
                "cancelled" | "indeterminate" => theme.warning,
                _ => theme.muted,
            };
            let display_status = match status.as_str() {
                "cancelled" => "interrupted",
                "indeterminate" => "state unknown",
                status => status,
            };
            let prefix = format!("  {icon} {name}");
            let status_text = format!(" ({display_status})");
            let reserved_width = UnicodeWidthStr::width(prefix.as_str())
                + UnicodeWidthStr::width(status_text.as_str());
            let target_width =
                width.saturating_sub(reserved_width + 2 * usize::from(target.is_some()));
            let target_str = target
                .as_deref()
                .map(|target| format!(": {}", truncate_to_display_width(target, target_width)))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("{prefix}{target_str}"), Style::default().fg(color)),
                Span::styled(status_text, Style::default().fg(theme.muted)),
            ]));
            if let Some(out) = output {
                append_tool_output_lines(lines, out, *expanded, force_expand, theme);
            }
            if let Some(diff) = diff {
                append_diff_lines(lines, diff, theme);
            }
        }
        ChatMessage::PlanUpdate { explanation, plan } => {
            append_archived_plan_lines(lines, explanation.as_deref(), plan, width, theme);
        }
        ChatMessage::Subagent {
            description,
            status,
            output,
            error,
            activity,
            activity_tail,
            turn,
            usage,
            expanded,
            ..
        } => {
            append_subagent_lines(
                lines,
                description,
                status,
                output,
                error,
                activity.as_deref(),
                activity_tail,
                *turn,
                *usage,
                theme,
                *expanded,
                force_expand,
            );
        }
        ChatMessage::Error(text) => {
            lines.push(Line::from(Span::styled(
                format!("ERROR: {text}"),
                Style::default().fg(theme.error),
            )));
            lines.push(Line::from(""));
        }
        ChatMessage::System(text) => {
            lines.push(Line::from(Span::styled(
                text.clone(),
                Style::default().fg(theme.muted),
            )));
            lines.push(Line::from(""));
        }
    }
}

fn plan_panel_height(state: &AppState) -> u16 {
    match &state.current_plan {
        Some((_, plan)) => {
            let items = plan.len() as u16;
            // 2 for border, 1 for title = items + 2, capped at 10
            (items + 2).min(10)
        }
        None => 0,
    }
}

fn render_plan_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    use orca_core::plan_types::PlanStatus;

    let Some((_, plan)) = &state.current_plan else {
        return;
    };

    let (title, border_color) = if state.plan_update_failed {
        (
            " Task Plan (last update failed — may be stale) ",
            theme.warning,
        )
    } else {
        (" Task Plan ", theme.border)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    let step_width = (inner.width as usize).saturating_sub(3);
    for item in plan {
        let (icon, color) = match item.status {
            PlanStatus::Completed => ("✓", theme.success),
            PlanStatus::InProgress => ("→", theme.warning),
            PlanStatus::Pending => ("•", theme.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {icon} "), Style::default().fg(color)),
            Span::styled(
                truncate_to_display_width(&item.step.replace('\n', " "), step_width),
                Style::default().fg(color),
            ),
        ]));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Render a finished plan as an inline checklist in the scrollback. Completed steps are dimmed and
/// struck through; the in-progress/pending steps keep their live-panel styling so the archived view
/// matches what the user saw in the bottom panel.
fn append_archived_plan_lines(
    lines: &mut Vec<Line<'static>>,
    explanation: Option<&str>,
    plan: &[orca_core::plan_types::PlanItem],
    width: usize,
    theme: &Theme,
) {
    use orca_core::plan_types::PlanStatus;

    lines.push(Line::from(Span::styled(
        "  Task Plan",
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    )));

    if let Some(note) = explanation.map(str::trim).filter(|n| !n.is_empty()) {
        lines.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    for item in plan {
        let (icon, text_style) = match item.status {
            PlanStatus::Completed => (
                "✓",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
            PlanStatus::InProgress => ("→", Style::default().fg(theme.warning)),
            PlanStatus::Pending => ("•", Style::default().fg(theme.muted)),
        };
        let icon_style = match item.status {
            PlanStatus::Completed => Style::default().fg(theme.success),
            PlanStatus::InProgress => Style::default().fg(theme.warning),
            PlanStatus::Pending => Style::default().fg(theme.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {icon} "), icon_style),
            Span::styled(
                truncate_to_display_width(&item.step.replace('\n', " "), width.saturating_sub(4)),
                text_style,
            ),
        ]));
    }

    lines.push(Line::from(""));
}

fn append_proposed_plan_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    theme: &Theme,
) {
    lines.push(Line::from(vec![Span::styled(
        "  Proposed Plan",
        Style::default()
            .fg(theme.approval)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in render_markdown(text, width.saturating_sub(2)) {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.muted)),
            Span::styled(line.to_string(), Style::default().fg(theme.text)),
        ]));
    }
    lines.push(Line::from(""));
}

fn append_subagent_lines(
    lines: &mut Vec<Line<'static>>,
    description: &str,
    status: &str,
    output: &Option<String>,
    error: &Option<String>,
    activity: Option<&str>,
    activity_tail: &[String],
    turn: Option<u32>,
    usage: Option<orca_core::cost_types::UsageTotals>,
    theme: &Theme,
    expanded: bool,
    force_expand: bool,
) {
    let (label, color) = match status {
        "success" | "completed" => ("done", theme.success),
        "running" => ("running", theme.border),
        "failed" => ("failed", theme.error),
        other => (other, theme.muted),
    };

    lines.push(Line::from(vec![
        Span::styled("  ┌─ delegated task", Style::default().fg(theme.border)),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(label.to_string(), Style::default().fg(color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  │ ", Style::default().fg(theme.border)),
        Span::styled(description.to_string(), Style::default().fg(theme.text)),
    ]));

    // The collapsed view keeps only the first few lines; when flushing to the immutable
    // scrollback (`force_expand`) we emit the whole result/error so nothing is truncated
    // beyond reach.
    let body_limit = if force_expand { usize::MAX } else { 3 };
    match (status, output, error) {
        ("running", _, _) => {
            let mut detail = activity.unwrap_or("working in a child context").to_string();
            if let Some(turn) = turn {
                detail = format!("turn {turn} · {detail}");
            }
            if let Some(usage) = usage {
                detail.push_str(&format!(" · {} tok", usage.total_tokens()));
            }
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(theme.border)),
                Span::styled(detail, Style::default().fg(theme.muted)),
            ]));
            if (expanded || force_expand) && !activity_tail.is_empty() {
                for item in activity_tail {
                    lines.push(Line::from(vec![
                        Span::styled("  │   ", Style::default().fg(theme.border)),
                        Span::styled(item.clone(), Style::default().fg(theme.muted)),
                    ]));
                }
            }
        }
        (_, _, Some(err)) => {
            lines.push(Line::from(vec![
                Span::styled("  │ error: ", Style::default().fg(theme.error)),
                Span::styled(
                    truncate_lines(err, body_limit),
                    Style::default().fg(theme.error),
                ),
            ]));
        }
        (_, Some(out), _) => {
            lines.push(Line::from(vec![
                Span::styled("  │ result: ", Style::default().fg(theme.success)),
                Span::styled(
                    truncate_lines(out, body_limit),
                    Style::default().fg(theme.muted),
                ),
            ]));
        }
        _ => {}
    }

    lines.push(Line::from(Span::styled(
        "  └─ returned to main agent",
        Style::default().fg(theme.muted),
    )));
}

fn append_diff_lines(lines: &mut Vec<Line<'static>>, diff: &str, theme: &Theme) {
    let mut diff_lines = diff.lines();
    for line in diff_lines.by_ref().take(80) {
        let color = if line.starts_with('+') && !line.starts_with("+++") {
            theme.diff_add
        } else if line.starts_with('-') && !line.starts_with("---") {
            theme.diff_remove
        } else if line.starts_with("@@") {
            theme.border
        } else {
            theme.muted
        };
        lines.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(color),
        )));
    }
    if diff_lines.next().is_some() {
        lines.push(Line::from(Span::styled(
            "    [... diff truncated ...]",
            Style::default().fg(theme.muted),
        )));
    }
}

fn append_tool_output_lines(
    lines: &mut Vec<Line<'static>>,
    output: &str,
    expanded: bool,
    force_expand: bool,
    theme: &Theme,
) {
    // Flushing to the immutable scrollback (`force_expand`) commits the entire output so
    // nothing is hidden behind a "[+N lines]" stub that `e` can no longer reveal. The live
    // pane caps the `e`-expanded view at 40 rows and the collapsed view at 2.
    let max_lines = if force_expand {
        usize::MAX
    } else if expanded {
        40
    } else {
        2
    };
    let mut output_lines = output.lines();
    for line in output_lines.by_ref().take(max_lines) {
        lines.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(theme.muted),
        )));
    }

    let hidden = output_lines.count();
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("    [+{hidden} lines]"),
            Style::default().fg(theme.muted),
        )));
    }
}

fn spinner_frame(tick: u64) -> &'static str {
    const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    SPINNER_FRAMES[((tick / 2) as usize) % SPINNER_FRAMES.len()]
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::ApprovalRequired => "approval required",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn task_status_color(status: TaskStatus, theme: &Theme) -> Color {
    match status {
        TaskStatus::Running | TaskStatus::Stopping => theme.warning,
        TaskStatus::Completed => theme.success,
        TaskStatus::Failed | TaskStatus::Cancelled => theme.error,
        TaskStatus::ApprovalRequired => theme.warning,
        TaskStatus::Queued | TaskStatus::Paused | TaskStatus::Stopped => theme.muted,
    }
}

fn render_input(
    frame: &mut Frame,
    area: Rect,
    textarea: &TextArea,
    state: &AppState,
    theme: &Theme,
) {
    let inner = if let Some(block) = textarea.block() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };

    // Transient "copied N chars" feedback overlays the right end of the top
    // border, mirroring the " Input " title on the left.
    if let Some(notice) = state.copy_notice_at(std::time::Instant::now()) {
        let text = if notice.local_only {
            format!(" copied {} chars (local clipboard only) ", notice.chars)
        } else {
            format!(" copied {} chars to clipboard ", notice.chars)
        };
        let text_width = UnicodeWidthStr::width(text.as_str()) as u16;
        // Keep one border cell visible on each side of the overlay.
        if area.height > 0 && text_width + 2 < area.width {
            let overlay = Rect::new(area.x + area.width - text_width - 2, area.y, text_width, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(text, Style::default().fg(theme.approval))),
                overlay,
            );
        }
    }

    if inner.is_empty() {
        return;
    }

    let (lines, cursor_line) = composer_visual_lines(textarea, inner.width as usize);
    let visible_height = inner.height as usize;
    let start = if lines.len() <= visible_height {
        0
    } else if cursor_line >= visible_height {
        cursor_line + 1 - visible_height
    } else {
        0
    };
    let end = (start + visible_height).min(lines.len());
    let visible = lines[start..end].to_vec();
    let paragraph = Paragraph::new(visible)
        .style(textarea.style())
        .alignment(textarea.alignment());
    frame.render_widget(paragraph, inner);
}

fn composer_input_height(area_width: u16, textarea: &TextArea) -> u16 {
    let input_lines = composer_visual_line_count(area_width, textarea).max(1) as u16;
    let block_extra = textarea
        .block()
        .map(|block| {
            let outer = Rect::new(0, 0, area_width, u16::MAX);
            u16::MAX.saturating_sub(block.inner(outer).height)
        })
        .unwrap_or(0);
    input_lines.saturating_add(block_extra)
}

fn composer_visual_line_count(area_width: u16, textarea: &TextArea) -> usize {
    let inner_width = textarea_inner_width(area_width, textarea) as usize;
    if textarea.is_empty() {
        return 1;
    }
    textarea
        .lines()
        .iter()
        .map(|line| textarea_wrap_ranges(line, inner_width).len())
        .sum::<usize>()
        .max(1)
}

fn textarea_inner_width(area_width: u16, textarea: &TextArea) -> u16 {
    textarea
        .block()
        .map(|block| block.inner(Rect::new(0, 0, area_width, 1)).width)
        .unwrap_or(area_width)
}

fn composer_visual_lines(textarea: &TextArea, width: usize) -> (Vec<Line<'static>>, usize) {
    if textarea.is_empty() {
        let mut spans = vec![Span::styled(" ", textarea.cursor_style())];
        if let Some(style) = textarea.placeholder_style() {
            spans.push(Span::styled(textarea.placeholder_text().to_string(), style));
        }
        return (vec![Line::from(spans)], 0);
    }

    let (cursor_row, cursor_col) = textarea.cursor();
    let selection = textarea.selection_range();
    let mut visual_lines = Vec::new();
    let mut cursor_visual_line = 0usize;

    for (row, logical_line) in textarea.lines().iter().enumerate() {
        let ranges = textarea_wrap_ranges(logical_line, width);
        for range in ranges {
            let visual_index = visual_lines.len();
            if row == cursor_row && cursor_in_visual_range(cursor_col, &range, logical_line) {
                cursor_visual_line = visual_index;
            }
            visual_lines.push(render_textarea_visual_line(
                logical_line,
                row,
                range,
                textarea,
                selection,
            ));
        }
    }

    if visual_lines.is_empty() {
        visual_lines.push(Line::from(Span::styled(" ", textarea.cursor_style())));
    }

    (visual_lines, cursor_visual_line)
}

fn cursor_in_visual_range(cursor_col: usize, range: &Range<usize>, logical_line: &str) -> bool {
    let line_len = logical_line.chars().count();
    (range.start <= cursor_col && cursor_col < range.end)
        || (cursor_col == line_len && range.end == line_len)
        || (range.is_empty() && cursor_col == range.start)
}

fn render_textarea_visual_line(
    logical_line: &str,
    row: usize,
    range: Range<usize>,
    textarea: &TextArea,
    selection: Option<((usize, usize), (usize, usize))>,
) -> Line<'static> {
    let (cursor_row, cursor_col) = textarea.cursor();
    let base_style = textarea.style();
    let cursor_style = textarea.cursor_style();
    let cursor_line_style = textarea.cursor_line_style();
    let selection_style = Style::default().bg(Color::LightBlue);
    let mut spans = Vec::new();
    let mut pending = String::new();
    let mut pending_style = base_style;

    for (col, ch) in logical_line
        .chars()
        .enumerate()
        .skip(range.start)
        .take(range.end.saturating_sub(range.start))
    {
        let style = if row == cursor_row && col == cursor_col {
            cursor_style
        } else if selection_contains(selection, row, col) {
            selection_style
        } else if row == cursor_row {
            cursor_line_style
        } else {
            base_style
        };
        push_styled_char(&mut spans, &mut pending, &mut pending_style, ch, style);
    }

    flush_pending_span(&mut spans, &mut pending, pending_style);

    if row == cursor_row && cursor_col == range.end && cursor_col == logical_line.chars().count() {
        spans.push(Span::styled(" ", cursor_style));
    } else if selection_contains(selection, row, range.end)
        && range.end == logical_line.chars().count()
    {
        spans.push(Span::styled(" ", selection_style));
    }

    Line::from(spans)
}

fn selection_contains(
    selection: Option<((usize, usize), (usize, usize))>,
    row: usize,
    col: usize,
) -> bool {
    let Some(((start_row, start_col), (end_row, end_col))) = selection else {
        return false;
    };
    (row > start_row || (row == start_row && col >= start_col))
        && (row < end_row || (row == end_row && col < end_col))
}

fn push_styled_char(
    spans: &mut Vec<Span<'static>>,
    pending: &mut String,
    pending_style: &mut Style,
    ch: char,
    style: Style,
) {
    if pending.is_empty() {
        *pending_style = style;
    } else if *pending_style != style {
        flush_pending_span(spans, pending, *pending_style);
        *pending_style = style;
    }
    pending.push(ch);
}

fn flush_pending_span(spans: &mut Vec<Span<'static>>, pending: &mut String, pending_style: Style) {
    if !pending.is_empty() {
        spans.push(Span::styled(std::mem::take(pending), pending_style));
    }
}

fn textarea_wrap_ranges(line: &str, width: usize) -> Vec<Range<usize>> {
    if line.is_empty() || width == 0 {
        return vec![0..line.chars().count()];
    }

    let mut ranges = Vec::new();
    let mut current_start = 0usize;
    let mut current_end = 0usize;
    let mut current_width = 0usize;
    let mut segment_start = 0usize;

    for segment in line.split_inclusive(|c: char| c.is_whitespace() || c == '/' || c == '-') {
        let segment_cols = segment.chars().count();
        let segment_width = UnicodeWidthStr::width(segment);
        if segment_width == 0 {
            segment_start += segment_cols;
            continue;
        }

        if segment_width > width {
            if current_width > 0 {
                ranges.push(current_start..current_end);
            }
            let (start, end, display_width) =
                push_hard_wrapped_segment(&mut ranges, segment, segment_start, width);
            current_start = start;
            current_end = end;
            current_width = display_width;
        } else if current_width == 0 {
            current_start = segment_start;
            current_end = segment_start + segment_cols;
            current_width = segment_width;
        } else if current_width + segment_width <= width {
            current_end = segment_start + segment_cols;
            current_width += segment_width;
        } else {
            ranges.push(current_start..current_end);
            current_start = segment_start;
            current_end = segment_start + segment_cols;
            current_width = segment_width;
        }

        segment_start += segment_cols;
    }

    if current_width > 0 || ranges.is_empty() {
        ranges.push(current_start..current_end);
    }
    ranges
}

fn push_hard_wrapped_segment(
    ranges: &mut Vec<Range<usize>>,
    segment: &str,
    segment_start: usize,
    width: usize,
) -> (usize, usize, usize) {
    let mut chunk_start = segment_start;
    let mut current_col = segment_start;
    let mut current_width = 0usize;

    for ch in segment.chars() {
        let ch_width = UnicodeWidthStr::width(ch.to_string().as_str()).max(1);
        if current_width > 0 && current_width + ch_width > width {
            ranges.push(chunk_start..current_col);
            chunk_start = current_col;
            current_width = 0;
        }
        current_col += 1;
        current_width += ch_width;
    }

    (chunk_start, current_col, current_width)
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // The live status dot + elapsed time moved to the activity line above the composer
    // (see `render_activity`); this bottom line is now purely persistent metadata.
    let line = status_line(state, theme, area.width as usize);

    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

fn status_line(state: &AppState, theme: &Theme, width: usize) -> Line<'static> {
    let separator = "  ·  ";
    let mode_prefix = separator;
    let mode_value = state.approval_mode.as_str();
    let mode_width = UnicodeWidthStr::width(mode_prefix) + UnicodeWidthStr::width(mode_value);
    let model = format!(
        " {} ({})",
        state.model_name,
        state.reasoning_effort.as_str()
    );
    let model = truncate_to_display_width(&model, width.saturating_sub(mode_width));
    let mut used = UnicodeWidthStr::width(model.as_str()) + mode_width;
    let mut spans = vec![
        Span::styled(model, Style::default().fg(theme.muted)),
        Span::styled(mode_prefix, Style::default().fg(theme.muted)),
        Span::styled(
            mode_value,
            Style::default().fg(approval_mode_color(state.approval_mode, theme)),
        ),
    ];

    let mut optional = Vec::new();
    if state.context_limit_tokens > 0 {
        optional.push(context_cell(state, theme));
    }
    // Session cost only appears once there is something to report; a fresh
    // session keeps the bar clean instead of showing zeros.
    if state.usage.total_tokens() > 0 {
        optional.push(Span::styled(
            format!(
                "{separator}{} tokens{separator}{}",
                format_token_count(state.usage.total_tokens()),
                format_cost(state.usage.estimated_cost_usd),
            ),
            Style::default().fg(theme.muted),
        ));
    }
    optional.push(Span::styled(
        format!("{separator}F1 shortcuts"),
        Style::default().fg(theme.muted),
    ));

    for span in optional {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_width <= width {
            used += span_width;
            spans.push(span);
        }
    }

    Line::from(spans)
}

/// Humanize token counts for the status bar: 950 → "950", 8_664 → "8.7k",
/// 1_250_000 → "1.3M". Full precision lives in `/cost`.
fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

/// Session cost with just enough precision to be meaningful: sub-cent costs
/// show four decimals, anything larger the familiar two.
fn format_cost(cost_usd: f64) -> String {
    if cost_usd < 0.01 {
        format!("${cost_usd:.4}")
    } else {
        format!("${cost_usd:.2}")
    }
}

fn approval_mode_color(mode: ApprovalMode, theme: &Theme) -> Color {
    match mode {
        ApprovalMode::Suggest => theme.border,
        ApprovalMode::AutoEdit => theme.approval,
        ApprovalMode::FullAuto => theme.error,
        ApprovalMode::Plan => theme.plan_mode,
    }
}

/// The activity indicator shown on its own line directly above the composer. Returns
/// `None` while idle so the line collapses to zero height and a resting session stays
/// clean; every other status renders a coloured dot, a label, and (while running) the
/// elapsed wall-clock time.
fn activity_line(state: &AppState, theme: &Theme) -> Option<(String, ratatui::style::Color)> {
    match &state.status {
        AppStatus::Idle | AppStatus::Setup | AppStatus::SessionPicker => None,
        AppStatus::Running => {
            let live_elapsed = state
                .running_started_at
                .map(|started| started.elapsed().as_secs())
                .unwrap_or_default();
            let persisted_goal_elapsed = state
                .current_goal
                .as_ref()
                .filter(|goal| goal.status.should_continue())
                .map(|goal| goal.time_used_seconds.max(0) as u64)
                .unwrap_or_default();
            let elapsed =
                format_elapsed_compact(persisted_goal_elapsed.saturating_add(live_elapsed));
            Some((format!("● running {elapsed}"), theme.warning))
        }
        AppStatus::Compacting => Some(("● Compacting context...".to_string(), theme.warning)),
        AppStatus::WaitingApproval => Some(("● approval".to_string(), theme.approval)),
        AppStatus::WaitingUserInput => Some(("● input".to_string(), theme.approval)),
    }
}

fn render_activity(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some((text, color)) = activity_line(state, theme) else {
        return;
    };
    // First row stays blank as a spacer between the transcript tail and the indicator.
    let paragraph = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(format!(" {text}"), Style::default().fg(color))),
    ]);
    frame.render_widget(paragraph, area);
}

fn format_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

/// Remaining context window as a percentage of the local compaction budget.
/// Pure local observability — this value is never sent upstream, so it cannot
/// affect DeepSeek's prefix cache. Hidden until a real budget is known.
fn context_cell(state: &AppState, theme: &Theme) -> Span<'static> {
    if state.context_limit_tokens == 0 {
        return Span::raw("");
    }
    let remaining = state
        .context_limit_tokens
        .saturating_sub(state.context_used_tokens);
    let percent = (remaining * 100) / state.context_limit_tokens;
    let color = if percent > 50 {
        theme.success
    } else if percent >= 20 {
        theme.warning
    } else {
        theme.error
    };
    Span::styled(
        format!("  ·  context {percent}%"),
        Style::default().fg(color),
    )
}

fn render_shortcuts(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let area = frame.area();
    let width = 58u16.min(area.width.saturating_sub(4));
    let max_height = area.height.saturating_sub(4);
    let scopes = active_shortcut_scopes(state);
    let lines = shortcuts::shortcut_lines(&scopes);
    let height = ((lines.len() as u16) + 2).min(max_height).max(3);
    let popup_area = centered_rect(area, width, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Shortcuts ")
        .border_style(Style::default().fg(theme.border));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, popup_area);
}

fn active_shortcut_scopes(state: &AppState) -> Vec<ShortcutScope> {
    match state.status {
        AppStatus::Idle => vec![ShortcutScope::Global, ShortcutScope::Idle],
        AppStatus::Running | AppStatus::Compacting => {
            vec![ShortcutScope::Global, ShortcutScope::Running]
        }
        AppStatus::WaitingApproval => vec![ShortcutScope::Global, ShortcutScope::Approval],
        AppStatus::WaitingUserInput => vec![ShortcutScope::Global, ShortcutScope::Idle],
        AppStatus::Setup | AppStatus::SessionPicker => vec![ShortcutScope::Global],
    }
}

/// The scrolled 12-item window both popup menus (slash, mention) use: the
/// selected row stays visible, pinned to the window's bottom while moving
/// down. Shared by the renderers and the mouse hit-tests so they can never
/// disagree.
fn popup_window(len: usize, selected: usize) -> (usize, usize) {
    let visible_count = len.min(12);
    let max_start = len.saturating_sub(visible_count);
    let start = selected
        .saturating_sub(visible_count.saturating_sub(1))
        .min(max_start);
    (start, (start + visible_count).min(len))
}

/// Which slash-menu item (of the active list — sub-menu when open) a click
/// lands on, replicating `render_slash_menu` geometry.
pub(crate) fn slash_menu_hit_index(state: &AppState, column: u16, row: u16) -> Option<usize> {
    let menu = state.slash_menu.as_ref()?;
    let input_area = state.input_area?;
    let (len, selected) = match &menu.sub_menu {
        Some(sub) => (sub.items.len(), sub.selected),
        None => (menu.items.len(), menu.selected),
    };
    let (start, end) = popup_window(len, selected);
    let height = ((end - start) as u16 + 2).min(14);
    let popup = Rect::new(
        input_area.x,
        input_area.y.saturating_sub(height),
        input_area.width,
        height,
    );
    hit_bordered_list_row(popup, column, row).and_then(|offset| {
        let index = start + offset;
        (index < end).then_some(index)
    })
}

/// Which mention candidate a click lands on, replicating
/// `render_mention_candidates` geometry. The trailing status row (if any)
/// is not a candidate and reports `None`.
pub(crate) fn mention_menu_hit_index(state: &AppState, column: u16, row: u16) -> Option<usize> {
    // The mention popup only renders while no slash menu is open; the
    // hit-test must honor the same gate.
    if state.slash_menu.is_some() {
        return None;
    }
    let phase = state.mention.phase.as_ref()?;
    let input_area = state.input_area?;
    let candidates = &state.mention.candidates;
    let (start, end) = popup_window(candidates.len(), state.mention.selected);
    let status = mention_status_text(
        phase,
        state.mention.progress.scanned_paths,
        candidates.is_empty(),
    );
    let height = (end - start) as u16 + u16::from(status.is_some()) + 2;
    let popup = Rect::new(
        input_area.x,
        input_area.y.saturating_sub(height),
        input_area.width,
        height,
    );
    hit_bordered_list_row(popup, column, row).and_then(|offset| {
        let index = start + offset;
        (index < end).then_some(index)
    })
}

/// Row offset within a bordered single-column list popup, if `column`/`row`
/// land inside its content area.
fn hit_bordered_list_row(popup: Rect, column: u16, row: u16) -> Option<usize> {
    if popup.width < 3 || popup.height < 3 {
        return None;
    }
    let content_left = popup.x + 1;
    let content_right = popup.x + popup.width - 1;
    let content_top = popup.y + 1;
    let content_bottom = popup.y + popup.height - 1;
    (column >= content_left && column < content_right && row >= content_top && row < content_bottom)
        .then(|| (row - content_top) as usize)
}

fn render_slash_menu(frame: &mut Frame, input_area: Rect, state: &AppState, theme: &Theme) {
    let menu = match &state.slash_menu {
        Some(m) => m,
        None => return,
    };

    // Determine items and title based on sub-menu state
    let (items, selected, title): (Vec<(&str, &str)>, usize, &str) =
        if let Some(sub) = &menu.sub_menu {
            let items: Vec<(&str, &str)> = sub.items.iter().map(|s| (s.as_str(), "")).collect();
            (items, sub.selected, &sub.title)
        } else {
            let items: Vec<(&str, &str)> = menu
                .items
                .iter()
                .map(|i| (i.command.as_str(), i.description.as_str()))
                .collect();
            (items, menu.selected, " Commands ")
        };

    let (start, end) = popup_window(items.len(), selected);
    let visible_count = end - start;
    let item_count = visible_count as u16;
    let height = (item_count + 2).min(14); // +2 for border
    let width = input_area.width;
    let y = input_area.y.saturating_sub(height);
    let popup_area = Rect::new(input_area.x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, (cmd, desc)) in items[start..end].iter().enumerate() {
        let item_index = start + i;
        let prefix = if item_index == selected { "▸ " } else { "  " };
        let style = if item_index == selected {
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        if desc.is_empty() {
            lines.push(Line::from(Span::styled(format!("{prefix}{cmd}"), style)));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{prefix}{cmd}"), style),
                Span::styled(format!("  {desc}"), Style::default().fg(theme.muted)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(theme.border));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_mention_candidates(frame: &mut Frame, input_area: Rect, state: &AppState, theme: &Theme) {
    let candidates = &state.mention.candidates;
    let Some(phase) = &state.mention.phase else {
        return;
    };

    let (start, end) = popup_window(candidates.len(), state.mention.selected);
    let visible_count = end - start;
    let status = mention_status_text(
        phase,
        state.mention.progress.scanned_paths,
        candidates.is_empty(),
    );
    let item_count = visible_count as u16 + u16::from(status.is_some());
    let height = item_count + 2;
    let width = input_area.width;
    let y = input_area.y.saturating_sub(height);
    let popup_area = Rect::new(input_area.x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let mut lines: Vec<Line> = candidates
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(i, candidate)| {
            let prefix = if i == state.mention.selected {
                "▸ "
            } else {
                "  "
            };
            let style = if i == state.mention.selected {
                Style::default()
                    .fg(theme.border)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![Span::styled(format!("{prefix}@"), style)];
            for (index, ch) in candidate.display.chars().enumerate() {
                let matched = candidate.indices.binary_search(&(index as u32)).is_ok();
                let char_style = if matched {
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                spans.push(Span::styled(ch.to_string(), char_style));
            }
            spans.push(Span::styled(
                format!("  [{}] {}", candidate.kind.label(), candidate.description),
                Style::default().fg(theme.muted),
            ));
            Line::from(spans)
        })
        .collect();
    if let Some((text, color)) = status {
        lines.push(Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(color),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Mentions ")
        .border_style(Style::default().fg(theme.border));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn mention_status_text(
    phase: &SearchPhase,
    scanned_paths: usize,
    candidates_empty: bool,
) -> Option<(String, Color)> {
    match phase {
        SearchPhase::Searching => Some(("Searching files…".to_string(), Color::DarkGray)),
        SearchPhase::Scanning => {
            Some((format!("Scanning… {scanned_paths} paths"), Color::DarkGray))
        }
        SearchPhase::Refreshing => Some(("Refreshing…".to_string(), Color::DarkGray)),
        SearchPhase::Complete if candidates_empty => {
            Some(("No matches".to_string(), Color::DarkGray))
        }
        SearchPhase::Complete => None,
        SearchPhase::Incomplete { .. } => Some(("Search incomplete".to_string(), Color::Red)),
        SearchPhase::Stopping => Some(("Stopping search…".to_string(), Color::DarkGray)),
    }
}

/// Shared layout for the approval dialog, used by the renderer and the mouse
/// hit-test so option rows can never drift apart.
struct ApprovalDialogGeometry {
    popup: Rect,
    shown_diff_lines: usize,
    diff_truncated: bool,
    first_option_row: u16,
}

fn approval_dialog_geometry(
    area: Rect,
    dialog: &crate::types::ApprovalDialog,
) -> ApprovalDialogGeometry {
    let width = 64u16.min(area.width.saturating_sub(4));
    let max_height = area.height.saturating_sub(4).max(8);
    let fixed_content_rows = 3 + dialog.options.len() as u16 + 2 + u16::from(dialog.diff.is_some());
    let available_diff_rows = max_height
        .saturating_sub(2)
        .saturating_sub(fixed_content_rows) as usize;
    let source_diff_lines = dialog
        .diff
        .as_ref()
        .map(|diff| diff.lines().count())
        .unwrap_or(0);
    let desired_diff_lines = source_diff_lines.min(12);
    let diff_truncated =
        source_diff_lines > desired_diff_lines || desired_diff_lines > available_diff_rows;
    let truncation_row = usize::from(diff_truncated && available_diff_rows > 0);
    let shown_diff_lines =
        desired_diff_lines.min(available_diff_rows.saturating_sub(truncation_row));
    let height = (fixed_content_rows
        + shown_diff_lines as u16
        + u16::from(diff_truncated && available_diff_rows > 0)
        + 2)
    .min(max_height)
    .max(8);
    let popup = centered_rect(area, width, height);
    // Border, then tool/target/blank, then the bounded diff block.
    let first_option_row = popup.y
        + 1
        + 3
        + shown_diff_lines as u16
        + truncation_row as u16
        + u16::from(dialog.diff.is_some());
    ApprovalDialogGeometry {
        popup,
        shown_diff_lines,
        diff_truncated,
        first_option_row,
    }
}

/// Which approval option a click lands on, if any.
pub(crate) fn approval_option_hit_index(state: &AppState, column: u16, row: u16) -> Option<usize> {
    let dialog = state.approval_dialog.as_ref()?;
    let area = state.frame_area?;
    let geometry = approval_dialog_geometry(area, dialog);
    let popup = geometry.popup;
    if popup.width < 3 || popup.height < 3 {
        return None;
    }
    if column <= popup.x || column + 1 >= popup.x + popup.width {
        return None;
    }
    if row + 1 >= popup.y + popup.height {
        return None;
    }
    let index = row.checked_sub(geometry.first_option_row)? as usize;
    (index < dialog.options.len()).then_some(index)
}

fn render_approval_dialog(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let Some(dialog) = &state.approval_dialog else {
        return;
    };

    let area = frame.area();
    let geometry = approval_dialog_geometry(area, dialog);
    let popup_area = geometry.popup;
    let shown_diff_lines = geometry.shown_diff_lines;
    let diff_truncated = geometry.diff_truncated;
    let target_str = dialog.target.as_deref().unwrap_or("(none)");
    let inner_width = popup_area.width.saturating_sub(2) as usize;
    let target_str = truncate_to_display_width(target_str, inner_width.saturating_sub(9));

    // Build the diff/preview lines (colored) if a preview is present.
    let diff_lines: Vec<Line<'static>> = match &dialog.diff {
        Some(diff) => diff
            .lines()
            .take(shown_diff_lines)
            .map(|line| {
                let color = if line.starts_with('+') {
                    theme.diff_add
                } else if line.starts_with('-') {
                    theme.diff_remove
                } else if line.starts_with("@@") || line.starts_with('$') {
                    theme.border
                } else {
                    theme.muted
                };
                Line::from(Span::styled(
                    truncate_to_display_width(&format!("  {line}"), inner_width),
                    Style::default().fg(color),
                ))
            })
            .collect(),
        None => Vec::new(),
    };

    frame.render_widget(Clear, popup_area);

    let mut content: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("  tool   ", Style::default().fg(theme.muted)),
            Span::styled(
                dialog.tool.clone(),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  target ", Style::default().fg(theme.muted)),
            Span::styled(target_str.clone(), Style::default().fg(theme.text)),
        ]),
        Line::from(""),
    ];

    content.extend(diff_lines);
    if diff_truncated {
        content.push(Line::from(Span::styled(
            "  … (preview truncated)",
            Style::default().fg(theme.muted),
        )));
    }
    if dialog.diff.is_some() {
        content.push(Line::from(""));
    }

    // The options, one per line, highlighted when selected.
    for (i, option) in dialog.options.iter().enumerate() {
        let selected = i == dialog.selected;
        let prefix = if selected { "▸ " } else { "  " };
        let key_color = match option {
            ApprovalOption::Deny => theme.error,
            _ => theme.success,
        };
        let label_style = if selected {
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
        };
        let label_text = match option {
            ApprovalOption::AlwaysTool => format!("always allow \"{}\"", dialog.tool),
            ApprovalOption::AlwaysTarget => "always allow this exact call".to_string(),
            _ => option.label().to_string(),
        };
        let label_text = truncate_to_display_width(&label_text, inner_width.saturating_sub(8));
        content.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(theme.border)),
            Span::styled(
                format!("[{}] ", option.key()),
                Style::default().fg(key_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label_text, label_style),
        ]));
    }

    content.push(Line::from(""));
    content.push(Line::from(Span::styled(
        "  ↑↓ select · Enter · 1/2/3/4 · legacy y/A/a/n",
        Style::default().fg(theme.muted),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(dialog.title())
        .border_style(Style::default().fg(theme.approval));

    let paragraph = Paragraph::new(content).block(block);
    frame.render_widget(paragraph, popup_area);
}

/// Which session (index into `session_picker_sessions`) a click lands on,
/// replicating `render_session_picker`'s line layout: three header lines,
/// then one title line per filtered session plus an optional metadata line.
/// Long wrapped titles can shift rows below them; the mapping is then off by
/// the wrapped amount, which degrades to selecting a neighbour.
pub(crate) fn session_picker_hit_index(state: &AppState, row: u16) -> Option<usize> {
    let area = state.frame_area?;
    if area.width < 3 || area.height < 3 {
        return None;
    }
    let inner_top = area.y + 1;
    let inner_bottom = area.y + area.height - 1;
    if row < inner_top + 3 || row >= inner_bottom {
        return None;
    }
    let mut current = inner_top + 3;
    for index in state.filtered_session_indices() {
        let session = &state.session_picker_sessions[index];
        let rows = 1 + u16::from(session_permission_metadata_label(session).is_some());
        if row >= current && row < current + rows {
            return Some(index);
        }
        current = current.saturating_add(rows);
        if current >= inner_bottom {
            break;
        }
    }
    None
}

/// Map a click inside the composer to a `(row, col)` cursor position in the
/// textarea, replicating `render_input`'s wrap and scroll behavior.
pub(crate) fn composer_click_target(
    textarea: &TextArea,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<(u16, u16)> {
    let inner = textarea
        .block()
        .map(|block| block.inner(area))
        .unwrap_or(area);
    if inner.is_empty() || !inner.contains(ratatui::layout::Position::new(column, row)) {
        return None;
    }
    if textarea.is_empty() {
        return Some((0, 0));
    }

    let width = inner.width as usize;
    let (cursor_row, cursor_col) = textarea.cursor();
    let mut visual: Vec<(usize, Range<usize>)> = Vec::new();
    let mut cursor_visual_line = 0usize;
    for (logical_row, logical_line) in textarea.lines().iter().enumerate() {
        for range in textarea_wrap_ranges(logical_line, width) {
            if logical_row == cursor_row && cursor_in_visual_range(cursor_col, &range, logical_line)
            {
                cursor_visual_line = visual.len();
            }
            visual.push((logical_row, range));
        }
    }
    if visual.is_empty() {
        return Some((0, 0));
    }

    // Same scroll-to-cursor behavior as render_input.
    let visible_height = inner.height as usize;
    let start = if visual.len() <= visible_height {
        0
    } else if cursor_visual_line >= visible_height {
        cursor_visual_line + 1 - visible_height
    } else {
        0
    };
    let clicked = (start + (row - inner.y) as usize).min(visual.len() - 1);
    let (logical_row, range) = visual[clicked].clone();
    let logical_line = textarea.lines()[logical_row].as_str();

    // Walk display widths to find the character cell under the pointer.
    let target = (column - inner.x) as usize;
    let mut acc = 0usize;
    let mut char_col = range.start;
    for ch in logical_line
        .chars()
        .skip(range.start)
        .take(range.end.saturating_sub(range.start))
    {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if ch_width > 0 && target < acc + ch_width {
            break;
        }
        acc += ch_width;
        char_col += 1;
    }
    Some((
        logical_row.min(u16::MAX as usize) as u16,
        char_col.min(u16::MAX as usize) as u16,
    ))
}

fn render_setup(frame: &mut Frame, state: &AppState, textarea: &TextArea, _theme: &Theme) {
    let area = frame.area();

    match state.setup_step {
        0 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 16u16.min(area.height.saturating_sub(2));
            let popup_area = centered_rect(area, width, height);

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
                .border_type(BorderType::Rounded)
                .title(" Welcome ")
                .border_style(Style::default().fg(Color::Cyan));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
        }
        1 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 14u16.min(area.height.saturating_sub(2));
            let popup_area = centered_rect(area, width, height);

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
                .border_type(BorderType::Rounded)
                .title(" Setup ")
                .border_style(Style::default().fg(Color::Cyan));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
            frame.render_widget(textarea, inner[1]);
        }
        2 => {
            let width = 60u16.min(area.width.saturating_sub(4));
            let height = 12u16.min(area.height.saturating_sub(2));
            let popup_area = centered_rect(area, width, height);

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
                .border_type(BorderType::Rounded)
                .title(" Setup Complete ")
                .border_style(Style::default().fg(Color::Green));

            let paragraph = Paragraph::new(content).block(block);
            frame.render_widget(paragraph, popup_area);
        }
        _ => {}
    }
}

fn render_markdown(input: &str, width: usize) -> Vec<Line<'static>> {
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
                    render_table(&table_rows, &mut lines, width);
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

fn render_table(rows: &[Vec<String>], lines: &mut Vec<Line<'static>>, available_width: usize) {
    if rows.is_empty() {
        return;
    }

    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return;
    }

    let ideal_widths: Vec<usize> = (0..num_cols)
        .map(|col| {
            rows.iter()
                .map(|row| {
                    row.get(col)
                        .map(|c| UnicodeWidthStr::width(c.as_str()))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();

    let col_gap: usize = 2;
    let overhead = col_gap * (num_cols.saturating_sub(1));
    let ideal_total = ideal_widths.iter().sum::<usize>() + overhead;

    if ideal_total <= available_width {
        render_table_grid(rows, &ideal_widths, col_gap, lines);
    } else {
        let col_widths = allocate_column_widths(&ideal_widths, available_width, col_gap);
        let max_col = col_widths.iter().copied().max().unwrap_or(0);
        if max_col < 12 && num_cols > 2 {
            render_table_as_records(rows, lines, available_width);
        } else {
            render_table_grid(rows, &col_widths, col_gap, lines);
        }
    }
    lines.push(Line::from(""));
}

fn allocate_column_widths(
    ideal_widths: &[usize],
    available_width: usize,
    col_gap: usize,
) -> Vec<usize> {
    let num_cols = ideal_widths.len();
    let overhead = col_gap * num_cols.saturating_sub(1);
    let usable = available_width.saturating_sub(overhead);

    let min_widths: Vec<usize> = ideal_widths.iter().map(|&w| w.min(6).max(3)).collect();
    let min_total: usize = min_widths.iter().sum();

    if usable <= min_total {
        return min_widths;
    }

    let ideal_total: usize = ideal_widths.iter().sum();
    if ideal_total <= usable {
        return ideal_widths.to_vec();
    }

    let mut widths = ideal_widths.to_vec();
    let mut excess = ideal_total - usable;

    while excess > 0 {
        let max_w = widths.iter().copied().max().unwrap_or(0);
        if max_w <= 6 {
            break;
        }
        let max_count = widths.iter().filter(|&&w| w == max_w).count();
        let second_max = widths
            .iter()
            .copied()
            .filter(|&w| w < max_w)
            .max()
            .unwrap_or(6);
        let shrink_each = (max_w - second_max).min((excess + max_count - 1) / max_count);
        for w in &mut widths {
            if *w == max_w {
                let s = shrink_each.min(excess);
                *w -= s;
                excess -= s;
                if excess == 0 {
                    break;
                }
            }
        }
    }

    for (w, &min_w) in widths.iter_mut().zip(min_widths.iter()) {
        *w = (*w).max(min_w);
    }
    widths
}

fn render_table_grid(
    rows: &[Vec<String>],
    col_widths: &[usize],
    col_gap: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(Color::White);
    let separator_style = Style::default().fg(Color::DarkGray);
    let gap_str: String = " ".repeat(col_gap);

    for (row_idx, row) in rows.iter().enumerate() {
        let wrapped_cells: Vec<Vec<String>> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = col_widths.get(i).copied().unwrap_or(6);
                wrap_text(cell, w)
            })
            .collect();

        let max_lines = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
        let style = if row_idx == 0 {
            header_style
        } else {
            cell_style
        };

        for line_idx in 0..max_lines {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (col_idx, wrapped) in wrapped_cells.iter().enumerate() {
                let w = col_widths.get(col_idx).copied().unwrap_or(6);
                let text = wrapped.get(line_idx).map(|s| s.as_str()).unwrap_or("");
                let display_width = UnicodeWidthStr::width(text);
                let padding = w.saturating_sub(display_width);
                spans.push(Span::styled(
                    format!("{text}{}", " ".repeat(padding)),
                    style,
                ));
                if col_idx < col_widths.len() - 1 {
                    spans.push(Span::styled(gap_str.clone(), separator_style));
                }
            }
            lines.push(Line::from(spans));
        }

        if row_idx == 0 {
            let sep: String = col_widths
                .iter()
                .enumerate()
                .map(|(i, &w)| {
                    let seg = "━".repeat(w);
                    if i < col_widths.len() - 1 {
                        format!("{seg}{}", " ".repeat(col_gap))
                    } else {
                        seg
                    }
                })
                .collect();
            lines.push(Line::from(Span::styled(sep, separator_style)));
        }
    }
}

fn render_table_as_records(
    rows: &[Vec<String>],
    lines: &mut Vec<Line<'static>>,
    available_width: usize,
) {
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Yellow);
    let value_style = Style::default().fg(Color::White);
    let separator_style = Style::default().fg(Color::DarkGray);

    let headers: Vec<&str> = rows
        .first()
        .map(|r| r.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let max_key_width = headers
        .iter()
        .map(|h| UnicodeWidthStr::width(*h))
        .max()
        .unwrap_or(0);

    let value_indent = max_key_width + 3;
    let value_width = available_width.saturating_sub(value_indent).max(10);

    for (row_idx, row) in rows.iter().enumerate().skip(1) {
        let record_label = format!("─── Record {} ", row_idx);
        let fill = "─"
            .repeat(available_width.saturating_sub(UnicodeWidthStr::width(record_label.as_str())));
        lines.push(Line::from(vec![
            Span::styled(record_label, separator_style),
            Span::styled(fill, separator_style),
        ]));

        for (col_idx, cell) in row.iter().enumerate() {
            let key = headers.get(col_idx).copied().unwrap_or("?");
            let key_pad = max_key_width.saturating_sub(UnicodeWidthStr::width(key));

            let wrapped_value = wrap_text(cell, value_width);
            if let Some(first_line) = wrapped_value.first() {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}{}: ", " ".repeat(key_pad), key), key_style),
                    Span::styled(first_line.clone(), value_style),
                ]));
            }
            for extra_line in wrapped_value.iter().skip(1) {
                lines.push(Line::from(vec![
                    Span::styled(" ".repeat(value_indent).to_string(), value_style),
                    Span::styled(extra_line.clone(), value_style),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    if !headers.is_empty() && rows.len() > 1 {
        let header_line = headers.join(" │ ");
        lines.insert(
            lines.len().saturating_sub(rows.len()), // insert near the top section
            Line::from(Span::styled(
                format!("Columns: {header_line}"),
                header_style,
            )),
        );
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let text_width = UnicodeWidthStr::width(text);
    if text_width <= width {
        return vec![text.to_string()];
    }

    let mut result: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut current_width: usize = 0;

    for word in text.split_inclusive(|c: char| c.is_whitespace() || c == '/' || c == '-') {
        let word_width = UnicodeWidthStr::width(word);
        if current_width + word_width <= width || current_line.is_empty() {
            current_line.push_str(word);
            current_width += word_width;
        } else {
            result.push(current_line.trim_end().to_string());
            current_line = word.to_string();
            current_width = word_width;
        }
    }
    if !current_line.is_empty() {
        result.push(current_line.trim_end().to_string());
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SlashMenu, SlashMenuItem, TuiEvent};
    use chrono::Utc;
    use crossbeam_channel as mpsc;
    use orca_core::config::AdditionalWorkingDirectory;
    use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus};
    use orca_core::plan_types::{PlanItem, PlanStatus};
    use orca_runtime::history::SessionSummary;
    use std::time::{Duration, Instant};

    #[test]
    fn welcome_lines_use_configured_app_version() {
        let (tx, _rx) = mpsc::unbounded();
        let state = AppState::new(
            tx,
            "9.8.7-test".to_string(),
            "deepseek-v4-pro".to_string(),
            "/tmp/project".to_string(),
        );
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let rendered = build_welcome_lines(&state, &theme)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("v9.8.7-test"));
    }

    #[test]
    fn terminal_tool_rows_render_interrupted_and_state_unknown_labels() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let messages = [
            ChatMessage::ToolCall {
                id: "cancelled".to_string(),
                name: "bash".to_string(),
                target: None,
                status: "cancelled".to_string(),
                output: Some("turn interrupted".to_string()),
                diff: None,
                kind: Some("cancelled".to_string()),
                expanded: false,
            },
            ChatMessage::ToolCall {
                id: "indeterminate".to_string(),
                name: "deploy".to_string(),
                target: None,
                status: "indeterminate".to_string(),
                output: Some("inspect external state".to_string()),
                diff: None,
                kind: Some("indeterminate".to_string()),
                expanded: false,
            },
        ];

        let rendered = build_lines_for_messages(&messages, &theme, 100, 0, false)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();

        assert!(rendered.contains("(interrupted)"));
        assert!(rendered.contains("(state unknown)"));
        assert!(!rendered.contains("(completed)"));
    }

    fn test_state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        AppState::new(
            tx,
            "0.0.0".to_string(),
            "deepseek".to_string(),
            "/tmp".to_string(),
        )
    }

    fn goal_with_elapsed(status: ThreadGoalStatus, time_used_seconds: i64) -> ThreadGoal {
        ThreadGoal {
            session_id: "goal-session".to_string(),
            objective: "finish the migration".to_string(),
            status,
            token_budget: None,
            tokens_used: 42,
            time_used_seconds,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn long_goal_objective_keeps_status_visible_on_one_line() {
        let mut state = test_state();
        let mut goal = goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60);
        goal.objective = "将当前项目重构为分层清晰的生产级 Agent SDK monorepo".repeat(8);
        state.current_goal = Some(goal);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let width = 80u16;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 3))
            .expect("test backend");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_goal_banner(frame, area, &state, &theme);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let content_row = (0..width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect::<String>();
        assert!(content_row.contains('…'));
        assert!(content_row.contains("● active"));
        assert!(content_row.contains("auto-continue"));
        assert!(!content_row.contains("monorepomonorepo"));
    }

    fn session_summary(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_string(),
            title: title.to_string(),
            cwd: "/workspace/project".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            path: "/tmp/session.jsonl".into(),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        }
    }

    #[test]
    fn session_picker_labels_additional_directories_under_runtime_workspace_roots() {
        let mut state = test_state();
        state.status = AppStatus::SessionPicker;
        let mut session = session_summary("session-1", "workspace permissions");
        session.runtime_workspace_roots = vec!["/workspace/project".into()];
        session.additional_working_directories = vec![AdditionalWorkingDirectory::new(
            "/workspace/project/docs",
            "session",
        )];
        state.session_picker_sessions = vec![session];

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains(":workspace_roots/docs"));
        assert!(rendered.contains("session"));
    }

    #[test]
    fn workspace_relative_path_label_prefers_longest_matching_runtime_root() {
        let roots = vec!["/workspace".into(), "/workspace/project".into()];

        assert_eq!(
            workspace_relative_path_label(Path::new("/workspace/project"), &roots),
            ":workspace_roots"
        );
        assert_eq!(
            workspace_relative_path_label(Path::new("/workspace/project/docs"), &roots),
            ":workspace_roots/docs"
        );
        assert_eq!(
            workspace_relative_path_label(Path::new("/var/tmp/cache"), &roots),
            "/var/tmp/cache"
        );
    }

    #[test]
    fn waiting_approval_does_not_render_composer_under_dialog() {
        let mut state = test_state();
        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-1".to_string(),
            tool: "web_search".to_string(),
            target: Some("A股 2026年6月30日 尾盘资金走向".to_string()),
            preview: None,
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Approval Required"));
        assert!(
            !rendered.contains("Input"),
            "approval modal should own the foreground without drawing the idle composer"
        );
    }

    #[test]
    fn waiting_approval_renders_numeric_shortcuts_in_semantic_order() {
        let mut state = test_state();
        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-1".to_string(),
            tool: "edit".to_string(),
            target: Some("src/main.rs".to_string()),
            preview: None,
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        let once = rendered.find("[1] allow this once").expect("once option");
        let exact = rendered
            .find("[2] always allow this exact call")
            .expect("exact-call option");
        let tool = rendered
            .find("[3] always allow \"edit\"")
            .expect("tool-wide option");
        let deny = rendered.find("[4] deny").expect("deny option");

        assert!(once < exact);
        assert!(exact < tool);
        assert!(tool < deny);
        assert!(rendered.contains("1/2/3/4"));
        assert!(rendered.contains("legacy y/A/a/n"));
    }

    #[test]
    fn waiting_permission_approval_renders_specific_risk_title() {
        let mut state = test_state();
        state.update(TuiEvent::PermissionApprovalNeeded {
            id: "approval-1".to_string(),
            tool: "bash".to_string(),
            target: Some("curl https://api.orca.invalid".to_string()),
            preview: Some("bash attempted network access to api.orca.invalid".to_string()),
            permission_kind:
                orca_runtime::runtime_permission::RuntimePermissionRequestKind::NetworkBlock,
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Network Permission Required"));
        assert!(!rendered.contains("Approval Required"));
    }

    #[test]
    fn approval_dialog_keeps_actions_visible_with_long_content() {
        let mut state = test_state();
        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-long".to_string(),
            tool: "bash".to_string(),
            target: Some(format!("echo {}", "very-long-target ".repeat(30))),
            preview: Some(
                (0..20)
                    .map(|index| format!("preview {index}: {}", "x".repeat(120)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        });
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20))
            .expect("test backend");

        terminal
            .draw(|frame| render_approval_dialog(frame, &state, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains('…'));
        assert!(rendered.contains("[1] allow this once"));
        assert!(rendered.contains("[2] always allow this exact call"));
        assert!(rendered.contains("[3] always allow \"bash\""));
        assert!(rendered.contains("[4] deny"));
        assert!(rendered.contains("preview truncated"));
        assert!(rendered.contains("↑↓ select · Enter · 1/2/3/4"));
    }

    #[test]
    fn long_slash_and_mention_menus_keep_selection_visible() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.slash_menu = Some(SlashMenu {
            items: (0..20)
                .map(|index| SlashMenuItem {
                    command: format!("/command-{index:02}"),
                    description: format!("command {index}"),
                })
                .collect(),
            selected: 19,
            sub_menu: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20))
            .expect("test backend");
        terminal
            .draw(|frame| {
                render_slash_menu(frame, Rect::new(0, 18, 70, 1), &state, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("▸ /command-19"));
        assert!(!rendered.contains("/command-00"));

        state.slash_menu = None;
        state.mention.candidates = (0..20)
            .map(|index| {
                orca_runtime::mentions::MentionCandidate::from_file_match(
                    &orca_file_search::SearchMatch {
                        root: std::path::PathBuf::from("/workspace"),
                        path: format!("file-{index:02}.rs"),
                        kind: orca_file_search::MatchKind::File,
                        score: 1,
                        indices: Vec::new(),
                    },
                )
            })
            .collect();
        state.mention.selected = 19;
        state.mention.phase = Some(orca_file_search::SearchPhase::Complete);
        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 18, 70, 1), &state, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("▸ @file-19.rs"));
        assert!(!rendered.contains("@file-00.rs"));
    }

    #[test]
    fn mention_popup_reports_every_streaming_phase() {
        assert_eq!(
            mention_status_text(&SearchPhase::Searching, 0, true),
            Some(("Searching files…".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Scanning, 42, false),
            Some(("Scanning… 42 paths".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Refreshing, 42, false),
            Some(("Refreshing…".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Complete, 42, true),
            Some(("No matches".to_string(), Color::DarkGray))
        );
        assert_eq!(
            mention_status_text(
                &SearchPhase::Incomplete {
                    message: "walk failed".to_string(),
                },
                42,
                false,
            ),
            Some(("Search incomplete".to_string(), Color::Red))
        );
        assert_eq!(
            mention_status_text(&SearchPhase::Stopping, 42, false),
            Some(("Stopping search…".to_string(), Color::DarkGray))
        );
        assert_eq!(mention_status_text(&SearchPhase::Complete, 42, false), None);
    }

    #[test]
    fn mention_popup_highlights_unicode_character_indices() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.mention.candidates = vec![orca_runtime::mentions::MentionCandidate::from_file_match(
            &orca_file_search::SearchMatch {
                root: std::path::PathBuf::from("/workspace"),
                path: "src/你好.rs".to_string(),
                kind: orca_file_search::MatchKind::File,
                score: 1,
                indices: vec![4],
            },
        )];
        state.mention.phase = Some(SearchPhase::Complete);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test backend");

        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 5, 20, 1), &state, &theme);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let highlighted = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "你")
            .unwrap();
        assert_eq!(highlighted.style().fg, Some(theme.warning));
    }

    #[test]
    fn mention_popup_renders_in_a_narrow_terminal() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.mention.candidates = vec![orca_runtime::mentions::MentionCandidate::from_file_match(
            &orca_file_search::SearchMatch {
                root: std::path::PathBuf::from("/workspace"),
                path: "src/a-very-long-file-name.rs".to_string(),
                kind: orca_file_search::MatchKind::File,
                score: 1,
                indices: Vec::new(),
            },
        )];
        state.mention.phase = Some(SearchPhase::Scanning);
        state.mention.progress.scanned_paths = 10;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 5))
            .expect("test backend");

        terminal
            .draw(|frame| {
                render_mention_candidates(frame, Rect::new(0, 4, 12, 1), &state, &theme);
            })
            .expect("draw");

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Mentions"));
    }

    #[test]
    fn live_pane_honours_scroll_offset_when_content_overflows() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let body = (0..50)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");

        // Auto-scroll on: the pane pins to the bottom and shows the last lines.
        let mut auto = test_state();
        auto.messages.push(ChatMessage::Assistant(body.clone()));
        auto.auto_scroll = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_live_messages(frame, area, &mut auto, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("L49"), "auto-scroll should show the tail");
        assert!(
            !rendered.contains("L0 "),
            "auto-scroll should not show the very first line"
        );

        // Scrolled to the top: the pane shows the earliest lines instead of the tail.
        let mut scrolled = test_state();
        scrolled.messages.push(ChatMessage::Assistant(body));
        scrolled.auto_scroll = false;
        scrolled.scroll_offset = 0;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 6))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_live_messages(frame, area, &mut scrolled, &theme);
            })
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(
            rendered.contains("L0"),
            "scroll-to-top should show the first line"
        );
        assert!(
            !rendered.contains("L49"),
            "scroll-to-top should not show the tail"
        );
    }

    #[test]
    fn live_pane_auto_scrolls_cjk_content_to_the_tail() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let body = (0..24)
            .map(|i| format!("第{i}行中文内容，用来测试首问长答案是否能正确顶到底部"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut state = test_state();
        state.messages.push(ChatMessage::Assistant(body));
        state.auto_scroll = true;

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(24, 8))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_live_messages(frame, area, &mut state, &theme);
            })
            .expect("draw");

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(
            rendered.contains("第23行"),
            "auto-scroll should pin the tail of long CJK content"
        );
    }

    #[test]
    fn completed_turn_auto_scrolls_markdown_table_tail_above_composer() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let diff = (0..96)
            .map(|_| {
                "+ .hero .meta-row { margin-top: 28px; display: flex; justify-content: center; gap: 32px; flex-wrap: wrap; font-size: 14px; opacity: 0.8; }"
            })
            .collect::<Vec<_>>()
            .join("\n");
        let answer =
            r#"报告已生成，保存在 `tavily-research-report.html`。下面是这份报告覆盖的核心内容概要：

📋 报告结构 (10 大章节)

| 章节 | 要点 |
| --- | --- |
| 一、公司概览 | 2024 年成立于以色列，CEO Rotem Weiss，定位 "AI Agent 的 Google" |
| 二、发展历程 | 成立 → 2025 年 17x 增长 → $25M Series A → 2026.02 被 Nebius $2.75 亿收购 |
| 三、核心产品与技术 | Search/Extract/Crawl/Research/MCP 五大 API，GAIA Benchmark SOTA |
| 四、定价模型 | Free (1K/月) → Developer ($20) → Pro ($150) → Enterprise 定制 |
| 五、竞争格局 | 与 Exa、Brave、Serper、Perplexity 的 8 维度横向对比 |
| 六、Nebius 收购分析 | $275M-$400M 交易，战略意义：补全 AI 云平台搜索能力 |
| 七、应用场景 | 编码助手/RAG/市场调研/新闻监控/学术文献 六大场景 |
| 八、关键洞察 | 成功原因 + 风险挑战 + 未来趋势判断 |
| 九、开发者资源 | SDK、MCP、LangChain、文档等速查链接 |
| 十、总结 | Agentic Search 正在成为 AI 基础设施标配 |

你可以直接在浏览器中打开 `tavily-research-report.html`
查看完整的可视化报告，支持响应式布局，手机和桌面均可阅读。"#
                .to_string();

        let mut textarea = TextArea::default();
        textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Input "),
        );
        for (width, height) in [
            (90, 24),
            (120, 32),
            (150, 42),
            (180, 52),
            (180, 63),
            (200, 70),
        ] {
            let mut state = test_state();
            state.status = AppStatus::Idle;
            state.auto_scroll = true;
            state.messages.push(ChatMessage::ToolCall {
                id: "tool-1".to_string(),
                name: "edit".to_string(),
                target: Some("site/styles.css".to_string()),
                status: "completed".to_string(),
                output: None,
                diff: Some(diff.clone()),
                kind: None,
                expanded: false,
            });
            state.messages.push(ChatMessage::Reasoning(
                "The HTML report has been created. Let me verify it and provide a summary to the user."
                    .to_string(),
            ));
            state.messages.push(ChatMessage::Assistant(answer.clone()));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("test backend");

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("draw");
            let rendered = format!("{:?}", terminal.backend().buffer());

            assert!(
                rendered.contains("支持响应式布局"),
                "completed answer tail should be visible immediately at {width}x{height}, not only after the next prompt"
            );
            assert!(
                rendered.contains("Input"),
                "composer should remain pinned below the transcript at {width}x{height}"
            );
        }
    }

    #[test]
    fn context_cell_is_hidden_until_a_budget_is_known() {
        let state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        // limit_tokens == 0 means no turn has reported a budget yet.
        assert_eq!(context_cell(&state, &theme).content.as_ref(), "");
    }

    #[test]
    fn context_cell_shows_remaining_percentage() {
        let mut state = test_state();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 250;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let cell = context_cell(&state, &theme);
        assert_eq!(cell.content.as_ref(), "  ·  context 75%");
        assert_eq!(cell.style.fg, Some(theme.success));
    }

    #[test]
    fn context_cell_warns_then_errors_as_budget_shrinks() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let mut warn = test_state();
        warn.context_limit_tokens = 1000;
        warn.context_used_tokens = 700; // 30% remaining
        assert_eq!(context_cell(&warn, &theme).style.fg, Some(theme.warning));

        let mut danger = test_state();
        danger.context_limit_tokens = 1000;
        danger.context_used_tokens = 900; // 10% remaining
        assert_eq!(context_cell(&danger, &theme).style.fg, Some(theme.error));
    }

    #[test]
    fn approval_modes_use_distinct_semantic_colors() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        assert_eq!(
            approval_mode_color(ApprovalMode::Suggest, &theme),
            theme.border
        );
        assert_eq!(
            approval_mode_color(ApprovalMode::AutoEdit, &theme),
            theme.approval
        );
        assert_eq!(
            approval_mode_color(ApprovalMode::FullAuto, &theme),
            theme.error
        );
        assert_eq!(
            approval_mode_color(ApprovalMode::Plan, &theme),
            theme.plan_mode
        );

        let colors = [theme.border, theme.approval, theme.error, theme.plan_mode];
        for (index, color) in colors.iter().enumerate() {
            assert!(!colors[..index].contains(color));
        }
    }

    #[test]
    fn status_line_renders_each_approval_mode_in_its_semantic_color() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        for mode in [
            ApprovalMode::Suggest,
            ApprovalMode::AutoEdit,
            ApprovalMode::FullAuto,
            ApprovalMode::Plan,
        ] {
            let mut state = test_state();
            state.approval_mode = mode;
            let width = 180u16;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1))
                .expect("test backend");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_status(frame, area, &state, &theme);
                })
                .expect("draw");

            let buffer = terminal.backend().buffer();
            let row = (0..width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>();
            let marker = format!("  ·  {}", mode.as_str());
            let marker_start = row.find(&marker).expect("mode should be visible");
            let value_x = (marker_start + "  ·  ".len()) as u16;
            assert_eq!(
                buffer[(value_x, 0)].fg,
                approval_mode_color(mode, &theme),
                "wrong status color for {}",
                mode.as_str()
            );
        }
    }

    #[test]
    fn responsive_status_line_keeps_mode_and_context_before_optional_metadata() {
        let mut state = test_state();
        state.context_limit_tokens = 1000;
        state.context_used_tokens = 250;
        state.usage.input_tokens = 8_000;
        state.usage.output_tokens = 664;
        state.usage.estimated_cost_usd = 0.003852;
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let narrow = status_line(&state, &theme, 46).to_string();
        assert!(narrow.contains("suggest"));
        assert!(narrow.contains("context 75%"));
        assert!(!narrow.contains("tokens"));
        assert!(!narrow.contains("shortcuts"));

        let wide = status_line(&state, &theme, 180).to_string();
        // Token counts humanize (8664 → 8.7k) and sub-cent costs keep 4 decimals.
        assert!(wide.contains("8.7k tokens"));
        assert!(wide.contains("$0.0039"));
        // Drag-to-copy is native now; the old shift+drag hint is gone.
        assert!(!wide.contains("shift+drag"));
        assert!(wide.contains("shortcuts"));
    }

    #[test]
    fn status_line_hides_usage_until_tokens_accumulate() {
        let state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);

        let text = status_line(&state, &theme, 180).to_string();
        assert!(!text.contains("tokens"));
        assert!(!text.contains('$'));
        assert!(text.contains("shortcuts"));
    }

    #[test]
    fn token_and_cost_formatting_scale_with_magnitude() {
        assert_eq!(format_token_count(950), "950");
        assert_eq!(format_token_count(8_664), "8.7k");
        assert_eq!(format_token_count(1_250_000), "1.2M");
        assert_eq!(format_cost(0.003852), "$0.0039");
        assert_eq!(format_cost(1.25), "$1.25");
    }

    #[test]
    fn jump_pill_appears_when_scrolled_up_and_leaves_with_follow() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        for index in 0..80 {
            state
                .messages
                .push(ChatMessage::System(format!("line {index}")));
        }
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");
        let mut draw = |state: &mut AppState| {
            terminal
                .draw(|frame| render(frame, state, &textarea, &theme))
                .expect("draw");
            format!("{:?}", terminal.backend().buffer())
        };

        // Following the tail: no pill.
        let following = draw(&mut state);
        assert!(!following.contains("Jump to bottom"));
        assert_eq!(state.jump_to_bottom_area, None);

        // Scrolled up: the pill appears and registers its click target.
        state.scroll_up(10);
        let scrolled = draw(&mut state);
        assert!(scrolled.contains("Jump to bottom (click) ↓"));
        assert!(state.jump_to_bottom_area.is_some());

        // Messages landing while detached turn it into an unread counter.
        state.push_message(ChatMessage::System("late one".to_string()));
        let one = draw(&mut state);
        assert!(one.contains("1 new message (click) ↓"));
        state.push_message(ChatMessage::System("late two".to_string()));
        let two = draw(&mut state);
        assert!(two.contains("2 new messages (click) ↓"));

        // Back at the bottom: gone again, count cleared for the next detach.
        state.scroll_to_bottom();
        let back = draw(&mut state);
        assert!(!back.contains("Jump to bottom"));
        assert!(!back.contains("new message"));
        assert_eq!(state.jump_to_bottom_area, None);
        assert_eq!(state.unseen_messages, 0);

        // Messages arriving while FOLLOWING never count as unread.
        state.push_message(ChatMessage::System("seen".to_string()));
        assert_eq!(state.unseen_messages, 0);
        state.scroll_up(10);
        let detached_again = draw(&mut state);
        assert!(detached_again.contains("Jump to bottom (click) ↓"));
    }

    #[test]
    fn copy_notice_overlays_input_border_and_expires() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let vim = crate::vim::VimState::new(false);
        let textarea = crate::composer_textarea::make_textarea(&vim, &theme);
        let mut state = test_state();
        let staged_at = std::time::Instant::now();
        state.stage_clipboard_copy("hello".to_string(), staged_at);
        assert_eq!(state.pending_clipboard_copy.as_deref(), Some("hello"));

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");
        let mut draw = |state: &mut AppState| {
            terminal
                .draw(|frame| render(frame, state, &textarea, &theme))
                .expect("draw");
            format!("{:?}", terminal.backend().buffer())
        };

        // Fresh: the notice overlays the input box's top border, on the right.
        let fresh = draw(&mut state);
        assert!(fresh.contains("copied 5 chars to clipboard"));

        // Status line never carries the notice anymore.
        assert!(
            !status_line(&state, &theme, 120)
                .to_string()
                .contains("copied")
        );

        // Expired: gone again.
        state.copy_notice = state.copy_notice.take().map(|mut notice| {
            notice.at = staged_at
                .checked_sub(crate::types::AppState::COPY_NOTICE_TTL)
                .expect("test instant");
            notice
        });
        let expired = draw(&mut state);
        assert!(!expired.contains("copied"));

        // Oversized for OSC 52: the notice admits only the local clipboard
        // saw it instead of overclaiming a remote copy.
        state.stage_clipboard_copy(
            "x".repeat(crate::clipboard::OSC52_MAX_TEXT_BYTES + 1),
            staged_at,
        );
        let degraded = draw(&mut state);
        assert!(degraded.contains("(local clipboard only)"));
    }

    #[test]
    fn welcome_screen_text_is_selectable_and_copyable() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        assert!(state.messages.is_empty());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let area = state.transcript_area.expect("transcript area recorded");

        let mut scratch = TextArea::default();
        let now = std::time::Instant::now();
        let event_at = |kind, column, row| {
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        crate::input_event_actions::handle_mouse_event(
            &event_at(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                area.x,
                area.y,
            ),
            &mut state,
            &mut scratch,
            now,
        );
        crate::input_event_actions::handle_mouse_event(
            &event_at(
                crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                area.x + area.width - 1,
                area.y + 6,
            ),
            &mut state,
            &mut scratch,
            now,
        );
        crate::input_event_actions::handle_mouse_event(
            &event_at(
                crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                area.x + area.width - 1,
                area.y + 6,
            ),
            &mut state,
            &mut scratch,
            now,
        );

        let copied = state
            .pending_clipboard_copy
            .as_deref()
            .expect("welcome text should be copyable");
        assert!(!copied.trim().is_empty());
    }

    #[test]
    fn long_plan_steps_and_tool_targets_stay_on_single_rows() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.current_plan = Some((
            None,
            vec![
                PlanItem {
                    step: "inspect the complete workspace topology and every package boundary"
                        .repeat(3),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    step: "run verification".to_string(),
                    status: PlanStatus::Pending,
                },
            ],
        ));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 4))
            .expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_panel(frame, area, &state, &theme);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let first_step = (0..40).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
        let second_step = (0..40).map(|x| buffer[(x, 2)].symbol()).collect::<String>();
        assert!(first_step.contains('…'));
        assert!(second_step.contains("run verification"));

        let tool = ChatMessage::ToolCall {
            id: "tool-long".to_string(),
            name: "bash".to_string(),
            target: Some("cargo test --workspace --all-features ".repeat(20)),
            status: "completed".to_string(),
            output: None,
            diff: None,
            kind: Some("success".to_string()),
            expanded: false,
        };
        let rendered = build_lines_for_messages(&[tool], &theme, 40, 0, false);
        assert_eq!(rendered[0].width(), 40);
        assert!(rendered[0].to_string().contains('…'));
        assert!(rendered[0].to_string().ends_with("(completed)"));
    }

    #[test]
    fn running_activity_line_shows_elapsed_time() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.running_started_at = Some(Instant::now() - Duration::from_secs(65));

        let (text, color) = activity_line(&state, &theme).expect("running shows an activity line");
        assert_eq!(text, "● running 1m 05s");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn active_goal_activity_line_adds_persisted_and_live_elapsed_time() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

        let (text, color) = activity_line(&state, &theme).expect("running shows an activity line");

        assert_eq!(text, "● running 13m 10s");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn active_goal_activity_line_never_decreases_across_continuations() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, 13 * 60));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));
        let first = activity_line(&state, &theme).unwrap().0;

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        state.update(TuiEvent::GoalStatus(Some(goal_with_elapsed(
            ThreadGoalStatus::Active,
            13 * 60 + 20,
        ))));
        state.update(TuiEvent::TurnStarted {
            turn: 2,
            task: None,
        });
        state.running_started_at = Some(Instant::now() - Duration::from_secs(5));
        let second = activity_line(&state, &theme).unwrap().0;

        assert_eq!(first, "● running 13m 10s");
        assert_eq!(second, "● running 13m 25s");
    }

    #[test]
    fn inactive_goal_does_not_change_the_current_turn_timer() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        for status in [
            ThreadGoalStatus::Paused,
            ThreadGoalStatus::Blocked,
            ThreadGoalStatus::UsageLimited,
            ThreadGoalStatus::BudgetLimited,
            ThreadGoalStatus::Complete,
        ] {
            let mut state = test_state();
            state.status = AppStatus::Running;
            state.current_goal = Some(goal_with_elapsed(status, 13 * 60));
            state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

            assert_eq!(activity_line(&state, &theme).unwrap().0, "● running 10s");
        }
    }

    #[test]
    fn active_goal_activity_line_clamps_negative_persisted_time() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Running;
        state.current_goal = Some(goal_with_elapsed(ThreadGoalStatus::Active, -20));
        state.running_started_at = Some(Instant::now() - Duration::from_secs(10));

        assert_eq!(activity_line(&state, &theme).unwrap().0, "● running 10s");
    }

    #[test]
    fn compacting_activity_line_shows_context_status() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Compacting;

        let (text, color) =
            activity_line(&state, &theme).expect("compacting shows an activity line");

        assert_eq!(text, "● Compacting context...");
        assert_eq!(color, theme.warning);
    }

    #[test]
    fn idle_has_no_activity_line() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.status = AppStatus::Idle;

        assert!(
            activity_line(&state, &theme).is_none(),
            "idle sessions must not render an activity line above the composer"
        );
    }

    #[test]
    fn workflow_progress_label_summarizes_agents_and_phases() {
        let task = BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "Audit".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(3),
            workflow_progress: Some(orca_core::task_types::WorkflowTaskProgress {
                total_agents: 5,
                running_agents: 2,
                completed_agents: 2,
                failed_agents: 1,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            }),
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
        };

        assert_eq!(
            workflow_progress_label(&task),
            "agents 2/5, running 2, failed 1, phases 1/3"
        );
    }

    #[test]
    fn workflows_panel_renders_async_subagent_tasks() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-subagent".to_string(),
            task_type: TaskType::Subagent,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "inspect auth".to_string(),
            command: None,
            agent_type: Some("general".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: Some(
                "/repo/.orca/workflow-sessions/s1/workflow-runs/run-1/script.js".to_string(),
            ),
            workflow_launch_input: Some(orca_core::workflow_types::WorkflowInput {
                name: Some("audit".to_string()),
                args: Some(serde_json::json!({ "target": "src" })),
                ..Default::default()
            }),
            workflow_final_summary: Some("completed with fallback review".to_string()),
            workflow_failure_count: 1,
            usage: Some(orca_core::cost_types::UsageTotals {
                input_tokens: 120,
                output_tokens: 30,
                cache_tokens: 10,
                estimated_cost_usd: 0.0000252,
            }),
            subagent_current_activity: Some("bash: cargo test".to_string()),
            subagent_turn: Some(2),
            last_activity_at_ms: Some(1_500),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            result: None,
            error: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 16))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("inspect auth"));
        assert!(rendered.contains("subagent"));
        assert!(rendered.contains("turn 2"));
        assert!(rendered.contains("150 tok"));
        assert!(rendered.contains("bash: cargo test"));
    }

    #[test]
    fn workflows_panel_renders_selected_workflow_agent_rows() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-workflow".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Completed,
            is_backgrounded: false,
            description: "Audit".to_string(),
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: vec![orca_core::task_types::WorkflowPhaseTaskSummary {
                name: "scan".to_string(),
                status: orca_core::workflow_types::WorkflowRunStatus::Failed,
                agent_count: 1,
                error: Some("scan failed".to_string()),
                fallback: Some("value".to_string()),
            }],
            workflow_agents: vec![orca_core::task_types::WorkflowAgentTaskSummary {
                call_id: "agent-1".to_string(),
                call_path: "root:1".to_string(),
                team: Some("backend".to_string()),
                status: orca_core::workflow_types::WorkflowAgentStatus::Completed,
                attempt: 2,
                max_attempts: 2,
                previous_errors: vec!["first attempt failed".to_string()],
                error: None,
                transcript_path: Some("/tmp/agent-1.json".to_string()),
                started_at_ms: Some(1_000),
                completed_at_ms: Some(3_500),
                usage: Some(orca_core::cost_types::UsageTotals {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_tokens: 10,
                    estimated_cost_usd: 0.0000252,
                }),
            }],
            workflow_script_path: Some(
                "/repo/.orca/workflow-sessions/s1/workflow-runs/run-1/script.js".to_string(),
            ),
            workflow_launch_input: Some(orca_core::workflow_types::WorkflowInput {
                name: Some("audit".to_string()),
                args: Some(serde_json::json!({ "target": "src" })),
                ..Default::default()
            }),
            workflow_final_summary: Some("completed with fallback review".to_string()),
            workflow_failure_count: 1,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(2_000),
            result: None,
            error: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 30))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("root:1"));
        assert!(rendered.contains("team backend"));
        assert!(rendered.contains("scan"));
        assert!(rendered.contains("fallback value"));
        assert!(rendered.contains("scan failed"));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("attempt 2/2"));
        assert!(rendered.contains("retry errors 1"));
        assert!(rendered.contains("elapsed 2s"));
        assert!(rendered.contains("150 tok"));
        assert!(rendered.contains("$0.000025"));
        assert!(rendered.contains("run workflow-run-1"));
        assert!(
            rendered
                .contains("script /repo/.orca/workflow-sessions/s1/workflow-runs/run-1/script.js")
        );
        assert!(rendered.contains("launch name=audit args={\"target\":\"src\"}"));
        assert!(rendered.contains("failures 1"));
        assert!(rendered.contains("final completed with fallback review"));
    }

    #[test]
    fn agents_panel_renders_all_workflow_agent_rows() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Agents;
        state.workflow_panel.tasks = vec![
            workflow_task_for_agent_dashboard(
                "audit",
                "scan",
                orca_core::workflow_types::WorkflowAgentStatus::Running,
            ),
            workflow_task_for_agent_dashboard(
                "review",
                "review",
                orca_core::workflow_types::WorkflowAgentStatus::Completed,
            ),
        ];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 18))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Agents"));
        assert!(rendered.contains("audit"));
        assert!(rendered.contains("review"));
        assert!(rendered.contains("scan"));
        assert!(rendered.contains("team scan"));
        assert!(rendered.contains("team review"));
        assert!(rendered.contains("root:scan"));
        assert!(rendered.contains("root:review"));
        assert!(rendered.contains("running"));
        assert!(rendered.contains("completed"));
        assert!(rendered.contains("150 tok"));
    }

    #[test]
    fn workflow_panel_labels_main_session_tasks() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Completed,
            is_backgrounded: false,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
        };

        assert_eq!(task_type_label(&task), "session");
        assert_eq!(task_detail_label(&task), "elapsed 3s");
    }

    #[test]
    fn workflow_panel_labels_backgrounded_main_session_tasks() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Running,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
        };

        assert!(task_detail_label(&task).starts_with("backgrounded • elapsed "));
    }

    #[test]
    fn workflow_panel_labels_backgrounded_approval_tool() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: Some("task_list".to_string()),
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
        };

        assert_eq!(
            task_detail_label(&task),
            "waiting on task_list • backgrounded • elapsed 3s"
        );
    }

    #[test]
    fn workflows_panel_renders_selected_task_error_detail() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Failed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: Some("model timed out".to_string()),
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("error"));
        assert!(rendered.contains("model timed out"));
    }

    #[test]
    fn workflows_panel_renders_selected_task_multiline_error_detail() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Failed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: Some("first failure\nsecond failure\nthird failure\nfourth failure".to_string()),
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 14))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("error"));
        assert!(rendered.contains("first failure"));
        assert!(rendered.contains("second failure"));
        assert!(rendered.contains("third failure"));
        assert!(!rendered.contains("fourth failure"));
    }

    #[test]
    fn workflow_metadata_row_count_counts_bounded_task_detail_rows() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Completed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: Some("line one\nline two\nline three\nline four".to_string()),
            error: None,
        };

        assert_eq!(workflow_metadata_row_count(&task), 3);
    }

    #[test]
    fn workflows_panel_renders_selected_task_result_detail() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Completed,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(4_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: Some("summary ready".to_string()),
            error: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("result"));
        assert!(rendered.contains("summary ready"));
    }

    #[test]
    fn workflows_panel_renders_contextual_action_hints_for_selected_task() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Running,
            is_backgrounded: true,
            description: "Summarize architecture".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("↑↓ select"));
        assert!(rendered.contains("s stop"));
        assert!(rendered.contains("Esc close"));

        state.workflow_panel.tasks[0].status = TaskStatus::Completed;
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw terminal task");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("↑↓ select"));
        assert!(!rendered.contains("s stop"));
        assert!(rendered.contains("Esc close"));
    }

    #[test]
    fn workflows_panel_renders_foreground_action_hint_for_backgrounded_main_session() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-main".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Running,
            is_backgrounded: true,
            description: "Long answer".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: None,
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(4_000),
            result: None,
            error: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("f foreground"));
    }

    #[test]
    fn workflows_panel_renders_approval_action_hint_for_selected_background_approval() {
        let mut state = test_state();
        state.panel_mode = PanelMode::Workflows;
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-approval".to_string(),
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "Needs approval".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: Some("bash".to_string()),
            pending_tool_call: Some(orca_core::task_types::PendingToolCallSummary {
                id: "mock-tool-1".to_string(),
                name: "bash".to_string(),
                action: orca_core::approval_types::ActionKind::Write,
                target: Some("cargo test".to_string()),
                arguments: "{}".to_string(),
            }),
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(1_000),
            result: None,
            error: None,
        }];
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("Enter approve"));
        assert!(rendered.contains("s stop"));
    }

    fn workflow_task_for_agent_dashboard(
        workflow_name: &str,
        call_suffix: &str,
        status: orca_core::workflow_types::WorkflowAgentStatus,
    ) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: format!("task-{workflow_name}"),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: workflow_name.to_string(),
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(workflow_name.to_string()),
            workflow_run_id: Some(format!("run-{workflow_name}")),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: vec![orca_core::task_types::WorkflowAgentTaskSummary {
                call_id: format!("agent-{call_suffix}"),
                call_path: format!("root:{call_suffix}"),
                team: Some(call_suffix.to_string()),
                status,
                attempt: 1,
                max_attempts: 2,
                previous_errors: Vec::new(),
                error: None,
                transcript_path: None,
                started_at_ms: Some(1_000),
                completed_at_ms: Some(4_000),
                usage: Some(orca_core::cost_types::UsageTotals {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_tokens: 10,
                    estimated_cost_usd: 0.0000252,
                }),
            }],
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn composer_layout_counts_soft_wrapped_visual_lines() {
        let mut textarea = TextArea::from(vec!["alpha bravo charlie".to_string()]);
        textarea.set_block(Block::default().borders(Borders::ALL));

        assert_eq!(composer_input_height(12, &textarea), 5);
    }

    #[test]
    fn composer_render_soft_wraps_long_pasted_lines() {
        let mut state = test_state();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::from(vec!["alpha bravo charlie".to_string()]);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 8))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("bravo"));
        assert!(rendered.contains("charlie"));
    }

    #[test]
    fn composer_cursor_at_wrap_boundary_belongs_to_next_visual_line() {
        let mut textarea = TextArea::default();
        for ch in "alpha bravo".chars() {
            textarea.insert_char(ch);
        }
        for _ in 0.."bravo".chars().count() {
            textarea.move_cursor(tui_textarea::CursorMove::Back);
        }

        let (_lines, cursor_line) = composer_visual_lines(&textarea, 6);

        assert_eq!(cursor_line, 1);
    }

    /// Wrapped height the scroll math sees for `text` at `width` — the same
    /// `Paragraph::line_count` call `render_live_messages` uses.
    fn measured_rows(text: &str, width: u16) -> usize {
        Paragraph::new(Line::from(text))
            .wrap(Wrap { trim: false })
            .line_count(width)
    }

    #[test]
    fn line_count_matches_ratatui_word_wrap() {
        assert_eq!(measured_rows("alpha bravo charlie", 10), 3);
    }

    #[test]
    fn line_count_hard_wraps_long_tokens() {
        assert_eq!(measured_rows("abcdefghijkl", 5), 3);
    }

    #[test]
    fn line_count_keeps_hyphenated_tokens_whole() {
        // ratatui breaks only on whitespace, so "bb-cc-dd" is one 8-wide token that
        // wraps as a unit after "aa": "aa" / "bb-cc-" / "dd" = 3 rows. A measure that
        // also broke on '-' would pack tighter and undercount to 2, under-scrolling the
        // newest content out of view.
        assert_eq!(measured_rows("aa bb-cc-dd", 6), 3);
    }

    #[test]
    fn assistant_delta_advances_only_the_tail_message_revision() {
        let mut state = test_state();
        state.push_message(ChatMessage::User("prompt".to_string()));
        state.push_message(ChatMessage::Assistant("alpha bravo charlie".to_string()));
        let before = state.message_revisions.clone();

        state.update(TuiEvent::MessageDelta(" delta".to_string()));

        assert_eq!(state.message_revisions[0], before[0]);
        assert_ne!(state.message_revisions[1], before[1]);
    }

    #[test]
    fn completed_turn_keeps_tail_marker_visible_after_large_diff() {
        let mut state = test_state();
        state.messages.push(ChatMessage::User(
            "生成一份长报告，并在最后输出固定尾部标记。".to_string(),
        ));
        let diff = (0..96)
            .map(|index| {
                format!(
                    "+     .summary-card-{index:02} {{ grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); margin-bottom: 30px; border-radius: 12px; }}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(ChatMessage::ToolCall {
            id: "tool-write".to_string(),
            name: "write_file".to_string(),
            target: Some("stock_report_20260702.html".to_string()),
            status: "completed".to_string(),
            output: Some("wrote report".to_string()),
            diff: Some(diff),
            kind: Some("success".to_string()),
            expanded: false,
        });
        let mut answer = String::new();
        answer.push_str("HTML 报告已生成：`/tmp/stock_report_20260702.html`\n\n");
        answer.push_str("📊 7月2日早市速览\n");
        for index in 1..=32 {
            answer.push_str(&format!(
                "• 第 {index:02} 条：板块分化剧烈，资金偏好在高股息、防御资产与成长题材之间快速切换，需要关注成交量、波动率和风险偏好变化。\n"
            ));
        }
        answer.push_str("EXACT_TAIL_VISIBLE_20260702");
        state.messages.push(ChatMessage::Assistant(answer));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let rendered = format!("{:?}", terminal.backend().buffer());

        assert!(
            rendered.contains("EXACT_TAIL_VISIBLE_20260702"),
            "completed assistant tail marker should be visible above the composer; rendered buffer:\n{rendered}"
        );
    }

    #[test]
    fn completed_turn_keeps_tail_marker_visible_after_large_diff_and_markdown_table() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut failures = Vec::new();
        for width in [92, 120, 160, 210, 260] {
            for height in [24, 36, 48, 64, 72] {
                let mut state = completed_table_tail_state();
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .expect("test backend");

                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .expect("draw");
                let rendered = format!("{:?}", terminal.backend().buffer());
                if !rendered.contains("EXACT_TABLE_TAIL_VISIBLE_20260702") {
                    failures.push(format!("{width}x{height}"));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "completed assistant tail marker after a wide markdown table should be visible above the composer; missing at: {}",
            failures.join(", ")
        );
    }

    fn completed_table_tail_state() -> AppState {
        let mut state = test_state();
        state.messages.push(ChatMessage::User(
            "生成一份包含宽表格的市场报告，并在最后输出固定尾部标记。".to_string(),
        ));
        let diff = (0..96)
            .map(|index| {
                format!(
                    "+     .index-card-{index:02} {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); padding: 60px 40px 50px; border-radius: 14px; }}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(ChatMessage::ToolCall {
            id: "tool-write".to_string(),
            name: "write_file".to_string(),
            target: Some("market_table_report_20260702.html".to_string()),
            status: "completed".to_string(),
            output: Some("wrote report".to_string()),
            diff: Some(diff),
            kind: Some("success".to_string()),
            expanded: false,
        });
        let mut answer = String::new();
        answer.push_str(
            "[thinking] The HTML report has been created. Let me provide a summary to the user.\n",
        );
        answer.push_str(
            "报告已生成，保存至 `/Users/bytedance/美股走势分析报告_2026年7月.html`。\n\n",
        );
        answer.push_str("📊 报告核心亮点\n\n");
        answer.push_str("| 章节 | 内容 |\n");
        answer.push_str("| --- | --- |\n");
        answer.push_str(
            "| 指数速览 | S&P 500 -0.62%、纳指 -1.21%、道指 -0.18%，盘中曾创新高但尾盘回落 |\n",
        );
        answer.push_str(
            "| Q2 回顾 | 纳指 Q2 狂飙 +21%，六年最佳；费半 +81%，历史最佳，但季末急跌预警 |\n",
        );
        answer.push_str("| 板块轮动 | 科技成长仍是主线，能源、金融和防御板块出现明显分化 |\n");
        answer.push_str("| 风险提示 | 估值扩张、流动性预期和财报窗口同时影响短线风险偏好 |\n");
        answer.push_str("| 后市展望 | 维持中性偏多，但需要观察成交量、波动率和资金流向的确认 |\n");
        answer.push_str("| 操作建议 | 仓位控制在 6-7 成，保留机动资金应对外围不确定性 |\n\n");
        for index in 1..=24 {
            answer.push_str(&format!(
                "• 第 {index:02} 条：表格之后的补充要点需要完整可见，不能停在表格开头或摘要中段。\n"
            ));
        }
        answer.push_str("EXACT_TABLE_TAIL_VISIBLE_20260702");
        state.messages.push(ChatMessage::Assistant(answer));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        state
    }

    #[test]
    fn completed_turn_keeps_tail_marker_visible_with_mixed_width_cjk_runs() {
        // Long runs mixing 2-cell CJK and 1-cell ASCII are the worst case for wrap-height
        // accounting: when a row has 1 cell left and the next char is 2 cells wide,
        // ratatui wraps early and "wastes" the cell, so a cells/width estimate undercounts
        // rows. The undercount accumulates across paragraphs and used to push the newest
        // lines below the viewport. Each width's paragraphs were found by fuzzing the old
        // estimator against ratatui's real word wrapper (estimate < actual at that width).
        let cases: &[(u16, &[&str])] = &[
            (
                34,
                &[
                    "，d能dA栈首d全2芯业是、型环训（练力栈全3首b3/片闭b）芯a全d%b是型训）模d1闭2（I型、（能型模：/1首c模能训参fd芯，型）。栈闭a首闭力-3e环 a",
                    "I-（a片%首栈界）型型数参闭）界A栈d环力1ffI型参c训环数d能闭cI，模首界gb/片闭参22业f，）（e片2业能。闭是-数A。是d闭首数数%能是（2能1gga首是A是2模个（/c环f栈全栈片全-Ia能3环训 能是芯cb栈b环 是-力%d，f1A3a片%",
                    "是模1界：（模训a，数be环、cd全/b/这闭参c，能能e2。g，A业1力能环gIb全个能bb闭首1训芯：界）模Ic力界g芯首A全型数。e （-c模首A）环首-a）、）",
                ],
            ),
            (
                61,
                &[
                    "能模型2栈环型b能-AAA数（/c2（e-环/A栈A：、力栈。闭c环界个AA力b全这个d%bbI/力这闭数A数g、bb：1芯界Ie（-I环-（、：力，片a（（。g闭能：）A（是I练 3%练模界栈界能（%力能-%：/e片a%个界c练a2",
                    "，这数%首是（/1全业b是个型闭I片栈I、能（/环数环栈力片。cg数练（全是2业。训模芯闭1界）业是%业I数栈、个。个/-界参闭e环f个首，型：能、，-栈力栈全，是个 环f（练是力闭芯数栈1环芯，c模训业I",
                    "模/个Ie练A参/栈力全Ic。 型A）A界是c片fI练2全全a：能模（gb环模，芯：f片首，）/全1a型A环%这全片模：）：这3aA 、个训Ia参芯。e2这数：：c界fggg2训是3fa",
                ],
            ),
            (
                92,
                &[
                    "%这e全练、闭。环A，、-（参I这型g（能全界参环Ag3ba模g型，21Ac训界环c。g/练个2片片全1闭：能（片%闭a片g能）环业数eb闭%首栈）d3（I型I数a能片，参1界练1训（d栈e力-A 模数栈是c1数是个力3I%、ea",
                    "个2）芯片A闭d3业（闭2这数训1。数/界全c练型训能%A1）练型训2训首是芯%，数d界c闭是练栈b、片片/练芯训d2能数-数是f（3，模Ic -：数个这这、ecgI力：型是bd环b-，界，个23片环（，片片）。3ca3e参I",
                    "能全e是栈闭。型业模力数2模：d。这、2个32首、g片数闭芯界/练模界a-。：，，1是b栈闭模e训能，这。个个全力31能型界力能a是参个、3栈环参是（1（练dc、首（g片/个栈参闭训），I 1A闭c 芯首-业：，c",
                ],
            ),
        ];

        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut failures = Vec::new();
        for &(width, paragraphs) in cases {
            for height in [24u16, 36, 48] {
                let mut state = test_state();
                state.messages.push(ChatMessage::User(
                    "输出多段中英混排长文本，并在最后输出固定尾部标记。".to_string(),
                ));
                let mut answer = String::new();
                for _ in 0..4 {
                    for paragraph in paragraphs {
                        answer.push_str(paragraph);
                        answer.push_str("\n\n");
                    }
                }
                answer.push_str("EXACT_CJK_TAIL_VISIBLE_20260702");
                state.messages.push(ChatMessage::Assistant(answer));
                state.update(TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                });

                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .expect("test backend");
                terminal
                    .draw(|frame| render(frame, &mut state, &textarea, &theme))
                    .expect("draw");
                let rendered = format!("{:?}", terminal.backend().buffer());
                if !rendered.contains("EXACT_CJK_TAIL_VISIBLE_20260702") {
                    failures.push(format!("{width}x{height}"));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "auto-scrolled tail must stay visible for mixed-width CJK/ASCII runs; missing at: {}",
            failures.join(", ")
        );
    }

    #[test]
    fn streaming_deltas_keep_the_newest_line_visible_without_user_input() {
        // Mirrors the app loop: each TuiEvent is applied, then `scroll_to_bottom()` runs
        // while auto_scroll is on, then a frame is drawn. The newest streamed text must
        // be on screen after every frame — no manual scrolling.
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        state
            .messages
            .push(ChatMessage::User("流式输出一篇长文".to_string()));
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        for index in 0..120u32 {
            state.update(TuiEvent::MessageDelta(format!(
                "第{index:03}段:混排AI模型栈能力闭环片全2芯业是、型环训（练力栈全3首b3/片闭b）尾标{index:03}\n\n"
            )));
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("draw");
            let rendered = format!("{:?}", terminal.backend().buffer());
            assert!(
                rendered.contains(&format!("尾标{index:03}")),
                "delta {index} scrolled out of view; auto_scroll={} scroll_offset={} total={} visible={}",
                state.auto_scroll,
                state.scroll_offset,
                state.total_lines,
                state.visible_height,
            );
        }
    }

    #[test]
    fn stray_wheel_up_on_first_screen_does_not_break_streaming_follow() {
        // Reported regression: after the first screenful, new streamed content stopped
        // being followed. Trigger: a wheel-up (trackpad inertia counts) while the
        // transcript still fit on one screen disarmed auto-follow with no visual
        // feedback, so the pane silently stopped tracking once content overflowed.
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        state
            .messages
            .push(ChatMessage::User("流式输出一篇长文".to_string()));
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        for index in 0..60u32 {
            state.update(TuiEvent::MessageDelta(format!(
                "第{index:03}段:混排AI模型栈能力闭环片全2芯业是、型环训（练力栈全3首b3/片闭b）尾标{index:03}\n\n"
            )));
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .expect("draw");
            // A stray wheel tick lands while everything still fits on the first screen.
            if index == 2 {
                state.scroll_up(3);
            }
            let rendered = format!("{:?}", terminal.backend().buffer());
            assert!(
                rendered.contains(&format!("尾标{index:03}")),
                "delta {index} scrolled out of view; auto_scroll={} scroll_offset={} total={} visible={}",
                state.auto_scroll,
                state.scroll_offset,
                state.total_lines,
                state.visible_height,
            );
        }
    }

    #[test]
    fn scrolling_back_to_bottom_mid_stream_re_arms_follow() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = TextArea::default();
        let mut state = test_state();
        state
            .messages
            .push(ChatMessage::User("流式输出一篇长文".to_string()));
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(92, 24))
            .expect("test backend");

        let mut draw = |state: &mut AppState| {
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
            terminal
                .draw(|frame| render(frame, state, &textarea, &theme))
                .expect("draw");
            format!("{:?}", terminal.backend().buffer())
        };

        // Stream well past one screen, then deliberately scroll up: follow disarms.
        for index in 0..40u32 {
            state.update(TuiEvent::MessageDelta(format!(
                "第{index:03}段:内容片全芯业型环训练力栈全首片闭\n\n"
            )));
            draw(&mut state);
        }
        state.scroll_up(6);
        draw(&mut state);
        assert!(
            !state.auto_scroll,
            "deliberate scroll-up should disarm follow"
        );

        // Wheel back down until the bottom is reached: follow re-arms and new
        // deltas are tracked again without further input.
        while !state.auto_scroll {
            state.scroll_down(3);
            draw(&mut state);
        }
        state.update(TuiEvent::MessageDelta(
            "重新跟随后的新内容尾标RESUME\n\n".to_string(),
        ));
        let rendered = draw(&mut state);
        assert!(
            rendered.contains("尾标RESUME"),
            "after re-arming, new deltas must be visible again"
        );
    }

    #[test]
    fn ground_truth_ratatui_wraps_hyphenated_token_on_whitespace_only() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        // Render through the real widget at the same width the scroll math uses and count
        // rows that received any glyph. This pins `Paragraph::line_count` (an unstable
        // ratatui feature) to actual render behavior, so a semantic change in a ratatui
        // upgrade shows up here instead of as a mis-scrolled transcript.
        let area = Rect::new(0, 0, 6, 8);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(Line::from("aa bb-cc-dd"))
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);

        let used_rows = (0..area.height)
            .filter(|&y| (0..area.width).any(|x| !buffer[(x, y)].symbol().trim().is_empty()))
            .count();

        assert_eq!(used_rows, 3);
        assert_eq!(measured_rows("aa bb-cc-dd", 6), used_rows);
    }

    #[test]
    fn centered_rect_stays_inside_a_non_origin_inline_viewport() {
        use ratatui::layout::Rect;
        // Reproduces the approval-dialog panic: under the inline viewport the frame area is
        // anchored below the origin (the real crash had `Rect{x:0,y:31,width:90,height:24}`).
        // A popup centered relative to (0,0) lands above the buffer and panics in
        // `Buffer::index_of`. `centered_rect` must keep the popup fully inside `area`.
        let area = Rect::new(0, 31, 90, 24);
        let popup = centered_rect(area, 64, 12);
        assert!(
            popup.y >= area.y,
            "popup top {} above viewport {}",
            popup.y,
            area.y
        );
        assert!(
            popup.bottom() <= area.bottom(),
            "popup bottom {} past viewport {}",
            popup.bottom(),
            area.bottom()
        );
        assert!(popup.right() <= area.right());
        assert!(popup.x >= area.x);
    }

    #[test]
    fn centered_rect_clamps_oversized_popup_to_area() {
        use ratatui::layout::Rect;
        // A popup larger than the (small) inline viewport must shrink to fit, never overflow.
        let area = Rect::new(0, 10, 40, 6);
        let popup = centered_rect(area, 64, 20);
        assert_eq!(popup.width, area.width);
        assert_eq!(popup.height, area.height);
        assert!(popup.bottom() <= area.bottom());
        assert!(popup.right() <= area.right());
    }

    #[test]
    fn overflowing_transcript_keeps_input_and_status_pinned() {
        // Regression: a transcript taller than the screen must NOT squeeze the input box or
        // status line off-screen. The fixed chrome stays; the transcript yields. (Previously
        // the messages area used `Constraint::Min(5)`, which has higher solver priority than
        // the `Length` chrome and stole its rows when content overflowed.)
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let mut state = test_state();
        state.status = AppStatus::Idle;
        let body = (0..80)
            .map(|i| format!("数据行内容{i}测试"))
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(ChatMessage::Assistant(body));
        state.auto_scroll = true;
        // Real composer carries a bordered "Input" block (3 rows tall), like make_textarea.
        let mut textarea = TextArea::default();
        textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Input "),
        );
        let h = 24u16;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, h))
            .expect("test backend");
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let row_text =
            |y: u16| -> String { (0..50).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        let has = |needle: &str| (0..h).any(|y| row_text(y).contains(needle));

        assert!(
            has("Input"),
            "input box must stay visible when the transcript overflows"
        );
        assert!(
            has("suggest"),
            "status line must stay visible when the transcript overflows"
        );
        // The composer (input) needs its full height; the messages area is everything above
        // the input + status, so visible_height must leave room for them.
        assert!(
            state.visible_height <= (h - 2) as usize,
            "messages area ({}) must not consume the input/status rows (term {h})",
            state.visible_height
        );
    }
}
