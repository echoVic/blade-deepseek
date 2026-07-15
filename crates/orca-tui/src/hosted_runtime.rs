use std::io;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use orca_core::cancel::{CancelToken, OperationId};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_runtime::runtime_host::{
    GenerationContext, HostedOperationKind, HostedTurnRequest, ThreadOperationExecutor,
};
use orca_runtime::thread::RuntimeThread;

use crate::agent_runner::{
    PendingWorkflowNotifications, TuiAgentTurnResult, TuiBackgroundTurnCompletion,
    TuiBackgroundTurnCompletionHandler, TuiBackgroundTurnContinuationRequest,
    continue_approved_background_turn_for_tui_with_events, run_agent_for_tui_with_event_factory,
};
use crate::bridge::{TuiHostedConversationSession, TuiSession};
use crate::operation_controller::TuiOperationController;
use crate::task_supervisor::TuiTaskSpawner;
use crate::types::TuiEvent;

pub(crate) struct TuiHostedOperationResult {
    pub(crate) operation_id: OperationId,
    pub(crate) outcome: Result<TuiHostedOperationOutcome, String>,
    pub(crate) terminal_event: Option<TuiEvent>,
}

pub(crate) enum TuiHostedOperationOutcome {
    Turn(TuiAgentTurnResult),
    ManualCompaction,
}

pub(crate) struct TuiThreadOperationExecutor {
    controller: TuiOperationController,
    event_tx: Sender<TuiEvent>,
    pending_workflow_notifications: PendingWorkflowNotifications,
    task_spawner: TuiTaskSpawner,
    result_tx: Sender<TuiHostedOperationResult>,
}

impl TuiThreadOperationExecutor {
    pub(crate) fn new(
        controller: TuiOperationController,
        event_tx: Sender<TuiEvent>,
        pending_workflow_notifications: PendingWorkflowNotifications,
        task_spawner: TuiTaskSpawner,
        result_tx: Sender<TuiHostedOperationResult>,
    ) -> Self {
        Self {
            controller,
            event_tx,
            pending_workflow_notifications,
            task_spawner,
            result_tx,
        }
    }

    fn execute(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        cancel: &CancelToken,
    ) -> io::Result<RunStatus> {
        let control = self
            .controller
            .wait_for_hosted(generation.fence().operation_id(), cancel)?;
        let mut relay = TuiOperationEventRelay::spawn(self.event_tx.clone())?;
        let operation_event_tx = relay.sender();
        let mut session = TuiHostedConversationSession::new(thread);

        let result = match request.operation_kind() {
            HostedOperationKind::Turn => {
                let turn = self.run_turn(
                    &mut session,
                    request,
                    generation.config(),
                    &operation_event_tx,
                    &control,
                    cancel,
                    events,
                );
                let status = run_status_for_tui_status(&turn.status);
                (TuiHostedOperationOutcome::Turn(turn), status)
            }
            HostedOperationKind::ManualCompaction => {
                let config = generation.config();
                let cwd = config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let _ = operation_event_tx.send(TuiEvent::CompactionStarted);
                let (before_messages, after_messages) = session.compact(config, &cwd, cancel);
                let _ = operation_event_tx.send(TuiEvent::Compacted {
                    before_messages,
                    after_messages,
                    reason: "manual".to_string(),
                    strategy: "manual".to_string(),
                    collapsed_messages: before_messages.saturating_sub(after_messages),
                    status_text: "compacted context manually".to_string(),
                });
                (
                    TuiHostedOperationOutcome::ManualCompaction,
                    RunStatus::Success,
                )
            }
            HostedOperationKind::BackgroundContinuation { task_id } => {
                let before_usage = session.runtime_usage_totals();
                let started_at = Instant::now();
                let continuation = TuiBackgroundTurnContinuationRequest::new(task_id.clone());
                let turn = continue_approved_background_turn_for_tui_with_events(
                    generation.config(),
                    &mut session,
                    &continuation,
                    &operation_event_tx,
                    cancel,
                    Some(&self.pending_workflow_notifications),
                    events,
                );
                if request.tracks_goal_usage()
                    && let Some(session_id) = session.session_id().map(str::to_string)
                {
                    account_goal_usage_for_tui(
                        &session_id,
                        goal_token_delta(before_usage, session.runtime_usage_totals()),
                        elapsed_seconds(started_at),
                        &self.event_tx,
                    );
                }
                let status = run_status_for_tui_status(&turn.status);
                (TuiHostedOperationOutcome::Turn(turn), status)
            }
        };

        drop(operation_event_tx);
        let terminal_event = relay.finish()?;
        let (outcome, status) = result;
        self.result_tx
            .send(TuiHostedOperationResult {
                operation_id: generation.fence().operation_id(),
                outcome: Ok(outcome),
                terminal_event,
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI hosted result receiver closed",
                )
            })?;
        Ok(status)
    }

    fn run_turn(
        &self,
        session: &mut dyn TuiSession,
        request: &HostedTurnRequest,
        config: &orca_core::config::RunConfig,
        event_tx: &Sender<TuiEvent>,
        control: &crate::operation_controller::TuiTurnControl,
        cancel: &CancelToken,
        events: &mut EventFactory,
    ) -> TuiAgentTurnResult {
        let before_usage = session.runtime_usage_totals();
        let started_at = Instant::now();
        let goal_session_id = request
            .tracks_goal_usage()
            .then(|| session.session_id().map(str::to_string))
            .flatten();
        let background_goal_accounting = goal_session_id.clone().map(|session_id| {
            BackgroundGoalAccounting::new(session_id, started_at, self.event_tx.clone())
        });
        let background_completion_handler = background_goal_accounting
            .as_ref()
            .map(BackgroundGoalAccounting::completion_handler);
        let turn = run_agent_for_tui_with_event_factory(
            config,
            session,
            request.prompt(),
            event_tx,
            &self.event_tx,
            control,
            cancel,
            request.allows_goal_tools(),
            request.task_description(),
            request.is_backtrack_target(),
            Some(&self.pending_workflow_notifications),
            background_completion_handler,
            &self.task_spawner,
            events,
        );
        if let Some(session_id) = goal_session_id {
            let token_delta = goal_token_delta(before_usage, session.runtime_usage_totals());
            if turn.status == "backgrounded" {
                if let Some(accounting) = background_goal_accounting {
                    accounting.record_foreground(token_delta);
                }
            } else {
                account_goal_usage_for_tui(
                    &session_id,
                    token_delta,
                    elapsed_seconds(started_at),
                    &self.event_tx,
                );
            }
        }
        turn
    }
}

impl ThreadOperationExecutor for TuiThreadOperationExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        _writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<RunStatus> {
        let operation_id = generation.fence().operation_id();
        match self.execute(thread, request, generation, events, cancel) {
            Ok(status) => Ok(status),
            Err(error) => {
                let _ = self.result_tx.send(TuiHostedOperationResult {
                    operation_id,
                    outcome: Err(error.to_string()),
                    terminal_event: None,
                });
                Err(error)
            }
        }
    }
}

struct TuiOperationEventRelay {
    sender: Option<Sender<TuiEvent>>,
    terminal_event: Arc<Mutex<Option<TuiEvent>>>,
    join: Option<JoinHandle<()>>,
}

impl TuiOperationEventRelay {
    fn spawn(target: Sender<TuiEvent>) -> io::Result<Self> {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let terminal_event = Arc::new(Mutex::new(None));
        let relay_terminal = Arc::clone(&terminal_event);
        let join = thread::Builder::new()
            .name("orca-tui-operation-events".to_string())
            .spawn(move || relay_operation_events(receiver, target, relay_terminal))?;
        Ok(Self {
            sender: Some(sender),
            terminal_event,
            join: Some(join),
        })
    }

    fn sender(&self) -> Sender<TuiEvent> {
        self.sender
            .as_ref()
            .expect("TUI operation relay sender available")
            .clone()
    }

    fn finish(&mut self) -> io::Result<Option<TuiEvent>> {
        self.sender.take();
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| io::Error::other("TUI operation event relay panicked"))?;
        }
        Ok(self
            .terminal_event
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take())
    }
}

impl Drop for TuiOperationEventRelay {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn relay_operation_events(
    receiver: Receiver<TuiEvent>,
    target: Sender<TuiEvent>,
    terminal_event: Arc<Mutex<Option<TuiEvent>>>,
) {
    while let Ok(event) = receiver.recv() {
        if is_operation_terminal_event(&event) {
            *terminal_event
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(event);
        } else if target.send(event).is_err() {
            break;
        }
    }
}

fn is_operation_terminal_event(event: &TuiEvent) -> bool {
    matches!(
        event,
        TuiEvent::SessionCompleted { .. } | TuiEvent::Compacted { .. }
    )
}

fn run_status_for_tui_status(status: &str) -> RunStatus {
    match status {
        "success" | "backgrounded" => RunStatus::Success,
        "interrupted" | "cancelled" => RunStatus::Cancelled,
        "approval_required" => RunStatus::ApprovalRequired,
        "verification_failed" => RunStatus::VerificationFailed,
        "budget_exhausted" => RunStatus::BudgetExhausted,
        _ => RunStatus::Failed,
    }
}

fn goal_token_delta(before: UsageTotals, after: UsageTotals) -> i64 {
    after
        .input_tokens
        .saturating_sub(before.input_tokens)
        .saturating_add(after.output_tokens.saturating_sub(before.output_tokens))
        .min(i64::MAX as u64) as i64
}

fn elapsed_seconds(started_at: Instant) -> i64 {
    started_at.elapsed().as_secs().min(i64::MAX as u64) as i64
}

fn account_goal_usage_for_tui(
    session_id: &str,
    token_delta: i64,
    elapsed_delta: i64,
    event_tx: &Sender<TuiEvent>,
) {
    if token_delta <= 0 && elapsed_delta <= 0 {
        return;
    }
    if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().account_usage(
        session_id,
        token_delta,
        elapsed_delta,
    ) {
        let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal)));
    }
}

#[derive(Clone)]
struct BackgroundGoalAccounting {
    session_id: String,
    started_at: Instant,
    event_tx: Sender<TuiEvent>,
    state: Arc<Mutex<BackgroundGoalAccountingState>>,
}

#[derive(Default)]
struct BackgroundGoalAccountingState {
    foreground_tokens: Option<i64>,
    background_tokens: Option<i64>,
    accounted: bool,
}

impl BackgroundGoalAccounting {
    fn new(session_id: String, started_at: Instant, event_tx: Sender<TuiEvent>) -> Self {
        Self {
            session_id,
            started_at,
            event_tx,
            state: Arc::new(Mutex::new(BackgroundGoalAccountingState::default())),
        }
    }

    fn completion_handler(&self) -> TuiBackgroundTurnCompletionHandler {
        let accounting = self.clone();
        Box::new(move |completion: TuiBackgroundTurnCompletion| {
            let background_tokens = completion
                .usage
                .map(|usage| {
                    usage
                        .input_tokens
                        .saturating_add(usage.output_tokens)
                        .min(i64::MAX as u64) as i64
                })
                .unwrap_or_default();
            accounting.record_background(background_tokens);
        })
    }

    fn record_foreground(&self, tokens: i64) {
        let total = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.foreground_tokens = Some(tokens);
            take_background_goal_total(&mut state)
        };
        self.account(total);
    }

    fn record_background(&self, tokens: i64) {
        let total = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.background_tokens = Some(tokens);
            take_background_goal_total(&mut state)
        };
        self.account(total);
    }

    fn account(&self, total: Option<i64>) {
        if let Some(total) = total {
            account_goal_usage_for_tui(
                &self.session_id,
                total,
                elapsed_seconds(self.started_at),
                &self.event_tx,
            );
        }
    }
}

fn take_background_goal_total(state: &mut BackgroundGoalAccountingState) -> Option<i64> {
    if state.accounted {
        return None;
    }
    let (Some(foreground), Some(background)) = (state.foreground_tokens, state.background_tokens)
    else {
        return None;
    };
    state.accounted = true;
    Some(foreground.saturating_add(background))
}

pub(crate) fn receive_hosted_result(
    receiver: &Receiver<TuiHostedOperationResult>,
    operation_id: OperationId,
) -> io::Result<TuiHostedOperationResult> {
    let result = receiver.try_recv().map_err(|error| match error {
        crossbeam_channel::TryRecvError::Empty => {
            io::Error::other("TUI hosted operation completed without publishing its fenced result")
        }
        crossbeam_channel::TryRecvError::Disconnected => io::Error::new(
            io::ErrorKind::BrokenPipe,
            "TUI hosted operation result channel closed",
        ),
    })?;
    if result.operation_id != operation_id {
        return Err(io::Error::other(format!(
            "TUI hosted operation result mismatch: expected {:?}, found {:?}",
            operation_id, result.operation_id
        )));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn operation_event_relay_holds_terminal_until_join() {
        let (target_tx, target_rx) = crossbeam_channel::unbounded();
        let mut relay = TuiOperationEventRelay::spawn(target_tx).expect("event relay");
        let sender = relay.sender();
        sender
            .send(TuiEvent::Notice("live".to_string()))
            .expect("live event");
        sender
            .send(TuiEvent::SessionCompleted {
                status: "success".to_string(),
            })
            .expect("terminal event");
        drop(sender);

        assert!(matches!(
            target_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::Notice(message)) if message == "live"
        ));
        assert!(target_rx.try_recv().is_err());

        let terminal = relay.finish().expect("relay joined");
        assert!(matches!(
            terminal,
            Some(TuiEvent::SessionCompleted { status }) if status == "success"
        ));
    }

    #[test]
    fn operation_event_relay_holds_manual_compaction_terminal() {
        let (target_tx, target_rx) = crossbeam_channel::unbounded();
        let mut relay = TuiOperationEventRelay::spawn(target_tx).expect("event relay");
        let sender = relay.sender();
        sender.send(TuiEvent::CompactionStarted).unwrap();
        sender
            .send(TuiEvent::Compacted {
                before_messages: 4,
                after_messages: 2,
                reason: "manual".to_string(),
                strategy: "manual".to_string(),
                collapsed_messages: 2,
                status_text: "compacted context manually".to_string(),
            })
            .unwrap();
        drop(sender);

        assert!(matches!(
            target_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::CompactionStarted)
        ));
        assert!(target_rx.try_recv().is_err());
        assert!(matches!(
            relay.finish().expect("relay joined"),
            Some(TuiEvent::Compacted {
                before_messages: 4,
                after_messages: 2,
                ..
            })
        ));
    }

    #[test]
    fn background_goal_accounting_does_not_block_when_completion_arrives_first() {
        let mut state = BackgroundGoalAccountingState {
            background_tokens: Some(7),
            ..BackgroundGoalAccountingState::default()
        };
        assert_eq!(take_background_goal_total(&mut state), None);

        state.foreground_tokens = Some(5);
        assert_eq!(take_background_goal_total(&mut state), Some(12));
        assert_eq!(take_background_goal_total(&mut state), None);
    }
}
