use std::io;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use orca_core::cancel::{CancelToken, OperationId};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::task_types::TaskStatus;
use orca_runtime::controller::ThreadTurnOutcome;
use orca_runtime::runtime_host::{
    GenerationContext, HostedOperationKind, HostedTurnRequest, ThreadOperationExecutor,
};
use orca_runtime::tasks::MainSessionTerminalUpdate;
use orca_runtime::thread::RuntimeThread;

use crate::agent_runner::{
    BackgroundProviderCompletionContext, PendingWorkflowNotifications, TuiAgentTurnContinuation,
    TuiAgentTurnResult, TuiBackgroundTurnCompletion, TuiBackgroundTurnCompletionHandler,
    TuiBackgroundTurnContinuationRequest, TuiRuntimeEventObserver,
    continue_approved_background_turn_for_tui_with_events, send_runtime_event_as_tui,
    send_task_status_updated_for_tui, spawn_background_provider_suspension_completion,
    take_pending_workflow_notification, task_summary_for_tui,
};
use crate::bridge::{TuiBudgetAdmissionError, TuiSession};
use crate::operation_controller::TuiOperationController;
use crate::runtime_interaction_adapter::{
    TuiApprovalHandler, TuiMcpElicitationHandler, TuiPermissionRequestHandler, TuiUserInputHandler,
};
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
        let mut session = TuiSession::new(thread);

        let result = match request.operation_kind() {
            HostedOperationKind::Turn => {
                let turn = self.run_canonical_turn(
                    &mut session,
                    request,
                    generation,
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

    fn run_canonical_turn(
        &self,
        session: &mut TuiSession<'_>,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        event_tx: &Sender<TuiEvent>,
        control: &crate::operation_controller::TuiTurnControl,
        cancel: &CancelToken,
        events: &mut EventFactory,
    ) -> TuiAgentTurnResult {
        let config = generation.config();
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
        let usage_ledger = session.usage_ledger();
        let admission = match usage_ledger.admit_budgeted_request(config.max_budget_usd, cancel) {
            Ok(admission) => admission,
            Err(TuiBudgetAdmissionError::BudgetExhausted(totals)) => {
                let error = format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd,
                    config.max_budget_usd.unwrap_or_default()
                );
                finish_canonical_turn_locally(
                    session,
                    event_tx,
                    events,
                    request.task_id(),
                    RunStatus::BudgetExhausted,
                    Some(&error),
                );
                return TuiAgentTurnResult::new(RunStatus::BudgetExhausted.as_str());
            }
            Err(TuiBudgetAdmissionError::Cancelled) => {
                finish_canonical_turn_locally(
                    session,
                    event_tx,
                    events,
                    request.task_id(),
                    RunStatus::Cancelled,
                    None,
                );
                return TuiAgentTurnResult::new(RunStatus::Cancelled.as_str());
            }
        };
        let pending_interactions = session.pending_interactions();
        let canonical_request = request
            .thread_turn_request(generation)
            .with_event_observer(Arc::new(TuiRuntimeEventObserver::new(event_tx.clone())))
            .with_provider_suspension_control(Arc::new(control.clone()))
            .with_approval_handler(Arc::new(
                TuiApprovalHandler::new(event_tx.clone(), control.clone())
                    .with_pending_interactions(pending_interactions.clone()),
            ))
            .with_permission_handler(Arc::new(
                TuiPermissionRequestHandler::new(event_tx.clone(), control.clone())
                    .with_pending_interactions(pending_interactions.clone()),
            ))
            .with_threaded_user_input_handler(Arc::new(
                TuiUserInputHandler::new(event_tx.clone(), control.clone())
                    .with_pending_interactions(pending_interactions.clone()),
            ))
            .with_mcp_elicitation_handler(Arc::new(
                TuiMcpElicitationHandler::new(event_tx.clone(), control.clone())
                    .with_pending_interactions(pending_interactions),
            ));
        let mut canonical_config = config.clone();
        canonical_config.max_budget_usd =
            effective_runtime_budget(config.max_budget_usd, session.usage_totals(), before_usage);
        let outcome = session
            .runtime_mut()
            .run_request_with_event_factory_and_cancel_outcome(
                &canonical_config,
                &canonical_request,
                io::sink(),
                events,
                cancel.clone(),
            );
        let after_usage = session.runtime_usage_totals();
        let shared_totals = usage_ledger.add(usage_delta(before_usage, after_usage));
        if shared_totals != after_usage {
            send_runtime_event_as_tui(event_tx, events.usage_updated(shared_totals));
        }
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                drop(admission);
                let status = if error.kind() == io::ErrorKind::Interrupted || cancel.is_cancelled()
                {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                };
                let error_message = error.to_string();
                finish_canonical_turn_locally(
                    session,
                    event_tx,
                    events,
                    request.task_id(),
                    status,
                    (status != RunStatus::Cancelled).then_some(error_message.as_str()),
                );
                return TuiAgentTurnResult::new(status.as_str());
            }
        };

        match outcome {
            ThreadTurnOutcome::Completed(status) => {
                drop(admission);
                if let Some(session_id) = goal_session_id {
                    account_goal_usage_for_tui(
                        &session_id,
                        goal_token_delta(before_usage, after_usage),
                        elapsed_seconds(started_at),
                        &self.event_tx,
                    );
                }
                if status == RunStatus::Success
                    && let Some(notification) = take_pending_workflow_notification(Some(
                        &self.pending_workflow_notifications,
                    ))
                {
                    return TuiAgentTurnResult::with_continuation(
                        status.as_str(),
                        TuiAgentTurnContinuation::WorkflowNotification(notification),
                    );
                }
                TuiAgentTurnResult::new(status.as_str())
            }
            ThreadTurnOutcome::ProviderSuspended(suspension) => {
                let Some(task_id) = request.task_id().map(str::to_string) else {
                    drop(admission);
                    finish_canonical_turn_locally(
                        session,
                        event_tx,
                        events,
                        None,
                        RunStatus::Failed,
                        Some("canonical provider suspension requires a main-session task"),
                    );
                    return TuiAgentTurnResult::new(RunStatus::Failed.as_str());
                };
                if let Err(error) = session.task_registry().mark_backgrounded(&task_id) {
                    drop(admission);
                    finish_canonical_turn_locally(
                        session,
                        event_tx,
                        events,
                        Some(&task_id),
                        RunStatus::Failed,
                        Some(&error),
                    );
                    return TuiAgentTurnResult::new(RunStatus::Failed.as_str());
                }
                if let Some(task) = task_summary_for_tui(session.task_registry(), &task_id) {
                    send_task_status_updated_for_tui(event_tx, events, &task);
                }
                let model = suspension.model().map(str::to_string);
                let history_writer = session.writer_mut().cloned();
                let spawn = spawn_background_provider_suspension_completion(
                    suspension,
                    BackgroundProviderCompletionContext {
                        task_registry: session.task_registry().clone(),
                        history_writer,
                        model,
                        usage_ledger,
                        budget_admission: Some(admission),
                        max_budget_usd: config.max_budget_usd,
                        event_tx: self.event_tx.clone(),
                        run_id: events.run_id().to_string(),
                        task_id: task_id.clone(),
                        completion_handler: background_completion_handler,
                    },
                    &self.task_spawner,
                );
                if let Err(error) = spawn {
                    let stopped = session
                        .task_registry()
                        .get(&task_id)
                        .is_some_and(|task| task.status == TaskStatus::Stopped);
                    let status = if stopped {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Failed
                    };
                    let error_message = error.to_string();
                    finish_canonical_turn_locally(
                        session,
                        event_tx,
                        events,
                        Some(&task_id),
                        status,
                        (status != RunStatus::Cancelled).then_some(error_message.as_str()),
                    );
                    return TuiAgentTurnResult::new(status.as_str());
                }
                if let Some(accounting) = background_goal_accounting {
                    accounting.record_foreground(goal_token_delta(before_usage, after_usage));
                }
                TuiAgentTurnResult::new("backgrounded")
            }
        }
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

fn finish_canonical_turn_locally(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_id: Option<&str>,
    status: RunStatus,
    error: Option<&str>,
) {
    if let Some(error) = error {
        send_runtime_event_as_tui(event_tx, events.error(error));
    }
    if let Some(task_id) = task_id {
        let task_is_terminal = session.task_registry().get(task_id).is_some_and(|task| {
            matches!(
                task.status,
                TaskStatus::Stopped
                    | TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::ApprovalRequired
                    | TaskStatus::Cancelled
            )
        });
        if !task_is_terminal {
            let usage = Some(session.runtime_usage_totals());
            match status {
                RunStatus::Success => {
                    let _ = session.task_registry().apply_main_session_terminal_update(
                        task_id,
                        MainSessionTerminalUpdate::Completed {
                            result: status.as_str().to_string(),
                        },
                        usage,
                    );
                }
                RunStatus::Cancelled => {
                    let _ = session.task_registry().stop_with_usage(
                        task_id,
                        status.as_str().to_string(),
                        usage,
                    );
                }
                RunStatus::Failed
                | RunStatus::ApprovalRequired
                | RunStatus::BudgetExhausted
                | RunStatus::VerificationFailed => {
                    let _ = session.task_registry().apply_main_session_terminal_update(
                        task_id,
                        MainSessionTerminalUpdate::Failed {
                            error: error.unwrap_or(status.as_str()).to_string(),
                        },
                        usage,
                    );
                }
            }
            if let Some(task) = task_summary_for_tui(session.task_registry(), task_id) {
                send_task_status_updated_for_tui(event_tx, events, &task);
            }
        }
    }
    session.finish_agent_lifecycle_task(status);
    if let Some(error) = error {
        session.complete_with_error(status.as_str(), error);
    } else {
        session.complete(status.as_str());
    }
    send_runtime_event_as_tui(event_tx, events.session_completed(status));
}

fn effective_runtime_budget(
    max_budget_usd: Option<f64>,
    shared_usage: UsageTotals,
    runtime_usage: UsageTotals,
) -> Option<f64> {
    max_budget_usd.map(|max_budget| {
        let external_usage =
            (shared_usage.estimated_cost_usd - runtime_usage.estimated_cost_usd).max(0.0);
        (max_budget - external_usage).max(0.0)
    })
}

fn usage_delta(before: UsageTotals, after: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        cache_tokens: after.cache_tokens.saturating_sub(before.cache_tokens),
        estimated_cost_usd: (after.estimated_cost_usd - before.estimated_cost_usd).max(0.0),
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
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn with_orca_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().expect("ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home.path())));
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

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

    #[test]
    fn hosted_goal_token_delta_counts_cache_as_input_subset() {
        let incident_usage = UsageTotals {
            input_tokens: 49_909_209,
            output_tokens: 191_567,
            cache_tokens: 47_879_040,
            estimated_cost_usd: 3.156_464_565,
        };

        assert_eq!(
            goal_token_delta(UsageTotals::default(), incident_usage),
            50_100_776
        );
    }

    #[test]
    fn hosted_background_goal_accounting_commits_exactly_once() {
        with_orca_home(|_| {
            let session_id = "hosted-background-goal-usage";
            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    session_id,
                    "account hosted provider usage",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();
            let (event_tx, event_rx) = crossbeam_channel::unbounded();
            let accounting =
                BackgroundGoalAccounting::new(session_id.to_string(), Instant::now(), event_tx);

            accounting.record_foreground(200);
            accounting.record_background(50_100_776);
            accounting.record_background(999);

            let goal = orca_runtime::goals::GoalStore::load_default()
                .get(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(goal.tokens_used, 50_100_976);
            assert_eq!(
                event_rx
                    .try_iter()
                    .filter(|event| matches!(event, TuiEvent::GoalStatus(Some(_))))
                    .count(),
                1
            );
        });
    }
}
