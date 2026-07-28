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
use crate::types::{AppState, AppStatus};
use crate::vim::VimState;

pub(crate) fn refresh_input_menus(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    if state.status == AppStatus::Idle {
        update_slash_menu(textarea, state, config);
    } else {
        state.slash_menu = None;
    }
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
        vim_state.cancel_pending_command();
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

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::config::ThemeName;

    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::types::{AppState, AppStatus};

    #[test]
    fn running_slash_text_never_opens_local_command_menu() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.status = AppStatus::Running;
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("/compac", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "/compact");
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn tab_clears_pending_vim_prefix_before_direct_textarea_input() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = crate::test_support::test_run_config();
        config.vim_mode = true;
        let theme = Theme::named(ThemeName::Dark);
        let mut expected_vim = VimState::new(true);
        let mut expected = make_textarea_with_text("abcd", &expected_vim, &theme);
        expected.move_cursor(tui_textarea::CursorMove::Head);
        let mut vim = VimState::new(true);
        let mut textarea = make_textarea_with_text("abcd", &vim, &theme);
        textarea.move_cursor(tui_textarea::CursorMove::Head);

        for code in [KeyCode::Tab, KeyCode::Char('x')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            apply_composer_key_input(
                &Event::Key(key),
                &key,
                &mut state,
                &config,
                &mut expected,
                &mut expected_vim,
                &theme,
            );
        }

        for code in [KeyCode::Char('2'), KeyCode::Tab, KeyCode::Char('x')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            apply_composer_key_input(
                &Event::Key(key),
                &key,
                &mut state,
                &config,
                &mut textarea,
                &mut vim,
                &theme,
            );
        }

        assert_eq!(textarea_text(&textarea), textarea_text(&expected));
        assert_eq!(textarea.cursor(), expected.cursor());
        assert!(!vim.has_pending_command_for_test());
    }
}
