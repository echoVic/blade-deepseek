use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
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
            state.atomic_skill_tokens.clear();
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
        state.atomic_skill_tokens.clear();
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
    if delete_atomic_skill_token(key, state, textarea, vim_state, theme) {
        refresh_input_menus(textarea, state, config);
        return true;
    }
    let normalized_event = normalize_windows_altgr_event(ev);
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
            textarea.input(Input::from(normalized_event.clone()))
        }
    } else if vim_state.enabled {
        vim_state.handle(Input::from(normalized_event.clone()), textarea, theme)
    } else {
        textarea.input(Input::from(normalized_event))
    };
    if changed {
        state
            .atomic_skill_tokens
            .reconcile(&textarea_text(textarea));
        state.reset_history_navigation();
        refresh_input_menus(textarea, state, config);
    }
    changed
}

pub(crate) fn delete_atomic_skill_token(
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    if !matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        || (vim_state.enabled && vim_state.mode != crate::vim::VimMode::Insert)
    {
        return false;
    }
    let text = textarea_text(textarea);
    state.atomic_skill_tokens.reconcile(&text);
    let cursor = textarea_cursor_byte_index(textarea);
    let range = state
        .atomic_skill_tokens
        .bindings()
        .iter()
        .find(|binding| match key.code {
            KeyCode::Backspace => binding.start < cursor && cursor <= binding.end,
            KeyCode::Delete => binding.start <= cursor && cursor < binding.end,
            _ => false,
        })
        .map(|binding| binding.start..binding.end);
    let Some(range) = range else {
        return false;
    };

    let mut next = text;
    next.replace_range(range.clone(), "");
    state.atomic_skill_tokens.reconcile(&next);
    state.mention_bindings.reconcile(&next);
    state.mention.clear_projection();
    state.reset_history_navigation();
    *textarea = make_textarea_with_text_at_cursor(&next, range.start, vim_state, theme);
    true
}

fn normalize_windows_altgr_event(ev: &Event) -> Event {
    #[cfg(windows)]
    if let Event::Key(key) = ev
        && matches!(key.code, KeyCode::Char(_))
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT)
    {
        let mut normalized = *key;
        normalized
            .modifiers
            .remove(crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT);
        return Event::Key(normalized);
    }

    ev.clone()
}

#[cfg(test)]
mod windows_input_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn altgr_char_is_normalized_to_text_input_on_windows_only() {
        let event = Event::Key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        let normalized = normalize_windows_altgr_event(&event);

        if cfg!(windows) {
            assert_eq!(
                normalized,
                Event::Key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE))
            );
        } else {
            assert_eq!(normalized, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::config::ThemeName;
    use orca_runtime::mentions::{MentionBinding, MentionBindings, MentionTarget};
    use std::path::PathBuf;

    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::types::{AppState, AppStatus};

    fn bind_atomic_skill(state: &mut AppState, text: &str, visible: &str) {
        let start = text.find(visible).unwrap();
        state.atomic_skill_tokens = MentionBindings::from_bindings(
            text,
            vec![MentionBinding {
                start,
                end: start + visible.len(),
                visible: visible.to_string(),
                target: MentionTarget::Skill {
                    id: visible.trim_start_matches('$').to_string(),
                    path: PathBuf::from("/skills/test/SKILL.md"),
                },
            }],
        );
    }

    #[test]
    fn selected_skill_backspace_deletes_the_atomic_name_not_one_character() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let text = "$algorithmic-art ";
        bind_atomic_skill(&mut state, text, "$algorithmic-art");
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text(text, &vim, &theme);
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea_text(&textarea), "$algorithmic-art");

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );
        assert_eq!(textarea_text(&textarea), "");
        assert!(state.atomic_skill_tokens.is_empty());
    }

    #[test]
    fn manually_typed_skill_like_text_keeps_character_deletion() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("$algorithmic-art", &vim, &theme);
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "$algorithmic-ar");
    }

    #[test]
    fn delete_inside_selected_skill_removes_the_whole_atomic_name() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let text = "use $algorithmic-art next";
        bind_atomic_skill(&mut state, text, "$algorithmic-art");
        let config = crate::test_support::test_run_config();
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let cursor = text.find("$algorithmic-art").unwrap() + 5;
        let mut textarea = make_textarea_with_text_at_cursor(text, cursor, &vim, &theme);
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);

        apply_composer_key_input(
            &Event::Key(key),
            &key,
            &mut state,
            &config,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(textarea_text(&textarea), "use  next");
        assert!(state.atomic_skill_tokens.is_empty());
    }

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
