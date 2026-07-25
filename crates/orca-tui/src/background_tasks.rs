use crossbeam_channel as mpsc;

use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};

use crate::surface_actions::TuiSurfaceActions;
use crate::types::TuiEvent;

pub(crate) fn stop_task_for_tui(
    actions: Option<&TuiSurfaceActions>,
    task_id: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot stop task before a session exists".to_string(),
        ));
        return false;
    };
    match actions.stop_task(task_id) {
        Ok(tasks) => {
            let _ = event_tx.send(TuiEvent::WorkflowTasksUpdated { tasks });
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Task stop requested for {task_id}."
            )));
            true
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            false
        }
    }
}

pub(crate) fn foreground_task_for_tui(
    actions: Option<&TuiSurfaceActions>,
    task_id: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> bool {
    let Some(actions) = actions else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot foreground task before a session exists".to_string(),
        ));
        return false;
    };

    match actions.foreground_task(task_id) {
        Ok(tasks) => {
            let _ = event_tx.send(TuiEvent::WorkflowTasksUpdated { tasks });
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Task {task_id} returned to foreground."
            )));
            true
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            false
        }
    }
}

pub(crate) fn notify_recovered_background_approvals_for_tui(
    actions: &TuiSurfaceActions,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> usize {
    let tasks = actions.task_summaries();
    let recovered_tools = tasks
        .iter()
        .filter(|task| recovered_background_approval_task(task))
        .filter_map(|task| {
            task.pending_tool_call
                .as_ref()
                .map(|pending_tool| pending_tool.name.clone())
        })
        .collect::<Vec<_>>();

    if recovered_tools.is_empty() {
        return 0;
    }

    let count = recovered_tools.len();
    let _ = event_tx.send(TuiEvent::WorkflowTasksUpdated { tasks });
    let summary = if count == 1 {
        format!(
            "Recovered background session waiting for approval for {}.",
            recovered_tools[0]
        )
    } else {
        format!(
            "Recovered {count} background sessions waiting for approval: {}.",
            recovered_tools.join(", ")
        )
    };
    let _ = event_tx.send(TuiEvent::Notice(summary));
    count
}

pub(crate) fn is_terminal_task_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Stopped
    )
}

fn recovered_background_approval_task(task: &BackgroundTaskSummary) -> bool {
    task.task_type == TaskType::MainSession
        && task.is_backgrounded
        && task.status == TaskStatus::ApprovalRequired
        && task.pending_tool_call.is_some()
}
