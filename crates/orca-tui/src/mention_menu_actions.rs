use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::RunConfig;
use orca_runtime::mentions;

use crate::composer_textarea::{make_textarea_with_text, textarea_text};
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

pub(crate) fn update_mention_candidates(
    textarea: &TextArea,
    state: &mut AppState,
    config: &RunConfig,
) {
    if state.slash_menu.is_some() {
        state.mention_candidates.clear();
        state.mention_selected = 0;
        return;
    }
    let text = textarea_text(textarea);
    let has_at_token = text
        .rfind('@')
        .is_some_and(|pos| pos == 0 || text.as_bytes()[pos - 1].is_ascii_whitespace());
    if !has_at_token {
        state.mention_candidates.clear();
        state.mention_selected = 0;
        return;
    }
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let candidates = mentions::list_mention_candidates(&text, &cwd);
    if candidates != state.mention_candidates {
        state.mention_selected = 0;
    }
    state.mention_candidates = candidates;
}

pub(crate) fn handle_mention_menu_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    match key.code {
        KeyCode::Up => {
            state.mention_selected = state.mention_selected.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            let max = state.mention_candidates.len().saturating_sub(1);
            if state.mention_selected < max {
                state.mention_selected += 1;
            }
            true
        }
        KeyCode::Tab | KeyCode::Enter => {
            if let Some(candidate) = state
                .mention_candidates
                .get(state.mention_selected)
                .cloned()
            {
                let text = textarea_text(textarea);
                let applied = mentions::apply_mention_selection(&text, &candidate);
                *textarea = make_textarea_with_text(&applied, vim_state, theme);
                state.mention_candidates.clear();
                state.mention_selected = 0;
                let cwd = config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                if candidate.ends_with('/') {
                    state.mention_candidates = mentions::list_mention_candidates(&applied, &cwd);
                }
            }
            true
        }
        KeyCode::Esc => {
            state.mention_candidates.clear();
            state.mention_selected = 0;
            true
        }
        _ => {
            textarea.input(Input::from(ev.clone()));
            update_mention_candidates(textarea, state, config);
            true
        }
    }
}
