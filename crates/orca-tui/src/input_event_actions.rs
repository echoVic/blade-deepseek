use std::time::Instant;

use crossterm::event::{Event, MouseEventKind};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_input_actions::refresh_input_menus;
use crate::composer_textarea::insert_pasted_text;
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
            if insert_pasted_text(textarea, pasted) {
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
    if state.panel_mode == PanelMode::Conversation && state.accepts_mouse_scroll_at(now) {
        match mouse.kind {
            MouseEventKind::ScrollUp => state.scroll_up(3),
            MouseEventKind::ScrollDown => state.scroll_down(3),
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    use super::{BatchedInputEvent, coalesce_input_events};

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
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
