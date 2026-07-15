pub(crate) use crate::agent_runner::{
    PendingWorkflowNotifications, TuiAgentTurnContinuation, TuiBackgroundTurnContinuationRequest,
};
#[allow(unused_imports)]
pub(crate) use crate::agent_runner::{
    TuiBackgroundTurnCompletion, TuiBackgroundTurnCompletionHandler,
    continue_approved_background_turn_for_tui, run_agent_for_tui_with_notification_queue,
};
pub use crate::agent_runner::{launch_saved_workflow_for_tui, run_agent_for_tui};

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::RunStatus;
use orca_core::provider_types::Usage;
use orca_mcp::McpRegistry;
use orca_runtime::controller::ThreadTurnRequest;
use orca_runtime::cost::CostTracker;
use orca_runtime::history;
use orca_runtime::hooks::HookRunner;
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::lifecycle::{RuntimeTaskKind, RuntimeTurnRunner};
use orca_runtime::memory::MemoryBlock;
use orca_runtime::runtime_pending_interaction::RuntimePendingInteractionStore;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::thread::RuntimeThread;

use crate::types::TuiTaskLifecycle;

pub struct TuiConversationSession {
    runtime: RuntimeThread,
    pending_interactions: RuntimePendingInteractionStore,
    usage_ledger: TuiUsageLedger,
}

pub(crate) struct TuiHostedConversationSession<'a> {
    runtime: &'a mut RuntimeThread,
    pending_interactions: RuntimePendingInteractionStore,
    usage_ledger: TuiUsageLedger,
}

#[derive(Clone)]
struct TuiSessionAuxiliaryState {
    pending_interactions: RuntimePendingInteractionStore,
    usage_ledger: TuiUsageLedger,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TuiUsageLedger {
    state: Arc<UsageLedgerState>,
}

#[derive(Debug, Default)]
struct UsageLedgerState {
    inner: Mutex<UsageLedgerInner>,
    admission_changed: Condvar,
}

#[derive(Debug, Default)]
struct UsageLedgerInner {
    totals: UsageTotals,
    budget_request_in_flight: bool,
}

pub(crate) struct TuiBudgetAdmission {
    state: Arc<UsageLedgerState>,
    admitted: bool,
}

#[derive(Debug)]
pub(crate) enum TuiBudgetAdmissionError {
    BudgetExhausted(UsageTotals),
    Cancelled,
}

impl TuiUsageLedger {
    fn from_totals(totals: UsageTotals) -> Self {
        Self {
            state: Arc::new(UsageLedgerState {
                inner: Mutex::new(UsageLedgerInner {
                    totals,
                    budget_request_in_flight: false,
                }),
                admission_changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn add(&self, usage: UsageTotals) -> UsageTotals {
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.totals.input_tokens = inner.totals.input_tokens.saturating_add(usage.input_tokens);
        inner.totals.output_tokens = inner
            .totals
            .output_tokens
            .saturating_add(usage.output_tokens);
        inner.totals.cache_tokens = inner.totals.cache_tokens.saturating_add(usage.cache_tokens);
        inner.totals.estimated_cost_usd += usage.estimated_cost_usd;
        inner.totals
    }

    pub(crate) fn totals(&self) -> UsageTotals {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .totals
    }

    pub(crate) fn admit_budgeted_request(
        &self,
        max_budget_usd: Option<f64>,
        cancel: &CancelToken,
    ) -> Result<TuiBudgetAdmission, TuiBudgetAdmissionError> {
        let Some(max_budget) = max_budget_usd else {
            return Ok(TuiBudgetAdmission {
                state: Arc::clone(&self.state),
                admitted: false,
            });
        };
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while inner.budget_request_in_flight {
            if cancel.is_cancelled() {
                return Err(TuiBudgetAdmissionError::Cancelled);
            }
            let (next, _) = self
                .state
                .admission_changed
                .wait_timeout(inner, Duration::from_millis(25))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = next;
        }
        if cancel.is_cancelled() {
            return Err(TuiBudgetAdmissionError::Cancelled);
        }
        if inner.totals.estimated_cost_usd > max_budget {
            return Err(TuiBudgetAdmissionError::BudgetExhausted(inner.totals));
        }
        inner.budget_request_in_flight = true;
        Ok(TuiBudgetAdmission {
            state: Arc::clone(&self.state),
            admitted: true,
        })
    }
}

impl Drop for TuiBudgetAdmission {
    fn drop(&mut self) {
        if !self.admitted {
            return;
        }
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.budget_request_in_flight = false;
        self.state.admission_changed.notify_one();
    }
}

fn preloaded_usage_baseline(
    history_mode: &HistoryMode,
    preloaded: Option<&history::SessionTranscript>,
) -> UsageTotals {
    if matches!(history_mode, HistoryMode::Resume(_)) {
        preloaded
            .and_then(|transcript| transcript.usage)
            .unwrap_or_default()
    } else {
        UsageTotals::default()
    }
}

impl TuiConversationSession {
    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<history::SessionTranscript>,
    ) -> std::io::Result<Self> {
        let usage_baseline = preloaded_usage_baseline(&config.history_mode, preloaded.as_ref());
        let runtime = RuntimeThread::start_with_preloaded(config, prompt_for_title, preloaded)?;
        Ok(Self {
            runtime,
            pending_interactions: RuntimePendingInteractionStore::default(),
            usage_ledger: TuiUsageLedger::from_totals(usage_baseline),
        })
    }

    pub fn new_with_preloaded_and_mcp_registry(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<history::SessionTranscript>,
        mcp_registry: McpRegistry,
    ) -> std::io::Result<Self> {
        let usage_baseline = preloaded_usage_baseline(&config.history_mode, preloaded.as_ref());
        let runtime = RuntimeThread::start_with_preloaded_and_mcp_registry(
            config,
            prompt_for_title,
            preloaded,
            mcp_registry,
        )?;
        Ok(Self {
            runtime,
            pending_interactions: RuntimePendingInteractionStore::default(),
            usage_ledger: TuiUsageLedger::from_totals(usage_baseline),
        })
    }
}

impl<'a> TuiHostedConversationSession<'a> {
    pub(crate) fn new(runtime: &'a mut RuntimeThread) -> Self {
        let aggregate_usage = runtime.session().aggregate_usage_totals();
        let auxiliary = runtime
            .thread_extensions()
            .get_or_init(|| TuiSessionAuxiliaryState {
                pending_interactions: RuntimePendingInteractionStore::default(),
                usage_ledger: TuiUsageLedger::from_totals(aggregate_usage),
            });
        Self {
            runtime,
            pending_interactions: auxiliary.pending_interactions.clone(),
            usage_ledger: auxiliary.usage_ledger.clone(),
        }
    }
}

#[allow(dead_code)]
pub(crate) trait TuiSession {
    fn runtime(&self) -> &RuntimeThread;
    fn runtime_mut(&mut self) -> &mut RuntimeThread;
    fn pending_interaction_store(&self) -> RuntimePendingInteractionStore;
    fn shared_usage_ledger(&self) -> TuiUsageLedger;

    fn runtime_session(&self) -> &orca_runtime::session::InteractiveSession {
        self.runtime().session()
    }

    fn runtime_session_mut(&mut self) -> &mut orca_runtime::session::InteractiveSession {
        self.runtime_mut().session_mut()
    }

    fn conversation(&self) -> &orca_core::conversation::Conversation {
        self.runtime().session().conversation()
    }

    fn conversation_mut(&mut self) -> &mut orca_core::conversation::Conversation {
        self.runtime_mut().session_mut().conversation_mut()
    }

    fn writer_mut(&mut self) -> Option<&mut orca_runtime::history::SessionWriter> {
        self.runtime_mut().session_mut().writer_mut()
    }

    fn instructions(&self) -> &ProjectInstructions {
        self.runtime().session().instructions()
    }

    fn cost_tracker_mut(&mut self) -> &mut CostTracker {
        self.runtime_mut().session_mut().cost_tracker_mut()
    }

    fn record_provider_usage(&mut self, usage: Usage) -> UsageTotals {
        let before = self.runtime().session().usage_totals();
        let after = self
            .runtime_mut()
            .session_mut()
            .cost_tracker_mut()
            .add_usage(usage);
        self.shared_usage_ledger().add(usage_delta(before, after))
    }

    fn record_external_usage(&mut self, usage: UsageTotals) -> UsageTotals {
        self.runtime_mut()
            .session_mut()
            .cost_tracker_mut()
            .merge_totals(usage);
        self.shared_usage_ledger().add(usage)
    }

    fn usage_ledger(&self) -> TuiUsageLedger {
        self.shared_usage_ledger()
    }

    fn mcp_registry(&self) -> &McpRegistry {
        self.runtime().session().mcp_registry()
    }

    fn hooks(&self) -> &HookRunner {
        self.runtime().session().hooks()
    }

    fn memory(&self) -> &MemoryBlock {
        self.runtime().session().memory()
    }

    fn task_registry(&self) -> &TaskRegistry {
        self.runtime().session().task_registry()
    }

    fn pending_interactions(&self) -> RuntimePendingInteractionStore {
        self.pending_interaction_store()
    }

    fn append_message(&mut self, message: &orca_core::conversation::Message) {
        self.runtime_mut().session_mut().append_message(message);
    }

    fn complete(&mut self, status: &str) {
        self.runtime_mut().session_mut().complete(status);
    }

    fn complete_with_error(&mut self, status: &str, error: &str) {
        self.runtime_mut()
            .session_mut()
            .complete_with_error(status, Some(error));
    }

    fn start_agent_lifecycle_task_with_id(&mut self, task_id: &str) {
        self.runtime_mut()
            .lifecycle_mut()
            .start_task_with_id(RuntimeTaskKind::Agent, task_id.to_string());
    }

    fn finish_agent_lifecycle_task(&mut self, status: RunStatus) {
        let _ = self.runtime_mut().lifecycle_mut().finish_task(status);
    }

    fn session_id(&self) -> Option<&str> {
        self.runtime().session().session_id()
    }

    fn completion_error(&self) -> Option<&str> {
        self.runtime().session().completion_error()
    }

    fn thread_extensions(&self) -> &orca_runtime::extension::ExtensionData {
        self.runtime().thread_extensions()
    }

    fn thread_extensions_handle(&self) -> std::sync::Arc<orca_runtime::extension::ExtensionData> {
        self.runtime().thread_extensions_handle()
    }

    fn usage_totals(&self) -> UsageTotals {
        self.shared_usage_ledger().totals()
    }

    fn runtime_usage_totals(&self) -> UsageTotals {
        self.runtime().session().usage_totals()
    }

    fn run_request_with_cancel_for_tui(
        &mut self,
        config: &RunConfig,
        request: &ThreadTurnRequest,
        writer: &mut dyn std::io::Write,
        cancel: CancelToken,
    ) -> std::io::Result<RunStatus> {
        let usage_ledger = self.shared_usage_ledger();
        let admission = match usage_ledger.admit_budgeted_request(config.max_budget_usd, &cancel) {
            Ok(admission) => admission,
            Err(TuiBudgetAdmissionError::BudgetExhausted(current_totals)) => {
                let error = format!(
                    "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                    current_totals.estimated_cost_usd,
                    config.max_budget_usd.unwrap_or_default()
                );
                self.runtime_mut()
                    .session_mut()
                    .complete_with_error(RunStatus::BudgetExhausted.as_str(), Some(&error));
                return Ok(RunStatus::BudgetExhausted);
            }
            Err(TuiBudgetAdmissionError::Cancelled) => return Ok(RunStatus::Cancelled),
        };
        let before = self.runtime().session().usage_totals();
        let result = self
            .runtime_mut()
            .run_request_with_cancel(config, request, writer, cancel);
        let after = self.runtime().session().usage_totals();
        let totals = usage_ledger.add(usage_delta(before, after));
        drop(admission);
        let status = result?;
        if status != RunStatus::BudgetExhausted
            && let Some(max_budget) = config.max_budget_usd
            && totals.estimated_cost_usd > max_budget
        {
            let error = format!(
                "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                totals.estimated_cost_usd, max_budget
            );
            self.runtime_mut()
                .session_mut()
                .complete_with_error(RunStatus::BudgetExhausted.as_str(), Some(&error));
            return Ok(RunStatus::BudgetExhausted);
        }
        Ok(status)
    }

    fn has_active_workflows(&self) -> bool {
        self.runtime().session().has_active_workflows()
    }

    fn backtrack_last_user(&mut self) -> Option<String> {
        self.runtime_mut().session_mut().backtrack_last_user()
    }

    fn set_model(&mut self, model: Option<&str>) {
        self.runtime_mut().session_mut().set_model(model);
    }

    fn add_pinned_context(&mut self, content: String) {
        self.runtime_mut().session_mut().add_pinned_context(content);
    }

    fn replace_goal_context(&mut self, content: String) {
        self.runtime_mut()
            .session_mut()
            .replace_goal_context(content);
    }

    fn replace_skill_context(&mut self, content: Option<String>) {
        self.runtime_mut()
            .session_mut()
            .replace_skill_context(content);
    }

    fn compact(
        &mut self,
        config: &RunConfig,
        cwd: &Path,
        cancel: &orca_core::cancel::CancelToken,
    ) -> (usize, usize) {
        self.runtime_mut()
            .session_mut()
            .compact(config, cwd, cancel)
    }

    fn next_turn_lifecycle(&mut self) -> (u32, Option<TuiTaskLifecycle>) {
        if self.runtime().lifecycle().active_task().is_none() {
            self.runtime_mut()
                .lifecycle_mut()
                .start_task(RuntimeTaskKind::Agent);
        }
        let started = RuntimeTurnRunner::new(self.runtime_mut().lifecycle_mut()).advance_turn();
        let task = started.task().map(|task| TuiTaskLifecycle {
            id: task.id().to_string(),
            kind: lifecycle_kind_label(task.kind()).to_string(),
            status: lifecycle_status_label(task.status()).to_string(),
            turn: task.current_turn(),
        });
        (started.turn(), task)
    }
}

impl TuiSession for TuiConversationSession {
    fn runtime(&self) -> &RuntimeThread {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut RuntimeThread {
        &mut self.runtime
    }

    fn pending_interaction_store(&self) -> RuntimePendingInteractionStore {
        self.pending_interactions.clone()
    }

    fn shared_usage_ledger(&self) -> TuiUsageLedger {
        self.usage_ledger.clone()
    }
}

impl TuiSession for TuiHostedConversationSession<'_> {
    fn runtime(&self) -> &RuntimeThread {
        self.runtime
    }

    fn runtime_mut(&mut self) -> &mut RuntimeThread {
        self.runtime
    }

    fn pending_interaction_store(&self) -> RuntimePendingInteractionStore {
        self.pending_interactions.clone()
    }

    fn shared_usage_ledger(&self) -> TuiUsageLedger {
        self.usage_ledger.clone()
    }
}

fn usage_delta(before: UsageTotals, after: UsageTotals) -> UsageTotals {
    UsageTotals {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        cache_tokens: after.cache_tokens.saturating_sub(before.cache_tokens),
        estimated_cost_usd: (after.estimated_cost_usd - before.estimated_cost_usd).max(0.0),
    }
}

fn lifecycle_kind_label(kind: orca_runtime::lifecycle::RuntimeTaskKind) -> &'static str {
    match kind {
        orca_runtime::lifecycle::RuntimeTaskKind::Agent => "agent",
        orca_runtime::lifecycle::RuntimeTaskKind::Workflow => "workflow",
        orca_runtime::lifecycle::RuntimeTaskKind::Subagent => "subagent",
        orca_runtime::lifecycle::RuntimeTaskKind::Shell => "shell",
    }
}

fn lifecycle_status_label(status: orca_runtime::lifecycle::RuntimeTaskStatus) -> &'static str {
    match status {
        orca_runtime::lifecycle::RuntimeTaskStatus::Running => "running",
        orca_runtime::lifecycle::RuntimeTaskStatus::Succeeded => "succeeded",
        orca_runtime::lifecycle::RuntimeTaskStatus::Failed => "failed",
        orca_runtime::lifecycle::RuntimeTaskStatus::Cancelled => "cancelled",
        orca_runtime::lifecycle::RuntimeTaskStatus::ApprovalRequired => "approval_required",
        orca_runtime::lifecycle::RuntimeTaskStatus::BudgetExhausted => "budget_exhausted",
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use std::time::Duration;

    use super::*;

    fn usage(input_tokens: u64, output_tokens: u64, cache_tokens: u64, cost: f64) -> UsageTotals {
        UsageTotals {
            input_tokens,
            output_tokens,
            cache_tokens,
            estimated_cost_usd: cost,
        }
    }

    fn assert_usage(actual: UsageTotals, expected: UsageTotals) {
        assert_eq!(actual.input_tokens, expected.input_tokens);
        assert_eq!(actual.output_tokens, expected.output_tokens);
        assert_eq!(actual.cache_tokens, expected.cache_tokens);
        assert!((actual.estimated_cost_usd - expected.estimated_cost_usd).abs() < 1e-12);
    }

    #[test]
    fn resume_initializes_shared_usage_ledger_from_transcript_aggregate() {
        let baseline = usage(130, 30, 55, 0.15);
        let transcript = history::SessionTranscript {
            meta: history::create_meta(Path::new("/tmp"), "mock", None, "resume usage"),
            messages: Vec::new(),
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: Some(baseline),
            plan: None,
            completion_status: None,
            completion_error: None,
            path: Path::new("/tmp/resume-usage.jsonl").to_path_buf(),
        };

        let loaded = preloaded_usage_baseline(
            &HistoryMode::Resume("resume-usage".to_string()),
            Some(&transcript),
        );
        let ledger = TuiUsageLedger::from_totals(loaded);
        let background = ledger.clone();
        background.add(usage(20, 5, 8, 0.03));

        assert_usage(ledger.totals(), usage(150, 35, 63, 0.18));
        assert_eq!(
            preloaded_usage_baseline(
                &HistoryMode::Fork("resume-usage".to_string()),
                Some(&transcript),
            ),
            UsageTotals::default()
        );
    }

    #[test]
    fn budget_admission_serializes_requests_and_rechecks_usage_after_waiting() {
        let ledger = TuiUsageLedger::default();
        let cancel = CancelToken::new();
        let first = ledger
            .admit_budgeted_request(Some(1.0), &cancel)
            .expect("first budgeted request admitted");
        let waiting_ledger = ledger.clone();
        let waiting_cancel = cancel.clone();
        let (started_tx, started_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::unbounded();

        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = waiting_ledger
                .admit_budgeted_request(Some(1.0), &waiting_cancel)
                .map(drop);
            result_tx.send(result).unwrap();
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second request started");
        assert!(
            result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "second provider request must wait while the first admission is held"
        );

        let charged = ledger.add(usage(1_000, 100, 800, 1.25));
        drop(first);

        let rejected = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiting request completed")
            .expect_err("waiting request must recheck the updated total");
        waiter.join().expect("waiting admission thread");
        match rejected {
            TuiBudgetAdmissionError::BudgetExhausted(rejected) => assert_usage(rejected, charged),
            TuiBudgetAdmissionError::Cancelled => panic!("request should fail on budget"),
        }
    }

    #[test]
    fn budget_admission_wait_exits_promptly_when_cancelled() {
        let ledger = TuiUsageLedger::default();
        let holder_cancel = CancelToken::new();
        let _first = ledger
            .admit_budgeted_request(Some(1.0), &holder_cancel)
            .expect("first budgeted request admitted");
        let waiting_ledger = ledger.clone();
        let waiting_cancel = CancelToken::new();
        let cancel_from_test = waiting_cancel.clone();
        let (result_tx, result_rx) = mpsc::unbounded();

        let waiter = std::thread::spawn(move || {
            result_tx
                .send(waiting_ledger.admit_budgeted_request(Some(1.0), &waiting_cancel))
                .unwrap();
        });
        cancel_from_test.cancel();

        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled admission completed");
        assert!(matches!(result, Err(TuiBudgetAdmissionError::Cancelled)));
        waiter.join().expect("waiting admission thread");
    }
}
