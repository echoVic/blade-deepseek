use std::sync::{Arc, Mutex};

use orca_core::config::RunConfig;

use crate::types::{AppState, ChatMessage};

pub(crate) fn cycle_approval_mode(
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
) {
    let next = config.approval_mode.next();
    config.approval_mode = next;
    if let Ok(mut cfg) = shared_config.lock() {
        cfg.approval_mode = next;
    }
    state.approval_mode = next;
    state.messages.push(ChatMessage::System(format!(
        "Approval mode switched to {}.",
        next.as_str()
    )));
    state.scroll_to_bottom();
}
