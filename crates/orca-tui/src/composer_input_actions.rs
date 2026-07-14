use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;
use orca_runtime::mentions;

use crate::composer_textarea::{
    make_textarea_with_text, make_textarea_with_text_at_cursor, textarea_cursor_byte_index,
    textarea_text,
};
use crate::slash_menu_actions::update_slash_menu;
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

pub(crate) fn refresh_input_menus(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    update_slash_menu(textarea, state, config);
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
        let text = textarea_text(textarea);
        let cursor = textarea_cursor_byte_index(textarea);
        let candidates = state
            .mention
            .candidates
            .iter()
            .map(|candidate| candidate.display.clone())
            .collect::<Vec<_>>();
        let token_is_current =
            mentions::mention_token_at_cursor(&text, cursor).is_some_and(|token| {
                state.mention.pending_query.as_deref() == Some(token.query.as_str())
            });
        if let Some(edit) = token_is_current
            .then(|| {
                mentions::complete_file_mention_from_candidates_at_cursor(
                    &text,
                    cursor,
                    &candidates,
                )
            })
            .flatten()
        {
            *textarea =
                make_textarea_with_text_at_cursor(&edit.text, edit.cursor, vim_state, theme);
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
