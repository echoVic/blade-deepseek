use crossbeam_channel as mpsc;

use crate::bridge;
use crate::surface_actions::TuiSurfaceActions;
use crate::types::TuiEvent;

pub(crate) fn submit_background_approval_response_for_tui(
    actions: Option<&TuiSurfaceActions>,
    approval_id: &str,
    approved: bool,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<bridge::TuiBackgroundTurnContinuationRequest> {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot resolve background approval before a session exists".to_string(),
        ));
        return None;
    };

    match actions.resolve_background_approval(approval_id, approved) {
        Ok((task_id, tasks)) => {
            let _ = event_tx.send(TuiEvent::WorkflowTasksUpdated { tasks });
            let decision = if approved { "approved" } else { "denied" };
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Background approval {decision} for {task_id}."
            )));
            if approved {
                Some(bridge::TuiBackgroundTurnContinuationRequest::new(task_id))
            } else {
                None
            }
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            None
        }
    }
}
