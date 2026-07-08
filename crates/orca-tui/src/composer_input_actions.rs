use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;
use orca_runtime::mentions;

use crate::composer_textarea::{make_textarea_with_text, textarea_text};
use crate::mention_menu_actions::update_mention_candidates;
use crate::slash_menu_actions::update_slash_menu;
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

pub(crate) fn refresh_input_menus(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    update_slash_menu(textarea, state, config);
    update_mention_candidates(textarea, state, config);
}

pub(crate) fn insert_composer_newline(textarea: &mut TextArea, state: &mut AppState) {
    textarea.insert_newline();
    state.reset_history_navigation();
}

pub(crate) fn recall_previous_history(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) {
    if key.code == KeyCode::Up && textarea.lines().len() > 1 {
        textarea.input(Input::from(ev.clone()));
    } else {
        let draft = textarea_text(textarea);
        if let Some(history) = state.history_previous(draft) {
            *textarea = make_textarea_with_text(&history, vim_state, theme);
        }
    }
}

pub(crate) fn recall_next_history(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) {
    if key.code == KeyCode::Down && textarea.lines().len() > 1 {
        textarea.input(Input::from(ev.clone()));
    } else if let Some(history) = state.history_next() {
        *textarea = make_textarea_with_text(&history, vim_state, theme);
    }
}

pub(crate) fn apply_composer_key_input(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    let changed = if key.code == KeyCode::Tab {
        let cwd = config
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let text = textarea_text(textarea);
        if let Some(completed) = mentions::complete_file_mention(&text, &cwd) {
            *textarea = make_textarea_with_text(&completed, vim_state, theme);
            true
        } else {
            textarea.input(Input::from(ev.clone()))
        }
    } else if vim_state.enabled {
        vim_state.handle(Input::from(ev.clone()), textarea, theme)
    } else {
        textarea.input(Input::from(ev.clone()))
    };
    if changed {
        state.reset_history_navigation();
        refresh_input_menus(textarea, state, config);
    }
    changed
}
