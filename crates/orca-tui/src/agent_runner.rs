use crossbeam_channel::{self as mpsc, Receiver, Sender};
use std::io;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventFactory};
use orca_core::event_sink::EventObserver;
use orca_core::hook_types::HookEvent;
use orca_core::model::ModelRouteContext;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::{BackgroundTaskSummary, PendingToolCallSummary, TaskStatus, TaskType};
use orca_core::tool_types;
use orca_core::workflow_types::WorkflowInput;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::{
    deepseek_goal_tools_schema_with_mcp_and_external, deepseek_tools_schema_with_mcp_and_external,
};
use orca_runtime::agent_common;
use orca_runtime::controller::ThreadTurnRequest;
use orca_runtime::hooks::{HookContext, conversation_with_hook_context};
use orca_runtime::memory;
use orca_runtime::runtime_state::{RuntimeToolFinish, RuntimeTurnReducer};
use orca_runtime::tasks::MainSessionTerminalUpdate;
#[cfg(test)]
use orca_runtime::thread::RuntimeThread;

#[cfg(test)]
use crate::action_dispatcher::TuiActionDispatcher;
use crate::agent_subagent_execution::{
    collect_subagent_batch, config_for_remaining_subagent_budget, execute_subagent_batch_for_tui,
    should_run_subagent_batch,
};
use crate::agent_tool_execution::{
    execute_readonly_batch_for_tui, execute_tool_for_tui_with_background_events,
};
use crate::agent_workflow_execution::execute_workflow_for_tui;
use crate::bridge::{TuiBudgetAdmission, TuiSession, TuiUsageLedger};
#[cfg(test)]
use crate::operation_controller::TuiOperationController;
use crate::operation_controller::TuiTurnControl;
use crate::runtime_event_projection::tui_event_from_runtime_event;
use crate::task_supervisor::TuiTaskSpawner;
#[cfg(test)]
use crate::task_supervisor::TuiTaskSupervisor;
#[cfg(test)]
use crate::types::UserAction;
use crate::types::{PendingWorkflowNotification, TuiEvent};

pub(crate) const DEFAULT_MAX_TURNS: u32 = 128;
const PROVIDER_STREAM_CAPACITY: usize = 256;

pub(crate) type PendingWorkflowNotifications = crate::types::PendingWorkflowNotificationQueue;

enum ProviderStreamEvent {
    Step(ProviderStep),
    Done(ProviderResponse),
}

struct ProviderStreamTask {
    receiver: Option<Receiver<ProviderStreamEvent>>,
    cancel: CancelToken,
    handle: Option<JoinHandle<()>>,
}

impl ProviderStreamTask {
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProviderStreamEvent, mpsc::RecvTimeoutError> {
        self.receiver
            .as_ref()
            .expect("provider stream receiver available")
            .recv_timeout(timeout)
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| io::Error::other("TUI provider stream worker panicked"))
    }

    fn cancel_and_join(&mut self) -> io::Result<()> {
        if self.handle.is_none() {
            return Ok(());
        }
        self.cancel();
        self.receiver.take();
        self.join()
    }
}

impl Drop for ProviderStreamTask {
    fn drop(&mut self) {
        let _ = self.cancel_and_join();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TuiAgentTurnContinuation {
    WorkflowNotification(PendingWorkflowNotification),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TuiAgentTurnResult {
    pub(crate) status: String,
    pub(crate) continuation: Option<TuiAgentTurnContinuation>,
}

impl TuiAgentTurnResult {
    fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            continuation: None,
        }
    }

    fn with_continuation(
        status: impl Into<String>,
        continuation: TuiAgentTurnContinuation,
    ) -> Self {
        Self {
            status: status.into(),
            continuation: Some(continuation),
        }
    }
}

fn send_error_for_tui(event_tx: &Sender<TuiEvent>, events: &mut EventFactory, message: &str) {
    send_runtime_event_as_tui(event_tx, events.error(message));
}

fn send_session_completed_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    status: orca_core::event_schema::RunStatus,
) {
    send_runtime_event_as_tui(event_tx, events.session_completed(status));
}

fn send_session_completed_status_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    status: &str,
) {
    let status = match status {
        "success" => orca_core::event_schema::RunStatus::Success,
        "failed" => orca_core::event_schema::RunStatus::Failed,
        "interrupted" | "cancelled" => orca_core::event_schema::RunStatus::Cancelled,
        "approval_required" => orca_core::event_schema::RunStatus::ApprovalRequired,
        "verification_failed" => orca_core::event_schema::RunStatus::VerificationFailed,
        "budget_exhausted" => orca_core::event_schema::RunStatus::BudgetExhausted,
        _ => orca_core::event_schema::RunStatus::Failed,
    };
    send_session_completed_for_tui(event_tx, events, status);
}

pub(crate) fn send_runtime_event_as_tui(event_tx: &Sender<TuiEvent>, event: EventEnvelope) {
    if let Some(event) = tui_event_from_runtime_event(&event) {
        let _ = event_tx.send(event);
    }
}

struct TuiRuntimeEventObserver {
    event_tx: Sender<TuiEvent>,
}

impl TuiRuntimeEventObserver {
    fn new(event_tx: Sender<TuiEvent>) -> Self {
        Self { event_tx }
    }
}

impl EventObserver for TuiRuntimeEventObserver {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        if let Some(event) = tui_event_from_runtime_event(event) {
            self.event_tx.send(event).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI runtime event mailbox disconnected",
                )
            })?;
        }
        Ok(())
    }
}

pub(crate) fn send_workflow_tasks_updated_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    tasks: &[BackgroundTaskSummary],
) {
    send_runtime_event_as_tui(event_tx, events.workflow_tasks_updated(tasks));
}

pub(crate) fn send_task_status_updated_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task: &BackgroundTaskSummary,
) {
    send_runtime_event_as_tui(event_tx, events.task_status_updated(task));
}

pub(crate) fn task_summary_for_tui(
    registry: &orca_runtime::tasks::TaskRegistry,
    task_id: &str,
) -> Option<BackgroundTaskSummary> {
    registry.list().into_iter().find(|task| task.id == task_id)
}

pub(crate) enum TuiMainSessionTaskStart<'a> {
    Create(&'a str),
    Adopt(&'a str),
}

fn start_main_session_task_for_tui(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_start: TuiMainSessionTaskStart<'_>,
) -> String {
    match task_start {
        TuiMainSessionTaskStart::Adopt(task_id) => {
            if let Some(task) = task_summary_for_tui(session.task_registry(), task_id) {
                send_task_status_updated_for_tui(event_tx, events, &task);
            }
            task_id.to_string()
        }
        TuiMainSessionTaskStart::Create(description) => {
            let task = session
                .task_registry()
                .create_main_session(description.to_string());
            let _ = session.task_registry().mark_running(&task.id);
            session.start_agent_lifecycle_task_with_id(&task.id);
            if let Some(task) = task_summary_for_tui(session.task_registry(), &task.id) {
                send_task_status_updated_for_tui(event_tx, events, &task);
            }
            task.id
        }
    }
}

fn poll_background_current_turn_for_tui(
    session: &TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    control: &TuiTurnControl,
    task_id: &str,
    is_backgrounded: &mut bool,
) {
    if *is_backgrounded {
        return;
    }

    if control.take_background_current()
        && session.task_registry().mark_backgrounded(task_id).is_ok()
    {
        *is_backgrounded = true;
        if let Some(backgrounded_task) = task_summary_for_tui(session.task_registry(), task_id) {
            send_task_status_updated_for_tui(event_tx, events, &backgrounded_task);
        }
    }
}

fn spawn_provider_stream(
    provider: orca_core::config::ProviderKind,
    conversation: orca_core::conversation::Conversation,
    provider_config: ProviderConfig,
    cancel: CancelToken,
) -> io::Result<ProviderStreamTask> {
    let (tx, rx) = mpsc::bounded(PROVIDER_STREAM_CAPACITY);
    let worker_cancel = cancel.clone();
    let handle = thread::Builder::new()
        .name("orca-tui-provider-stream".to_string())
        .spawn(move || {
            let step_tx = tx.clone();
            let response = orca_provider::call_streaming(
                provider,
                &conversation,
                &provider_config,
                &worker_cancel,
                &mut |step| {
                    let _ = step_tx.send(ProviderStreamEvent::Step(step.clone()));
                },
            );
            let _ = tx.send(ProviderStreamEvent::Done(response));
        })?;
    Ok(ProviderStreamTask {
        receiver: Some(rx),
        cancel,
        handle: Some(handle),
    })
}

fn provider_response_status(response: &ProviderResponse) -> &'static str {
    if !response.tool_calls.is_empty()
        || response
            .steps
            .iter()
            .any(|step| matches!(step, ProviderStep::ToolCall(_)))
    {
        return "approval_required";
    }
    if response
        .steps
        .iter()
        .any(|step| matches!(step, ProviderStep::Error(_)))
    {
        "failed"
    } else {
        "success"
    }
}

fn provider_response_error(response: &ProviderResponse) -> Option<String> {
    response.steps.iter().find_map(|step| match step {
        ProviderStep::Error(error) => Some(error.clone()),
        _ => None,
    })
}

fn provider_response_usage_totals(
    response: &ProviderResponse,
    model: Option<&str>,
) -> Option<orca_core::cost_types::UsageTotals> {
    let usage = response.usage.filter(|usage| !usage.is_empty())?;
    let mut tracker = orca_runtime::cost::CostTracker::new(model);
    Some(tracker.add_usage(usage))
}

fn usage_budget_error(
    totals: orca_core::cost_types::UsageTotals,
    max_budget_usd: Option<f64>,
) -> Option<String> {
    let max_budget = max_budget_usd?;
    (totals.estimated_cost_usd > max_budget).then(|| {
        format!(
            "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
            totals.estimated_cost_usd, max_budget
        )
    })
}

fn persist_merged_usage(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    usage: orca_core::cost_types::UsageTotals,
) -> orca_core::cost_types::UsageTotals {
    let totals = session.record_external_usage(usage);
    send_runtime_event_as_tui(event_tx, events.usage_updated(totals));
    let foreground_totals = session.runtime_session().usage_totals();
    if let Some(writer) = session.writer_mut() {
        let _ = writer.append_usage(foreground_totals);
    }
    totals
}

fn finish_budget_exhausted_after_usage(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_id: &str,
    totals: orca_core::cost_types::UsageTotals,
    max_budget_usd: Option<f64>,
) -> Option<TuiAgentTurnResult> {
    let error = usage_budget_error(totals, max_budget_usd)?;
    send_error_for_tui(event_tx, events, &error);
    send_session_completed_for_tui(
        event_tx,
        events,
        orca_core::event_schema::RunStatus::BudgetExhausted,
    );
    finish_main_session_task_with_error_for_tui(
        session,
        event_tx,
        events,
        task_id,
        "budget_exhausted",
        Some(&error),
    );
    session.complete_with_error("budget_exhausted", &error);
    Some(TuiAgentTurnResult::new("budget_exhausted"))
}

fn usage_totals_delta(before: UsageTotals, after: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        cache_tokens: after.cache_tokens.saturating_sub(before.cache_tokens),
        estimated_cost_usd: (after.estimated_cost_usd - before.estimated_cost_usd).max(0.0),
    }
}

fn add_usage_totals(base: UsageTotals, delta: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: base.input_tokens.saturating_add(delta.input_tokens),
        output_tokens: base.output_tokens.saturating_add(delta.output_tokens),
        cache_tokens: base.cache_tokens.saturating_add(delta.cache_tokens),
        estimated_cost_usd: base.estimated_cost_usd + delta.estimated_cost_usd,
    }
}

fn provider_response_pending_tool_call(
    response: &ProviderResponse,
) -> Option<PendingToolCallSummary> {
    response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(PendingToolCallSummary {
                id: request.id.clone(),
                name: request.name.as_str().to_string(),
                action: request.action,
                target: request.target.clone(),
                arguments: request
                    .raw_arguments
                    .clone()
                    .unwrap_or_else(|| "{}".to_string()),
            }),
            _ => None,
        })
        .or_else(|| {
            response
                .tool_calls
                .first()
                .map(|tool_call| PendingToolCallSummary {
                    id: tool_call.id.clone(),
                    name: tool_call.function_name.clone(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: tool_call.arguments.clone(),
                })
        })
}

struct BackgroundProviderCompletionContext {
    task_registry: orca_runtime::tasks::TaskRegistry,
    history_writer: Option<orca_runtime::history::SessionWriter>,
    model: Option<String>,
    usage_ledger: TuiUsageLedger,
    budget_admission: Option<TuiBudgetAdmission>,
    max_budget_usd: Option<f64>,
    event_tx: Sender<TuiEvent>,
    run_id: String,
    task_id: String,
    completion_handler: Option<TuiBackgroundTurnCompletionHandler>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TuiBackgroundTurnCompletion {
    pub(crate) usage: Option<UsageTotals>,
}

pub(crate) type TuiBackgroundTurnCompletionHandler =
    Box<dyn FnOnce(TuiBackgroundTurnCompletion) + Send>;

fn spawn_background_provider_completion(
    mut provider_task: ProviderStreamTask,
    context: BackgroundProviderCompletionContext,
    task_spawner: &TuiTaskSpawner,
) -> io::Result<()> {
    let BackgroundProviderCompletionContext {
        task_registry,
        history_writer,
        model,
        usage_ledger,
        budget_admission,
        max_budget_usd,
        event_tx,
        run_id,
        task_id,
        completion_handler,
    } = context;
    let fallback_registry = task_registry.clone();
    let mut fallback_history_writer = history_writer.clone();
    let fallback_event_tx = event_tx.clone();
    let fallback_run_id = run_id.clone();
    let fallback_task_id = task_id.clone();
    let completion_handler = Arc::new(Mutex::new(completion_handler));
    let worker_completion_handler = Arc::clone(&completion_handler);
    let task_name = format!("provider-completion-{task_id}");
    let spawn_result = task_spawner.spawn(task_name, move |supervisor_cancel| {
        let mut history_writer = history_writer;
        let _budget_admission = budget_admission;
        let mut status = "failed";
        let mut pending_tool_call = None;
        let mut pending_provider_response = None;
        let mut failure_error = None;
        let mut usage = None;
        let mut usage_totals = None;
        let mut buffered_steps = Vec::new();
        let mut events = EventFactory::new(run_id);
        let mut cancelled = false;
        loop {
            if !cancelled
                && (supervisor_cancel.is_cancelled() || task_registry.is_cancelled(&task_id))
            {
                cancelled = true;
                status = "cancelled";
                provider_task.cancel();
            }
            let event = match provider_task.recv_timeout(Duration::from_millis(10)) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let join_error = provider_task.join().err().map(|error| error.to_string());
                    if !cancelled {
                        failure_error = join_error.or_else(|| {
                            Some("provider stream ended without a response".to_string())
                        });
                    }
                    break;
                }
            };
            match event {
                ProviderStreamEvent::Step(step) => {
                    if cancelled {
                        continue;
                    }
                    if background_task_is_foregrounded(&task_registry, &task_id) {
                        forward_foregrounded_background_steps(
                            &event_tx,
                            &mut events,
                            &mut buffered_steps,
                        );
                        forward_foregrounded_background_step(&event_tx, &mut events, &step);
                    } else if background_step_is_tui_visible(&step) {
                        buffered_steps.push(step);
                    }
                }
                ProviderStreamEvent::Done(response) => {
                    match provider_task.join() {
                        Ok(()) => {
                            usage = provider_response_usage_totals(&response, model.as_deref());
                            if let Some(usage) = usage {
                                let totals = usage_ledger.add(usage);
                                usage_totals = Some(totals);
                                if !cancelled
                                    && let Some(error) = usage_budget_error(totals, max_budget_usd)
                                {
                                    status = "budget_exhausted";
                                    failure_error = Some(error);
                                }
                            }
                            if !cancelled {
                                if status != "budget_exhausted" {
                                    status = provider_response_status(&response);
                                    failure_error = provider_response_error(&response);
                                }
                                pending_tool_call = provider_response_pending_tool_call(&response);
                                if status == "approval_required" {
                                    pending_provider_response = Some(response);
                                }
                            }
                        }
                        Err(error) => {
                            if !cancelled {
                                failure_error = Some(error.to_string());
                            }
                        }
                    }
                    break;
                }
            }
        }
        let pending_tool_name = pending_tool_call
            .as_ref()
            .map(|pending_tool_call| pending_tool_call.name.clone());
        let failure_message = failure_error.clone().unwrap_or_else(|| status.to_string());

        if let Some(totals) = usage_totals {
            send_runtime_event_as_tui(&event_tx, events.usage_updated(totals));
        }

        let was_backgrounded = task_registry
            .get(&task_id)
            .is_some_and(|task| task.is_backgrounded);
        let result = if status == "cancelled" {
            task_registry
                .stop_with_usage(&task_id, "cancelled".to_string(), usage)
                .map(|()| (was_backgrounded, TaskStatus::Stopped))
        } else {
            let terminal_update = match status {
                "success" => MainSessionTerminalUpdate::Completed {
                    result: status.to_string(),
                },
                "approval_required" => MainSessionTerminalUpdate::ApprovalRequired {
                    summary: status.to_string(),
                    pending_tool_call,
                    pending_provider_response,
                },
                _ => MainSessionTerminalUpdate::Failed {
                    error: failure_message.clone(),
                },
            };
            task_registry
                .apply_main_session_terminal_update(&task_id, terminal_update, usage)
                .map(|transition| {
                    let status = task_registry
                        .get(&task_id)
                        .map(|task| task.status)
                        .unwrap_or(TaskStatus::Failed);
                    (transition.is_backgrounded, status)
                })
        };
        if matches!(result, Ok((_, TaskStatus::Stopped))) {
            status = "cancelled";
            failure_error = None;
        }
        if let Some(writer) = &mut history_writer {
            let error = failure_error.as_deref();
            if let Err(error) =
                writer.append_background_task_provider_response(&task_id, status, error, usage)
            {
                eprintln!("orca: warning: background provider history write failed: {error}");
            }
        }
        invoke_background_completion_handler(&worker_completion_handler, usage);
        if let Ok((is_backgrounded, _)) = result {
            if let Some(updated_task) = task_summary_for_tui(&task_registry, &task_id) {
                send_task_status_updated_for_tui(&event_tx, &mut events, &updated_task);
            }
            if is_backgrounded {
                let _ = event_tx.send(TuiEvent::Notice(background_completion_notice(
                    status,
                    pending_tool_name.as_deref(),
                )));
            } else {
                forward_foregrounded_background_steps(&event_tx, &mut events, &mut buffered_steps);
                if let Some(error) = failure_error.as_deref() {
                    send_error_for_tui(&event_tx, &mut events, error);
                }
                send_session_completed_status_for_tui(&event_tx, &mut events, status);
            }
        }
    });

    if let Err(error) = spawn_result {
        let error_message = error.to_string();
        let mut events = EventFactory::new(fallback_run_id);
        let _ = fallback_registry.apply_main_session_terminal_update(
            &fallback_task_id,
            MainSessionTerminalUpdate::Failed {
                error: error_message.clone(),
            },
            None,
        );
        let stopped = fallback_registry
            .get(&fallback_task_id)
            .is_some_and(|task| task.status == TaskStatus::Stopped);
        let (status, terminal_error) = if stopped {
            ("cancelled", None)
        } else {
            ("failed", Some(error_message.as_str()))
        };
        if let Some(writer) = &mut fallback_history_writer {
            let _ = writer.append_background_task_provider_response(
                &fallback_task_id,
                status,
                terminal_error,
                None,
            );
        }
        invoke_background_completion_handler(&completion_handler, None);
        if let Some(updated_task) = task_summary_for_tui(&fallback_registry, &fallback_task_id) {
            send_task_status_updated_for_tui(&fallback_event_tx, &mut events, &updated_task);
        }
        if let Some(error) = terminal_error {
            send_error_for_tui(&fallback_event_tx, &mut events, error);
        }
        send_session_completed_status_for_tui(&fallback_event_tx, &mut events, status);
        return Err(error);
    }
    Ok(())
}

fn invoke_background_completion_handler(
    completion_handler: &Arc<Mutex<Option<TuiBackgroundTurnCompletionHandler>>>,
    usage: Option<UsageTotals>,
) {
    let handler = completion_handler
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(handler) = handler {
        handler(TuiBackgroundTurnCompletion { usage });
    }
}

fn background_task_is_foregrounded(
    task_registry: &orca_runtime::tasks::TaskRegistry,
    task_id: &str,
) -> bool {
    task_registry.get(task_id).is_some_and(|task| {
        task.task_type == TaskType::MainSession
            && task.status == TaskStatus::Running
            && !task.is_backgrounded
    })
}

fn background_step_is_tui_visible(step: &ProviderStep) -> bool {
    matches!(
        step,
        ProviderStep::ReasoningDelta(_)
            | ProviderStep::MessageDelta(_)
            | ProviderStep::ToolCallProgress(_)
    )
}

fn forward_foregrounded_background_steps(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    steps: &mut Vec<ProviderStep>,
) {
    for step in steps.drain(..) {
        forward_foregrounded_background_step(event_tx, events, &step);
    }
}

fn forward_foregrounded_background_step(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    step: &ProviderStep,
) {
    match step {
        ProviderStep::ReasoningDelta(text) => {
            send_runtime_event_as_tui(event_tx, events.assistant_reasoning_delta(text));
        }
        ProviderStep::MessageDelta(text) => {
            send_runtime_event_as_tui(event_tx, events.assistant_message_delta(text));
        }
        ProviderStep::ToolCallProgress(progress) => {
            send_runtime_event_as_tui(event_tx, events.tool_call_progress(progress));
        }
        _ => {}
    }
}

fn background_completion_notice(status: &str, pending_tool: Option<&str>) -> String {
    match status {
        "approval_required" => match pending_tool {
            Some(tool) => {
                format!("Background session needs approval for {tool} before it can continue.")
            }
            None => "Background session needs approval before it can continue.".to_string(),
        },
        _ => format!("Background session completed: {status}"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TuiBackgroundTurnContinuationRequest {
    task_id: String,
}

impl TuiBackgroundTurnContinuationRequest {
    pub(crate) fn new(task_id: String) -> Self {
        Self { task_id }
    }

    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }
}

#[allow(dead_code)]
pub(crate) fn continue_approved_background_turn_for_tui(
    config: &RunConfig,
    session: &mut TuiSession<'_>,
    continuation: &TuiBackgroundTurnContinuationRequest,
    event_tx: &Sender<TuiEvent>,
    cancel: &CancelToken,
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
) -> TuiAgentTurnResult {
    let mut runtime_events = EventFactory::new(
        session
            .session_id()
            .unwrap_or("tui-agent-session")
            .to_string(),
    );
    continue_approved_background_turn_for_tui_with_events(
        config,
        session,
        continuation,
        event_tx,
        cancel,
        pending_workflow_notifications,
        &mut runtime_events,
    )
}

pub(crate) fn continue_approved_background_turn_for_tui_with_events(
    config: &RunConfig,
    session: &mut TuiSession<'_>,
    continuation: &TuiBackgroundTurnContinuationRequest,
    event_tx: &Sender<TuiEvent>,
    cancel: &CancelToken,
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
    runtime_events: &mut EventFactory,
) -> TuiAgentTurnResult {
    let mut runtime_events = runtime_events;
    let task_id = continuation.task_id();
    let task_usage_before = session
        .task_registry()
        .get(task_id)
        .and_then(|task| task.usage)
        .unwrap_or_default();
    let runtime_usage_before = session.runtime_usage_totals();
    let runtime_continuation =
        match orca_runtime::background_turn::take_approved_background_turn_continuation(
            session.task_registry(),
            task_id,
        ) {
            Ok(Some(continuation)) => continuation.into_runtime_turn_continuation(),
            Ok(None) => {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "background task {task_id} has no approved provider response to continue"
                )));
                return TuiAgentTurnResult::new("failed");
            }
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(error));
                return TuiAgentTurnResult::new("failed");
            }
        };

    if let Some(continued_task) = task_summary_for_tui(session.task_registry(), task_id) {
        send_task_status_updated_for_tui(event_tx, &mut runtime_events, &continued_task);
    }

    let mut continuation_config = config.clone();
    continuation_config.output_format = OutputFormat::Jsonl;
    let request = ThreadTurnRequest::new("")
        .with_continuation(runtime_continuation)
        .with_session_completed_event(false)
        .with_event_observer(Arc::new(TuiRuntimeEventObserver::new(event_tx.clone())));
    let mut continuation_output = io::sink();
    let status = match session.run_request_with_cancel_for_tui(
        &continuation_config,
        &request,
        &mut continuation_output,
        cancel.clone(),
    ) {
        Ok(status) => status,
        Err(error) => {
            let error = error.to_string();
            let task_usage = add_usage_totals(
                task_usage_before,
                usage_totals_delta(runtime_usage_before, session.runtime_usage_totals()),
            );
            send_error_for_tui(event_tx, &mut runtime_events, &error);
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Failed,
            );
            finish_main_session_task_with_error_and_usage_for_tui(
                session,
                event_tx,
                &mut runtime_events,
                task_id,
                "failed",
                Some(&error),
                Some(task_usage),
            );
            session.complete_with_error("failed", &error);
            return TuiAgentTurnResult::new("failed");
        }
    };
    let status = status.as_str();
    let task_usage = add_usage_totals(
        task_usage_before,
        usage_totals_delta(runtime_usage_before, session.runtime_usage_totals()),
    );
    send_runtime_event_as_tui(
        event_tx,
        runtime_events.usage_updated(session.usage_totals()),
    );
    let completion_error = session.completion_error().map(str::to_string);

    if status == "success"
        && let Some(notification) =
            take_pending_workflow_notification(pending_workflow_notifications)
    {
        send_session_completed_for_tui(
            event_tx,
            &mut runtime_events,
            orca_core::event_schema::RunStatus::Success,
        );
        finish_main_session_task_with_error_and_usage_for_tui(
            session,
            event_tx,
            &mut runtime_events,
            task_id,
            "success",
            None,
            Some(task_usage),
        );
        return TuiAgentTurnResult::with_continuation(
            "success",
            TuiAgentTurnContinuation::WorkflowNotification(notification),
        );
    }

    send_session_completed_status_for_tui(event_tx, &mut runtime_events, status);
    if let Some(error) = completion_error.as_deref() {
        finish_main_session_task_with_error_and_usage_for_tui(
            session,
            event_tx,
            &mut runtime_events,
            task_id,
            status,
            Some(error),
            Some(task_usage),
        );
    } else {
        finish_main_session_task_with_error_and_usage_for_tui(
            session,
            event_tx,
            &mut runtime_events,
            task_id,
            status,
            None,
            Some(task_usage),
        );
    }
    TuiAgentTurnResult::new(status)
}

fn finish_main_session_task_for_tui(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_id: &str,
    status: &str,
) {
    finish_main_session_task_with_error_for_tui(session, event_tx, events, task_id, status, None);
}

fn finish_main_session_task_with_error_for_tui(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_id: &str,
    status: &str,
    error: Option<&str>,
) {
    let usage = session.usage_totals();
    finish_main_session_task_with_error_and_usage_for_tui(
        session,
        event_tx,
        events,
        task_id,
        status,
        error,
        Some(usage),
    );
}

fn finish_main_session_task_with_error_and_usage_for_tui(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_id: &str,
    status: &str,
    error: Option<&str>,
    usage: Option<UsageTotals>,
) {
    let result = match status {
        "success" => {
            session
                .task_registry()
                .complete_with_usage(task_id, status.to_string(), usage)
        }
        "interrupted" | "cancelled" => session.task_registry().stop(task_id, status.to_string()),
        _ => session.task_registry().fail_with_usage(
            task_id,
            error.unwrap_or(status).to_string(),
            usage,
        ),
    };
    if result.is_ok() {
        if let Some(finished_task) = task_summary_for_tui(session.task_registry(), task_id) {
            send_task_status_updated_for_tui(event_tx, events, &finished_task);
        }
    }
    session.finish_agent_lifecycle_task(run_status_for_tui_status(status));
}

fn run_status_for_tui_status(status: &str) -> orca_core::event_schema::RunStatus {
    match status {
        "success" => orca_core::event_schema::RunStatus::Success,
        "interrupted" | "cancelled" => orca_core::event_schema::RunStatus::Cancelled,
        "approval_required" => orca_core::event_schema::RunStatus::ApprovalRequired,
        "verification_failed" => orca_core::event_schema::RunStatus::VerificationFailed,
        "budget_exhausted" => orca_core::event_schema::RunStatus::BudgetExhausted,
        _ => orca_core::event_schema::RunStatus::Failed,
    }
}

fn maybe_stop_cancelled_main_session_for_tui(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    task_id: &str,
    pending_tool_requests: &[tool_types::ToolRequest],
) -> Option<TuiAgentTurnResult> {
    if !session.task_registry().is_cancelled(task_id) {
        return None;
    }
    close_unstarted_tool_requests_for_tui(
        session,
        event_tx,
        events,
        pending_tool_requests,
        "the main TUI session stopped before sibling dispatch",
    );
    send_session_completed_for_tui(
        event_tx,
        events,
        orca_core::event_schema::RunStatus::Cancelled,
    );
    finish_main_session_task_for_tui(session, event_tx, events, task_id, "interrupted");
    session.complete("interrupted");
    Some(TuiAgentTurnResult::new("interrupted"))
}

fn close_unstarted_tool_requests_for_tui(
    session: &mut TuiSession<'_>,
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    pending_tool_requests: &[tool_types::ToolRequest],
    reason: &str,
) {
    for request in pending_tool_requests {
        send_tool_requested_for_tui(event_tx, events, request);
        let result = tool_types::ToolResult::cancelled_before_start(request, reason);
        send_tool_completed_for_tui(event_tx, events, &result, None);
        let content = agent_common::format_tool_result_for_model(&result);
        session
            .conversation_mut()
            .add_tool_result_with_terminal(&result, content);
        if let Some(message) = session.conversation().messages.last().cloned() {
            session.append_message(&message);
        }
    }
}

pub(crate) fn send_tool_requested_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    request: &tool_types::ToolRequest,
) {
    send_runtime_event_as_tui(event_tx, events.tool_call_requested(request));
}

pub(crate) fn send_tool_completed_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    result: &tool_types::ToolResult,
    diff: Option<String>,
) {
    if let Some(TuiEvent::ToolCompleted {
        id,
        name,
        status,
        output,
        kind,
        ..
    }) = tui_event_from_runtime_event(&events.tool_call_completed(result))
    {
        let _ = event_tx.send(TuiEvent::ToolCompleted {
            id,
            name,
            status,
            output,
            diff,
            kind,
        });
    }
}

pub(crate) fn send_subagent_started_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    id: &str,
    description: &str,
) {
    send_runtime_event_as_tui(event_tx, events.subagent_started(id, description));
}

pub(crate) fn send_subagent_completed_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    id: &str,
    description: &str,
    status: orca_core::event_schema::RunStatus,
    output: Option<&str>,
    error: Option<&str>,
) {
    send_runtime_event_as_tui(
        event_tx,
        events.subagent_completed(id, description, status, output, error),
    );
}

pub(crate) struct WorkflowNotificationPayload<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) run_id: &'a str,
    pub(crate) tool_use_id: &'a str,
    pub(crate) workflow_name: &'a str,
    pub(crate) status: &'a str,
    pub(crate) summary: &'a str,
}

pub(crate) fn send_workflow_notification_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    payload: WorkflowNotificationPayload<'_>,
) {
    let event = if payload.status == "completed" {
        events.workflow_result_available(
            payload.task_id,
            payload.run_id,
            payload.workflow_name,
            Some(payload.tool_use_id),
            payload.status,
            payload.summary,
        )
    } else {
        events.workflow_failed(
            payload.task_id,
            payload.run_id,
            payload.workflow_name,
            Some(payload.tool_use_id),
            payload.summary,
        )
    };
    send_runtime_event_as_tui(event_tx, event);
}

pub(crate) fn launch_saved_workflow_for_tui_with_registry(
    config: &RunConfig,
    session_id: Option<&str>,
    task_registry: &orca_runtime::tasks::TaskRegistry,
    name: &str,
    raw_args: Option<&str>,
    event_tx: &Sender<TuiEvent>,
) {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let args = match raw_args.map(parse_saved_workflow_args).transpose() {
        Ok(args) => args,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            return;
        }
    };
    let input = WorkflowInput {
        name: Some(name.to_string()),
        args,
        ..Default::default()
    };
    let raw_arguments = match serde_json::to_string(&input) {
        Ok(raw_arguments) => raw_arguments,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error.to_string()));
            return;
        }
    };
    let request = tool_types::ToolRequest {
        id: format!("tui-workflow-{}", now_ms()),
        name: tool_types::ToolName::Workflow,
        action: orca_core::approval_types::ActionKind::Agent,
        target: Some(name.to_string()),
        raw_arguments: Some(raw_arguments),
    };
    let mut events = EventFactory::new(session_id.unwrap_or("tui-workflow-session").to_string());
    send_tool_requested_for_tui(event_tx, &mut events, &request);
    let result = execute_workflow_for_tui(config, &cwd, &request, event_tx, task_registry);
    send_tool_completed_for_tui(event_tx, &mut events, &result, None);
}

fn parse_saved_workflow_args(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    if trimmed.starts_with('{') {
        let value: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|error| error.to_string())?;
        if value.is_object() {
            return Ok(value);
        }
        return Err("workflow args JSON must be an object".to_string());
    }

    let mut object = serde_json::Map::new();
    for part in trimmed.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("workflow arg `{part}` must use key=value"));
        };
        if key.trim().is_empty() {
            return Err("workflow arg key cannot be empty".to_string());
        }
        let parsed_value = serde_json::from_str(value)
            .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
        object.insert(key.to_string(), parsed_value);
    }
    Ok(serde_json::Value::Object(object))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn tui_tools_schema(
    mcp_registry: &McpRegistry,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
    allow_goal_tools: bool,
) -> Vec<serde_json::Value> {
    if allow_goal_tools {
        deepseek_goal_tools_schema_with_mcp_and_external(Some(mcp_registry), external_tools)
    } else {
        deepseek_tools_schema_with_mcp_and_external(Some(mcp_registry), external_tools)
    }
}

#[cfg(test)]
pub(crate) fn run_agent_for_tui(
    config: &RunConfig,
    runtime: &mut RuntimeThread,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    cancel: &CancelToken,
    allow_goal_tools: bool,
) -> String {
    let operation = crate::test_support::HostedOperationHarness::start();
    let controller = operation.controller().clone();
    if cancel.is_cancelled() {
        controller.interrupt_current();
    }
    let control = operation.control();
    let mut session = TuiSession::new(runtime);
    let (mut dispatcher, _command_rx) = match TuiActionDispatcher::spawn(
        action_rx.clone(),
        event_tx.clone(),
        controller,
        crate::channels::USER_ACTION_CAPACITY,
        crate::channels::USER_ACTION_CAPACITY,
    ) {
        Ok(dispatcher) => dispatcher,
        Err(error) => return format!("failed: {error}"),
    };
    let mut tasks = TuiTaskSupervisor::new(8);
    let task_spawner = tasks.spawner();
    let status = run_agent_for_tui_with_notification_queue(
        config,
        &mut session,
        prompt,
        event_tx,
        &control,
        operation.cancel_token(),
        allow_goal_tools,
        None,
        true,
        None,
        None,
        &task_spawner,
    )
    .status;
    if let Err(error) = tasks.shutdown() {
        eprintln!("orca: warning: TUI test task shutdown failed: {error}");
    }
    if let Err(error) = dispatcher.shutdown() {
        eprintln!("orca: warning: TUI test dispatcher shutdown failed: {error}");
    }
    status
}

#[cfg(test)]
pub(crate) fn run_agent_for_tui_with_notification_queue(
    config: &RunConfig,
    session: &mut TuiSession<'_>,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    control: &TuiTurnControl,
    cancel: &CancelToken,
    allow_goal_tools: bool,
    task_description: Option<&str>,
    backtrack_target: bool,
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
    background_completion_handler: Option<TuiBackgroundTurnCompletionHandler>,
    task_spawner: &TuiTaskSpawner,
) -> TuiAgentTurnResult {
    let mut runtime_events = EventFactory::new(
        session
            .session_id()
            .unwrap_or("tui-agent-session")
            .to_string(),
    );
    run_agent_for_tui_with_event_factory(
        config,
        session,
        prompt,
        event_tx,
        event_tx,
        control,
        cancel,
        allow_goal_tools,
        TuiMainSessionTaskStart::Create(task_description.unwrap_or(prompt)),
        backtrack_target,
        pending_workflow_notifications,
        background_completion_handler,
        task_spawner,
        &mut runtime_events,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_agent_for_tui_with_event_factory(
    config: &RunConfig,
    session: &mut TuiSession<'_>,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    background_event_tx: &Sender<TuiEvent>,
    control: &TuiTurnControl,
    cancel: &CancelToken,
    allow_goal_tools: bool,
    main_session_task: TuiMainSessionTaskStart<'_>,
    backtrack_target: bool,
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
    background_completion_handler: Option<TuiBackgroundTurnCompletionHandler>,
    task_spawner: &TuiTaskSpawner,
    runtime_events: &mut EventFactory,
) -> TuiAgentTurnResult {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let tools_override = tui_tools_schema(
        session.mcp_registry(),
        &config.external_tools,
        allow_goal_tools,
    );
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(tools_override),
        mcp_registry: Some(session.mcp_registry().clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let ctx_config = orca_provider::context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let mut permission_overlay = orca_runtime::lifecycle::TurnPermissionOverlay::default();
    let mut turn: u32 = 0;
    let mut tui_compaction = orca_runtime::TuiAgentTurnCompactionState::new();
    let mut runtime_events = runtime_events;
    let main_session_task_id =
        start_main_session_task_for_tui(session, event_tx, &mut runtime_events, main_session_task);
    let mut main_session_backgrounded = false;
    let mut background_completion_handler = background_completion_handler;
    let mut provider_budget_admission = None;
    poll_background_current_turn_for_tui(
        session,
        event_tx,
        &mut runtime_events,
        control,
        &main_session_task_id,
        &mut main_session_backgrounded,
    );
    if let Some(error) = usage_budget_error(session.usage_totals(), config.max_budget_usd) {
        send_error_for_tui(event_tx, &mut runtime_events, &error);
        send_session_completed_for_tui(
            event_tx,
            &mut runtime_events,
            orca_core::event_schema::RunStatus::BudgetExhausted,
        );
        finish_main_session_task_with_error_for_tui(
            session,
            event_tx,
            &mut runtime_events,
            &main_session_task_id,
            "budget_exhausted",
            Some(&error),
        );
        session.complete_with_error("budget_exhausted", &error);
        return TuiAgentTurnResult::new("budget_exhausted");
    }
    session.replace_skill_context(agent_common::explicit_skill_context(&cwd, prompt));
    if backtrack_target {
        session.conversation_mut().add_user(prompt.to_string());
    } else {
        session
            .conversation_mut()
            .add_user_pinned(prompt.to_string());
    }
    if let Some(message) = session.conversation().messages.last().cloned() {
        session.append_message(&message);
    }

    loop {
        turn += 1;

        if turn > DEFAULT_MAX_TURNS {
            send_error_for_tui(event_tx, &mut runtime_events, "max turns exhausted");
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::BudgetExhausted,
            );
            finish_main_session_task_for_tui(
                session,
                event_tx,
                &mut runtime_events,
                &main_session_task_id,
                "budget_exhausted",
            );
            session.complete("budget_exhausted");
            return TuiAgentTurnResult::new("budget_exhausted");
        }

        let runtime_event_observer = Arc::new(TuiRuntimeEventObserver::new(event_tx.clone()));
        let mut runtime_event_output = io::sink();
        let context_usage = match orca_runtime::run_tui_agent_turn_compaction(
            session.runtime_session_mut(),
            orca_runtime::TuiAgentTurnCompactionInput {
                provider: config.provider,
                context_config: &ctx_config,
                provider_config: &provider_config,
                cwd: &cwd,
                prompt,
                subagent_depth: 0,
                subagent_type: &SubagentType::General,
                emit_deltas: true,
                cancel,
                events: &mut runtime_events,
                event_observer: Some(runtime_event_observer),
                writer: &mut runtime_event_output,
            },
        ) {
            Ok(context_usage) => context_usage,
            Err(error) => {
                send_error_for_tui(
                    event_tx,
                    &mut runtime_events,
                    &format!("context compaction failed: {error}"),
                );
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::Failed,
                );
                finish_main_session_task_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    "failed",
                );
                session.complete("failed");
                return TuiAgentTurnResult::new("failed");
            }
        };

        let _ = event_tx.send(TuiEvent::ContextUpdated {
            used_tokens: context_usage.used_tokens,
            limit_tokens: context_usage.limit_tokens,
        });

        let (turn, task) = session.next_turn_lifecycle();
        let _ = event_tx.send(TuiEvent::TurnStarted { turn, task });
        let turn_extension_id = session
            .session_id()
            .map(|session_id| format!("{session_id}:turn-{turn}"));

        let route_decision = config.model.route(ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
        });
        session
            .cost_tracker_mut()
            .set_model(Some(&route_decision.actual_model));
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model.clone());

        let pre_model_outcome = match session.hooks().run(
            HookEvent::PreModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                send_error_for_tui(
                    event_tx,
                    &mut runtime_events,
                    &format!("pre_model_call hook failed: {error}"),
                );
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::Failed,
                );
                finish_main_session_task_with_error_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    "failed",
                    Some(&error),
                );
                session.complete_with_error("failed", &error);
                return TuiAgentTurnResult::new("failed");
            }
        };
        let model_conversation =
            conversation_with_hook_context(session.conversation(), &pre_model_outcome);

        if provider_budget_admission.is_none() {
            provider_budget_admission = match session
                .usage_ledger()
                .admit_budgeted_request(config.max_budget_usd, cancel)
            {
                Ok(admission) => Some(admission),
                Err(crate::bridge::TuiBudgetAdmissionError::BudgetExhausted(totals)) => {
                    let error = usage_budget_error(totals, config.max_budget_usd)
                        .unwrap_or_else(|| "budget exhausted".to_string());
                    send_error_for_tui(event_tx, &mut runtime_events, &error);
                    send_session_completed_for_tui(
                        event_tx,
                        &mut runtime_events,
                        orca_core::event_schema::RunStatus::BudgetExhausted,
                    );
                    finish_main_session_task_with_error_for_tui(
                        session,
                        event_tx,
                        &mut runtime_events,
                        &main_session_task_id,
                        "budget_exhausted",
                        Some(&error),
                    );
                    session.complete_with_error("budget_exhausted", &error);
                    return TuiAgentTurnResult::new("budget_exhausted");
                }
                Err(crate::bridge::TuiBudgetAdmissionError::Cancelled) => {
                    send_session_completed_for_tui(
                        event_tx,
                        &mut runtime_events,
                        orca_core::event_schema::RunStatus::Cancelled,
                    );
                    finish_main_session_task_for_tui(
                        session,
                        event_tx,
                        &mut runtime_events,
                        &main_session_task_id,
                        "cancelled",
                    );
                    session.complete("cancelled");
                    return TuiAgentTurnResult::new("cancelled");
                }
            };
        }

        let mut emitted_message_delta = false;
        let mut stream_events = EventFactory::new(runtime_events.run_id().to_string());
        let mut provider_task = match spawn_provider_stream(
            config.provider,
            model_conversation.clone(),
            turn_provider_config.clone(),
            cancel.clone(),
        ) {
            Ok(provider_task) => provider_task,
            Err(error) => {
                let error = format!("failed to start provider stream: {error}");
                send_error_for_tui(event_tx, &mut runtime_events, &error);
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::Failed,
                );
                finish_main_session_task_with_error_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    "failed",
                    Some(&error),
                );
                session.complete_with_error("failed", &error);
                return TuiAgentTurnResult::new("failed");
            }
        };
        let response = loop {
            match provider_task.recv_timeout(Duration::from_millis(10)) {
                Ok(ProviderStreamEvent::Step(ProviderStep::ReasoningDelta(text))) => {
                    poll_background_current_turn_for_tui(
                        session,
                        event_tx,
                        &mut stream_events,
                        control,
                        &main_session_task_id,
                        &mut main_session_backgrounded,
                    );
                    send_runtime_event_as_tui(
                        event_tx,
                        stream_events.assistant_reasoning_delta(&text),
                    );
                }
                Ok(ProviderStreamEvent::Step(ProviderStep::MessageDelta(text))) => {
                    poll_background_current_turn_for_tui(
                        session,
                        event_tx,
                        &mut stream_events,
                        control,
                        &main_session_task_id,
                        &mut main_session_backgrounded,
                    );
                    emitted_message_delta = true;
                    send_runtime_event_as_tui(
                        event_tx,
                        stream_events.assistant_message_delta(&text),
                    );
                }
                Ok(ProviderStreamEvent::Step(ProviderStep::ToolCallProgress(progress))) => {
                    poll_background_current_turn_for_tui(
                        session,
                        event_tx,
                        &mut stream_events,
                        control,
                        &main_session_task_id,
                        &mut main_session_backgrounded,
                    );
                    send_runtime_event_as_tui(
                        event_tx,
                        stream_events.tool_call_progress(&progress),
                    );
                }
                Ok(ProviderStreamEvent::Step(_)) => {}
                Ok(ProviderStreamEvent::Done(response)) => {
                    if let Err(error) = provider_task.join() {
                        let error = error.to_string();
                        send_error_for_tui(event_tx, &mut runtime_events, &error);
                        send_session_completed_for_tui(
                            event_tx,
                            &mut runtime_events,
                            orca_core::event_schema::RunStatus::Failed,
                        );
                        finish_main_session_task_with_error_for_tui(
                            session,
                            event_tx,
                            &mut runtime_events,
                            &main_session_task_id,
                            "failed",
                            Some(&error),
                        );
                        session.complete_with_error("failed", &error);
                        return TuiAgentTurnResult::new("failed");
                    }
                    break response;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    poll_background_current_turn_for_tui(
                        session,
                        event_tx,
                        &mut stream_events,
                        control,
                        &main_session_task_id,
                        &mut main_session_backgrounded,
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let join_error = provider_task.join().err();
                    let error = join_error
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "provider stream ended without a response".to_string());
                    send_error_for_tui(event_tx, &mut runtime_events, &error);
                    send_session_completed_for_tui(
                        event_tx,
                        &mut runtime_events,
                        orca_core::event_schema::RunStatus::Failed,
                    );
                    finish_main_session_task_with_error_for_tui(
                        session,
                        event_tx,
                        &mut runtime_events,
                        &main_session_task_id,
                        "failed",
                        Some(&error),
                    );
                    session.complete_with_error("failed", &error);
                    return TuiAgentTurnResult::new("failed");
                }
            }

            if main_session_backgrounded {
                let history_writer = session.writer_mut().map(|writer| writer.clone());
                let usage_ledger = session.usage_ledger();
                if let Err(error) = spawn_background_provider_completion(
                    provider_task,
                    BackgroundProviderCompletionContext {
                        task_registry: session.task_registry().clone(),
                        history_writer,
                        model: Some(route_decision.actual_model.clone()),
                        usage_ledger,
                        budget_admission: provider_budget_admission.take(),
                        max_budget_usd: config.max_budget_usd,
                        event_tx: background_event_tx.clone(),
                        run_id: runtime_events.run_id().to_string(),
                        task_id: main_session_task_id.clone(),
                        completion_handler: background_completion_handler.take(),
                    },
                    task_spawner,
                ) {
                    session.finish_agent_lifecycle_task(orca_core::event_schema::RunStatus::Failed);
                    session.complete_with_error("failed", &error.to_string());
                    return TuiAgentTurnResult::new("failed");
                }
                return TuiAgentTurnResult::new("backgrounded");
            }
        };

        if let Err(error) = session.hooks().run(
            HookEvent::PostModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: response.usage.as_ref(),
            },
        ) {
            send_error_for_tui(
                event_tx,
                &mut runtime_events,
                &format!("post_model_call hook failed: {error}"),
            );
        }

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            let totals = session.record_provider_usage(usage);
            send_runtime_event_as_tui(event_tx, runtime_events.usage_updated(totals));
            let history_totals = session.runtime_session().usage_totals();
            if let Some(writer) = session.writer_mut() {
                let _ = writer.append_usage(history_totals);
            }
            if let Some(error) = usage_budget_error(totals, config.max_budget_usd) {
                send_error_for_tui(event_tx, &mut runtime_events, &error);
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::BudgetExhausted,
                );
                finish_main_session_task_with_error_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    "budget_exhausted",
                    Some(&error),
                );
                session.complete_with_error("budget_exhausted", &error);
                return TuiAgentTurnResult::new("budget_exhausted");
            }
        }
        provider_budget_admission = None;

        if cancel.is_cancelled() {
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Cancelled,
            );
            finish_main_session_task_for_tui(
                session,
                event_tx,
                &mut runtime_events,
                &main_session_task_id,
                "interrupted",
            );
            session.complete("interrupted");
            return TuiAgentTurnResult::new("interrupted");
        }

        let runtime_event_observer = Arc::new(TuiRuntimeEventObserver::new(event_tx.clone()));
        let mut runtime_event_output = io::sink();
        match orca_runtime::handle_tui_agent_provider_error(
            session.runtime_session_mut(),
            &mut tui_compaction,
            &response,
            orca_runtime::TuiAgentTurnCompactionInput {
                provider: config.provider,
                context_config: &ctx_config,
                provider_config: &provider_config,
                cwd: &cwd,
                prompt,
                subagent_depth: 0,
                subagent_type: &SubagentType::General,
                emit_deltas: true,
                cancel,
                events: &mut runtime_events,
                event_observer: Some(runtime_event_observer),
                writer: &mut runtime_event_output,
            },
        ) {
            Ok(orca_runtime::TuiAgentProviderErrorAction::NoError) => {}
            Ok(orca_runtime::TuiAgentProviderErrorAction::RetryAfterCompaction) => continue,
            Ok(orca_runtime::TuiAgentProviderErrorAction::SurfaceError(error)) => {
                send_error_for_tui(event_tx, &mut runtime_events, &error);
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::Failed,
                );
                finish_main_session_task_with_error_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    "failed",
                    Some(&error),
                );
                session.complete_with_error("failed", &error);
                return TuiAgentTurnResult::new("failed");
            }
            Err(error) => {
                send_error_for_tui(
                    event_tx,
                    &mut runtime_events,
                    &format!("context compaction failed: {error}"),
                );
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::Failed,
                );
                finish_main_session_task_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    "failed",
                );
                session.complete("failed");
                return TuiAgentTurnResult::new("failed");
            }
        }

        if response.tool_calls.is_empty() {
            if !emitted_message_delta
                && let Some(content) = response.assistant_content.as_deref()
                && !content.is_empty()
            {
                send_runtime_event_as_tui(
                    event_tx,
                    runtime_events.assistant_message_delta(content),
                );
            }
            session.conversation_mut().add_assistant(
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
            );
            if let Some(message) = session.conversation().messages.last().cloned() {
                session.append_message(&message);
            }
            if config.auto_memory {
                let provider_kind = config.provider;
                let provider_config = ProviderConfig {
                    api_key: config.api_key.clone(),
                    base_url: config.base_url.clone(),
                    model: Some(orca_core::model::auxiliary_model().to_string()),
                    reasoning_effort: config.reasoning_effort,
                    tools_override: Some(Vec::new()),
                    mcp_registry: None,
                    external_tools: Vec::new(),
                };
                let memory_cwd = cwd.clone();
                let messages = session.conversation().messages.clone();
                let memory_tx = background_event_tx.clone();
                let run_id = runtime_events.run_id().to_string();
                if let Err(error) = task_spawner.spawn("auto-memory", move |memory_cancel| {
                    if let Err(error) = memory::extract_project_memory_with_cancel(
                        provider_kind,
                        &provider_config,
                        &memory_cwd,
                        &messages,
                        &memory_cancel,
                    ) {
                        let mut events = EventFactory::new(run_id);
                        send_error_for_tui(
                            &memory_tx,
                            &mut events,
                            &format!("memory extraction failed: {error}"),
                        );
                    }
                }) {
                    send_error_for_tui(
                        event_tx,
                        &mut runtime_events,
                        &format!("failed to start memory extraction: {error}"),
                    );
                }
            }
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Success,
            );
            finish_main_session_task_for_tui(
                session,
                event_tx,
                &mut runtime_events,
                &main_session_task_id,
                "success",
            );
            session.complete("success");
            return TuiAgentTurnResult::new("success");
        }

        session.conversation_mut().add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );
        if let Some(message) = session.conversation().messages.last().cloned() {
            session.append_message(&message);
        }

        let tool_requests: Vec<tool_types::ToolRequest> = response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
                _ => None,
            })
            .collect();
        let mut index = 0;
        while index < tool_requests.len() {
            if should_run_subagent_batch(config, &tool_requests[index], 0) {
                let batch_end = collect_subagent_batch(config, &tool_requests, index);
                let results = execute_subagent_batch_for_tui(
                    config,
                    &cwd,
                    &tool_requests[index..batch_end],
                    event_tx,
                    control,
                    0,
                    session.instructions(),
                    session.memory(),
                    session.mcp_registry(),
                    session.hooks(),
                    Some(session.task_registry()),
                );
                let mut batch_terminal_status = None;
                for (should_stop, result, child_cost) in results {
                    if let Some(turn_extension_id) = turn_extension_id.as_deref() {
                        record_tui_goal_tool_finish(session, turn_extension_id, &result);
                    }
                    let child_usage = child_cost.totals();
                    if child_usage.total_tokens() > 0
                        || child_usage.cache_tokens > 0
                        || child_usage.estimated_cost_usd > 0.0
                    {
                        let totals = persist_merged_usage(
                            session,
                            event_tx,
                            &mut runtime_events,
                            child_usage,
                        );
                        if let Some(result) = finish_budget_exhausted_after_usage(
                            session,
                            event_tx,
                            &mut runtime_events,
                            &main_session_task_id,
                            totals,
                            config.max_budget_usd,
                        ) {
                            return result;
                        }
                    }
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    session
                        .conversation_mut()
                        .add_tool_result_with_terminal(&result, result_content);
                    if let Some(message) = session.conversation().messages.last().cloned() {
                        session.append_message(&message);
                    }
                    if should_stop && batch_terminal_status.is_none() {
                        batch_terminal_status = Some(result.status);
                    }
                }
                if let Some(result) = maybe_stop_cancelled_main_session_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    &tool_requests[batch_end..],
                ) {
                    return result;
                }
                if let Some(terminal_status) = batch_terminal_status {
                    close_unstarted_tool_requests_for_tui(
                        session,
                        event_tx,
                        &mut runtime_events,
                        &tool_requests[batch_end..],
                        "an earlier subagent ended the TUI tool turn",
                    );
                    let status = match terminal_status {
                        tool_types::ToolStatus::Denied => "approval_required",
                        tool_types::ToolStatus::Cancelled => "interrupted",
                        _ => "failed",
                    };
                    send_session_completed_status_for_tui(event_tx, &mut runtime_events, status);
                    finish_main_session_task_for_tui(
                        session,
                        event_tx,
                        &mut runtime_events,
                        &main_session_task_id,
                        status,
                    );
                    session.complete(status);
                    return TuiAgentTurnResult::new(status);
                }
                index = batch_end;
                continue;
            }

            if orca_tools::should_run_readonly_batch(
                config.tools.max_read_parallel,
                &tool_requests[index],
            ) {
                let batch_end = orca_tools::collect_readonly_batch(
                    config.tools.max_read_parallel,
                    &tool_requests,
                    index,
                );
                let results = execute_readonly_batch_for_tui(
                    config,
                    &cwd,
                    &tool_requests[index..batch_end],
                    event_tx,
                    session.mcp_registry(),
                    session.hooks(),
                    config.tools.output_truncation,
                );
                for result in results {
                    if let Some(turn_extension_id) = turn_extension_id.as_deref() {
                        record_tui_goal_tool_finish(session, turn_extension_id, &result);
                    }
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    session
                        .conversation_mut()
                        .add_tool_result_with_terminal(&result, result_content);
                    if let Some(message) = session.conversation().messages.last().cloned() {
                        session.append_message(&message);
                    }
                }
                if let Some(result) = maybe_stop_cancelled_main_session_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    &tool_requests[batch_end..],
                ) {
                    return result;
                }
                index = batch_end;
                continue;
            }

            let tool_request = &tool_requests[index];
            let child_budget_config = (tool_request.name == tool_types::ToolName::Subagent)
                .then(|| config_for_remaining_subagent_budget(config, session.usage_totals()));
            let tool_config = child_budget_config.as_ref().unwrap_or(config);
            let (should_stop, result, child_cost) = execute_tool_for_tui_with_background_events(
                tool_config,
                &cwd,
                tool_request,
                event_tx,
                background_event_tx,
                control,
                Some(session.pending_interactions()),
                0,
                session.session_id(),
                Some(session.thread_extensions_handle()),
                &policy,
                session.instructions(),
                session.memory(),
                session.mcp_registry(),
                session.hooks(),
                Some(session.task_registry()),
                &mut permission_overlay,
                cancel,
            );

            if let Some(c) = child_cost {
                let child_usage = c.totals();
                if child_usage.total_tokens() > 0
                    || child_usage.cache_tokens > 0
                    || child_usage.estimated_cost_usd > 0.0
                {
                    let totals =
                        persist_merged_usage(session, event_tx, &mut runtime_events, child_usage);
                    if let Some(turn_result) = finish_budget_exhausted_after_usage(
                        session,
                        event_tx,
                        &mut runtime_events,
                        &main_session_task_id,
                        totals,
                        config.max_budget_usd,
                    ) {
                        let result_content = agent_common::format_tool_result_for_model(&result);
                        session
                            .conversation_mut()
                            .add_tool_result(tool_request.id.clone(), result_content);
                        if let Some(message) = session.conversation().messages.last().cloned() {
                            session.append_message(&message);
                        }
                        return turn_result;
                    }
                }
            }

            if let Some(turn_extension_id) = turn_extension_id.as_deref() {
                record_tui_goal_tool_finish(session, turn_extension_id, &result);
            }

            if tool_request.name == tool_types::ToolName::UpdatePlan
                && result.status == tool_types::ToolStatus::Completed
            {
                if let Ok(update) = orca_tools::update_plan::parse_args(tool_request) {
                    session.conversation_mut().replace_plan_state(
                        orca_tools::update_plan::format_context_message(&update),
                    );
                    if let Some(writer) = session.writer_mut() {
                        let _ = writer.append_plan_state(update.explanation, update.plan);
                    }
                }
            }

            let result_content = agent_common::format_tool_result_for_model(&result);
            session
                .conversation_mut()
                .add_tool_result_with_terminal(&result, result_content);
            if let Some(message) = session.conversation().messages.last().cloned() {
                session.append_message(&message);
            }

            if let Some(result) = maybe_stop_cancelled_main_session_for_tui(
                session,
                event_tx,
                &mut runtime_events,
                &main_session_task_id,
                &tool_requests[index + 1..],
            ) {
                return result;
            }

            if should_stop {
                close_unstarted_tool_requests_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &tool_requests[index + 1..],
                    "an earlier sibling ended the TUI tool turn",
                );
                let status = match result.status {
                    tool_types::ToolStatus::Denied => "approval_required",
                    tool_types::ToolStatus::Cancelled => "interrupted",
                    _ => "failed",
                };
                send_session_completed_status_for_tui(event_tx, &mut runtime_events, status);
                finish_main_session_task_for_tui(
                    session,
                    event_tx,
                    &mut runtime_events,
                    &main_session_task_id,
                    status,
                );
                session.complete(status);
                return TuiAgentTurnResult::new(status);
            }
            index += 1;
        }
        if let Some(notification) =
            take_pending_workflow_notification(pending_workflow_notifications)
        {
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Success,
            );
            finish_main_session_task_for_tui(
                session,
                event_tx,
                &mut runtime_events,
                &main_session_task_id,
                "success",
            );
            session.complete("success");
            return TuiAgentTurnResult::with_continuation(
                "success",
                TuiAgentTurnContinuation::WorkflowNotification(notification),
            );
        }
    }
}

fn take_pending_workflow_notification(
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
) -> Option<PendingWorkflowNotification> {
    pending_workflow_notifications.and_then(PendingWorkflowNotifications::pop_notification)
}

fn record_tui_goal_tool_finish(
    session: &TuiSession<'_>,
    turn_extension_id: &str,
    result: &tool_types::ToolResult,
) {
    if result.status != tool_types::ToolStatus::Completed {
        return;
    }

    let turn_store = orca_runtime::extension::ExtensionData::new(turn_extension_id);
    RuntimeTurnReducer::new(session.thread_extensions(), &turn_store).record_tool_finish(
        RuntimeToolFinish {
            tool_name: result.name.as_str(),
            call_id: &result.id,
            outcome: orca_runtime::extension::ToolCallOutcome::Completed,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_subagent_execution::{
        collect_subagent_batch, config_for_remaining_subagent_budget,
        execute_subagent_batch_for_tui, execute_subagent_for_tui, execute_subagent_status_for_tui,
        run_child_agent_for_tui_silent, should_run_subagent_batch,
    };
    use crate::agent_tool_execution::{canonical_action_for_tool, execute_tool_for_tui};
    use crossbeam_channel as mpsc;
    use orca_runtime::hooks::HookRunner;
    use orca_runtime::instructions::ProjectInstructions;
    use orca_runtime::memory::MemoryBlock;
    use orca_runtime::tasks::TaskRegistry;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use orca_core::event_schema::RunStatus;
    use orca_core::model::ModelSelection;
    use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
    use orca_runtime::workflow::host::WorkflowHost;

    fn with_isolated_orca_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().expect("temp ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let result = f(home.path());
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        result
    }

    fn test_task_supervisor() -> (TuiTaskSupervisor, TuiTaskSpawner) {
        let supervisor = TuiTaskSupervisor::new(8);
        let spawner = supervisor.spawner();
        (supervisor, spawner)
    }

    fn test_turn() -> (
        TuiOperationController,
        crate::test_support::HostedOperationHarness,
        TuiTurnControl,
    ) {
        let operation = crate::test_support::HostedOperationHarness::start();
        let controller = operation.controller().clone();
        let control = operation.control();
        (controller, operation, control)
    }

    fn test_provider_stream_task(receiver: Receiver<ProviderStreamEvent>) -> ProviderStreamTask {
        ProviderStreamTask {
            receiver: Some(receiver),
            cancel: CancelToken::new(),
            handle: Some(thread::spawn(|| {})),
        }
    }

    fn spawn_test_background_provider_completion(
        provider_rx: Receiver<ProviderStreamEvent>,
        context: BackgroundProviderCompletionContext,
    ) -> TuiTaskSupervisor {
        let (supervisor, spawner) = test_task_supervisor();
        spawn_background_provider_completion(
            test_provider_stream_task(provider_rx),
            context,
            &spawner,
        )
        .expect("background provider completion admitted");
        supervisor
    }

    fn cancellable_test_provider_stream_task() -> (ProviderStreamTask, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::bounded(1);
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let joined = Arc::new(AtomicBool::new(false));
        let worker_joined = Arc::clone(&joined);
        let handle = thread::spawn(move || {
            while !worker_cancel.is_cancelled() {
                thread::yield_now();
            }
            worker_joined.store(true, Ordering::SeqCst);
            drop(tx);
        });
        (
            ProviderStreamTask {
                receiver: Some(rx),
                cancel,
                handle: Some(handle),
            },
            joined,
        )
    }

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            subagents: Default::default(),
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn tui_runtime_event_observer_forwards_typed_event() {
        let (event_tx, event_rx) = mpsc::bounded(1);
        let observer = TuiRuntimeEventObserver::new(event_tx);
        let mut events = EventFactory::new("run-observer".to_string());

        observer
            .observe(&events.assistant_message_delta("hello"))
            .expect("event should reach the TUI mailbox");

        match event_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            TuiEvent::MessageDelta(text) => assert_eq!(text, "hello"),
            event => panic!("expected message delta, got {event:?}"),
        }
    }

    #[test]
    fn tui_runtime_event_observer_reports_disconnected_mailbox() {
        let (event_tx, event_rx) = mpsc::bounded(1);
        drop(event_rx);
        let observer = TuiRuntimeEventObserver::new(event_tx);
        let mut events = EventFactory::new("run-observer-disconnected".to_string());

        let error = observer
            .observe(&events.assistant_message_delta("hello"))
            .expect_err("disconnected mailbox should fail the operation");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    fn full_auto_config() -> RunConfig {
        RunConfig {
            approval_mode: ApprovalMode::FullAuto,
            ..config()
        }
    }

    #[test]
    fn tui_surface_error_preserves_redacted_failure_diagnostics() {
        with_isolated_orca_home(|home| {
            let mut config = config();
            config.history_mode = HistoryMode::Record;
            let mut session =
                RuntimeThread::start_with_preloaded(&config, "provider failure", None)
                    .expect("session");
            let session_id = {
                let session = TuiSession::new(&mut session);
                session.session_id().expect("session id").to_string()
            };
            let (event_tx, event_rx) = mpsc::unbounded();
            let (_action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();
            let error = "mock provider error: api_key=super-secret";

            let status = run_agent_for_tui(
                &config,
                &mut session,
                "mock_provider_error",
                &event_tx,
                &action_rx,
                &cancel,
                false,
            );

            assert_eq!(status, "failed");
            assert!(
                event_rx
                    .try_iter()
                    .any(|event| matches!(event, TuiEvent::Error(message) if message == error))
            );
            let task = TuiSession::new(&mut session)
                .task_registry()
                .list()
                .into_iter()
                .find(|task| task.task_type == TaskType::MainSession)
                .expect("main session task");
            assert_eq!(task.status, TaskStatus::Failed);
            assert_eq!(task.error.as_deref(), Some(error));

            let transcript =
                orca_runtime::history::load_session("latest").expect("failed session transcript");
            assert_eq!(
                transcript.completion_error.as_deref(),
                Some("mock provider error: api_key=<redacted>")
            );
            let tasks_path = home
                .join("task-sessions")
                .join(session_id)
                .join("tasks.json");
            let persisted_tasks = std::fs::read_to_string(tasks_path).expect("persisted tasks");
            assert!(!persisted_tasks.contains("super-secret"));
            assert!(persisted_tasks.contains("api_key=<redacted>"));
        });
    }

    #[test]
    fn background_provider_failure_preserves_error() {
        with_isolated_orca_home(|home| {
            let mut config = config();
            config.history_mode = HistoryMode::Record;
            let mut session =
                RuntimeThread::start_with_preloaded(&config, "background failure", None)
                    .expect("session");
            let (session_id, registry, history_writer) = {
                let mut tui_session = TuiSession::new(&mut session);
                (
                    tui_session.session_id().expect("session id").to_string(),
                    tui_session.task_registry().clone(),
                    tui_session.writer_mut().cloned(),
                )
            };
            let task = registry.create_main_session("Provider failure".to_string());
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();
            let (event_tx, event_rx) = mpsc::unbounded();
            let (provider_tx, provider_rx) = mpsc::unbounded();
            let error = "DeepSeek provider error: api_key=super-secret";

            let _background_tasks = spawn_test_background_provider_completion(
                provider_rx,
                BackgroundProviderCompletionContext {
                    task_registry: registry.clone(),
                    history_writer,
                    model: Some(orca_core::model::FLASH_MODEL.to_string()),
                    usage_ledger: TuiUsageLedger::default(),
                    budget_admission: None,
                    max_budget_usd: None,
                    event_tx,
                    run_id: session_id.clone(),
                    task_id: task.id.clone(),
                    completion_handler: None,
                },
            );
            TuiSession::new(&mut session).complete("success");
            provider_tx
                .send(ProviderStreamEvent::Done(ProviderResponse {
                    steps: vec![ProviderStep::Error(error.to_string())],
                    assistant_content: None,
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: Some(orca_core::provider_types::Usage {
                        input_tokens: 120,
                        output_tokens: 30,
                        cache_tokens: 10,
                    }),
                }))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    event
                        if task_update_matches(&event, |task| {
                            task.status == TaskStatus::Failed
                        }) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            let record = registry.get(&task.id).unwrap();
            assert_eq!(record.error.as_deref(), Some(error));
            let usage = record.usage.expect("background failure usage");
            assert_eq!(usage.input_tokens, 120);
            assert_eq!(usage.output_tokens, 30);
            assert_eq!(usage.cache_tokens, 10);
            assert!(usage.estimated_cost_usd > 0.0);
            let transcript =
                orca_runtime::history::load_session(&session_id).expect("background transcript");
            assert_eq!(transcript.completion_status.as_deref(), Some("success"));
            assert_eq!(transcript.completion_error, None);
            let transcript_usage = transcript.usage.expect("background transcript usage");
            assert_eq!(transcript_usage.input_tokens, 120);
            assert_eq!(transcript_usage.output_tokens, 30);
            assert_eq!(transcript_usage.cache_tokens, 10);
            assert!(transcript_usage.estimated_cost_usd > 0.0);
            let persisted_session =
                std::fs::read_to_string(&transcript.path).expect("persisted session");
            assert_eq!(
                persisted_session
                    .lines()
                    .filter(|line| line.contains("\"type\":\"session.completed\""))
                    .count(),
                1
            );
            let background_record = persisted_session
                .lines()
                .find(|line| line.contains("\"type\":\"background_task.provider_response\""))
                .expect("task-correlated background provider response");
            assert!(background_record.contains("DeepSeek provider error: api_key=<redacted>"));
            assert!(!background_record.contains("super-secret"));
            assert!(background_record.contains("\"input_tokens\":120"));
            let tasks_path = home
                .join("task-sessions")
                .join(session_id)
                .join("tasks.json");
            let persisted_tasks = std::fs::read_to_string(tasks_path).expect("persisted tasks");
            assert!(!persisted_tasks.contains("super-secret"));
            assert!(persisted_tasks.contains("api_key=<redacted>"));
        });
    }

    #[test]
    fn approved_background_failure_preserves_controller_error() {
        with_isolated_orca_home(|_| {
            let mut config = config();
            config.history_mode = HistoryMode::Record;
            let mut runtime =
                RuntimeThread::start_with_preloaded(&config, "approved failure", None)
                    .expect("session");
            let mut session = TuiSession::new(&mut runtime);
            let session_id = session.session_id().expect("session id").to_string();
            session
                .conversation_mut()
                .add_user("mock_provider_error".to_string());
            let user_message = session
                .conversation()
                .messages
                .last()
                .cloned()
                .expect("user message");
            session.append_message(&user_message);

            let registry = session.task_registry().clone();
            let task = registry.create_main_session("Provider failure".to_string());
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();
            let tool_request = tool_types::ToolRequest {
                id: "mock-tool-1".to_string(),
                name: tool_types::ToolName::TaskList,
                action: orca_core::approval_types::ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            };
            let raw_tool_call = orca_core::conversation::RawToolCall {
                id: tool_request.id.clone(),
                function_name: tool_request.name.as_str().to_string(),
                arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
            };
            registry
                .approval_required_for_pending_provider_response(
                    &task.id,
                    "approval_required".to_string(),
                    ProviderResponse {
                        steps: vec![ProviderStep::ToolCall(tool_request)],
                        assistant_content: Some("I need task state.".to_string()),
                        assistant_reasoning: Some("Inspect tasks.".to_string()),
                        tool_calls: vec![raw_tool_call],
                        usage: None,
                    },
                )
                .unwrap();
            registry
                .submit_pending_tool_approval_response(&task.id, true)
                .unwrap();

            let (event_tx, _event_rx) = mpsc::unbounded();
            let result = continue_approved_background_turn_for_tui(
                &config,
                &mut session,
                &TuiBackgroundTurnContinuationRequest::new(task.id.clone()),
                &event_tx,
                &CancelToken::new(),
                None,
            );

            assert_eq!(result.status, "failed");
            assert_eq!(
                registry.get(&task.id).unwrap().error.as_deref(),
                Some("mock provider error: api_key=super-secret")
            );
            let transcript =
                orca_runtime::history::load_session(&session_id).expect("continued transcript");
            assert_eq!(
                transcript.completion_error.as_deref(),
                Some("mock provider error: api_key=<redacted>")
            );
            let persisted =
                std::fs::read_to_string(&transcript.path).expect("continued session JSONL");
            assert_eq!(
                persisted
                    .lines()
                    .filter(|line| line.contains("\"type\":\"session.completed\""))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn approved_background_continuation_adds_task_local_usage_exactly_once() {
        with_isolated_orca_home(|_| {
            let mut config = full_auto_config();
            config.history_mode = HistoryMode::Record;
            let mut runtime =
                RuntimeThread::start_with_preloaded(&config, "approved task-local usage", None)
                    .expect("session");
            let mut session = TuiSession::new(&mut runtime);
            session.record_external_usage(UsageTotals {
                input_tokens: 10_000,
                output_tokens: 1_000,
                cache_tokens: 8_000,
                estimated_cost_usd: 1.0,
            });

            let registry = session.task_registry().clone();
            let task = registry.create_main_session("Usage child".to_string());
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();
            let initial_task_usage = UsageTotals {
                input_tokens: 40,
                output_tokens: 10,
                cache_tokens: 30,
                estimated_cost_usd: 0.01,
            };
            session.usage_ledger().add(initial_task_usage);
            let tool_request = tool_types::ToolRequest {
                id: "usage-child-1".to_string(),
                name: tool_types::ToolName::Subagent,
                action: orca_core::approval_types::ActionKind::Agent,
                target: Some("usage child".to_string()),
                raw_arguments: Some(
                    serde_json::json!({
                        "description": "usage child",
                        "prompt": "mock_usage"
                    })
                    .to_string(),
                ),
            };
            let raw_tool_call = orca_core::conversation::RawToolCall {
                id: tool_request.id.clone(),
                function_name: tool_request.name.as_str().to_string(),
                arguments: tool_request.raw_arguments.clone().unwrap(),
            };
            registry
                .approval_required_for_pending_provider_response_with_usage(
                    &task.id,
                    "approval_required".to_string(),
                    ProviderResponse {
                        steps: vec![ProviderStep::ToolCall(tool_request)],
                        assistant_content: None,
                        assistant_reasoning: Some("Delegate usage work.".to_string()),
                        tool_calls: vec![raw_tool_call],
                        usage: None,
                    },
                    Some(initial_task_usage),
                )
                .unwrap();
            registry
                .submit_pending_tool_approval_response(&task.id, true)
                .unwrap();

            let (event_tx, _event_rx) = mpsc::unbounded();
            let request = TuiBackgroundTurnContinuationRequest::new(task.id.clone());
            let result = continue_approved_background_turn_for_tui(
                &config,
                &mut session,
                &request,
                &event_tx,
                &CancelToken::new(),
                None,
            );

            assert_eq!(result.status, "success");
            let task_usage = registry
                .get(&task.id)
                .and_then(|task| task.usage)
                .expect("continued task usage");
            assert_eq!(task_usage.input_tokens, 160);
            assert_eq!(task_usage.output_tokens, 40);
            assert_eq!(task_usage.cache_tokens, 40);
            assert_ne!(task_usage, session.usage_totals());

            let duplicate = continue_approved_background_turn_for_tui(
                &config,
                &mut session,
                &request,
                &event_tx,
                &CancelToken::new(),
                None,
            );
            assert_eq!(duplicate.status, "failed");
            assert_eq!(registry.get(&task.id).unwrap().usage, Some(task_usage));
        });
    }

    fn task_update_matches(
        event: &TuiEvent,
        predicate: impl Fn(&BackgroundTaskSummary) -> bool,
    ) -> bool {
        match event {
            TuiEvent::WorkflowTasksUpdated { tasks } => tasks.iter().any(predicate),
            TuiEvent::WorkflowTaskUpdated { task } => predicate(task),
            _ => false,
        }
    }

    #[test]
    fn saved_workflow_args_parse_key_value_and_json_objects() {
        let value = parse_saved_workflow_args("target=src maxAgents=8 dryRun=true").unwrap();
        assert_eq!(value["target"], "src");
        assert_eq!(value["maxAgents"], 8);
        assert_eq!(value["dryRun"], true);

        let value = parse_saved_workflow_args(r#"{"target":"crates","maxAgents":4}"#).unwrap();
        assert_eq!(value["target"], "crates");
        assert_eq!(value["maxAgents"], 4);
    }

    #[test]
    fn borrowed_tui_session_reuses_runtime_owned_auxiliary_state() {
        let config = config();
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "borrowed session", None).unwrap();
        let charged = UsageTotals {
            input_tokens: 40,
            output_tokens: 10,
            cache_tokens: 20,
            estimated_cost_usd: 0.01,
        };

        {
            let mut session = TuiSession::new(&mut runtime);
            session.record_external_usage(charged);
        }
        let session = TuiSession::new(&mut runtime);

        assert_eq!(session.usage_totals(), charged);
    }

    #[test]
    fn tui_completed_tool_finish_updates_runtime_reducer_progress() {
        let config = config();
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "goal progress", None).expect("session");
        let request = tool_types::ToolRequest {
            id: "call-1".to_string(),
            name: tool_types::ToolName::plain("bash"),
            action: orca_core::approval_types::ActionKind::Shell,
            target: None,
            raw_arguments: None,
        };
        let result = tool_types::ToolResult::completed(&request, "ok".to_string(), false);

        {
            let session = TuiSession::new(&mut runtime);
            record_tui_goal_tool_finish(&session, "turn-1", &result);
        }

        let progress = runtime
            .thread_extensions()
            .get::<orca_runtime::goals::GoalToolProgressState>()
            .expect("completed TUI tool should update runtime goal progress");
        assert_eq!(progress.completed_tool_attempts(), 1);
        assert_eq!(progress.last_turn_id().as_deref(), Some("turn-1"));
        assert_eq!(progress.last_call_id().as_deref(), Some("call-1"));
    }

    #[test]
    fn tui_session_reuses_conversation_across_submits() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "first", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );
        run_agent_for_tui(
            &config,
            &mut session,
            "mock_history_echo",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let echoed = events.iter().find_map(|event| match event {
            TuiEvent::MessageDelta(text) if text.contains("Mock history users") => {
                Some(text.as_str())
            }
            _ => None,
        });
        assert!(
            echoed
                .unwrap_or_default()
                .contains("first prompt | mock_history_echo")
        );
    }

    #[test]
    fn tui_resume_drops_reasoning_only_assistant_turn() {
        let mut config = config();
        config.history_mode = HistoryMode::Resume("latest".to_string());
        let temp = tempfile::tempdir().expect("temp history dir");
        let cwd = temp.path().to_path_buf();
        config.cwd = Some(cwd.clone());
        let transcript_path = cwd.join("reasoning-only-tui.jsonl");
        std::fs::write(&transcript_path, "").expect("seed resumable history file");
        let transcript = orca_runtime::history::SessionTranscript {
            meta: orca_runtime::history::create_meta(
                &cwd,
                "deepseek",
                None,
                "resume reasoning-only history",
            ),
            messages: vec![
                orca_core::conversation::Message::user("first".to_string()),
                orca_core::conversation::Message::Assistant {
                    content: None,
                    reasoning_content: Some("synthetic private reasoning".to_string()),
                    tool_calls: vec![],
                    pinned: false,
                },
                orca_core::conversation::Message::user("second".to_string()),
            ],
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: None,
            plan: None,
            completion_status: None,
            completion_error: None,
            path: transcript_path,
        };

        let mut runtime = RuntimeThread::start_with_preloaded(
            &config,
            "resume reasoning-only history",
            Some(transcript),
        )
        .expect("TUI session resumes malformed legacy history");
        let session = TuiSession::new(&mut runtime);

        assert!(
            !session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::Assistant {
                        content: None,
                        tool_calls,
                        ..
                    } if tool_calls.is_empty()
                ))
        );
        assert!(
            session
                .conversation()
                .messages
                .iter()
                .any(|message| matches!(
                    message,
                    orca_core::conversation::Message::User { content, .. } if content == "second"
                ))
        );
    }

    #[test]
    fn tui_displays_final_assistant_content_without_stream_delta() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "silent", None).expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        assert_eq!(status, "success");
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::MessageDelta(text) if text.contains("Mock silent final response.")
            )
        }));
    }

    #[test]
    fn tui_turn_started_events_include_agent_task_lifecycle() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "task lifecycle", None).expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let turn = event_rx
            .try_iter()
            .find_map(|event| match event {
                TuiEvent::TurnStarted { turn, task } => task.map(|task| (turn, task)),
                _ => None,
            })
            .expect("turn started with task lifecycle");
        assert_eq!(turn.0, 1);
        assert_eq!(turn.1.kind, "agent");
        assert_eq!(turn.1.status, "running");
        assert_eq!(turn.1.turn, 1);
    }

    #[test]
    fn tui_turn_started_task_matches_main_session_task_registry() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "task identity", None).expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let events = event_rx.try_iter().collect::<Vec<_>>();
        let main_session_id = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::WorkflowTasksUpdated { tasks } => tasks
                    .iter()
                    .find(|task| task.task_type == TaskType::MainSession)
                    .map(|task| task.id.as_str()),
                TuiEvent::WorkflowTaskUpdated { task }
                    if task.task_type == TaskType::MainSession =>
                {
                    Some(task.id.as_str())
                }
                _ => None,
            })
            .expect("main session task update");
        let turn_task_id = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::TurnStarted {
                    task: Some(task), ..
                } => Some(task.id.as_str()),
                _ => None,
            })
            .expect("turn started task");

        assert_eq!(turn_task_id, main_session_id);
    }

    #[test]
    fn tui_main_turn_updates_main_session_task_registry() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session = RuntimeThread::start_with_preloaded(&config, "main session task", None)
            .expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let main_tasks = TuiSession::new(&mut session)
            .runtime_session()
            .task_registry()
            .list()
            .into_iter()
            .filter(|task| task.task_type == TaskType::MainSession)
            .collect::<Vec<_>>();
        assert_eq!(main_tasks.len(), 1);
        assert_eq!(main_tasks[0].status, TaskStatus::Completed);
        assert_eq!(main_tasks[0].description, "mock_silent_final");
        assert_eq!(main_tasks[0].agent_type.as_deref(), Some("main-session"));
        assert!(main_tasks[0].started_at_ms.is_some());
        assert!(main_tasks[0].completed_at_ms.is_some());

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| task_update_matches(event, |task| {
                    task.task_type == TaskType::MainSession
                        && task.status == TaskStatus::Running
                        && task.description == "mock_silent_final"
                }))
        );
        assert!(
            events
                .iter()
                .any(|event| task_update_matches(event, |task| {
                    task.task_type == TaskType::MainSession
                        && task.status == TaskStatus::Completed
                        && task.description == "mock_silent_final"
                }))
        );
    }

    #[test]
    fn tui_background_current_turn_marks_main_session_task() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "background turn", None).expect("session");

        action_tx
            .send(UserAction::BackgroundCurrentTurn)
            .expect("send background action");
        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let main_task = TuiSession::new(&mut session)
            .runtime_session()
            .task_registry()
            .list()
            .into_iter()
            .find(|task| task.task_type == TaskType::MainSession)
            .expect("main session task");
        assert!(main_task.is_backgrounded);

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| task_update_matches(event, |task| {
                    task.task_type == TaskType::MainSession
                        && task.status == TaskStatus::Running
                        && task.is_backgrounded
                }))
        );
    }

    #[test]
    fn background_poll_consumes_only_the_operation_scoped_request() {
        let config = config();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let mut runtime_events = EventFactory::new("background-poll".to_string());
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "background poll", None).expect("session");
        let session = TuiSession::new(&mut runtime);
        let task = session
            .runtime_session()
            .task_registry()
            .create_main_session("background poll".to_string());
        session
            .runtime_session()
            .task_registry()
            .mark_running(&task.id)
            .expect("running main session");
        let operation = crate::test_support::HostedOperationHarness::start();
        let controller = operation.controller();
        let control = operation.control();
        let mut is_backgrounded = false;

        poll_background_current_turn_for_tui(
            &session,
            &event_tx,
            &mut runtime_events,
            &control,
            &task.id,
            &mut is_backgrounded,
        );

        assert!(!is_backgrounded);
        assert!(controller.request_background_current());
        poll_background_current_turn_for_tui(
            &session,
            &event_tx,
            &mut runtime_events,
            &control,
            &task.id,
            &mut is_backgrounded,
        );
        assert!(is_backgrounded);
        assert!(!control.take_background_current());
    }

    #[test]
    fn tui_task_stop_can_stop_active_main_session_task() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session = RuntimeThread::start_with_preloaded(&config, "main session stop", None)
            .expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "task_stop_main_session",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "interrupted");
        let main_task = TuiSession::new(&mut session)
            .runtime_session()
            .task_registry()
            .list()
            .into_iter()
            .find(|task| task.task_type == TaskType::MainSession)
            .expect("main session task");
        assert_eq!(main_task.status, TaskStatus::Stopped);
        assert_eq!(main_task.description, "task_stop_main_session");
        assert!(main_task.completed_at_ms.is_some());

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::ToolCompleted { name, status, output, .. }
                if name == "task_stop"
                    && status == "completed"
                    && output.contains("Task stop requested")
            )
        }));
        assert!(
            events
                .iter()
                .any(|event| task_update_matches(event, |task| {
                    task.task_type == TaskType::MainSession
                        && task.status == TaskStatus::Stopped
                        && task.description == "task_stop_main_session"
                }))
        );
    }

    #[test]
    fn tui_task_stop_can_clear_approval_required_background_main_session() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session = RuntimeThread::start_with_preloaded(&config, "main session stop", None)
            .expect("session");
        let task = {
            let tui_session = TuiSession::new(&mut session);
            let registry = tui_session.runtime_session().task_registry();
            let task = registry.create_main_session("background approval".to_string());
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();
            registry
                .approval_required_for_tool(
                    &task.id,
                    "approval_required".to_string(),
                    Some("task_list".to_string()),
                )
                .unwrap();
            task
        };

        let status = run_agent_for_tui(
            &config,
            &mut session,
            &format!("task_stop {}", task.id),
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let stopped_task = TuiSession::new(&mut session)
            .runtime_session()
            .task_registry()
            .get(&task.id)
            .unwrap();
        assert_eq!(stopped_task.status, TaskStatus::Stopped);
        assert_eq!(stopped_task.result.as_deref(), Some("Task stopped"));

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::ToolCompleted { name, status, output, .. }
                if name == "task_stop"
                    && status == "completed"
                    && output.contains("Task stopped")
            )
        }));
        assert!(
            events
                .iter()
                .any(|event| task_update_matches(event, |task| {
                    task.task_type == TaskType::MainSession
                        && task.status == TaskStatus::Stopped
                        && task.description == "background approval"
                }))
        );
    }

    #[test]
    fn tui_tool_schema_exposes_goal_tool_only_for_goal_turns() {
        let config = config();
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "first", None).expect("session");
        let mut session = TuiSession::new(&mut runtime);
        session.replace_goal_context("goal instructions".to_string());

        let base_names = tui_tools_schema(session.mcp_registry(), &config.external_tools, false)
            .into_iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        let goal_names = tui_tools_schema(session.mcp_registry(), &config.external_tools, true)
            .into_iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();

        assert!(!base_names.contains(&"update_goal".to_string()));
        assert!(goal_names.contains(&"update_goal".to_string()));
    }

    #[test]
    fn tui_session_exposes_runtime_owned_workflow_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config();
        config.cwd = Some(temp.path().to_path_buf());
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "workflow state", None).expect("session");
        let session = TuiSession::new(&mut runtime);

        assert!(!session.runtime_session().has_active_workflows());
        let handle = session.runtime_session().task_registry().create_workflow(
            "run-1".to_string(),
            "demo".to_string(),
            "demo workflow".to_string(),
            1,
        );
        session
            .runtime_session()
            .task_registry()
            .mark_running(&handle.id)
            .expect("running");

        assert!(session.has_active_workflows());
    }

    #[test]
    fn tui_task_list_uses_runtime_task_registry() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "task_list", None).expect("session");
        let _task = {
            let tui_session = TuiSession::new(&mut session);
            let registry = tui_session.runtime_session().task_registry();
            let task = registry.create_workflow(
                "workflow-run-1".to_string(),
                "mock-workflow".to_string(),
                "demo workflow".to_string(),
                1,
            );
            registry
                .mark_running(&task.id)
                .expect("mark workflow running");
            task
        };

        run_agent_for_tui(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let task_list = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::ToolCompleted {
                    name,
                    status,
                    output,
                    ..
                } if name == "task_list" => Some((status.as_str(), output.as_str())),
                _ => None,
            })
            .expect("task_list tool completion");

        assert_eq!(
            task_list.0, "completed",
            "expected completed task_list, got {}",
            task_list.1
        );
        assert!(
            task_list.1.contains("demo workflow"),
            "expected runtime task output, got {}",
            task_list.1
        );
        assert!(
            !task_list
                .1
                .contains("task_list tool must be executed by the runtime"),
            "TUI must not route task_list through the placeholder executor"
        );
    }

    #[test]
    fn tui_task_list_marks_backgrounded_main_session_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "task_list", None).expect("session");
        let _task = {
            let tui_session = TuiSession::new(&mut session);
            let registry = tui_session.runtime_session().task_registry();
            let task = registry.create_main_session("backgrounded turn".to_string());
            registry
                .mark_running(&task.id)
                .expect("mark main session running");
            registry
                .mark_backgrounded(&task.id)
                .expect("mark main session backgrounded");
            task
        };

        run_agent_for_tui(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let output = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::ToolCompleted { name, output, .. } if name == "task_list" => Some(output),
                _ => None,
            })
            .expect("task_list tool completion");
        let value: serde_json::Value = serde_json::from_str(output).expect("task_list json");

        assert!(value["tasks"].as_array().unwrap().iter().any(|task| {
            task["task_type"] == "main_session"
                && task["subject"] == "backgrounded turn"
                && task["isBackgrounded"] == true
        }));
    }

    #[test]
    fn failed_workflow_notification_is_returned_after_tool_batch_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_controller, operation, control) = test_turn();
        let pending_notifications = PendingWorkflowNotifications::new();
        assert!(
            pending_notifications.push_unique(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification><status>failed</status></task-notification>"
                    .to_string(),
            })
        );
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "task_list", None).expect("session");
        let mut session = TuiSession::new(&mut runtime);
        let (_tasks, task_spawner) = test_task_supervisor();

        let result = run_agent_for_tui_with_notification_queue(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &control,
            operation.cancel_token(),
            false,
            None,
            true,
            Some(&pending_notifications),
            None,
            &task_spawner,
        );

        assert_eq!(result.status, "success");
        assert_eq!(
            result
                .continuation
                .as_ref()
                .map(|continuation| match continuation {
                    TuiAgentTurnContinuation::WorkflowNotification(notification) => {
                        (notification.id.as_str(), notification.prompt.as_str())
                    }
                }),
            Some((
                "notification-1",
                "<task-notification><status>failed</status></task-notification>"
            ))
        );
        assert!(pending_notifications.is_empty());
        assert!(event_rx.try_iter().any(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        }));
    }

    #[test]
    fn empty_failed_workflow_notification_queue_does_not_inject_after_tool_batch() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (_controller, operation, control) = test_turn();
        let pending_notifications = PendingWorkflowNotifications::new();
        let mut runtime =
            RuntimeThread::start_with_preloaded(&config, "task_list", None).expect("session");
        let mut session = TuiSession::new(&mut runtime);
        let (_tasks, task_spawner) = test_task_supervisor();

        let result = run_agent_for_tui_with_notification_queue(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &control,
            operation.cancel_token(),
            false,
            None,
            true,
            Some(&pending_notifications),
            None,
            &task_spawner,
        );

        assert_eq!(result.status, "success");
        assert!(result.continuation.is_none());
    }

    #[test]
    fn tui_workflow_tool_launches_runtime_instead_of_placeholder_executor() {
        if !WorkflowHost::node_available() {
            return;
        }

        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "workflow inline", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "workflow inline",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let mut events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !workflow_runtime_events_complete(&events) {
            if let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
                events.push(event);
            }
        }
        let workflow = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::ToolCompleted {
                    name,
                    status,
                    output,
                    ..
                } if name == "Workflow" => Some((status.as_str(), output.as_str())),
                _ => None,
            })
            .expect("workflow tool completion");

        assert_eq!(workflow.0, "completed");
        assert!(
            workflow.1.contains("\"status\":\"async_launched\""),
            "expected async workflow launch output, got {}",
            workflow.1
        );
        assert!(
            !workflow
                .1
                .contains("Workflow must be executed by the runtime controller"),
            "TUI must not route Workflow through the placeholder executor"
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| task.workflow_run_id.is_some())
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| {
                    task.workflow_progress
                        .map(|progress| {
                            progress.total_agents > 0
                                && progress.completed_agents + progress.failed_agents > 0
                        })
                        .unwrap_or(false)
                })
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowNotification { prompt, status, summary, .. }
                if prompt.contains("<task-notification>")
                    && prompt.contains("<status>completed</status>")
                    && *status == "completed"
                    && summary.contains("mock-workflow")
            )
        }));
    }

    fn workflow_runtime_events_complete(events: &[TuiEvent]) -> bool {
        let has_notification = events
            .iter()
            .any(|event| matches!(event, TuiEvent::WorkflowNotification { .. }));
        let has_terminal_progress = events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| {
                    task.workflow_progress
                        .map(|progress| {
                            progress.total_agents > 0
                                && progress.completed_agents + progress.failed_agents > 0
                        })
                        .unwrap_or(false)
                })
            )
        });
        has_notification && has_terminal_progress
    }

    #[test]
    fn tui_workflow_draft_tool_uses_runtime_draft_store() {
        let mut config = full_auto_config();
        config.output_format = OutputFormat::Jsonl;
        let temp = tempfile::tempdir().unwrap();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "workflow draft", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "workflow draft",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let draft_tool = events.iter().find_map(|event| match event {
            TuiEvent::ToolCompleted {
                name,
                status,
                output,
                ..
            } if name == "WorkflowDraft" => Some((status.as_str(), output.as_str())),
            _ => None,
        });
        let (status, output) = draft_tool.expect("workflow draft tool completed");
        assert_eq!(status, "completed");
        assert!(output.contains("\"draftId\""));
        assert!(
            !output.contains("WorkflowDraft must be executed by the runtime controller"),
            "TUI must not route WorkflowDraft through the placeholder executor"
        );
    }

    #[test]
    fn tui_streaming_bash_observes_turn_cancel() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let turn_cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "bash", None).expect("session");

        let handle = std::thread::spawn(move || {
            run_agent_for_tui(
                &config,
                &mut session,
                "bash printf 'before\\n'; sleep 5; printf after",
                &event_tx,
                &action_rx,
                &turn_cancel,
                false,
            )
        });

        let start = Instant::now();
        let mut observed_events = Vec::new();
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("TUI event before timeout");
            let saw_streaming_output = matches!(&event, TuiEvent::ToolOutputDelta { chunk, .. } if chunk.contains("before"));
            if let TuiEvent::SessionCompleted { status } = &event {
                panic!("session completed before streaming output: {status}");
            }
            observed_events.push(event);
            if saw_streaming_output {
                action_tx
                    .send(UserAction::Interrupt)
                    .expect("send turn interrupt");
                break;
            }
        }

        let status = handle.join().expect("turn thread joined");
        observed_events.extend(event_rx.try_iter());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancelled TUI streaming bash should not wait for shell timeout"
        );
        assert_eq!(status, "interrupted");
        assert!(observed_events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::ToolCompleted { name, status, .. }
                    if name == "bash" && status == "cancelled"
            )
        }));
    }

    #[test]
    fn tui_main_session_stop_closes_every_sibling_tool_row() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session = RuntimeThread::start_with_preloaded(&config, "task stop siblings", None)
            .expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "task_stop_main_session_with_siblings",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "interrupted");
        let session = TuiSession::new(&mut session);
        let assistant_call_ids = session
            .conversation()
            .messages
            .iter()
            .flat_map(|message| match message {
                orca_core::conversation::Message::Assistant { tool_calls, .. } => tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.as_str())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let terminal_ids = session
            .conversation()
            .messages
            .iter()
            .filter_map(|message| match message {
                orca_core::conversation::Message::Tool { tool_call_id, .. } => {
                    Some(tool_call_id.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistant_call_ids, terminal_ids);
        assert_eq!(terminal_ids.len(), 4);

        let completed = event_rx
            .try_iter()
            .filter_map(|event| match event {
                TuiEvent::ToolCompleted { id, status, .. } => Some((id, status)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 4);
        assert_eq!(
            completed
                .iter()
                .filter(|(_, status)| status == "cancelled")
                .count(),
            2
        );
        assert!(
            completed
                .iter()
                .all(|(_, status)| !matches!(status.as_str(), "running" | "receiving"))
        );
    }

    #[test]
    fn tui_approval_action_rejects_caller_supplied_read_for_shell() {
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Read,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let registry = McpRegistry::default();

        assert_eq!(
            canonical_action_for_tool(&request, &registry, &[]),
            orca_core::approval_types::ActionKind::Shell
        );
    }

    #[test]
    fn background_provider_completion_stores_pending_provider_response_for_approval() {
        let registry = TaskRegistry::new("session-background-response".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let (event_tx, event_rx) = mpsc::unbounded();
        let (provider_tx, provider_rx) = mpsc::unbounded();
        let tool_request = tool_types::ToolRequest {
            id: "mock-tool-1".to_string(),
            name: tool_types::ToolName::TaskList,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let _background_tasks = spawn_test_background_provider_completion(
            provider_rx,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: Some(orca_core::model::FLASH_MODEL.to_string()),
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-response".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
        );
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: vec![ProviderStep::ToolCall(tool_request)],
                assistant_content: Some("I need task_list.".to_string()),
                assistant_reasoning: Some("Need task state.".to_string()),
                tool_calls: Vec::new(),
                usage: Some(orca_core::provider_types::Usage {
                    input_tokens: 80,
                    output_tokens: 20,
                    cache_tokens: 5,
                }),
            }))
            .unwrap();

        loop {
            match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                event
                    if task_update_matches(&event, |task| {
                        task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                            && task.pending_tool_call.is_some()
                    }) =>
                {
                    break;
                }
                _ => {}
            }
        }

        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();
        let continuation =
            orca_runtime::background_turn::take_approved_background_turn_continuation(
                &registry, &task.id,
            )
            .unwrap()
            .expect("approved background continuation");

        assert_eq!(
            continuation.response.assistant_content.as_deref(),
            Some("I need task_list.")
        );
        assert_eq!(
            continuation.preapproved_tool_call_id.as_deref(),
            Some("mock-tool-1")
        );
        assert_eq!(continuation.response.steps.len(), 1);
        let usage = registry
            .get(&task.id)
            .and_then(|record| record.usage)
            .expect("approval-required response usage");
        assert_eq!(usage.input_tokens, 80);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_tokens, 5);
        assert!(usage.estimated_cost_usd > 0.0);
    }

    #[test]
    fn background_task_stop_cancels_and_joins_provider_once() {
        let registry = TaskRegistry::new("session-background-stop".to_string());
        let task = registry.create_main_session("Stop provider".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (provider_task, provider_joined) = cancellable_test_provider_stream_task();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (mut supervisor, spawner) = test_task_supervisor();
        spawn_background_provider_completion(
            provider_task,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-stop".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
            &spawner,
        )
        .expect("background provider admitted");

        registry.request_stop(&task.id).expect("stop requested");
        let mut stopped_updates = 0;
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("stopped task update");
            if task_update_matches(&event, |summary| summary.status == TaskStatus::Stopped) {
                stopped_updates += 1;
                break;
            }
        }
        supervisor.shutdown().expect("background tasks joined");
        stopped_updates += event_rx
            .try_iter()
            .filter(|event| {
                task_update_matches(event, |summary| summary.status == TaskStatus::Stopped)
            })
            .count();

        assert!(provider_joined.load(Ordering::SeqCst));
        assert_eq!(registry.get(&task.id).unwrap().status, TaskStatus::Stopped);
        assert_eq!(stopped_updates, 1);
    }

    #[test]
    fn background_stop_preserves_already_queued_provider_usage() {
        let registry = TaskRegistry::new("session-background-stop-usage".to_string());
        let task = registry.create_main_session("Stop completed provider".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry.request_stop(&task.id).expect("stop requested");
        let (provider_tx, provider_rx) = mpsc::bounded(1);
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("done".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: Some(orca_core::provider_types::Usage {
                    input_tokens: 90,
                    output_tokens: 12,
                    cache_tokens: 7,
                }),
            }))
            .expect("queued provider terminal");
        let provider_task = ProviderStreamTask {
            receiver: Some(provider_rx),
            cancel: CancelToken::new(),
            handle: Some(thread::spawn(|| {})),
        };
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (mut supervisor, spawner) = test_task_supervisor();

        spawn_background_provider_completion(
            provider_task,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: Some(orca_core::model::FLASH_MODEL.to_string()),
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-stop-usage".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
            &spawner,
        )
        .expect("background provider admitted");
        supervisor.shutdown().expect("background tasks joined");

        let record = registry.get(&task.id).expect("stopped task");
        assert_eq!(record.status, TaskStatus::Stopped);
        let usage = record.usage.expect("already incurred usage retained");
        assert_eq!(usage.input_tokens, 90);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.cache_tokens, 7);
    }

    #[test]
    fn stop_wins_while_background_completion_is_joining_provider() {
        let registry = TaskRegistry::new("session-background-stop-race".to_string());
        let task = registry.create_main_session("Stop completed provider".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (provider_tx, provider_rx) = mpsc::bounded(1);
        let (release_tx, release_rx) = mpsc::bounded(1);
        let provider_task = ProviderStreamTask {
            receiver: Some(provider_rx),
            cancel: CancelToken::new(),
            handle: Some(thread::spawn(move || {
                let _ = release_rx.recv();
            })),
        };
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_supervisor, spawner) = test_task_supervisor();
        spawn_background_provider_completion(
            provider_task,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-stop-race".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
            &spawner,
        )
        .expect("background provider admitted");
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("done".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            }))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !provider_tx.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            provider_tx.is_empty(),
            "completion did not receive provider terminal"
        );

        registry.request_stop(&task.id).expect("stop requested");
        release_tx.send(()).expect("release provider join");
        loop {
            let event = event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("terminal task update");
            if task_update_matches(&event, |summary| {
                matches!(summary.status, TaskStatus::Completed | TaskStatus::Stopped)
            }) {
                break;
            }
        }

        assert_eq!(registry.get(&task.id).unwrap().status, TaskStatus::Stopped);
    }

    #[test]
    fn supervisor_shutdown_cancels_and_joins_background_provider() {
        let registry = TaskRegistry::new("session-background-shutdown".to_string());
        let task = registry.create_main_session("Shutdown provider".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (provider_task, provider_joined) = cancellable_test_provider_stream_task();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (mut supervisor, spawner) = test_task_supervisor();
        spawn_background_provider_completion(
            provider_task,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-shutdown".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
            &spawner,
        )
        .expect("background provider admitted");

        supervisor.shutdown().expect("background tasks joined");

        assert!(provider_joined.load(Ordering::SeqCst));
        assert_eq!(registry.get(&task.id).unwrap().status, TaskStatus::Stopped);
    }

    #[test]
    fn stop_wins_when_supervisor_rejects_background_handoff() {
        let registry = TaskRegistry::new("session-background-admission-stop".to_string());
        let task = registry.create_main_session("Reject provider handoff".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry.request_stop(&task.id).expect("stop requested");
        let (provider_task, provider_joined) = cancellable_test_provider_stream_task();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (completion_tx, completion_rx) = mpsc::bounded(1);
        let supervisor = TuiTaskSupervisor::new(0);

        let error = spawn_background_provider_completion(
            provider_task,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-admission-stop".to_string(),
                task_id: task.id.clone(),
                completion_handler: Some(Box::new(move |completion| {
                    completion_tx.send(completion).expect("completion callback");
                })),
            },
            &supervisor.spawner(),
        )
        .expect_err("zero-capacity supervisor rejects handoff");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(provider_joined.load(Ordering::SeqCst));
        assert_eq!(registry.get(&task.id).unwrap().status, TaskStatus::Stopped);
        assert_eq!(
            completion_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("one completion callback")
                .usage,
            None
        );
        assert!(completion_rx.try_recv().is_err());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "cancelled")
        }));
        assert!(!events.iter().any(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "failed")
        }));
    }

    #[test]
    fn foregrounded_background_provider_completion_forwards_future_message_deltas() {
        let registry = TaskRegistry::new("session-background-foreground".to_string());
        let task = registry.create_main_session("Long answer".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let (event_tx, event_rx) = mpsc::unbounded();
        let (provider_tx, provider_rx) = mpsc::unbounded();
        let _background_tasks = spawn_test_background_provider_completion(
            provider_rx,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-foreground".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
        );
        provider_tx
            .send(ProviderStreamEvent::Step(ProviderStep::MessageDelta(
                "still hidden".to_string(),
            )))
            .unwrap();
        assert!(event_rx.recv_timeout(Duration::from_millis(100)).is_err());

        registry.mark_foregrounded(&task.id).unwrap();
        provider_tx
            .send(ProviderStreamEvent::Step(ProviderStep::MessageDelta(
                "now visible".to_string(),
            )))
            .unwrap();

        let visible_deltas = collect_message_deltas(&event_rx, 2);
        assert_eq!(visible_deltas, vec!["still hidden", "now visible"]);

        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("now visible".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: Some(orca_core::provider_types::Usage {
                    input_tokens: 60,
                    output_tokens: 15,
                    cache_tokens: 4,
                }),
            }))
            .unwrap();
        let completed_status = loop {
            match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                TuiEvent::SessionCompleted { status } => break status,
                _ => {}
            }
        };
        assert_eq!(completed_status, "success");
        let usage = registry
            .get(&task.id)
            .and_then(|record| record.usage)
            .expect("successful background response usage");
        assert_eq!(usage.input_tokens, 60);
        assert_eq!(usage.output_tokens, 15);
        assert_eq!(usage.cache_tokens, 4);
        assert!(usage.estimated_cost_usd > 0.0);
    }

    #[test]
    fn foregrounded_background_provider_failure_emits_error_before_completion() {
        let registry = TaskRegistry::new("session-background-failure".to_string());
        let task = registry.create_main_session("Provider failure".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let (event_tx, event_rx) = mpsc::unbounded();
        let (provider_tx, provider_rx) = mpsc::unbounded();
        let _background_tasks = spawn_test_background_provider_completion(
            provider_rx,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-failure".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
        );
        registry.mark_foregrounded(&task.id).unwrap();
        let provider_error = "DeepSeek provider error: empty assistant response";
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: vec![ProviderStep::Error(provider_error.to_string())],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            }))
            .unwrap();

        let mut observed_error = false;
        loop {
            match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                TuiEvent::Error(message) => {
                    assert_eq!(message, provider_error);
                    observed_error = true;
                }
                TuiEvent::SessionCompleted { status } => {
                    assert_eq!(status, "failed");
                    assert!(
                        observed_error,
                        "provider error must be emitted before session completion"
                    );
                    break;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn background_provider_invokes_completion_handler_exactly_once() {
        let registry = TaskRegistry::new("session-background-goal-usage".to_string());
        let task = registry.create_main_session("Goal response".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let (event_tx, _event_rx) = mpsc::unbounded();
        let (provider_tx, provider_rx) = mpsc::unbounded();
        let (completion_tx, completion_rx) = mpsc::unbounded();
        let _background_tasks = spawn_test_background_provider_completion(
            provider_rx,
            BackgroundProviderCompletionContext {
                task_registry: registry,
                history_writer: None,
                model: Some(orca_core::model::FLASH_MODEL.to_string()),
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-goal-usage".to_string(),
                task_id: task.id,
                completion_handler: Some(Box::new(move |completion| {
                    completion_tx.send(completion).unwrap();
                })),
            },
        );
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("done".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: Some(orca_core::provider_types::Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_tokens: 10,
                }),
            }))
            .unwrap();

        let completion = completion_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("background completion callback");
        let usage = completion.usage.expect("background completion usage");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(usage.cache_tokens, 10);
        assert!(usage.estimated_cost_usd > 0.0);
        assert!(matches!(
            completion_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn background_provider_usage_enforces_shared_budget() {
        let registry = TaskRegistry::new("session-background-budget".to_string());
        let task = registry.create_main_session("Budgeted response".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let usage_ledger = TuiUsageLedger::default();

        let (event_tx, event_rx) = mpsc::unbounded();
        let (provider_tx, provider_rx) = mpsc::unbounded();
        let _background_tasks = spawn_test_background_provider_completion(
            provider_rx,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: Some(orca_core::model::FLASH_MODEL.to_string()),
                usage_ledger: usage_ledger.clone(),
                budget_admission: None,
                max_budget_usd: Some(0.0),
                event_tx,
                run_id: "run-background-budget".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
        );
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("done".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: Some(orca_core::provider_types::Usage {
                    input_tokens: 100,
                    output_tokens: 25,
                    cache_tokens: 8,
                }),
            }))
            .unwrap();

        let mut emitted_totals = None;
        loop {
            match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                TuiEvent::UsageUpdated(totals) => emitted_totals = Some(totals),
                event
                    if task_update_matches(&event, |task| {
                        task.status == TaskStatus::Failed
                            && task
                                .error
                                .as_deref()
                                .is_some_and(|error| error.contains("budget exhausted"))
                    }) =>
                {
                    break;
                }
                _ => {}
            }
        }

        let totals = emitted_totals.expect("background usage event");
        assert_eq!(totals, usage_ledger.totals());
        assert_eq!(totals.input_tokens, 100);
        assert_eq!(totals.output_tokens, 25);
        assert_eq!(totals.cache_tokens, 8);
        assert!(totals.estimated_cost_usd > 0.0);
        let task_usage = registry
            .get(&task.id)
            .and_then(|record| record.usage)
            .expect("budget-exhausted task usage");
        assert_eq!(task_usage.input_tokens, 100);
        assert_eq!(task_usage.output_tokens, 25);
        assert_eq!(task_usage.cache_tokens, 8);
    }

    #[test]
    fn sync_subagent_usage_is_persisted_emitted_and_budget_checked_immediately() {
        with_isolated_orca_home(|_| {
            let mut config = full_auto_config();
            config.history_mode = HistoryMode::Record;
            config.max_budget_usd = Some(0.0);
            let mut session = RuntimeThread::start_with_preloaded(&config, "budgeted child", None)
                .expect("session");
            let session_id = {
                let session = TuiSession::new(&mut session);
                session.session_id().expect("session id").to_string()
            };
            let (event_tx, event_rx) = mpsc::unbounded();
            let (_action_tx, action_rx) = mpsc::unbounded();

            let status = run_agent_for_tui(
                &config,
                &mut session,
                "subagent schema_ok",
                &event_tx,
                &action_rx,
                &CancelToken::new(),
                false,
            );

            assert_eq!(status, "budget_exhausted");
            let totals = TuiSession::new(&mut session).usage_totals();
            assert_eq!(totals.input_tokens, 120);
            assert_eq!(totals.output_tokens, 30);
            assert_eq!(totals.cache_tokens, 10);
            assert!(totals.estimated_cost_usd > 0.0);

            let events: Vec<_> = event_rx.try_iter().collect();
            assert!(
                events.iter().any(
                    |event| matches!(event, TuiEvent::UsageUpdated(usage) if *usage == totals)
                )
            );
            assert!(events.iter().any(
                |event| matches!(event, TuiEvent::Error(error) if error.contains("budget exhausted"))
            ));
            assert!(events.iter().any(|event| {
                matches!(
                    event,
                    TuiEvent::SubagentCompleted { status, error, .. }
                        if status == "budget_exhausted"
                            && error
                                .as_deref()
                                .is_some_and(|error| error.contains("budget exhausted"))
                )
            }));
            assert!(events.iter().any(|event| {
                matches!(event, TuiEvent::SessionCompleted { status } if status == "budget_exhausted")
            }));

            let transcript =
                orca_runtime::history::load_session(&session_id).expect("parent transcript");
            assert_eq!(transcript.usage, Some(totals));
            let persisted = std::fs::read_to_string(&transcript.path).expect("session JSONL");
            let usage_record = persisted
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .rfind(|record| record["type"] == "session.usage")
                .expect("immediate parent session usage record");
            assert_eq!(usage_record["input_tokens"], 120);
            assert_eq!(usage_record["output_tokens"], 30);
            assert_eq!(usage_record["cache_tokens"], 10);
            let assistant_index = transcript
                .messages
                .iter()
                .position(|message| matches!(message, orca_core::conversation::Message::Assistant { tool_calls, .. } if tool_calls.len() == 1))
                .expect("persisted assistant subagent call");
            let tool_call_id = match &transcript.messages[assistant_index] {
                orca_core::conversation::Message::Assistant { tool_calls, .. } => &tool_calls[0].id,
                _ => unreachable!(),
            };
            assert!(matches!(
                transcript.messages.get(assistant_index + 1),
                Some(orca_core::conversation::Message::Tool { tool_call_id: persisted_id, .. })
                    if persisted_id == tool_call_id
            ));
            assert!(
                !TuiSession::new(&mut session)
                    .conversation()
                    .messages
                    .iter()
                    .any(|message| {
                        message.content_str().is_some_and(|content| {
                            content == "Mock completed after tool execution."
                        })
                    }),
                "budget enforcement must stop before another parent provider request"
            );
        });
    }

    #[test]
    fn tui_sync_subagent_enforces_parent_remaining_budget_inside_child() {
        let mut config = full_auto_config();
        config.max_budget_usd = Some(0.25);
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "budgeted child", None).expect("session");
        TuiSession::new(&mut session)
            .usage_ledger()
            .add(UsageTotals {
                input_tokens: 100,
                output_tokens: 25,
                cache_tokens: 8,
                estimated_cost_usd: 0.25,
            });
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "subagent schema_ok",
            &event_tx,
            &action_rx,
            &CancelToken::new(),
            false,
        );

        assert_eq!(status, "budget_exhausted");
        assert!(event_rx.try_iter().any(|event| {
            matches!(
                event,
                TuiEvent::SubagentCompleted { status, error, .. }
                    if status == "budget_exhausted"
                        && error
                            .as_deref()
                            .is_some_and(|error| error.contains("limit $0.000000"))
            )
        }));
    }

    #[test]
    fn tui_rejects_next_turn_after_background_budget_exhaustion() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config();
        config.cwd = Some(temp.path().to_path_buf());
        config.max_budget_usd = Some(0.5);
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "budgeted turn", None).unwrap();
        TuiSession::new(&mut session)
            .usage_ledger()
            .add(orca_core::cost_types::UsageTotals {
                input_tokens: 100,
                output_tokens: 25,
                cache_tokens: 8,
                estimated_cost_usd: 0.75,
            });
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "must not call provider",
            &event_tx,
            &action_rx,
            &CancelToken::new(),
            false,
        );

        assert_eq!(status, "budget_exhausted");
        let session = TuiSession::new(&mut session);
        assert!(
            !session
                .conversation()
                .messages
                .iter()
                .any(|message| message.content_str() == Some("must not call provider")),
            "a budget-rejected prompt must not enter resumable conversation history"
        );
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::Error(error) if error.contains("budget exhausted"))
        ));
        let task = session
            .task_registry()
            .list()
            .into_iter()
            .find(|task| task.task_type == TaskType::MainSession)
            .expect("budget-exhausted main task");
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(
            task.error
                .as_deref()
                .is_some_and(|error| error.contains("budget exhausted"))
        );
    }

    #[test]
    fn foregrounded_background_completion_replays_buffered_message_deltas() {
        let registry = TaskRegistry::new("session-background-replay".to_string());
        let task = registry.create_main_session("Long answer".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let (event_tx, event_rx) = mpsc::unbounded();
        let (provider_tx, provider_rx) = mpsc::unbounded();
        let _background_tasks = spawn_test_background_provider_completion(
            provider_rx,
            BackgroundProviderCompletionContext {
                task_registry: registry.clone(),
                history_writer: None,
                model: None,
                usage_ledger: TuiUsageLedger::default(),
                budget_admission: None,
                max_budget_usd: None,
                event_tx,
                run_id: "run-background-replay".to_string(),
                task_id: task.id.clone(),
                completion_handler: None,
            },
        );
        provider_tx
            .send(ProviderStreamEvent::Step(ProviderStep::MessageDelta(
                "buffered while hidden".to_string(),
            )))
            .unwrap();
        assert!(event_rx.recv_timeout(Duration::from_millis(100)).is_err());

        registry.mark_foregrounded(&task.id).unwrap();
        provider_tx
            .send(ProviderStreamEvent::Done(ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("buffered while hidden".to_string()),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            }))
            .unwrap();

        let replayed_delta = collect_message_deltas(&event_rx, 1);
        assert_eq!(replayed_delta, vec!["buffered while hidden"]);
    }

    fn collect_message_deltas(event_rx: &mpsc::Receiver<TuiEvent>, count: usize) -> Vec<String> {
        let mut deltas = Vec::new();
        while deltas.len() < count {
            match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                TuiEvent::MessageDelta(text) => deltas.push(text),
                _ => {}
            }
        }
        deltas
    }

    #[test]
    fn tui_tool_approval_uses_runtime_handler_before_execution() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (controller, _operation, control) = test_turn();
        let responder = std::thread::spawn({
            let controller = controller.clone();
            move || match event_rx.recv().expect("approval event") {
                TuiEvent::ApprovalNeeded {
                    key,
                    tool,
                    target,
                    preview,
                } => {
                    assert_eq!(tool, "bash");
                    assert_eq!(target.as_deref(), Some("printf approved"));
                    assert_eq!(preview.as_deref(), Some("$ printf approved"));
                    controller
                        .broker()
                        .respond(&key, crate::types::TuiInteractionResponse::Approval(true))
                        .expect("send approval");
                }
                event => panic!("expected approval event, got {event:?}"),
            }
        });
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("printf approved".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "printf approved" }).to_string()),
        };

        let (should_stop, result, _) = execute_tool_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &control,
            None,
            0,
            Some("approval-session"),
            None,
            &ApprovalPolicy::new(config.approval_mode),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &HookRunner::default(),
            None,
            &mut orca_runtime::lifecycle::TurnPermissionOverlay::default(),
            &CancelToken::new(),
        );

        responder.join().expect("approval responder");
        assert!(!should_stop);
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("approved"));
    }

    #[test]
    fn tui_tool_approval_cancel_returns_denied_result() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (controller, _operation, control) = test_turn();
        let responder = std::thread::spawn({
            let controller = controller.clone();
            move || {
                assert!(matches!(
                    event_rx.recv().expect("approval event"),
                    TuiEvent::ApprovalNeeded { .. }
                ));
                controller.interrupt_current();
            }
        });
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("printf denied".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "printf denied" }).to_string()),
        };

        let (should_stop, result, _) = execute_tool_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &control,
            None,
            0,
            Some("approval-session"),
            None,
            &ApprovalPolicy::new(config.approval_mode),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &HookRunner::default(),
            None,
            &mut orca_runtime::lifecycle::TurnPermissionOverlay::default(),
            &CancelToken::new(),
        );

        responder.join().expect("cancel responder");
        assert!(should_stop);
        assert_eq!(result.status, tool_types::ToolStatus::Denied);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("interrupted"))
        );
    }

    #[test]
    fn tui_session_backtracks_last_user_before_next_submit() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "first", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );
        run_agent_for_tui(
            &config,
            &mut session,
            "second prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(
            TuiSession::new(&mut session).backtrack_last_user(),
            Some("second prompt".to_string())
        );

        run_agent_for_tui(
            &config,
            &mut session,
            "mock_history_echo",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let echoed = events.iter().rev().find_map(|event| match event {
            TuiEvent::MessageDelta(text) if text.contains("Mock history users") => {
                Some(text.as_str())
            }
            _ => None,
        });
        let echoed = echoed.unwrap_or_default();
        assert!(echoed.contains("first prompt | mock_history_echo"));
        assert!(!echoed.contains("second prompt"));
    }

    #[test]
    fn tui_workflow_notification_turn_is_not_backtrack_target() {
        let config = config();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "first", None).expect("session");
        let (_tasks, task_spawner) = test_task_supervisor();

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );
        let (_controller, operation, control) = test_turn();
        {
            let mut tui_session = TuiSession::new(&mut session);
            run_agent_for_tui_with_notification_queue(
                &config,
                &mut tui_session,
                "<task-notification>mock_history_echo</task-notification>",
                &event_tx,
                &control,
                operation.cancel_token(),
                false,
                Some("Workflow notification notification-1"),
                false,
                None,
                None,
                &task_spawner,
            );

            assert_eq!(
                tui_session.backtrack_last_user(),
                Some("first prompt".to_string())
            );
        }
    }

    #[test]
    fn tui_request_user_input_waits_for_answer_and_continues() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "ask", None).expect("session");

        let responder_tx = action_tx.clone();
        let responder = std::thread::spawn(move || {
            loop {
                match event_rx.recv().expect("event") {
                    TuiEvent::UserInputRequested { key, question, .. } => {
                        assert_eq!(question, "Continue?");
                        responder_tx
                            .send(UserAction::RespondToInteraction {
                                key,
                                response: crate::types::TuiInteractionResponse::UserInput(
                                    "yes".to_string(),
                                ),
                            })
                            .expect("send answer");
                        break;
                    }
                    TuiEvent::SessionCompleted { status } => {
                        panic!("completed before user input request: {status}");
                    }
                    _ => {}
                }
            }
        });

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "ask Continue?",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        responder.join().expect("responder joined");
        assert_eq!(status, "success");
    }

    #[test]
    fn tui_request_user_input_cancel_stops_turn() {
        let config = config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "ask", None).expect("session");

        let responder = std::thread::spawn(move || {
            loop {
                match event_rx.recv().expect("event") {
                    TuiEvent::UserInputRequested { .. } => {
                        action_tx
                            .send(UserAction::Interrupt)
                            .expect("send interrupt");
                        break;
                    }
                    TuiEvent::SessionCompleted { status } => {
                        panic!("completed before user input request: {status}");
                    }
                    _ => {}
                }
            }
        });

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "ask Continue?",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        responder.join().expect("responder joined");
        assert_eq!(status, "interrupted");
    }

    #[test]
    fn tui_child_agent_recovers_from_invalid_tool_arguments() {
        let config = full_auto_config();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let (_controller, _operation, control) = test_turn();

        let (child, _child_cost_tracker) = run_child_agent_for_tui_silent(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            "bad_plan_then_fix",
            None,
            1,
            &SubagentType::General,
            &instructions,
            &memory,
            &hooks,
            None,
            &control,
        );

        assert_eq!(child.status, RunStatus::Success);
        assert!(
            child
                .final_message
                .as_deref()
                .unwrap_or_default()
                .contains("Mock completed after fixing malformed tool arguments")
        );
    }

    #[test]
    fn tui_main_agent_recovers_from_unknown_tool_call() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (_action_tx, action_rx) = mpsc::unbounded();
        let cancel = CancelToken::new();
        let mut session =
            RuntimeThread::start_with_preloaded(&config, "unknown tool", None).expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "unknown_tool_then_fix",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let session = TuiSession::new(&mut session);
        let unknown_tool_result_index = session
            .conversation()
            .messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    orca_core::conversation::Message::Tool { content, .. }
                        if content.contains("unknown tool: wc -l")
                )
            })
            .expect("conversation should record the unknown tool validation failure");
        let corrected_assistant_index = session
            .conversation()
            .messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    orca_core::conversation::Message::Assistant {
                        content: Some(content),
                        ..
                    } if content.contains("Mock completed after correcting unknown tool call")
                )
            })
            .expect("conversation should record the corrected assistant response");
        assert!(corrected_assistant_index > unknown_tool_result_index);
    }

    #[test]
    fn tui_subagent_batch_records_child_failure_without_stopping_batch() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let (_controller, _operation, control) = test_turn();
        let failing = tool_types::ToolRequest {
            id: "subagent-failing".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("failing child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "failing child",
                    "prompt": "mock_fail"
                })
                .to_string(),
            ),
        };
        let succeeding = tool_types::ToolRequest {
            id: "subagent-succeeding".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("succeeding child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "succeeding child",
                    "prompt": "simple audit"
                })
                .to_string(),
            ),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[failing, succeeding],
            &event_tx,
            &control,
            0,
            &instructions,
            &memory,
            &McpRegistry::default(),
            &hooks,
            None,
        );

        assert_eq!(results.len(), 2);
        assert!(!results[0].0, "child failure should not stop parent batch");
        assert_eq!(results[0].1.status, tool_types::ToolStatus::Failed);
        assert!(!results[1].0);
        assert_eq!(results[1].1.status, tool_types::ToolStatus::Completed);
    }

    #[test]
    fn tui_subagent_batch_rejects_malformed_arguments_before_starting_child() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-malformed-subagent".to_string());
        let (_controller, _operation, control) = test_turn();
        let request = tool_types::ToolRequest {
            id: "subagent-malformed".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: None,
            raw_arguments: Some("{\"description\":\"broken".to_string()),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[request],
            &event_tx,
            &control,
            0,
            &instructions,
            &memory,
            &McpRegistry::default(),
            &hooks,
            Some(&registry),
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.status, tool_types::ToolStatus::Failed);
        assert!(
            results[0]
                .1
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("arguments are not valid JSON")
        );
        assert!(
            registry.list().is_empty(),
            "schema-invalid subagent arguments must not create a child task"
        );
        assert!(
            event_rx
                .try_iter()
                .all(|event| !matches!(event, TuiEvent::SubagentProgress { .. })),
            "schema-invalid subagent arguments must not run a child agent"
        );
    }

    #[test]
    fn tui_subagent_batch_emits_child_activity_progress() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let (_controller, _operation, control) = test_turn();
        let request = tool_types::ToolRequest {
            id: "subagent-progress".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("child progress".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "child progress",
                    "prompt": "bash echo child"
                })
                .to_string(),
            ),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[request],
            &event_tx,
            &control,
            0,
            &instructions,
            &memory,
            &McpRegistry::default(),
            &hooks,
            None,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.status, tool_types::ToolStatus::Completed);
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::SubagentProgress { id, activity, turn, .. }
                if id == "subagent-progress"
                    && activity.contains("bash")
                    && *turn == Some(1)
            )
        }));
    }

    #[test]
    fn tui_sync_subagent_batch_updates_task_registry_activity() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::unbounded();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-sync-progress".to_string());
        let (_controller, _operation, control) = test_turn();
        let request = tool_types::ToolRequest {
            id: "subagent-sync-progress".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("sync progress child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "sync progress child",
                    "prompt": "bash echo child"
                })
                .to_string(),
            ),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[request],
            &event_tx,
            &control,
            0,
            &instructions,
            &memory,
            &McpRegistry::default(),
            &hooks,
            Some(&registry),
        );

        assert_eq!(results[0].1.status, tool_types::ToolStatus::Completed);
        let tasks = registry.list();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].task_type,
            orca_core::task_types::TaskType::Subagent
        );
        assert!(
            tasks[0]
                .subagent_current_activity
                .as_deref()
                .unwrap_or_default()
                .contains("bash")
        );
        assert_eq!(
            tasks[0].status,
            orca_core::task_types::TaskStatus::Completed
        );
        assert!(
            event_rx
                .try_iter()
                .any(|event| task_update_matches(&event, |task| task.description
                    == "sync progress child"))
        );
    }

    #[test]
    fn tui_async_subagent_skips_sync_batch_path() {
        let config = full_auto_config();
        let request = tool_types::ToolRequest {
            id: "subagent-async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async child",
                    "prompt": "simple audit",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let requests = vec![request, {
            tool_types::ToolRequest {
                id: "subagent-sync".to_string(),
                name: tool_types::ToolName::Subagent,
                action: orca_core::approval_types::ActionKind::Agent,
                target: Some("sync child".to_string()),
                raw_arguments: Some(
                    serde_json::json!({
                        "description": "sync child",
                        "prompt": "simple audit"
                    })
                    .to_string(),
                ),
            }
        }];

        assert!(!should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 0), 0);
        assert!(should_run_subagent_batch(&config, &requests[1], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 1), 2);
    }

    #[test]
    fn tui_budget_mode_disables_parallel_subagent_batching() {
        let mut config = full_auto_config();
        config.max_budget_usd = Some(1.0);
        let request = tool_types::ToolRequest {
            id: "subagent-sync-budgeted".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("sync child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "sync child",
                    "prompt": "simple audit"
                })
                .to_string(),
            ),
        };

        assert!(!should_run_subagent_batch(&config, &request, 0));
    }

    #[test]
    fn tui_sync_subagent_receives_only_parent_remaining_aggregate_budget() {
        let mut config = full_auto_config();
        config.max_budget_usd = Some(0.5);
        let parent_usage = UsageTotals {
            input_tokens: 10,
            output_tokens: 5,
            cache_tokens: 2,
            estimated_cost_usd: 0.3,
        };

        let child_config = config_for_remaining_subagent_budget(&config, parent_usage);

        let remaining = child_config.max_budget_usd.expect("remaining budget");
        assert!((remaining - 0.2).abs() < 1e-12);
        assert_eq!(config.max_budget_usd, Some(0.5));
    }

    #[test]
    fn tui_budget_mode_rejects_async_subagent_before_task_launch() {
        let mut config = full_auto_config();
        config.max_budget_usd = Some(1.0);
        let (event_tx, event_rx) = mpsc::unbounded();
        let (_controller, _operation, control) = test_turn();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-budgeted-async".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-async-budgeted".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async child",
                    "prompt": "mock_usage",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        let (result, cost) = execute_subagent_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &control,
            None,
            0,
            &instructions,
            &memory,
            &hooks,
            Some(&registry),
        );

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("max_budget_usd is active"))
        );
        assert_eq!(cost.totals(), UsageTotals::default());
        assert!(registry.list().is_empty());
        assert!(event_rx.try_iter().any(|event| {
            matches!(
                event,
                TuiEvent::SubagentCompleted { status, error, .. }
                    if status == "failed"
                        && error
                            .as_deref()
                            .is_some_and(|error| error.contains("max_budget_usd is active"))
            )
        }));
    }

    #[test]
    fn tui_async_subagent_launches_task_and_status_returns_result() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (_controller, _operation, control) = test_turn();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-async".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async child",
                    "prompt": "mock_usage",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        let (result, _cost) = execute_subagent_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &control,
            None,
            0,
            &instructions,
            &memory,
            &hooks,
            Some(&registry),
        );

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let launched: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("launch output")).unwrap();
        assert_eq!(launched["status"], "async_launched");
        let agent_id = launched["agent_id"].as_str().expect("agent id");

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if registry
                .get(agent_id)
                .map(|record| record.status == orca_core::task_types::TaskStatus::Completed)
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let status_request = tool_types::ToolRequest {
            id: "subagent-status".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "agent_id": agent_id }).to_string()),
        };
        let status = execute_subagent_status_for_tui(&status_request, &registry);
        assert_eq!(status.status, tool_types::ToolStatus::Completed);
        let payload: serde_json::Value =
            serde_json::from_str(status.output.as_deref().expect("status output")).unwrap();
        assert_eq!(payload["status"], "completed");
        assert!(payload["created_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["started_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["completed_at_ms"].as_i64().unwrap() > 0);
        assert!(
            payload["output"]
                .as_str()
                .unwrap()
                .contains("Mock runtime completed")
        );
        assert_eq!(payload["usage"]["input_tokens"], 120);
        assert_eq!(payload["usage"]["output_tokens"], 30);
        assert_eq!(payload["usage"]["cache_tokens"], 10);
        assert_eq!(payload["usage"]["total_tokens"], 150);
        assert!(payload["usage"]["estimated_cost_usd"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn tui_async_subagent_records_live_activity_for_status() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (_controller, _operation, control) = test_turn();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-async-progress".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-async-progress".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async progress child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async progress child",
                    "prompt": "bash echo child",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        let (result, _cost) = execute_subagent_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &control,
            None,
            0,
            &instructions,
            &memory,
            &hooks,
            Some(&registry),
        );
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let launched: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("launch output")).unwrap();
        let agent_id = launched["agent_id"].as_str().expect("agent id");

        // Wait for the child to finish so the asserted registry state is
        // final rather than a transient mid-run snapshot. The specificity
        // rule keeps the tool activity ("bash: ...") in place through the
        // trailing turn-started/streaming events, and the mock provider
        // always runs exactly two turns (tool call, then final message).
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let completed = registry.get(agent_id).is_some_and(|record| {
                record.status == orca_core::task_types::TaskStatus::Completed
            });
            if completed {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let status_request = tool_types::ToolRequest {
            id: "subagent-status".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "agent_id": agent_id }).to_string()),
        };
        let status = execute_subagent_status_for_tui(&status_request, &registry);
        let payload: serde_json::Value =
            serde_json::from_str(status.output.as_deref().expect("status output")).unwrap();
        assert_eq!(payload["status"], "completed");
        assert!(
            payload["current_activity"]
                .as_str()
                .unwrap_or_default()
                .contains("bash"),
            "expected bash activity in status payload: {payload:?}; record: {:?}",
            registry.get(agent_id)
        );
        assert_eq!(payload["turn"], 2);
        assert!(payload["last_activity_at_ms"].as_i64().unwrap() > 0);
    }
}
