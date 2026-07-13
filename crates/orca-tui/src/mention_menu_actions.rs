use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_runtime::mentions;

use crate::composer_textarea::{
    make_textarea_with_text_at_cursor, textarea_cursor_byte_index, textarea_text,
};
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

pub(crate) fn handle_mention_menu_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    match key.code {
        KeyCode::Up => {
            state.mention.selected = state.mention.selected.saturating_sub(1);
            mark_manual_selection(state);
            true
        }
        KeyCode::Down => {
            let max = state.mention.candidates.len().saturating_sub(1);
            if state.mention.selected < max {
                state.mention.selected += 1;
            }
            mark_manual_selection(state);
            true
        }
        KeyCode::Tab | KeyCode::Enter => {
            if let Some(candidate) = state
                .mention
                .candidates
                .get(state.mention.selected)
                .map(|candidate| candidate.path.clone())
            {
                let text = textarea_text(textarea);
                let cursor = textarea_cursor_byte_index(textarea);
                if let Some(edit) =
                    mentions::apply_mention_selection_at_cursor(&text, cursor, &candidate)
                {
                    *textarea = make_textarea_with_text_at_cursor(
                        &edit.text,
                        edit.cursor,
                        vim_state,
                        theme,
                    );
                    state.mention.clear_projection();
                }
            }
            true
        }
        KeyCode::Esc => {
            let text = textarea_text(textarea);
            let cursor = textarea_cursor_byte_index(textarea);
            state.mention.dismissed_query =
                mentions::mention_token_at_cursor(&text, cursor).map(|token| token.query);
            state.mention.clear_projection();
            true
        }
        _ => {
            textarea.input(Input::from(ev.clone()));
            true
        }
    }
}

fn mark_manual_selection(state: &mut AppState) {
    state.mention.manual_selection = true;
    state.mention.selected_path = state
        .mention
        .candidates
        .get(state.mention.selected)
        .map(|candidate| candidate.path.clone());
}
