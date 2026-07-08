use std::sync::mpsc;

use orca_runtime::tasks::TaskRegistry;

use crate::bridge;
use crate::types::TuiEvent;

pub(crate) fn submit_background_approval_response_for_tui(
    task_registry: Option<&TaskRegistry>,
    approval_id: &str,
    approved: bool,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<bridge::TuiBackgroundTurnContinuationRequest> {
    let Some(task_registry) = task_registry else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot resolve background approval before a session exists".to_string(),
        ));
        return None;
    };

    match task_registry.submit_pending_tool_approval_response_by_request_id(approval_id, approved) {
        Ok(task_id) => {
            if !approved
                && let Err(error) = task_registry.finish_denied_pending_tool_approval(&task_id)
            {
                let _ = event_tx.send(TuiEvent::Error(error));
                return None;
            }
            let _ = event_tx.send(TuiEvent::WorkflowTasksUpdated {
                tasks: task_registry.list(),
            });
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
