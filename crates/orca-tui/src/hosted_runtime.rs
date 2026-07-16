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
    state: Mutex<TuiHostedEventObserverState>,
}

#[derive(Default)]
struct TuiHostedEventObserverState {
    foreground_finished: bool,
    terminal_event: Option<TuiEvent>,
}

impl TuiHostedEventObserver {
    pub(crate) fn new(event_tx: Sender<TuiEvent>) -> Self {
        Self {
            event_tx,
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
        drop(state);
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
    if task.task_type != TaskType::MainSession
        || !task.is_backgrounded
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
    Some(match task.status {
        TaskStatus::ApprovalRequired => match task.tool.as_deref() {
            Some(tool) => {
                format!("Background session needs approval for {tool} before it can continue.")
            }
            None => "Background session needs approval before it can continue.".to_string(),
        },
        TaskStatus::Completed => "Background session completed: success".to_string(),
        TaskStatus::Failed => "Background session completed: failed".to_string(),
        TaskStatus::Stopped => "Background session completed: stopped".to_string(),
        TaskStatus::Cancelled => "Background session completed: cancelled".to_string(),
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

    use super::*;

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
}
