use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};

use orca_core::config::{HistoryMode, RunConfig};
use orca_runtime::history::SessionTranscript;

use crate::types::{AppState, AppStatus};

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
    // The runtime owns session resolution and typed history projection. Clear
    // the old view now; the controller emits HistoryLoaded after it acquires
    // the resumed thread and reads its durable surface snapshot.
    state.replace_messages(Vec::new());
    state.scroll_offset = 0;
    state.auto_scroll = true;
    state.current_plan = None;
    state.plan_update_failed = false;
    state.finalized_count = 0;
    if let Ok(mut preloaded) = preloaded_transcript.lock() {
        *preloaded = None;
    }
    clear_terminal()?;
    state.set_status(AppStatus::Idle);
    Ok(())
}
