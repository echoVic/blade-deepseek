use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};

use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history;
use orca_runtime::history::SessionTranscript;

use crate::app::chat_message_from_history;
use crate::types::{AppState, AppStatus, ChatMessage};

pub(crate) fn handle_session_picker_key<F>(
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    preloaded_transcript: &Arc<Mutex<Option<SessionTranscript>>>,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    match key.code {
        KeyCode::Up => state.select_previous_session(),
        KeyCode::Down => state.select_next_session(),
        KeyCode::Backspace => state.session_query_pop(),
        KeyCode::Char(c) => state.session_query_push(c),
        KeyCode::Enter => {
            resume_selected_session(
                state,
                config,
                shared_config,
                preloaded_transcript,
                clear_terminal,
            )?;
        }
        KeyCode::Esc => {
            state.set_status(AppStatus::Idle);
            state.session_picker_sessions.clear();
            state.session_picker_query.clear();
        }
        _ => {}
    }
    Ok(())
}

fn resume_selected_session<F>(
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    preloaded_transcript: &Arc<Mutex<Option<SessionTranscript>>>,
    clear_terminal: F,
) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let Some(session_id) = state.selected_session_id() else {
        return Ok(());
    };
    config.history_mode = HistoryMode::Resume(session_id.clone());
    if let Ok(mut cfg) = shared_config.lock() {
        cfg.history_mode = HistoryMode::Resume(session_id.clone());
    }
    if let Ok(transcript) = history::load_session(&session_id) {
        state.messages.clear();
        state.flushed_count = 0;
        state.scroll_offset = 0;
        state.auto_scroll = true;
        for message in &transcript.messages {
            if let Some(chat_message) = chat_message_from_history(message.clone()) {
                state.messages.push(chat_message);
            }
        }
        if let Some((explanation, plan)) = &transcript.plan {
            state.current_plan = Some((explanation.clone(), plan.clone()));
        } else {
            state.current_plan = None;
        }
        state.plan_update_failed = false;
        state.messages.push(ChatMessage::System(
            "Resumed saved conversation.".to_string(),
        ));
        state.finalized_count = state.messages.len();
        if let Ok(mut preloaded) = preloaded_transcript.lock() {
            *preloaded = Some(transcript);
        }
        clear_terminal()?;
    }
    state.set_status(AppStatus::Idle);
    Ok(())
}
