use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_textarea::{expand_pending_pastes, make_textarea, textarea_text};
use crate::slash_command_actions::{SlashOutcome, handle_slash_command};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, ChatMessage, UserAction};
use crate::vim::VimState;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_submit(
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    state.slash_menu = None;
    let visible_text = textarea_text(textarea);
    state.mention_bindings.reconcile(&visible_text);
    let expanded_text = expand_pending_pastes(&visible_text, &state.pending_pastes);
    state.mention_bindings.reconcile(&expanded_text);
    let text = expanded_text.trim().to_string();
    state.mention_bindings.reconcile(&text);
    if text.is_empty() {
        return false;
    }

    if let Some(outcome) = handle_slash_command(&text, config, shared_config, state, action_tx) {
        match outcome {
            SlashOutcome::Continue => {
                state.pending_pastes.clear();
                state.mention_bindings.clear();
                reset_composer_after_submit(textarea, vim_state, theme);
                return true;
            }
        }
    }

    if state.status == AppStatus::WaitingUserInput {
        state.enter_running();
        state.scroll_to_bottom();
        if let Some(id) = state.pending_user_input_id.take() {
            let _ = action_tx.send(UserAction::RespondToUserInput { id, answer: text });
        }
    } else {
        state.record_prompt(text.clone());
        state.push_message(ChatMessage::User(visible_text.trim().to_string()));
        state.enter_running();
        state.scroll_to_bottom();
        let bindings = state.mention_bindings.clone();
        let _ = action_tx.send(UserAction::SubmitWithMentions {
            prompt: text,
            bindings,
        });
    }
    state.pending_pastes.clear();
    state.mention_bindings.clear();
    reset_composer_after_submit(textarea, vim_state, theme);
    true
}

fn reset_composer_after_submit(textarea: &mut TextArea, vim_state: &mut VimState, theme: &Theme) {
    vim_state.reset_insert(textarea, theme);
    *textarea = make_textarea(vim_state, theme);
}
