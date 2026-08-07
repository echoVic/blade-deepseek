use std::io;
use std::sync::Mutex;

use crossbeam_channel::Sender;
use orca_core::event_schema::EventEnvelope;
use orca_core::event_sink::EventObserver;
use orca_core::task_types::{TaskStatus, TaskType};

use crate::runtime_event_projection::tui_event_from_runtime_event;
use crate::types::TuiEvent;

pub(crate) enum TuiHostedOperationOutcome {
    Turn { status: String },
    ManualCompaction,
}

pub(crate) struct TuiHostedEventObserver {
    event_tx: Sender<TuiEvent>,
    queued_id: Option<u64>,
    state: Mutex<TuiHostedEventObserverState>,
}

#[derive(Default)]
struct TuiHostedEventObserverState {
    foreground_finished: bool,
    queued_submission_started: bool,
    terminal_event: Option<TuiEvent>,
}

impl TuiHostedEventObserver {
    pub(crate) fn new(event_tx: Sender<TuiEvent>) -> Self {
        Self::new_with_queued_id(event_tx, None)
    }

    pub(crate) fn new_with_queued_id(event_tx: Sender<TuiEvent>, queued_id: Option<u64>) -> Self {
        Self {
            event_tx,
            queued_id,
            state: Mutex::new(TuiHostedEventObserverState::default()),
        }
    }

    pub(crate) fn finish_foreground(&self) -> io::Result<bool> {
        let terminal_event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.foreground_finished = true;
            state.terminal_event.take()
        };
        if let Some(event) = terminal_event {
            self.send(event)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn queued_submission_started(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queued_submission_started
    }

    fn send(&self, event: TuiEvent) -> io::Result<()> {
        self.event_tx.send(event).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI event channel closed while observing hosted runtime event",
            )
        })
    }
}

impl EventObserver for TuiHostedEventObserver {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        let Some(event) = tui_event_from_runtime_event(event) else {
            return Ok(());
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if is_operation_terminal_event(&event) && !state.foreground_finished {
            state.terminal_event = Some(event);
            return Ok(());
        }
        if matches!(event, TuiEvent::TurnStarted { .. })
            && let Some(id) = self.queued_id
            && !state.queued_submission_started
        {
            state.queued_submission_started = true;
            drop(state);
            self.send(TuiEvent::QueuedSubmissionStarted { id })?;
        } else {
            drop(state);
        }
        let notice = background_task_notice_from_event(&event);
        self.send(event)?;
        if let Some(notice) = notice {
            self.send(TuiEvent::Notice(notice))?;
        }
        Ok(())
    }
}

fn background_task_notice_from_event(event: &TuiEvent) -> Option<String> {
    let TuiEvent::WorkflowTaskUpdated { task } = event else {
        return None;
    };
    if !task.is_backgrounded
        || !matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::ApprovalRequired
                | TaskStatus::Stopped
                | TaskStatus::Cancelled
        )
    {
        return None;
    }
    let subject = if task.task_type == TaskType::MainSession {
        "Background session".to_string()
    } else {
        format!("Background task '{}'", task.description)
    };
    Some(match task.status {
        TaskStatus::ApprovalRequired => match task.tool.as_deref() {
            Some(tool) => {
                format!("{subject} needs approval for {tool} before it can continue.")
            }
            None => format!("{subject} needs approval before it can continue."),
        },
        TaskStatus::Completed if task.result.is_some() => {
            format!("{subject} completed: success. Result is ready.")
        }
        TaskStatus::Completed => format!("{subject} completed: success"),
        TaskStatus::Failed => format!("{subject} completed: failed"),
        TaskStatus::Stopped => format!("{subject} completed: stopped"),
        TaskStatus::Cancelled => format!("{subject} completed: cancelled"),
        TaskStatus::Queued | TaskStatus::Running | TaskStatus::Paused | TaskStatus::Stopping => {
            unreachable!("non-terminal task status")
        }
    })
}

fn is_operation_terminal_event(event: &TuiEvent) -> bool {
    matches!(
        event,
        TuiEvent::SessionCompleted { .. } | TuiEvent::Compacted { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::observe_event;
    use orca_core::task_types::BackgroundTaskSummary;

    use super::*;

    fn background_task(task_type: TaskType, status: TaskStatus) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type,
            status,
            is_backgrounded: true,
            description: "RDAP lookup".to_string(),
            created_at_ms: 1,
            started_at_ms: Some(2),
            completed_at_ms: Some(3),
            command: None,
            agent_type: None,
            server: None,
            tool: Some("bash".to_string()),
            pending_tool_call: None,
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: Some(3),
            result: Some("domain is available".to_string()),
            error: None,
            retry_count: 0,
            output_truncated: false,
        }
    }

    #[test]
    fn background_shell_completion_generates_a_result_notice() {
        let task = background_task(TaskType::Shell, TaskStatus::Completed);
        let notice = background_task_notice_from_event(&TuiEvent::WorkflowTaskUpdated { task });

        assert_eq!(
            notice.as_deref(),
            Some("Background task 'RDAP lookup' completed: success. Result is ready.")
        );
    }

    #[test]
    fn hosted_observer_defers_terminal_until_operation_cleanup_finishes() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let observer = TuiHostedEventObserver::new(event_tx);
        let mut events = EventFactory::new("hosted-terminal-order".to_string());
        let identity = orca_core::thread_item_projection::ModelResponseIdentity::new(
            orca_core::thread_identity::TurnId::new(),
        );

        observe_event(
            Some(&observer),
            events.assistant_message_delta(&identity, "ready"),
        )
        .unwrap();
        observe_event(
            Some(&observer),
            events.session_completed(RunStatus::Success),
        )
        .unwrap();

        assert!(matches!(event_rx.try_recv(), Ok(TuiEvent::MessageDelta(text)) if text == "ready"));
        assert!(event_rx.try_recv().is_err());
        assert!(observer.finish_foreground().unwrap());
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SessionCompleted { status }) if status == "success"
        ));
    }

    #[test]
    fn hosted_observer_routes_late_background_events_after_foreground_handoff() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let observer = Arc::new(TuiHostedEventObserver::new(event_tx));
        assert!(!observer.finish_foreground().unwrap());
        let mut events = EventFactory::new("hosted-background-events".to_string());

        observe_event(
            Some(observer.as_ref()),
            events.session_completed(RunStatus::Cancelled),
        )
        .unwrap();

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SessionCompleted { status }) if status == "cancelled"
        ));
    }

    #[test]
    fn hosted_observer_acknowledges_queued_id_only_at_runtime_turn_start() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let observer = TuiHostedEventObserver::new_with_queued_id(event_tx, Some(42));
        let mut events = EventFactory::new("queued-turn-start".to_string());
        let turn_id = orca_core::thread_identity::TurnId::new();

        assert!(!observer.queued_submission_started());
        observe_event(
            Some(&observer),
            events.turn_started(&turn_id, 1, Some("expanded prompt")),
        )
        .unwrap();

        assert!(observer.queued_submission_started());
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::QueuedSubmissionStarted { id: 42 })
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::TurnStarted { turn: 1, .. })
        ));

        let next_turn_id = orca_core::thread_identity::TurnId::new();
        observe_event(
            Some(&observer),
            events.turn_started(&next_turn_id, 2, Some("automatic continuation")),
        )
        .unwrap();

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::TurnStarted { turn: 2, .. })
        ));
        assert!(event_rx.try_recv().is_err());
    }
}
