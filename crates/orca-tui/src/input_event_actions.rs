use std::time::Instant;

use crossterm::event::{Event, MouseEventKind};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_input_actions::refresh_input_menus;
use crate::composer_textarea::insert_pasted_text;
use crate::types::{AppState, AppStatus, PanelMode};

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
