pub(crate) use crate::agent_runner::{
    PendingWorkflowNotifications, continue_approved_background_turn_for_tui,
    run_agent_for_tui_with_notification_queue,
};
pub use crate::agent_runner::{launch_saved_workflow_for_tui, run_agent_for_tui};

use std::path::Path;

use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::RunStatus;
use orca_mcp::McpRegistry;
use orca_runtime::cost::CostTracker;
use orca_runtime::history;
use orca_runtime::hooks::HookRunner;
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::lifecycle::{RuntimeTaskKind, RuntimeTurnRunner};
use orca_runtime::memory::MemoryBlock;
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::thread::RuntimeThread;

use crate::types::TuiTaskLifecycle;

pub struct TuiConversationSession {
    runtime: RuntimeThread,
}

impl TuiConversationSession {
    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<history::SessionTranscript>,
    ) -> std::io::Result<Self> {
        let runtime = RuntimeThread::start_with_preloaded(config, prompt_for_title, preloaded)?;
        Ok(Self { runtime })
    }

    pub fn runtime_session(&self) -> &orca_runtime::session::InteractiveSession {
        self.runtime.session()
    }

    pub(crate) fn conversation(&self) -> &orca_core::conversation::Conversation {
        self.runtime.session().conversation()
    }

    pub(crate) fn conversation_mut(&mut self) -> &mut orca_core::conversation::Conversation {
        self.runtime.session_mut().conversation_mut()
    }

    pub(crate) fn writer_mut(&mut self) -> Option<&mut orca_runtime::history::SessionWriter> {
        self.runtime.session_mut().writer_mut()
    }

    pub(crate) fn instructions(&self) -> &ProjectInstructions {
        self.runtime.session().instructions()
    }

    pub(crate) fn cost_tracker_mut(&mut self) -> &mut CostTracker {
        self.runtime.session_mut().cost_tracker_mut()
    }

    pub(crate) fn mcp_registry(&self) -> &McpRegistry {
        self.runtime.session().mcp_registry()
    }

    pub(crate) fn hooks(&self) -> &HookRunner {
        self.runtime.session().hooks()
    }

    pub(crate) fn memory(&self) -> &MemoryBlock {
        self.runtime.session().memory()
    }

    pub(crate) fn task_registry(&self) -> &TaskRegistry {
        self.runtime.session().task_registry()
    }

    pub(crate) fn append_message(&mut self, message: &orca_core::conversation::Message) {
        self.runtime.session_mut().append_message(message);
    }

    pub(crate) fn complete(&mut self, status: &str) {
        self.runtime.session_mut().complete(status);
    }

    pub(crate) fn start_agent_lifecycle_task_with_id(&mut self, task_id: &str) {
        self.runtime
            .lifecycle_mut()
            .start_task_with_id(RuntimeTaskKind::Agent, task_id.to_string());
    }

    pub(crate) fn finish_agent_lifecycle_task(&mut self, status: RunStatus) {
        let _ = self.runtime.lifecycle_mut().finish_task(status);
    }

    pub fn session_id(&self) -> Option<&str> {
        self.runtime.session().session_id()
    }

    pub(crate) fn thread_extensions(&self) -> &orca_runtime::extension::ExtensionData {
        self.runtime.thread_extensions()
    }

    pub(crate) fn thread_extensions_handle(
        &self,
    ) -> std::sync::Arc<orca_runtime::extension::ExtensionData> {
        self.runtime.thread_extensions_handle()
    }

    pub fn usage_totals(&self) -> UsageTotals {
        self.runtime.session().usage_totals()
    }

    pub fn has_active_workflows(&self) -> bool {
        self.runtime.session().has_active_workflows()
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        self.runtime.session_mut().backtrack_last_user()
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.runtime.session_mut().set_model(model);
    }

    pub fn add_pinned_context(&mut self, content: String) {
        self.runtime.session_mut().add_pinned_context(content);
    }

    pub fn replace_goal_context(&mut self, content: String) {
        self.runtime.session_mut().replace_goal_context(content);
    }

    pub(crate) fn replace_skill_context(&mut self, content: Option<String>) {
        self.runtime.session_mut().replace_skill_context(content);
    }

    pub fn compact(&mut self, config: &RunConfig, cwd: &Path) -> (usize, usize) {
        self.runtime.session_mut().compact(config, cwd)
    }

    pub(crate) fn next_turn_lifecycle(&mut self) -> (u32, Option<TuiTaskLifecycle>) {
        if self.runtime.lifecycle().active_task().is_none() {
            self.runtime
                .lifecycle_mut()
                .start_task(RuntimeTaskKind::Agent);
        }
        let started = RuntimeTurnRunner::new(self.runtime.lifecycle_mut()).advance_turn();
        let task = started.task().map(|task| TuiTaskLifecycle {
            id: task.id().to_string(),
            kind: lifecycle_kind_label(task.kind()).to_string(),
            status: lifecycle_status_label(task.status()).to_string(),
            turn: task.current_turn(),
        });
        (started.turn(), task)
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
