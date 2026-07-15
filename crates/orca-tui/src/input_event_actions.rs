use std::time::Instant;

use crossterm::event::{Event, MouseButton, MouseEventKind};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_input_actions::refresh_input_menus;
use crate::composer_textarea::{insert_composer_paste, insert_pasted_text};
use crate::selection::TranscriptSelection;
use crate::types::{AppState, AppStatus, PanelMode};

#[derive(Debug, PartialEq)]
pub(crate) enum BatchedInputEvent {
    ScrollLines(i32),
    Event(Event),
}

pub(crate) fn coalesce_input_events(
    events: impl IntoIterator<Item = Event>,
    wheel_step: i32,
) -> Vec<BatchedInputEvent> {
    let mut batched = Vec::new();
    let mut pending_scroll = 0i32;

    let flush_scroll = |batched: &mut Vec<BatchedInputEvent>, pending: &mut i32| {
        if *pending != 0 {
            batched.push(BatchedInputEvent::ScrollLines(*pending));
            *pending = 0;
        }
    };

    for event in events {
        match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
                if pending_scroll > 0 {
                    flush_scroll(&mut batched, &mut pending_scroll);
                }
                pending_scroll = pending_scroll.saturating_sub(wheel_step);
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
                if pending_scroll < 0 {
                    flush_scroll(&mut batched, &mut pending_scroll);
                }
                pending_scroll = pending_scroll.saturating_add(wheel_step);
            }
            event => {
                flush_scroll(&mut batched, &mut pending_scroll);
                batched.push(BatchedInputEvent::Event(event));
            }
        }
    }
    flush_scroll(&mut batched, &mut pending_scroll);
    batched
}

pub(crate) fn handle_scroll_lines(state: &mut AppState, lines: i32, now: Instant) {
    if state.panel_mode != PanelMode::Conversation || !state.accepts_mouse_scroll_at(now) {
        return;
    }
    if lines < 0 {
        state.scroll_up(lines.unsigned_abs().min(u16::MAX as u32) as u16);
    } else {
        state.scroll_down((lines as u32).min(u16::MAX as u32) as u16);
    }
}

pub(crate) fn handle_paste_event(
    ev: &Event,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
) -> bool {
    let Event::Paste(pasted) = ev else {
        return false;
    };
    match state.status {
        AppStatus::Setup if state.setup_step == 1 => {
            insert_pasted_text(textarea, pasted);
        }
        AppStatus::Idle | AppStatus::WaitingUserInput => {
            if insert_composer_paste(textarea, &mut state.pending_pastes, pasted) {
                state.reset_history_navigation();
                refresh_input_menus(textarea, state, config);
            }
        }
        _ => {}
    }
    true
}

pub(crate) fn handle_mouse_event(ev: &Event, state: &mut AppState, now: Instant) -> bool {
    let Event::Mouse(mouse) = ev else {
        return false;
    };
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if state.panel_mode == PanelMode::Conversation && state.accepts_mouse_scroll_at(now) {
                if mouse.kind == MouseEventKind::ScrollUp {
                    state.scroll_up(3);
                } else {
                    state.scroll_down(3);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
            state.drag_edge_scroll = None;

            // The floating "jump to bottom" pill eats the click before any
            // selection handling: re-arm follow instead of starting a drag.
            if state.panel_mode == PanelMode::Conversation {
                if let Some(pill) = state.jump_to_bottom_area {
                    if pill.contains(ratatui::layout::Position::new(mouse.column, mouse.row)) {
                        state.scroll_to_bottom();
                        state.last_left_click = None;
                        return true;
                    }
                }
            }
            let double_click = state.last_left_click.is_some_and(|(at, column, row)| {
                now.duration_since(at) <= DOUBLE_CLICK_WINDOW
                    && column == mouse.column
                    && row == mouse.row
            });
            state.last_left_click = Some((now, mouse.column, mouse.row));

            // A press inside the transcript starts a fresh selection (or, on a
            // double click, selects the word and copies it right away); a
            // press anywhere else dismisses the current one.
            state.selection = if state.panel_mode == PanelMode::Conversation {
                let pos = state.transcript_pos_at(mouse.column, mouse.row);
                if double_click {
                    let selection =
                        pos.and_then(|pos| state.transcript_render_cache.word_selection_at(pos));
                    if let Some(selection) = &selection {
                        let text = state.transcript_render_cache.extract_text(selection);
                        if !text.is_empty() {
                            state.stage_clipboard_copy(text, now);
                        }
                    }
                    selection
                } else {
                    pos.map(TranscriptSelection::begin)
                }
            } else {
                None
            };
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if state.panel_mode == PanelMode::Conversation {
                // Dragging onto (or past) the first/last transcript row
                // scrolls, so the selection can grow beyond the visible
                // screen. The top edge must trigger on the row itself: the
                // transcript usually starts at y=0, so "above the area"
                // does not exist there.
                if let Some(area) = state.transcript_area {
                    if mouse.row <= area.y {
                        state.scroll_up(1usize);
                        state.drag_edge_scroll = Some((-1, mouse.column));
                    } else if mouse.row >= area.y.saturating_add(area.height).saturating_sub(1) {
                        state.scroll_down(1usize);
                        state.drag_edge_scroll = Some((1, mouse.column));
                    } else {
                        state.drag_edge_scroll = None;
                    }
                }
                let pos = state.transcript_pos_at_clamped(mouse.column, mouse.row);
                if let (Some(selection), Some(pos)) = (state.selection.as_mut(), pos) {
                    if selection.dragging {
                        selection.head = pos;
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            state.drag_edge_scroll = None;
            if let Some(selection) = state.selection.as_mut().filter(|sel| sel.dragging) {
                selection.dragging = false;
                let snapshot = *selection;
                if snapshot.is_empty() {
                    // A plain click selects nothing.
                    state.selection = None;
                } else {
                    let text = state.transcript_render_cache.extract_text(&snapshot);
                    if !text.is_empty() {
                        state.stage_clipboard_copy(text, now);
                    }
                }
            }
        }
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    use super::{BatchedInputEvent, coalesce_input_events, handle_mouse_event};
    use crate::theme::Theme;
    use crate::types::{AppState, ChatMessage, UserAction};

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// An AppState with one transcript message ("hello world") rendered into
    /// a 20x5 transcript area at the screen origin, scrolled to the top.
    fn state_with_transcript() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded::<UserAction>();
        let mut state = AppState::new(
            tx,
            "0.0.0".to_string(),
            "model".to_string(),
            "cwd".to_string(),
        );
        state.push_message(ChatMessage::System("seed".to_string()));
        state.reconcile_message_tracking();
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            20,
            &theme,
            0,
            false,
            |_, _, _, _, _| vec![Line::from("hello world")],
        );
        state.transcript_area = Some(Rect::new(0, 0, 20, 5));
        state.viewport_base_row = 0;
        state
    }

    #[test]
    fn drag_selects_and_release_stages_a_clipboard_copy() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 6, 0),
            &mut state,
            now,
        );
        assert!(state.selection.is_some_and(|sel| sel.dragging));

        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );

        assert!(state.selection.is_some_and(|sel| !sel.dragging));
        assert_eq!(state.pending_clipboard_copy.as_deref(), Some("world"));
    }

    #[test]
    fn plain_click_clears_selection_without_copying() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );

        assert_eq!(state.selection, None);
        assert_eq!(state.pending_clipboard_copy, None);
    }

    #[test]
    fn press_outside_the_transcript_dismisses_the_selection() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 6, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 10, 0),
            &mut state,
            now,
        );
        assert!(state.selection.is_some());

        // Next press lands below the transcript area (row 30).
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 6, 30),
            &mut state,
            now,
        );
        assert_eq!(state.selection, None);
    }

    #[test]
    fn drag_beyond_the_area_clamps_to_the_nearest_cell() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 0, 0),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 50, 50),
            &mut state,
            now,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 50, 50),
            &mut state,
            now,
        );

        // Clamped to the area's bottom-right cell; extraction still stops at
        // the actual content, so the whole line is copied.
        assert_eq!(state.pending_clipboard_copy.as_deref(), Some("hello world"));
    }

    #[test]
    fn wheel_events_still_scroll_and_do_not_touch_selection() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        handle_mouse_event(&mouse(MouseEventKind::ScrollUp), &mut state, now);
        assert_eq!(state.selection, None);
        assert_eq!(state.pending_clipboard_copy, None);
    }

    #[test]
    fn double_click_selects_the_word_and_copies_immediately() {
        let mut state = state_with_transcript();
        let now = Instant::now();

        for _ in 0..2 {
            handle_mouse_event(
                &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
                &mut state,
                now,
            );
            handle_mouse_event(
                &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
                &mut state,
                now,
            );
        }

        assert!(state.selection.is_some_and(|sel| !sel.dragging));
        assert_eq!(state.pending_clipboard_copy.as_deref(), Some("world"));
    }

    #[test]
    fn slow_second_click_does_not_word_select() {
        let mut state = state_with_transcript();
        let first = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &mut state,
            first,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &mut state,
            first,
        );
        let later = first + std::time::Duration::from_millis(800);
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &mut state,
            later,
        );
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &mut state,
            later,
        );

        assert_eq!(state.selection, None);
        assert_eq!(state.pending_clipboard_copy, None);
    }

    #[test]
    fn dragging_past_the_edges_scrolls_the_transcript() {
        let mut state = state_with_transcript();
        // Pretend the transcript overflows the area so scrolling is possible.
        state.total_lines = 50;
        state.visible_height = 5;
        state.scroll_offset = 10;
        state.auto_scroll = false;
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &mut state,
            now,
        );
        // Dragging below the bottom edge scrolls down...
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 40),
            &mut state,
            now,
        );
        assert_eq!(state.scroll_offset, 11);
        // ...and dragging onto the TOP ROW scrolls up. The transcript area
        // starts at y=0, so there is no "above the area" — the first row
        // itself must trigger.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert_eq!(state.scroll_offset, 10);
    }

    #[test]
    fn parked_pointer_at_the_edge_keeps_scrolling_via_animation_ticks() {
        let mut state = state_with_transcript();
        state.total_lines = 50;
        state.visible_height = 5;
        state.scroll_offset = 10;
        state.auto_scroll = false;
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 2),
            &mut state,
            now,
        );
        // Reaching the top row arms edge auto-scroll (and scrolls once).
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert_eq!(state.drag_edge_scroll, Some((-1, 3)));
        assert_eq!(state.scroll_offset, 9);
        let head_before = state.selection.unwrap().head;

        // With the pointer parked (no further mouse events), animation ticks
        // keep scrolling and keep growing the selection upward.
        state.apply_drag_edge_scroll();
        state.apply_drag_edge_scroll();
        assert_eq!(state.scroll_offset, 7);
        let head_after = state.selection.unwrap().head;
        assert_eq!(head_after.row, head_before.row.saturating_sub(2));

        // Dragging back inside the area disarms it...
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 2),
            &mut state,
            now,
        );
        assert_eq!(state.drag_edge_scroll, None);

        // ...and so does releasing the button at the edge.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Drag(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert!(state.drag_edge_scroll.is_some());
        handle_mouse_event(
            &mouse_at(MouseEventKind::Up(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert_eq!(state.drag_edge_scroll, None);
        // A settled (non-dragging) selection is no longer grown by ticks.
        let settled_head = state.selection.unwrap().head;
        state.apply_drag_edge_scroll();
        assert_eq!(state.selection.unwrap().head, settled_head);
    }

    #[test]
    fn clicking_the_jump_pill_rearms_follow_instead_of_selecting() {
        let mut state = state_with_transcript();
        state.total_lines = 50;
        state.visible_height = 5;
        state.scroll_offset = 10;
        state.auto_scroll = false;
        state.jump_to_bottom_area = Some(Rect::new(5, 4, 10, 1));
        let now = Instant::now();

        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 7, 4),
            &mut state,
            now,
        );

        assert!(state.auto_scroll);
        assert_eq!(state.scroll_offset, 45);
        // The click was consumed by the pill: no selection was started.
        assert_eq!(state.selection, None);

        // A click elsewhere still starts a selection as usual.
        handle_mouse_event(
            &mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 0),
            &mut state,
            now,
        );
        assert!(state.selection.is_some());
    }

    #[test]
    fn adjacent_wheel_events_collapse_to_a_signed_line_delta() {
        let events = vec![
            mouse(MouseEventKind::ScrollUp),
            mouse(MouseEventKind::ScrollUp),
            mouse(MouseEventKind::ScrollDown),
        ];

        assert_eq!(
            coalesce_input_events(events, 3),
            vec![
                BatchedInputEvent::ScrollLines(-6),
                BatchedInputEvent::ScrollLines(3),
            ]
        );
    }

    #[test]
    fn non_wheel_events_preserve_order_and_split_scroll_runs() {
        let resize = Event::Resize(120, 40);
        let key = Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        let events = vec![
            mouse(MouseEventKind::ScrollUp),
            resize.clone(),
            mouse(MouseEventKind::ScrollDown),
            key.clone(),
        ];

        assert_eq!(
            coalesce_input_events(events, 3),
            vec![
                BatchedInputEvent::ScrollLines(-3),
                BatchedInputEvent::Event(resize),
                BatchedInputEvent::ScrollLines(3),
                BatchedInputEvent::Event(key),
            ]
        );
    }
}
