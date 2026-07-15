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
                .cloned()
            {
                let text = textarea_text(textarea);
                let cursor = textarea_cursor_byte_index(textarea);
                if let Some(edit) =
                    mentions::apply_mention_selection_at_cursor(&text, cursor, &candidate.display)
                {
                    *textarea = make_textarea_with_text_at_cursor(
                        &edit.text,
                        edit.cursor,
                        vim_state,
                        theme,
                    );
                    if !candidate.is_directory() {
                        state.mention_bindings.apply_selection(
                            &text,
                            &edit,
                            candidate.target.clone(),
                        );
                    }
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

pub(crate) fn mark_manual_selection(state: &mut AppState) {
    state.mention.manual_selection = true;
    state.mention.selected_identity = state
        .mention
        .candidates
        .get(state.mention.selected)
        .map(|candidate| candidate.id.clone());
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use std::path::PathBuf;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::config::ThemeName;
    use orca_file_search::{MatchKind, SearchMatch};
    use orca_runtime::mentions::{MentionCandidate, MentionFileKind, MentionTarget};

    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};

    fn state() -> AppState {
        let (event_tx, _event_rx) = mpsc::unbounded();
        AppState::new(
            event_tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/workspace".to_string(),
        )
    }

    fn enter() -> (Event, KeyEvent) {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        (Event::Key(key), key)
    }

    #[test]
    fn selecting_file_inserts_visible_text_and_records_exact_target() {
        let mut state = state();
        state.mention.candidates = vec![MentionCandidate::from_file_match(&SearchMatch {
            root: PathBuf::from("/workspace/backend"),
            path: "same.txt".to_string(),
            kind: MatchKind::File,
            score: 42,
            indices: vec![0],
        })];
        let theme = Theme::named(ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("review @sa", &vim_state, &theme);
        let (event, key) = enter();

        assert!(handle_mention_menu_key(
            &event,
            &key,
            &mut state,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "review @same.txt ");
        assert_eq!(state.mention_bindings.bindings().len(), 1);
        assert_eq!(
            state.mention_bindings.bindings()[0].target,
            MentionTarget::File {
                root: PathBuf::from("/workspace/backend"),
                path: "same.txt".to_string(),
                kind: MentionFileKind::File,
            }
        );
        assert!(state.mention.candidates.is_empty());
    }

    #[test]
    fn selecting_directory_continues_browsing_without_atomic_binding() {
        let mut state = state();
        state.mention.candidates = vec![MentionCandidate::from_file_match(&SearchMatch {
            root: PathBuf::from("/workspace"),
            path: "src/".to_string(),
            kind: MatchKind::Directory,
            score: 42,
            indices: vec![0],
        })];
        let theme = Theme::named(ThemeName::Dark);
        let vim_state = VimState::new(false);
        let mut textarea = make_textarea_with_text("review @s", &vim_state, &theme);
        let (event, key) = enter();

        assert!(handle_mention_menu_key(
            &event,
            &key,
            &mut state,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "review @src/");
        assert!(state.mention_bindings.is_empty());
    }
}
