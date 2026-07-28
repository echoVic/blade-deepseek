use tui_textarea::TextArea;

use crate::composer_textarea::{make_textarea, make_textarea_with_text, textarea_text};
use crate::queued_input::QueuedUserMessage;
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, PanelMode};
use crate::vim::VimState;

pub(crate) fn enqueue_composer_follow_up(
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    let Some(message) = QueuedUserMessage::from_composer(
        textarea_text(textarea),
        state.pending_pastes.clone(),
        state.mention_bindings.clone(),
    ) else {
        return false;
    };

    if state.enqueue_user_message(message).is_err() {
        state.queued_input_error = Some("queued follow-up limit reached".to_string());
        return false;
    }

    state.slash_menu = None;
    state.mention.clear_projection();
    state.pending_pastes.clear();
    state.mention_bindings.clear();
    state.reset_history_navigation();
    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea(vim_state, theme);
    true
}

pub(crate) fn restore_latest_queued_message(
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    if state.panel_mode != PanelMode::Conversation
        || !matches!(state.status, AppStatus::Idle | AppStatus::Running)
        || state.transcript_search.open
        || state.show_shortcuts
        || state.slash_menu.is_some()
        || state.mention.phase.is_some()
    {
        return false;
    }
    let Some(composer) = state
        .pop_latest_queued_message()
        .map(QueuedUserMessage::into_composer_state)
    else {
        return false;
    };

    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea_with_text(&composer.visible_text, vim_state, theme);
    state.mention_bindings = composer.mention_bindings;
    state.pending_pastes = composer.pending_pastes;
    state.reset_history_navigation();
    true
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use orca_runtime::mentions::MentionBindings;

    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::queued_input::QueuedUserMessage;
    use crate::types::{AppState, AppStatus, ChatMessage};
    use crate::vim::VimState;

    fn state() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        state
    }

    fn queued(text: &str) -> QueuedUserMessage {
        QueuedUserMessage::from_composer(text.to_string(), Vec::new(), MentionBindings::default())
            .unwrap()
    }

    fn theme() -> Theme {
        Theme::named(ThemeName::Dark)
    }

    #[test]
    fn enqueue_from_composer_clears_only_after_acceptance() {
        let mut state = state();
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("follow up", &vim, &theme);

        assert!(enqueue_composer_follow_up(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(textarea_text(&textarea), "");
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.pending_pastes.is_empty());
        assert!(state.mention_bindings.is_empty());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::User(_)))
        );
    }

    #[test]
    fn full_queue_keeps_composer_and_emits_no_transcript_error() {
        let mut state = state();
        for index in 0..crate::channels::USER_ACTION_CAPACITY {
            state
                .enqueue_user_message(queued(&format!("{index}")))
                .unwrap();
        }
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("keep me", &vim, &theme);

        assert!(!enqueue_composer_follow_up(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_text(&textarea), "keep me");
        assert!(state.queued_input_error.is_some());
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::Error(_)))
        );
    }

    #[test]
    fn restore_latest_replaces_draft_and_preserves_earlier_fifo_items() {
        let mut state = state();
        state.enqueue_user_message(queued("first")).unwrap();
        state.enqueue_user_message(queued("latest")).unwrap();
        let theme = theme();
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("draft", &vim, &theme);

        assert!(restore_latest_queued_message(
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        ));
        assert_eq!(textarea_text(&textarea), "latest");
        assert_eq!(state.queued_user_messages.len(), 1);
        assert_eq!(
            state.queued_user_messages.front().unwrap().visible_text(),
            "first"
        );
        assert!(state.queued_input_error.is_none());
    }
}
