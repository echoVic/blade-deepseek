use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use orca_core::approval_types::ApprovalMode;
use orca_core::cost_types::UsageTotals;
use orca_core::goal_types::ThreadGoal;
use orca_core::plan_types::PlanItem;
use orca_core::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};
use orca_core::task_types::BackgroundTaskSummary;
use orca_runtime::history::SessionSummary;
use orca_runtime::runtime_pending_interaction::RuntimeMcpElicitationMode;
use orca_runtime::runtime_permission::RuntimePermissionRequestKind;

use crate::display_text::truncate_to_display_width;
use crate::transcript_view::TranscriptRenderCache;

const SUBAGENT_ACTIVITY_TAIL_LIMIT: usize = 6;
const GOAL_NOTICE_OBJECTIVE_WIDTH: usize = 80;

fn format_goal_notice(goal: &orca_core::goal_types::ThreadGoal) -> String {
    use orca_core::goal_types::{
        format_goal_elapsed_seconds, format_tokens_compact, goal_status_label,
    };

    let objective = goal
        .objective
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut parts = vec![
        format!("Goal {}", goal_status_label(goal.status)),
        truncate_to_display_width(&objective, GOAL_NOTICE_OBJECTIVE_WIDTH),
    ];
    if goal.time_used_seconds > 0 {
        parts.push(format_goal_elapsed_seconds(goal.time_used_seconds));
    }
    if let Some(token_budget) = goal.token_budget {
        parts.push(format!(
            "{}/{} tok",
            format_tokens_compact(goal.tokens_used),
            format_tokens_compact(token_budget)
        ));
    }
    parts.join(" · ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiTaskLifecycle {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub turn: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWorkflowNotification {
    pub id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Default)]
pub struct PendingWorkflowNotificationQueue {
    inner: Arc<Mutex<VecDeque<PendingWorkflowNotification>>>,
}

impl PendingWorkflowNotificationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_unique(&self, notification: PendingWorkflowNotification) -> bool {
        let Ok(mut queue) = self.inner.lock() else {
            return false;
        };
        push_pending_workflow_notification_unique(&mut queue, notification)
    }

    pub fn drain_into(&self, target: &mut VecDeque<PendingWorkflowNotification>) {
        let Ok(mut queue) = self.inner.lock() else {
            return;
        };
        while let Some(notification) = queue.pop_front() {
            target.push_back(notification);
        }
    }

    pub fn pop_notification(&self) -> Option<PendingWorkflowNotification> {
        self.inner.lock().ok()?.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .map(|queue| queue.is_empty())
            .unwrap_or(true)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|queue| queue.len())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn pop_front(&self) -> Option<PendingWorkflowNotification> {
        self.inner.lock().ok()?.pop_front()
    }
}

fn push_pending_workflow_notification_unique(
    queue: &mut VecDeque<PendingWorkflowNotification>,
    notification: PendingWorkflowNotification,
) -> bool {
    if queue.iter().any(|pending| pending.id == notification.id) {
        return false;
    }
    queue.push_back(notification);
    true
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TuiEvent {
    TurnStarted {
        turn: u32,
        task: Option<TuiTaskLifecycle>,
    },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolRequested {
        id: String,
        name: String,
        target: Option<String>,
    },
    ToolCallProgress {
        id: String,
        name: Option<String>,
        arguments_bytes: usize,
    },
    ToolOutputDelta {
        id: String,
        chunk: String,
    },
    ToolCompleted {
        id: String,
        name: String,
        status: String,
        output: String,
        diff: Option<String>,
        kind: Option<String>,
    },
    PlanUpdated {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    SubagentStarted {
        id: String,
        description: String,
    },
    SubagentCompleted {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
    },
    SubagentProgress {
        id: String,
        activity: String,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
    },
    WorkflowTasksUpdated {
        tasks: Vec<BackgroundTaskSummary>,
    },
    WorkflowTaskUpdated {
        task: BackgroundTaskSummary,
    },
    WorkflowNotification {
        id: String,
        prompt: String,
        status: String,
        summary: String,
    },
    ApprovalNeeded {
        id: String,
        tool: String,
        target: Option<String>,
        preview: Option<String>,
    },
    PermissionApprovalNeeded {
        id: String,
        tool: String,
        target: Option<String>,
        preview: Option<String>,
        permission_kind: RuntimePermissionRequestKind,
    },
    UserInputRequested {
        id: String,
        question: String,
        choices: Vec<String>,
    },
    McpElicitationRequested {
        id: String,
        server_name: String,
        mode: RuntimeMcpElicitationMode,
        message: String,
        url: Option<String>,
        requested_schema_json: Option<String>,
    },
    Notice(String),
    Error(String),
    UsageUpdated(UsageTotals),
    ContextUpdated {
        used_tokens: usize,
        limit_tokens: usize,
    },
    CompactionStarted,
    SessionCompleted {
        status: String,
    },
    Compacted {
        before_messages: usize,
        after_messages: usize,
        reason: String,
        strategy: String,
        collapsed_messages: usize,
        status_text: String,
    },
    GoalUpdated(ThreadGoal),
    GoalCleared,
    GoalStatus(Option<ThreadGoal>),
    Backtracked {
        prompt: String,
    },
}

#[derive(Debug, Clone)]
pub enum UserAction {
    Submit(String),
    SubmitWorkflowNotification(PendingWorkflowNotification),
    RunWorkflow {
        name: String,
        args: Option<String>,
    },
    SetModel(String),
    Remember(String),
    Compact,
    GoalShow,
    GoalSet(String),
    GoalEdit(String),
    GoalClear,
    GoalPause,
    GoalResume,
    Approve {
        id: String,
        approved: bool,
    },
    ResolveBackgroundApproval {
        id: String,
        approved: bool,
    },
    StopTask {
        task_id: String,
    },
    ForegroundTask {
        task_id: String,
    },
    RespondToUserInput {
        id: String,
        answer: String,
    },
    RespondToMcpElicitation {
        id: String,
        accepted: bool,
        content_json: Option<String>,
    },
    Backtrack,
    BackgroundCurrentTurn,
    Interrupt,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    Setup,
    SessionPicker,
    Idle,
    Running,
    Compacting,
    WaitingApproval,
    WaitingUserInput,
}

#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    Reasoning(String),
    Assistant(String),
    ProposedPlan(String),
    ToolCall {
        id: String,
        name: String,
        target: Option<String>,
        status: String,
        output: Option<String>,
        diff: Option<String>,
        kind: Option<String>,
        expanded: bool,
    },
    PlanUpdate {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    Subagent {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
        activity: Option<String>,
        activity_tail: Vec<String>,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
        expanded: bool,
    },
    Error(String),
    System(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOption {
    /// Approve this single call.
    Once,
    /// Approve and remember this tool for the rest of the session.
    AlwaysTool,
    /// Approve and remember this tool + target for the rest of the session.
    AlwaysTarget,
    /// Reject this call.
    Deny,
}

impl ApprovalOption {
    pub fn key(self) -> char {
        match self {
            ApprovalOption::Once => '1',
            ApprovalOption::AlwaysTarget => '2',
            ApprovalOption::AlwaysTool => '3',
            ApprovalOption::Deny => '4',
        }
    }

    pub fn legacy_key(self) -> char {
        match self {
            ApprovalOption::Once => 'y',
            ApprovalOption::AlwaysTool => 'a',
            ApprovalOption::AlwaysTarget => 'A',
            ApprovalOption::Deny => 'n',
        }
    }

    pub fn matches_key(self, key: char) -> bool {
        key == self.key() || key == self.legacy_key()
    }

    pub fn label(self) -> &'static str {
        match self {
            ApprovalOption::Once => "allow this once",
            ApprovalOption::AlwaysTool => "always allow",
            ApprovalOption::AlwaysTarget => "always allow this exact call",
            ApprovalOption::Deny => "deny",
        }
    }

    /// Whether choosing this option lets the tool run.
    pub fn is_approve(self) -> bool {
        !matches!(self, ApprovalOption::Deny)
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub id: String,
    pub tool: String,
    pub target: Option<String>,
    pub permission_kind: Option<RuntimePermissionRequestKind>,
    pub background_task_id: Option<String>,
    pub selected: usize,
    pub options: Vec<ApprovalOption>,
    pub diff: Option<String>,
}

impl ApprovalDialog {
    /// Tools whose target is inherently dynamic (e.g. search queries) —
    /// the "always allow this exact call" option would never match again,
    /// so we hide it to reduce noise.
    const DYNAMIC_TARGET_TOOLS: &[&str] = &["web_search", "search", "grep"];

    /// Returns the set of options to display. The `AlwaysTarget` option is
    /// only shown when a target is present AND the tool is likely to be
    /// called again with the same target (e.g. reading a fixed file path).
    pub fn options_for(tool: &str, target: Option<&str>) -> Vec<ApprovalOption> {
        let show_always_target =
            target.is_some() && !Self::DYNAMIC_TARGET_TOOLS.iter().any(|t| tool.contains(t));

        if show_always_target {
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTarget,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        } else {
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        }
    }

    pub fn current(&self) -> ApprovalOption {
        self.options
            .get(self.selected)
            .copied()
            .unwrap_or(ApprovalOption::Deny)
    }

    pub fn option_for_key(&self, key: char) -> Option<ApprovalOption> {
        self.options
            .iter()
            .copied()
            .find(|option| option.matches_key(key))
    }

    pub fn title(&self) -> &'static str {
        match self.permission_kind {
            Some(RuntimePermissionRequestKind::NetworkBlock) => " Network Permission Required ",
            Some(RuntimePermissionRequestKind::FilesystemWrite) => {
                " Filesystem Permission Required "
            }
            Some(RuntimePermissionRequestKind::UnsandboxedShellRetry) => {
                " Unsandboxed Shell Required "
            }
            None => " Approval Required ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    Conversation,
    Workflows,
    Agents,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowPanelState {
    pub selected: usize,
    pub tasks: Vec<BackgroundTaskSummary>,
}

#[derive(Debug, Clone)]
pub struct SlashMenuItem {
    pub command: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SlashMenu {
    pub items: Vec<SlashMenuItem>,
    pub selected: usize,
    pub sub_menu: Option<SubMenu>,
}

#[derive(Debug, Clone)]
pub struct SubMenu {
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    /// Carries a value chosen in an earlier step of a multi-step picker (e.g. the
    /// model picked in step 1 of `/model`, while step 2 asks for reasoning effort).
    /// Nothing is applied until the final step confirms, so Esc cancels cleanly.
    pub context: Option<String>,
}

pub struct AppState {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) message_revisions: Vec<u64>,
    next_message_revision: u64,
    pub(crate) transcript_render_cache: TranscriptRenderCache,
    /// Watermark splitting finished turns from the current turn. Streaming appends target
    /// the live suffix, but historical tool/subagent expansion can still mutate an older
    /// message and must advance that message's render revision.
    pub finalized_count: usize,
    /// How many messages are omitted from the live transcript renderer. This is zero in the
    /// current fullscreen TUI, but remains part of the state model for older inline-viewport
    /// behavior and tests that exercise finalized/live suffix boundaries.
    pub flushed_count: usize,
    pub status: AppStatus,
    pub running_started_at: Option<Instant>,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub total_lines: usize,
    pub visible_height: usize,
    pub app_version: String,
    pub model_name: String,
    pub reasoning_effort: orca_core::config::ReasoningEffort,
    pub approval_mode: ApprovalMode,
    pub cwd: String,
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<UserAction>,
    pub approval_dialog: Option<ApprovalDialog>,
    pub pending_user_input_id: Option<String>,
    /// Tool / "tool\u{0}target" keys the user chose to always allow this
    /// session. Checked when a new approval arrives so the dialog is skipped.
    pub approval_allowlist: std::collections::HashSet<String>,
    pub setup_step: u8,
    pub show_shortcuts: bool,
    pub input_history: Vec<String>,
    pub(crate) pending_pastes: Vec<(String, String)>,
    pub history_cursor: Option<usize>,
    pub draft_before_history: Option<String>,
    pub last_ctrl_c: Option<Instant>,
    pub last_completed_at: Option<Instant>,
    pub session_picker_sessions: Vec<SessionSummary>,
    pub session_picker_selected: usize,
    pub session_picker_query: String,
    pub usage: UsageTotals,
    pub context_used_tokens: usize,
    pub context_limit_tokens: usize,
    pub slash_menu: Option<SlashMenu>,
    pub mention_candidates: Vec<String>,
    pub mention_selected: usize,
    pub current_plan: Option<(Option<String>, Vec<PlanItem>)>,
    proposed_plan_parser: ProposedPlanStreamParser,
    /// The most recent update_plan call failed, so `current_plan` may be
    /// showing outdated statuses. Cleared by the next successful update.
    pub plan_update_failed: bool,
    pub current_goal: Option<ThreadGoal>,
    pub panel_mode: PanelMode,
    pub workflow_panel: WorkflowPanelState,
    pub pending_workflow_notifications: VecDeque<PendingWorkflowNotification>,
    pub suppress_background_main_session_output: bool,
    pub tick: u64,
}

pub trait ScrollAmount {
    fn as_usize(self) -> usize;
}

impl ScrollAmount for usize {
    fn as_usize(self) -> usize {
        self
    }
}

impl ScrollAmount for u16 {
    fn as_usize(self) -> usize {
        self as usize
    }
}

impl ScrollAmount for u32 {
    fn as_usize(self) -> usize {
        self as usize
    }
}

impl ScrollAmount for i32 {
    fn as_usize(self) -> usize {
        self.max(0) as usize
    }
}

impl AppState {
    pub fn new(
        event_tx: mpsc::Sender<UserAction>,
        app_version: String,
        model_name: String,
        cwd: String,
    ) -> Self {
        Self {
            messages: Vec::new(),
            message_revisions: Vec::new(),
            next_message_revision: 1,
            transcript_render_cache: TranscriptRenderCache::default(),
            finalized_count: 0,
            flushed_count: 0,
            status: AppStatus::Idle,
            running_started_at: None,
            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,
            visible_height: 0,
            app_version,
            model_name,
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            approval_mode: ApprovalMode::default(),
            cwd,
            event_tx,
            approval_dialog: None,
            pending_user_input_id: None,
            approval_allowlist: std::collections::HashSet::new(),
            setup_step: 0,
            show_shortcuts: false,
            input_history: Vec::new(),
            pending_pastes: Vec::new(),
            history_cursor: None,
            draft_before_history: None,
            last_ctrl_c: None,
            last_completed_at: None,
            session_picker_sessions: Vec::new(),
            session_picker_selected: 0,
            session_picker_query: String::new(),
            usage: UsageTotals::default(),
            context_used_tokens: 0,
            context_limit_tokens: 0,
            slash_menu: None,
            mention_candidates: Vec::new(),
            mention_selected: 0,
            current_plan: None,
            proposed_plan_parser: ProposedPlanStreamParser::default(),
            plan_update_failed: false,
            current_goal: None,
            panel_mode: PanelMode::Conversation,
            workflow_panel: WorkflowPanelState::default(),
            pending_workflow_notifications: VecDeque::new(),
            suppress_background_main_session_output: false,
            tick: 0,
        }
    }

    fn allocate_message_revision(&mut self) -> u64 {
        let revision = self.next_message_revision;
        self.next_message_revision = self.next_message_revision.wrapping_add(1).max(1);
        revision
    }

    fn push_goal_notice(&mut self, notice: String, suppress_duplicate: bool) {
        let duplicate = suppress_duplicate
            && self
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    ChatMessage::System(text) if text.starts_with("Goal ") => Some(text),
                    _ => None,
                })
                == Some(&notice);
        if !duplicate {
            self.push_message(ChatMessage::System(notice));
        }
    }

    pub(crate) fn reconcile_message_tracking(&mut self) {
        if self.message_revisions.len() > self.messages.len() {
            self.message_revisions.truncate(self.messages.len());
            self.transcript_render_cache.truncate(self.messages.len());
        }
        while self.message_revisions.len() < self.messages.len() {
            let revision = self.allocate_message_revision();
            self.message_revisions.push(revision);
        }
        self.transcript_render_cache
            .reconcile_len(self.messages.len());
    }

    fn reset_message_tracking(&mut self) {
        self.message_revisions.clear();
        self.transcript_render_cache.clear();
        while self.message_revisions.len() < self.messages.len() {
            let revision = self.allocate_message_revision();
            self.message_revisions.push(revision);
        }
        self.transcript_render_cache
            .reconcile_len(self.messages.len());
    }

    pub(crate) fn push_message(&mut self, message: ChatMessage) {
        self.reconcile_message_tracking();
        let revision = self.allocate_message_revision();
        self.messages.push(message);
        self.message_revisions.push(revision);
        self.transcript_render_cache
            .reconcile_len(self.messages.len());
    }

    pub(crate) fn replace_messages(&mut self, messages: impl IntoIterator<Item = ChatMessage>) {
        self.messages = messages.into_iter().collect();
        self.reset_message_tracking();
        self.finalized_count = 0;
        self.flushed_count = 0;
    }

    pub(crate) fn clear_messages(&mut self) {
        self.messages.clear();
        self.message_revisions.clear();
        self.transcript_render_cache.clear();
        self.finalized_count = 0;
        self.flushed_count = 0;
    }

    pub(crate) fn truncate_messages(&mut self, len: usize) {
        self.reconcile_message_tracking();
        self.messages.truncate(len);
        self.message_revisions.truncate(len);
        self.transcript_render_cache.truncate(len);
        self.finalized_count = self.finalized_count.min(len);
        self.flushed_count = self.flushed_count.min(len);
    }

    pub(crate) fn replace_message(&mut self, index: usize, message: ChatMessage) -> bool {
        self.reconcile_message_tracking();
        if index >= self.messages.len() {
            return false;
        }
        self.messages[index] = message;
        self.touch_message(index);
        true
    }

    pub(crate) fn mutate_message<R>(
        &mut self,
        index: usize,
        mutate: impl FnOnce(&mut ChatMessage) -> R,
    ) -> Option<R> {
        self.reconcile_message_tracking();
        let result = mutate(self.messages.get_mut(index)?);
        self.touch_message(index);
        Some(result)
    }

    pub(crate) fn touch_message(&mut self, index: usize) -> bool {
        self.reconcile_message_tracking();
        if index >= self.message_revisions.len() {
            return false;
        }
        let revision = self.allocate_message_revision();
        self.message_revisions[index] = revision;
        self.transcript_render_cache.invalidate(index);
        true
    }

    pub(crate) fn retain_messages(&mut self, mut keep: impl FnMut(&ChatMessage) -> bool) {
        self.reconcile_message_tracking();
        let messages = std::mem::take(&mut self.messages);
        let revisions = std::mem::take(&mut self.message_revisions);
        let finalized_count = self.finalized_count.min(messages.len());
        let flushed_count = self.flushed_count.min(messages.len());
        let mut retained_finalized = 0;
        let mut retained_flushed = 0;
        let mut retained_mask = Vec::with_capacity(messages.len());
        for (index, (message, revision)) in messages.into_iter().zip(revisions).enumerate() {
            let retain = keep(&message);
            retained_mask.push(retain);
            if retain {
                retained_finalized += usize::from(index < finalized_count);
                retained_flushed += usize::from(index < flushed_count);
                self.messages.push(message);
                self.message_revisions.push(revision);
            }
        }
        self.transcript_render_cache.retain(&retained_mask);
        self.finalized_count = retained_finalized;
        self.flushed_count = retained_flushed;
    }

    pub fn enter_running(&mut self) {
        if self.running_started_at.is_none() {
            self.running_started_at = Some(Instant::now());
        }
        self.status = AppStatus::Running;
    }

    pub fn set_status(&mut self, status: AppStatus) {
        if status == AppStatus::Running {
            self.enter_running();
        } else if matches!(
            status,
            AppStatus::Compacting | AppStatus::WaitingApproval | AppStatus::WaitingUserInput
        ) {
            self.status = status;
        } else {
            self.status = status;
            self.running_started_at = None;
        }
    }

    pub fn scroll_up(&mut self, lines: impl ScrollAmount) {
        let lines = lines.as_usize();
        // With everything already on screen there is nothing to scroll: a wheel tick
        // here (trackpad inertia, an accidental touch) must not silently unpin
        // auto-follow — the view wouldn't move, so the user gets no feedback that
        // follow was disarmed, and the transcript then stops tracking new content
        // the moment it grows past one screen.
        if self.total_lines <= self.visible_height {
            return;
        }
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, lines: impl ScrollAmount) {
        let lines = lines.as_usize();
        let max_scroll = self.total_lines.saturating_sub(self.visible_height);
        self.scroll_offset = self.scroll_offset.saturating_add(lines).min(max_scroll);
        if self.scroll_offset >= max_scroll {
            self.auto_scroll = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        let max_scroll = self.total_lines.saturating_sub(self.visible_height);
        self.scroll_offset = max_scroll;
        self.auto_scroll = true;
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = false;
    }

    pub fn accepts_mouse_scroll_at(&self, now: Instant) -> bool {
        const COMPLETION_MOUSE_SCROLL_GRACE: std::time::Duration =
            std::time::Duration::from_millis(800);
        self.last_completed_at.is_none_or(|completed_at| {
            now.duration_since(completed_at) >= COMPLETION_MOUSE_SCROLL_GRACE
        })
    }

    pub fn toggle_shortcuts(&mut self) {
        self.show_shortcuts = !self.show_shortcuts;
    }

    pub fn show_workflows(&mut self) {
        self.panel_mode = PanelMode::Workflows;
        if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
    }

    pub fn show_agents(&mut self) {
        self.panel_mode = PanelMode::Agents;
        if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
    }

    pub fn select_previous_workflow_task(&mut self) {
        self.workflow_panel.selected = self.workflow_panel.selected.saturating_sub(1);
    }

    pub fn select_next_workflow_task(&mut self) {
        let last = self.workflow_panel.tasks.len().saturating_sub(1);
        self.workflow_panel.selected = (self.workflow_panel.selected + 1).min(last);
    }

    pub fn open_selected_background_approval_dialog(&mut self) -> bool {
        let Some(task) = self.workflow_panel.tasks.get(self.workflow_panel.selected) else {
            return false;
        };
        if task.task_type != orca_core::task_types::TaskType::MainSession
            || task.status != orca_core::task_types::TaskStatus::ApprovalRequired
            || !task.is_backgrounded
        {
            return false;
        }
        let Some(pending_tool_call) = task.pending_tool_call.as_ref() else {
            return false;
        };

        let id = pending_tool_call.id.clone();
        let tool = pending_tool_call.name.clone();
        let target = pending_tool_call.target.clone();
        let background_task_id = task.id.clone();
        let preview = pending_tool_call.arguments.clone();
        let options = ApprovalDialog::options_for(&tool, target.as_deref());
        self.set_status(AppStatus::WaitingApproval);
        self.approval_dialog = Some(ApprovalDialog {
            id,
            tool,
            target,
            permission_kind: None,
            background_task_id: Some(background_task_id),
            selected: 0,
            options,
            diff: Some(preview),
        });
        true
    }

    fn push_pending_workflow_notification(
        &mut self,
        notification: PendingWorkflowNotification,
    ) -> bool {
        push_pending_workflow_notification_unique(
            &mut self.pending_workflow_notifications,
            notification,
        )
    }

    pub fn show_conversation(&mut self) {
        self.panel_mode = PanelMode::Conversation;
    }

    pub fn advance_tick(&mut self) {
        if self.status == AppStatus::Running {
            self.tick = self.tick.wrapping_add(1);
        }
    }

    pub fn toggle_latest_tool_output(&mut self) -> bool {
        // Only the live pane is mutable and re-renderable. Anything below `flushed_count`
        // has been committed to the terminal's immutable scrollback (in fully-expanded
        // form), so `e` can only toggle a live tool/subagent message.
        let live_start = self.flushed_count.min(self.messages.len());
        let Some(index) = self.messages[live_start..].iter().rposition(|message| {
            matches!(
                message,
                ChatMessage::ToolCall { .. } | ChatMessage::Subagent { .. }
            )
        }) else {
            return false;
        };
        self.mutate_message(live_start + index, |message| match message {
            ChatMessage::ToolCall { expanded, .. } | ChatMessage::Subagent { expanded, .. } => {
                *expanded = !*expanded;
            }
            _ => unreachable!(),
        });
        true
    }

    pub fn record_prompt(&mut self, prompt: String) {
        if self
            .input_history
            .last()
            .map(|last| last != &prompt)
            .unwrap_or(true)
        {
            self.input_history.push(prompt);
        }
        self.history_cursor = None;
        self.draft_before_history = None;
    }

    pub fn history_previous(&mut self, current_draft: String) -> Option<String> {
        if self.input_history.is_empty() {
            return None;
        }

        let next = match self.history_cursor {
            Some(0) => return None,
            Some(index) => index - 1,
            None => {
                self.draft_before_history = Some(current_draft);
                self.input_history.len() - 1
            }
        };
        self.history_cursor = Some(next);
        self.input_history.get(next).cloned()
    }

    pub fn history_next(&mut self) -> Option<String> {
        let cursor = self.history_cursor?;
        let next = cursor + 1;

        if next >= self.input_history.len() {
            self.history_cursor = None;
            return Some(self.draft_before_history.take().unwrap_or_default());
        }

        self.history_cursor = Some(next);
        self.input_history.get(next).cloned()
    }

    pub fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.draft_before_history = None;
    }

    /// Indices into `session_picker_sessions` whose title matches the current
    /// query (case-insensitive substring). Empty query matches everything.
    pub fn filtered_session_indices(&self) -> Vec<usize> {
        if self.session_picker_query.is_empty() {
            return (0..self.session_picker_sessions.len()).collect();
        }
        let needle = self.session_picker_query.to_lowercase();
        self.session_picker_sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.title.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn select_previous_session(&mut self) {
        let filtered = self.filtered_session_indices();
        if filtered.is_empty() {
            return;
        }
        let pos = filtered
            .iter()
            .position(|&i| i == self.session_picker_selected)
            .unwrap_or(0);
        let new_pos = pos.saturating_sub(1);
        self.session_picker_selected = filtered[new_pos];
    }

    pub fn select_next_session(&mut self) {
        let filtered = self.filtered_session_indices();
        if filtered.is_empty() {
            return;
        }
        let pos = filtered
            .iter()
            .position(|&i| i == self.session_picker_selected)
            .unwrap_or(0);
        let new_pos = (pos + 1).min(filtered.len() - 1);
        self.session_picker_selected = filtered[new_pos];
    }

    /// Append a character to the search query and reset selection to the first
    /// match so the highlighted row is always within the filtered set.
    pub fn session_query_push(&mut self, ch: char) {
        self.session_picker_query.push(ch);
        self.reset_session_selection_to_first_match();
    }

    pub fn session_query_pop(&mut self) {
        self.session_picker_query.pop();
        self.reset_session_selection_to_first_match();
    }

    fn reset_session_selection_to_first_match(&mut self) {
        if let Some(&first) = self.filtered_session_indices().first() {
            self.session_picker_selected = first;
        }
    }

    pub fn selected_session_id(&self) -> Option<String> {
        self.session_picker_sessions
            .get(self.session_picker_selected)
            .map(|session| session.session_id.clone())
    }

    /// Allowlist key for a tool alone.
    pub fn approval_key_tool(tool: &str) -> String {
        tool.to_string()
    }

    /// Allowlist key for a tool scoped to a specific target.
    pub fn approval_key_target(tool: &str, target: &str) -> String {
        format!("{tool}\u{0}{target}")
    }

    /// True if a pending approval for this tool/target was already granted an
    /// "always allow" this session.
    pub fn approval_is_allowlisted(&self, tool: &str, target: Option<&str>) -> bool {
        if self
            .approval_allowlist
            .contains(&Self::approval_key_tool(tool))
        {
            return true;
        }
        if let Some(target) = target {
            return self
                .approval_allowlist
                .contains(&Self::approval_key_target(tool, target));
        }
        false
    }

    pub fn update(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::TurnStarted { .. } => {
                self.suppress_background_main_session_output = false;
                self.enter_running();
            }
            TuiEvent::ReasoningDelta(text) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                let last = self.messages.len().saturating_sub(1);
                if matches!(self.messages.last(), Some(ChatMessage::Reasoning(_))) {
                    self.mutate_message(last, |message| {
                        let ChatMessage::Reasoning(existing) = message else {
                            unreachable!();
                        };
                        existing.push_str(&text);
                    });
                } else {
                    self.push_message(ChatMessage::Reasoning(text));
                }
            }
            TuiEvent::MessageDelta(text) => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.handle_message_delta(&text);
            }
            TuiEvent::ToolRequested { id, name, target } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if name == "subagent" || name == "update_plan" {
                    return;
                }
                if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, status, .. } if existing_id == &id && status == "receiving")
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall {
                            name: existing_name,
                            target: existing_target,
                            status,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        *existing_name = name;
                        *existing_target = target;
                        *status = "running".to_string();
                    });
                    return;
                }
                self.push_message(ChatMessage::ToolCall {
                    id,
                    name,
                    target,
                    status: "running".to_string(),
                    output: None,
                    diff: None,
                    kind: None,
                    expanded: false,
                });
            }
            TuiEvent::ToolCallProgress {
                id,
                name,
                arguments_bytes,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if name
                    .as_deref()
                    .is_some_and(is_panel_owned_tool_progress_name)
                {
                    return;
                }
                let progress_output = Some(format!(
                    "receiving arguments... {}",
                    format_argument_bytes(arguments_bytes)
                ));
                if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, status, .. } if existing_id == &id && status == "receiving")
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall {
                            name: existing_name,
                            status,
                            output,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        if let Some(name) = name {
                            *existing_name = name;
                        }
                        *status = "receiving".to_string();
                        *output = progress_output;
                    });
                } else {
                    self.push_message(ChatMessage::ToolCall {
                        id,
                        name: name.unwrap_or_else(|| "tool".to_string()),
                        target: None,
                        status: "receiving".to_string(),
                        output: progress_output,
                        diff: None,
                        kind: None,
                        expanded: false,
                    });
                }
            }
            TuiEvent::ToolOutputDelta { id, chunk } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == &id)
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall { output, .. } = message else {
                            unreachable!();
                        };
                        output.get_or_insert_with(String::new).push_str(&chunk);
                    });
                }
            }
            TuiEvent::ToolCompleted {
                id,
                name,
                status,
                output,
                diff,
                kind,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if name == "update_plan" {
                    // update_plan renders through the pinned plan panel, not
                    // the scrollback; a failed call means that panel is now
                    // showing outdated statuses.
                    if status != "completed" {
                        self.plan_update_failed = true;
                    }
                    return;
                }
                if name == "subagent" {
                    return;
                }
                let updated = if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == &id)
                })
                {
                    self.mutate_message(index, |message| {
                        let ChatMessage::ToolCall {
                            id: existing_id,
                            name: existing_name,
                            status: existing_status,
                            output: existing_output,
                            diff: existing_diff,
                            kind: existing_kind,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        *existing_id = id.clone();
                        *existing_name = name.clone();
                        *existing_status = status.clone();
                        if existing_output.is_none() || output.is_empty() {
                            *existing_output = if output.is_empty() {
                                None
                            } else {
                                Some(output.clone())
                            };
                        }
                        *existing_diff = diff.clone();
                        *existing_kind = kind.clone();
                    });
                    true
                } else {
                    false
                };
                if !updated {
                    self.push_message(ChatMessage::ToolCall {
                        id,
                        name,
                        target: None,
                        status,
                        output: if output.is_empty() {
                            None
                        } else {
                            Some(output)
                        },
                        diff,
                        kind,
                        expanded: false,
                    });
                }
            }
            TuiEvent::PlanUpdated { explanation, plan } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                // The live plan is shown in the bottom panel during the turn. It is archived
                // inline (and the panel cleared) when the turn completes, so we avoid pushing a
                // message on every update to keep the scrollback clean.
                self.plan_update_failed = false;
                self.current_plan = if plan.is_empty() {
                    None
                } else {
                    Some((explanation, plan))
                };
            }
            TuiEvent::SubagentStarted { id, description } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                self.push_message(ChatMessage::Subagent {
                    id,
                    description,
                    status: "running".to_string(),
                    output: None,
                    error: None,
                    activity: None,
                    activity_tail: Vec::new(),
                    turn: None,
                    usage: None,
                    expanded: false,
                });
            }
            TuiEvent::SubagentCompleted {
                id,
                description,
                status,
                output,
                error,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                let updated = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::Subagent { id: existing_id, .. } if existing_id == &id)
                });

                if let Some(index) = updated {
                    self.mutate_message(index, |message| {
                        let ChatMessage::Subagent {
                            status: existing_status,
                            output: existing_output,
                            error: existing_error,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        *existing_status = status;
                        *existing_output = output;
                        *existing_error = error;
                    });
                } else {
                    self.push_message(ChatMessage::Subagent {
                        id,
                        description,
                        status,
                        output,
                        error,
                        activity: None,
                        activity_tail: Vec::new(),
                        turn: None,
                        usage: None,
                        expanded: false,
                    });
                }
            }
            TuiEvent::SubagentProgress {
                id,
                activity,
                turn,
                usage,
            } => {
                if self.suppress_background_main_session_output {
                    return;
                }
                if let Some(index) = self.messages.iter().rposition(|message| {
                    matches!(message, ChatMessage::Subagent { id: existing_id, .. } if existing_id == &id)
                }) {
                    self.mutate_message(index, |message| {
                        let ChatMessage::Subagent {
                            activity: existing_activity,
                            activity_tail,
                            turn: existing_turn,
                            usage: existing_usage,
                            ..
                        } = message
                        else {
                            unreachable!();
                        };
                        push_subagent_activity_tail(activity_tail, &activity);
                        *existing_activity = Some(activity);
                        if turn.is_some() {
                            *existing_turn = turn;
                        }
                        if usage.is_some() {
                            *existing_usage = usage;
                        }
                    });
                }
            }
            TuiEvent::WorkflowTasksUpdated { tasks } => self.apply_workflow_tasks_update(tasks),
            TuiEvent::WorkflowTaskUpdated { task } => {
                let mut tasks = self.workflow_panel.tasks.clone();
                if let Some(existing) = tasks.iter_mut().find(|existing| existing.id == task.id) {
                    *existing = task;
                } else {
                    tasks.push(task);
                }
                self.apply_workflow_tasks_update(tasks);
            }
            TuiEvent::WorkflowNotification {
                id,
                prompt,
                status,
                summary,
            } => {
                if self
                    .push_pending_workflow_notification(PendingWorkflowNotification { id, prompt })
                {
                    self.push_message(ChatMessage::System(format!("Workflow {status}. {summary}")));
                }
            }
            TuiEvent::ApprovalNeeded {
                id,
                tool,
                target,
                preview,
            } => {
                self.set_status(AppStatus::WaitingApproval);
                let options = ApprovalDialog::options_for(&tool, target.as_deref());
                self.approval_dialog = Some(ApprovalDialog {
                    id,
                    tool,
                    target,
                    permission_kind: None,
                    background_task_id: None,
                    selected: 0,
                    options,
                    diff: preview,
                });
            }
            TuiEvent::PermissionApprovalNeeded {
                id,
                tool,
                target,
                preview,
                permission_kind,
            } => {
                self.set_status(AppStatus::WaitingApproval);
                let options = ApprovalDialog::options_for(&tool, target.as_deref());
                self.approval_dialog = Some(ApprovalDialog {
                    id,
                    tool,
                    target,
                    permission_kind: Some(permission_kind),
                    background_task_id: None,
                    selected: 0,
                    options,
                    diff: preview,
                });
            }
            TuiEvent::UserInputRequested {
                id,
                question,
                choices,
            } => {
                self.set_status(AppStatus::WaitingUserInput);
                self.pending_user_input_id = Some(id);
                let mut message = question;
                if !choices.is_empty() {
                    message.push_str("\nChoices: ");
                    message.push_str(&choices.join(", "));
                }
                self.push_message(ChatMessage::System(message));
            }
            TuiEvent::McpElicitationRequested {
                id,
                server_name,
                mode,
                message,
                url,
                requested_schema_json,
            } => {
                self.set_status(AppStatus::WaitingUserInput);
                self.pending_user_input_id = Some(id);
                let mut lines = vec![format!("MCP {server_name} requests input: {message}")];
                match mode {
                    RuntimeMcpElicitationMode::Form => {
                        lines.push("Mode: form".to_string());
                        if let Some(schema) = requested_schema_json {
                            lines.push(format!("Schema: {schema}"));
                        }
                    }
                    RuntimeMcpElicitationMode::Url => {
                        lines.push("Mode: url".to_string());
                        if let Some(url) = url {
                            lines.push(format!("URL: {url}"));
                        }
                    }
                }
                self.push_message(ChatMessage::System(lines.join("\n")));
            }
            TuiEvent::Error(msg) => {
                self.clear_receiving_tool_progress();
                self.push_message(ChatMessage::Error(msg));
            }
            TuiEvent::Notice(msg) => {
                self.push_message(ChatMessage::System(msg));
            }
            TuiEvent::UsageUpdated(usage) => {
                self.usage = usage;
            }
            TuiEvent::ContextUpdated {
                used_tokens,
                limit_tokens,
            } => {
                self.context_used_tokens = used_tokens;
                self.context_limit_tokens = limit_tokens;
            }
            TuiEvent::CompactionStarted => {
                self.set_status(AppStatus::Compacting);
            }
            TuiEvent::SessionCompleted { status } => {
                let was_backgrounded = self.suppress_background_main_session_output;
                self.suppress_background_main_session_output = false;
                self.clear_receiving_tool_progress();
                self.flush_proposed_plan_parser();
                self.promote_trailing_reasoning();
                self.archive_current_plan();
                if was_backgrounded {
                    self.push_message(ChatMessage::System(format!(
                        "Background session completed: {status}"
                    )));
                }
                self.finalize_turn();
                self.set_status(AppStatus::Idle);
                self.last_completed_at = Some(Instant::now());
                self.scroll_to_bottom();
            }
            TuiEvent::Compacted {
                before_messages,
                after_messages,
                reason,
                strategy,
                collapsed_messages,
                status_text,
            } => {
                self.push_message(ChatMessage::System(format_compaction_notice(
                    &reason,
                    &strategy,
                    before_messages,
                    after_messages,
                    collapsed_messages,
                    &status_text,
                )));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::GoalUpdated(goal) => {
                let should_keep_running =
                    self.status == AppStatus::Running && goal.status.should_continue();
                let notice = format_goal_notice(&goal);
                self.current_goal = Some(goal);
                self.push_goal_notice(notice, should_keep_running);
                if !should_keep_running {
                    self.set_status(AppStatus::Idle);
                }
            }
            TuiEvent::GoalCleared => {
                self.current_goal = None;
                self.push_message(ChatMessage::System("Goal cleared.".to_string()));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::GoalStatus(goal) => {
                self.current_goal = goal.clone();
                let mut should_keep_running = false;
                match goal {
                    Some(goal) => {
                        should_keep_running =
                            self.status == AppStatus::Running && goal.status.should_continue();
                        let notice = format_goal_notice(&goal);
                        self.push_goal_notice(notice, should_keep_running);
                    }
                    None => self
                        .push_message(ChatMessage::System("No goal is currently set.".to_string())),
                }
                if !should_keep_running {
                    self.set_status(AppStatus::Idle);
                }
            }
            TuiEvent::Backtracked { prompt } => {
                self.remove_after_last_user();
                self.push_message(ChatMessage::System(format!(
                    "Backtracked to previous prompt: {}",
                    prompt.trim()
                )));
                self.set_status(AppStatus::Idle);
            }
        }
    }

    fn promote_trailing_reasoning(&mut self) {
        let index = self.messages.len().saturating_sub(1);
        if let Some(ChatMessage::Reasoning(text)) = self.messages.get(index) {
            let text = text.clone();
            self.replace_message(index, ChatMessage::Assistant(text));
        }
    }

    fn handle_message_delta(&mut self, text: &str) {
        for segment in self.proposed_plan_parser.push(text) {
            self.push_proposed_plan_segment(segment);
        }
    }

    fn flush_proposed_plan_parser(&mut self) {
        for segment in self.proposed_plan_parser.finish() {
            self.push_proposed_plan_segment(segment);
        }
    }

    fn push_proposed_plan_segment(&mut self, segment: ProposedPlanSegment) {
        match segment {
            ProposedPlanSegment::Agent(text) => self.push_assistant_delta(text),
            ProposedPlanSegment::Plan(text) => self.push_proposed_plan_delta(text),
        }
    }

    fn push_assistant_delta(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        let last = self.messages.len().saturating_sub(1);
        if matches!(self.messages.last(), Some(ChatMessage::Assistant(_))) {
            self.mutate_message(last, |message| {
                let ChatMessage::Assistant(existing) = message else {
                    unreachable!();
                };
                existing.push_str(&text);
            });
        } else {
            self.push_message(ChatMessage::Assistant(text));
        }
    }

    fn push_proposed_plan_delta(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        let last = self.messages.len().saturating_sub(1);
        if matches!(self.messages.last(), Some(ChatMessage::ProposedPlan(_))) {
            self.mutate_message(last, |message| {
                let ChatMessage::ProposedPlan(existing) = message else {
                    unreachable!();
                };
                existing.push_str(&text);
            });
        } else {
            self.push_message(ChatMessage::ProposedPlan(text));
        }
    }

    /// Move the live plan out of the bottom panel and into the scrollback as an archived
    /// checklist when a turn ends, so the panel stops occluding content once work is done.
    fn archive_current_plan(&mut self) {
        self.plan_update_failed = false;
        if let Some((explanation, plan)) = self.current_plan.take()
            && !plan.is_empty()
        {
            self.push_message(ChatMessage::PlanUpdate { explanation, plan });
        }
    }

    /// Freeze the current turn: everything in `messages` becomes the immutable,
    /// finalized prefix. Called once a turn ends, after trailing reasoning is promoted
    /// and the live plan is archived, so the frozen transcript is in its final shape.
    fn finalize_turn(&mut self) {
        self.finalized_count = self.messages.len();
    }

    fn clear_receiving_tool_progress(&mut self) {
        let original_finalized_count = self.finalized_count;
        let has_receiving_progress = self.messages[original_finalized_count.min(self.messages.len())..]
            .iter()
            .any(|message| {
                matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
            });
        if !has_receiving_progress {
            return;
        }
        let mut index = 0;
        self.retain_messages(|message| {
            let remove = index >= original_finalized_count
                && matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving");
            index += 1;
            !remove
        });
    }

    fn apply_workflow_tasks_update(&mut self, tasks: Vec<BackgroundTaskSummary>) {
        let was_suppressing_background_output = self.suppress_background_main_session_output;
        let has_backgrounded_running_main_session =
            tasks.iter().any(is_backgrounded_running_main_session);
        let had_backgrounded_approval_main_session = self
            .workflow_panel
            .tasks
            .iter()
            .any(is_backgrounded_approval_main_session);
        let has_backgrounded_approval_main_session =
            tasks.iter().any(is_backgrounded_approval_main_session);
        self.suppress_background_main_session_output = has_backgrounded_running_main_session;
        if has_backgrounded_running_main_session {
            self.set_status(AppStatus::Idle);
        }
        let should_reveal_background_task =
            has_backgrounded_running_main_session && !was_suppressing_background_output;
        let should_reveal_background_approval =
            has_backgrounded_approval_main_session && !had_backgrounded_approval_main_session;
        let selected_was_backgrounded_main_session = self
            .workflow_panel
            .tasks
            .get(self.workflow_panel.selected)
            .is_some_and(is_backgrounded_running_main_session);
        let selected_task_id = self
            .workflow_panel
            .tasks
            .get(self.workflow_panel.selected)
            .map(|task| task.id.clone());
        self.workflow_panel.tasks = sort_workflow_tasks_for_panel(tasks);
        if should_reveal_background_approval {
            self.panel_mode = PanelMode::Workflows;
            if let Some(index) = self
                .workflow_panel
                .tasks
                .iter()
                .position(is_backgrounded_approval_main_session)
            {
                self.workflow_panel.selected = index;
            }
        } else if should_reveal_background_task {
            self.panel_mode = PanelMode::Workflows;
            if let Some(index) = self
                .workflow_panel
                .tasks
                .iter()
                .position(is_backgrounded_running_main_session)
            {
                self.workflow_panel.selected = index;
            }
        } else if let Some(selected_task_id) = selected_task_id
            && let Some(index) = self
                .workflow_panel
                .tasks
                .iter()
                .position(|task| task.id == selected_task_id)
        {
            let selected_is_now_foregrounded = selected_was_backgrounded_main_session
                && is_foregrounded_running_main_session(&self.workflow_panel.tasks[index]);
            self.workflow_panel.selected = index;
            if selected_is_now_foregrounded && self.panel_mode == PanelMode::Workflows {
                self.panel_mode = PanelMode::Conversation;
            }
        } else if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
    }

    /// Whether the message at `index` will never change again, so it is safe to flush
    /// into the append-only scrollback.
    ///
    /// - A finalized message (`index < finalized_count`) is frozen by definition.
    /// - A `ToolCall`/`Subagent` is settled once it leaves the `running` status; its
    ///   output/diff are then complete.
    /// - A `Reasoning`/`Assistant`/`ProposedPlan` block grows via streaming deltas only
    ///   while it is the last message, so it is settled once a newer message follows it,
    ///   or once the turn ends (`turn_ended`).
    /// - Everything else (`User`/`Error`/`System`/`PlanUpdate`) is immutable on arrival.
    fn message_is_settled(&self, index: usize, turn_ended: bool) -> bool {
        if index < self.finalized_count {
            return true;
        }
        let is_last = index + 1 == self.messages.len();
        match &self.messages[index] {
            ChatMessage::ToolCall { status, .. } | ChatMessage::Subagent { status, .. } => {
                !matches!(status.as_str(), "running" | "receiving")
            }
            ChatMessage::Reasoning(_)
            | ChatMessage::Assistant(_)
            | ChatMessage::ProposedPlan(_) => turn_ended || !is_last,
            ChatMessage::User(_)
            | ChatMessage::Error(_)
            | ChatMessage::System(_)
            | ChatMessage::PlanUpdate { .. } => true,
        }
    }

    /// The new value `flushed_count` may advance to: the end of the longest run of
    /// settled messages starting at the current `flushed_count`. Scrollback is
    /// append-only, so a single unsettled message (e.g. a still-running tool call)
    /// blocks everything after it from flushing, even if those later messages are
    /// themselves settled — flushing them now would print them out of order.
    pub fn flushable_prefix_end(&self, turn_ended: bool) -> usize {
        let mut end = self.flushed_count;
        while end < self.messages.len() && self.message_is_settled(end, turn_ended) {
            end += 1;
        }
        end
    }

    pub fn remove_after_last_user(&mut self) {
        if let Some(index) = self
            .messages
            .iter()
            .rposition(|message| matches!(message, ChatMessage::User(_)))
        {
            self.truncate_messages(index);
        }
    }
}

fn sort_workflow_tasks_for_panel(
    mut tasks: Vec<BackgroundTaskSummary>,
) -> Vec<BackgroundTaskSummary> {
    tasks.sort_by(|left, right| {
        workflow_task_panel_group(left)
            .cmp(&workflow_task_panel_group(right))
            .then_with(|| workflow_task_activity_ms(right).cmp(&workflow_task_activity_ms(left)))
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    tasks
}

fn is_backgrounded_running_main_session(task: &BackgroundTaskSummary) -> bool {
    task.task_type == orca_core::task_types::TaskType::MainSession
        && task.status == orca_core::task_types::TaskStatus::Running
        && task.is_backgrounded
}

fn is_backgrounded_approval_main_session(task: &BackgroundTaskSummary) -> bool {
    task.task_type == orca_core::task_types::TaskType::MainSession
        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
        && task.is_backgrounded
        && task.pending_tool_call.is_some()
}

fn is_foregrounded_running_main_session(task: &BackgroundTaskSummary) -> bool {
    task.task_type == orca_core::task_types::TaskType::MainSession
        && task.status == orca_core::task_types::TaskStatus::Running
        && !task.is_backgrounded
}

fn workflow_task_panel_group(task: &BackgroundTaskSummary) -> u8 {
    match task.status {
        orca_core::task_types::TaskStatus::ApprovalRequired => 0,
        orca_core::task_types::TaskStatus::Queued
        | orca_core::task_types::TaskStatus::Running
        | orca_core::task_types::TaskStatus::Paused
        | orca_core::task_types::TaskStatus::Stopping => 1,
        orca_core::task_types::TaskStatus::Stopped
        | orca_core::task_types::TaskStatus::Completed
        | orca_core::task_types::TaskStatus::Failed
        | orca_core::task_types::TaskStatus::Cancelled => 2,
    }
}

fn workflow_task_activity_ms(task: &BackgroundTaskSummary) -> i64 {
    task.last_activity_at_ms
        .or(task.completed_at_ms)
        .or(task.started_at_ms)
        .unwrap_or(task.created_at_ms)
}

fn push_subagent_activity_tail(tail: &mut Vec<String>, activity: &str) {
    if tail.last().is_some_and(|last| last == activity) {
        return;
    }
    tail.push(activity.to_string());
    if tail.len() > SUBAGENT_ACTIVITY_TAIL_LIMIT {
        tail.drain(0..tail.len() - SUBAGENT_ACTIVITY_TAIL_LIMIT);
    }
}

fn format_argument_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

fn is_panel_owned_tool_progress_name(name: &str) -> bool {
    matches!(name, "subagent" | "update_plan")
}

fn format_compaction_notice(
    reason: &str,
    strategy: &str,
    before_messages: usize,
    after_messages: usize,
    collapsed_messages: usize,
    status_text: &str,
) -> String {
    let label = compaction_notice_label(reason, status_text);
    let detail = if collapsed_messages > 0 && !strategy.trim().is_empty() {
        format!(" (collapsed {collapsed_messages}, {strategy})")
    } else if collapsed_messages > 0 {
        format!(" (collapsed {collapsed_messages})")
    } else if !strategy.trim().is_empty() {
        format!(" ({strategy})")
    } else {
        String::new()
    };
    format!(
        "Compacted conversation context {label}: {before_messages} -> {after_messages} messages{detail}."
    )
}

fn compaction_notice_label(reason: &str, status_text: &str) -> String {
    let status = status_text.trim();
    if let Some(rest) = status.strip_prefix("compacted context ") {
        return rest.to_string();
    }
    match reason {
        "prompt_too_long_recovery" => "after prompt-too-long".to_string(),
        "exceeded_context_limit" => "at token limit".to_string(),
        "approaching_context_limit" => "near token limit".to_string(),
        "manual" => "manually".to_string(),
        value if !value.trim().is_empty() => value.replace('_', " "),
        _ => "completed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::plan_types::PlanStatus;
    use orca_core::task_types::{TaskStatus, TaskType};

    fn state() -> AppState {
        let (tx, _rx) = mpsc::channel();
        AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    fn session(id: &str, title: &str) -> SessionSummary {
        use chrono::Utc;
        use std::path::PathBuf;
        SessionSummary {
            session_id: id.to_string(),
            title: title.to_string(),
            cwd: "/tmp".to_string(),
            provider: "deepseek".to_string(),
            model: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            path: PathBuf::from("/tmp"),
            archived: false,
            parent_id: None,
            forked: false,
            approval_mode: None,
            active_permission_profile: None,
            runtime_workspace_roots: Vec::new(),
            permission_rule_count: 0,
            additional_working_directories: Vec::new(),
            network_domain_permissions: Default::default(),
        }
    }

    fn workflow_task_summary(id: &str, name: &str) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            id: id.to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: name.to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(name.to_string()),
            workflow_run_id: Some(format!("run-{id}")),
            phase_count: Some(1),
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
            last_activity_at_ms: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn workflow_notification_action_carries_notification_boundary() {
        let source = include_str!("types.rs");
        let user_action = source
            .split("pub enum UserAction {")
            .nth(1)
            .expect("UserAction enum")
            .split("pub enum ApprovalOption")
            .next()
            .expect("UserAction enum body");

        assert!(
            user_action.contains("SubmitWorkflowNotification(PendingWorkflowNotification)"),
            "workflow notification actions should carry the typed notification boundary"
        );
        assert!(
            !user_action.contains("SubmitWorkflowNotification { id: String, prompt: String }"),
            "workflow notification actions should not split notification id and prompt"
        );
    }

    #[test]
    fn session_search_filters_by_title_and_keeps_selection_valid() {
        let mut state = state();
        state.session_picker_sessions = vec![
            session("a", "fix the failing auth test"),
            session("b", "add JWT auth middleware"),
            session("c", "refactor parser entrypoint"),
        ];
        state.session_picker_selected = 0;

        // No query → all match.
        assert_eq!(state.filtered_session_indices(), vec![0, 1, 2]);

        // Typing "auth" keeps only the two auth sessions and snaps selection
        // to the first match.
        for ch in "auth".chars() {
            state.session_query_push(ch);
        }
        assert_eq!(state.filtered_session_indices(), vec![0, 1]);
        assert_eq!(state.session_picker_selected, 0);

        // Down moves within the filtered set, not the raw list.
        state.select_next_session();
        assert_eq!(state.session_picker_selected, 1);
        state.select_next_session();
        assert_eq!(state.session_picker_selected, 1); // clamped to last match

        // Backspace widens the filter again.
        state.session_query_pop();
        assert_eq!(state.session_picker_query, "aut");
        assert_eq!(state.filtered_session_indices(), vec![0, 1]);
    }

    #[test]
    fn replacing_messages_resets_tracking_after_same_length_replacement() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("old session".to_string()));
        let old_revision = state.message_revisions[0];

        state.replace_messages([ChatMessage::Assistant("new session".to_string())]);

        assert_ne!(state.message_revisions[0], old_revision);
        assert_eq!(state.transcript_render_cache.len(), state.messages.len());
    }

    #[test]
    fn retaining_messages_rebases_watermarks_and_cache_entries() {
        let mut state = state();
        state.push_message(ChatMessage::User("keep before".to_string()));
        state.push_message(ChatMessage::System("remove before".to_string()));
        state.push_message(ChatMessage::Assistant("keep after".to_string()));
        state.finalized_count = 3;
        state.flushed_count = 2;
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            40,
            &theme,
            0,
            false,
            |message, _, _, _, _| vec![ratatui::text::Line::from(format!("{message:?}"))],
        );
        assert_eq!(state.transcript_render_cache.populated_len(), 3);

        state.retain_messages(
            |message| !matches!(message, ChatMessage::System(text) if text == "remove before"),
        );

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.message_revisions.len(), 2);
        assert_eq!(state.finalized_count, 2);
        assert_eq!(state.flushed_count, 1);
        assert_eq!(state.transcript_render_cache.len(), 2);
        assert_eq!(state.transcript_render_cache.populated_len(), 2);
    }

    #[test]
    fn approval_options_have_numeric_primary_keys_and_legacy_shortcuts() {
        assert_eq!(ApprovalOption::Once.key(), '1');
        assert_eq!(ApprovalOption::AlwaysTarget.key(), '2');
        assert_eq!(ApprovalOption::AlwaysTool.key(), '3');
        assert_eq!(ApprovalOption::Deny.key(), '4');

        assert!(ApprovalOption::Once.matches_key('1'));
        assert!(ApprovalOption::Once.matches_key('y'));
        assert!(ApprovalOption::AlwaysTarget.matches_key('2'));
        assert!(ApprovalOption::AlwaysTarget.matches_key('A'));
        assert!(ApprovalOption::AlwaysTool.matches_key('3'));
        assert!(ApprovalOption::AlwaysTool.matches_key('a'));
        assert!(ApprovalOption::Deny.matches_key('4'));
        assert!(ApprovalOption::Deny.matches_key('n'));

        assert!(!ApprovalOption::AlwaysTarget.matches_key('a'));
        assert!(!ApprovalOption::AlwaysTool.matches_key('A'));
    }

    #[test]
    fn approval_dialog_resolves_numeric_and_legacy_keys_by_visible_options() {
        let dialog = ApprovalDialog {
            id: "approval-1".to_string(),
            tool: "edit".to_string(),
            target: Some("src/main.rs".to_string()),
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: ApprovalDialog::options_for("edit", Some("src/main.rs")),
            diff: None,
        };

        assert_eq!(dialog.option_for_key('1'), Some(ApprovalOption::Once));
        assert_eq!(
            dialog.option_for_key('2'),
            Some(ApprovalOption::AlwaysTarget)
        );
        assert_eq!(dialog.option_for_key('3'), Some(ApprovalOption::AlwaysTool));
        assert_eq!(dialog.option_for_key('4'), Some(ApprovalOption::Deny));
        assert_eq!(dialog.option_for_key('y'), Some(ApprovalOption::Once));
        assert_eq!(
            dialog.option_for_key('A'),
            Some(ApprovalOption::AlwaysTarget)
        );
        assert_eq!(dialog.option_for_key('a'), Some(ApprovalOption::AlwaysTool));
        assert_eq!(dialog.option_for_key('n'), Some(ApprovalOption::Deny));

        let dynamic = ApprovalDialog {
            id: "approval-2".to_string(),
            tool: "web_search".to_string(),
            target: Some("query".to_string()),
            permission_kind: None,
            background_task_id: None,
            selected: 0,
            options: ApprovalDialog::options_for("web_search", Some("query")),
            diff: None,
        };
        assert_eq!(dynamic.option_for_key('2'), None);
        assert_eq!(
            dynamic.option_for_key('3'),
            Some(ApprovalOption::AlwaysTool)
        );
    }

    #[test]
    fn approval_dialog_has_four_options_with_target_and_three_without() {
        // Static-target tool (like read_file) shows AlwaysTarget option.
        let with_target = ApprovalDialog::options_for("read_file", Some("src/auth/token.rs"));
        assert_eq!(
            with_target,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTarget,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        );
        // No target — AlwaysTarget is hidden.
        let without = ApprovalDialog::options_for("read_file", None);
        assert_eq!(
            without,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        );
        // Dynamic-target tool (web_search) — AlwaysTarget is hidden even with a target.
        let dynamic = ApprovalDialog::options_for("web_search", Some("some query"));
        assert_eq!(
            dynamic,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::Deny,
            ]
        );
    }

    #[test]
    fn approval_allowlist_grants_matching_tool_and_target() {
        let mut tool_scope = state();

        // Initially nothing is allow-listed.
        assert!(!tool_scope.approval_is_allowlisted("edit", Some("src/a.rs")));

        // "Always allow tool" grants every target for that tool.
        tool_scope
            .approval_allowlist
            .insert(AppState::approval_key_tool("edit"));
        assert!(tool_scope.approval_is_allowlisted("edit", Some("src/a.rs")));
        assert!(tool_scope.approval_is_allowlisted("edit", Some("src/b.rs")));
        assert!(!tool_scope.approval_is_allowlisted("bash", Some("ls")));

        // "Always allow tool + target" is scoped to that one target.
        let mut scoped = state();
        scoped
            .approval_allowlist
            .insert(AppState::approval_key_target("bash", "cargo test"));
        assert!(scoped.approval_is_allowlisted("bash", Some("cargo test")));
        assert!(!scoped.approval_is_allowlisted("bash", Some("rm -rf /")));
    }

    #[test]
    fn approval_needed_event_populates_dialog_options_and_diff() {
        let mut state = state();
        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-1".to_string(),
            tool: "edit".to_string(),
            target: Some("src/auth/token.rs".to_string()),
            preview: Some("@@ token.rs @@\n- a\n+ b".to_string()),
        });
        let dialog = state.approval_dialog.expect("dialog present");
        assert_eq!(dialog.id, "approval-1");
        assert_eq!(dialog.options.len(), 4);
        assert!(dialog.diff.is_some());
        assert_eq!(dialog.current(), ApprovalOption::Once);
    }

    #[test]
    fn user_input_requested_event_tracks_pending_runtime_interaction_id() {
        let mut state = state();
        state.update(TuiEvent::UserInputRequested {
            id: "ask-1".to_string(),
            question: "Continue?".to_string(),
            choices: vec!["yes".to_string(), "no".to_string()],
        });

        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert_eq!(state.pending_user_input_id.as_deref(), Some("ask-1"));
    }

    #[test]
    fn mcp_elicitation_requested_event_tracks_pending_runtime_interaction_id() {
        let mut state = state();
        state.update(TuiEvent::McpElicitationRequested {
            id: "mcp_elicitation:github:42".to_string(),
            server_name: "github".to_string(),
            mode: RuntimeMcpElicitationMode::Url,
            message: "Authorize GitHub".to_string(),
            url: Some("https://github.com/login/device".to_string()),
            requested_schema_json: None,
        });

        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert_eq!(
            state.pending_user_input_id.as_deref(),
            Some("mcp_elicitation:github:42")
        );
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message))
                if message.contains("MCP github requests input: Authorize GitHub")
                    && message.contains("Mode: url")
                    && message.contains("URL: https://github.com/login/device")
        ));
    }

    #[test]
    fn subagent_events_update_existing_message() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });
        state.update(TuiEvent::SubagentCompleted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
            status: "completed".to_string(),
            output: Some("done".to_string()),
            error: None,
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::Subagent {
                id,
                description,
                status,
                output,
                error,
                ..
            } => {
                assert_eq!(id, "agent-1");
                assert_eq!(description, "inspect repo");
                assert_eq!(status, "completed");
                assert_eq!(output.as_deref(), Some("done"));
                assert!(error.is_none());
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn subagent_progress_updates_existing_message_without_adding_rows() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });
        state.update(TuiEvent::SubagentProgress {
            id: "agent-1".to_string(),
            activity: "bash: echo child".to_string(),
            turn: Some(1),
            usage: None,
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::Subagent {
                id,
                status,
                activity,
                activity_tail,
                turn,
                ..
            } => {
                assert_eq!(id, "agent-1");
                assert_eq!(status, "running");
                assert_eq!(activity.as_deref(), Some("bash: echo child"));
                assert_eq!(activity_tail, &vec!["bash: echo child".to_string()]);
                assert_eq!(*turn, Some(1));
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn subagent_progress_retains_recent_activity_tail() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });
        for index in 1..=8 {
            state.update(TuiEvent::SubagentProgress {
                id: "agent-1".to_string(),
                activity: format!("activity {index}"),
                turn: Some(index),
                usage: None,
            });
        }

        match &state.messages[0] {
            ChatMessage::Subagent {
                activity_tail,
                turn,
                ..
            } => {
                assert_eq!(*turn, Some(8));
                assert_eq!(activity_tail.len(), 6);
                assert_eq!(
                    activity_tail.first().map(String::as_str),
                    Some("activity 3")
                );
                assert_eq!(activity_tail.last().map(String::as_str), Some("activity 8"));
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn expand_toggle_flips_latest_live_subagent() {
        let mut state = state();

        state.update(TuiEvent::SubagentStarted {
            id: "agent-1".to_string(),
            description: "inspect repo".to_string(),
        });

        assert!(state.toggle_latest_tool_output());
        match &state.messages[0] {
            ChatMessage::Subagent { expanded, .. } => assert!(*expanded),
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn completed_subagent_without_start_adds_message() {
        let mut state = state();

        state.update(TuiEvent::SubagentCompleted {
            id: "agent-2".to_string(),
            description: "review code".to_string(),
            status: "failed".to_string(),
            output: None,
            error: Some("boom".to_string()),
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::Subagent {
                id,
                description,
                status,
                output,
                error,
                ..
            } => {
                assert_eq!(id, "agent-2");
                assert_eq!(description, "review code");
                assert_eq!(status, "failed");
                assert!(output.is_none());
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("expected subagent message, got {other:?}"),
        }
    }

    #[test]
    fn generic_subagent_tool_events_do_not_create_tool_rows() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "tool-subagent".to_string(),
            name: "subagent".to_string(),
            target: Some("inspect repo".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "tool-subagent".to_string(),
            name: "subagent".to_string(),
            status: "completed".to_string(),
            output: "Subagent status: success".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });

        assert!(state.messages.is_empty());
    }

    #[test]
    fn plan_lives_in_panel_during_turn_and_archives_inline_on_completion() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "tool-plan".to_string(),
            name: "update_plan".to_string(),
            target: Some("2 items".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "tool-plan".to_string(),
            name: "update_plan".to_string(),
            status: "completed".to_string(),
            output: "Plan updated".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });
        state.update(TuiEvent::PlanUpdated {
            explanation: Some("starting".to_string()),
            plan: vec![
                PlanItem {
                    step: "Inspect".to_string(),
                    status: PlanStatus::Completed,
                },
                PlanItem {
                    step: "Patch".to_string(),
                    status: PlanStatus::InProgress,
                },
            ],
        });

        // During the turn the plan only lives in the bottom panel, not the scrollback.
        assert!(state.messages.is_empty());
        assert!(state.current_plan.is_some());

        // When the turn completes the panel clears and the plan is archived inline.
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(state.current_plan.is_none());
        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::PlanUpdate { explanation, plan } => {
                assert_eq!(explanation.as_deref(), Some("starting"));
                assert_eq!(plan.len(), 2);
                assert_eq!(plan[1].step, "Patch");
            }
            other => panic!("expected plan update message, got {other:?}"),
        }
    }

    #[test]
    fn proposed_plan_tags_stream_as_dedicated_tui_message() {
        let mut state = state();

        state.update(TuiEvent::MessageDelta("Intro\n<proposed".to_string()));
        state.update(TuiEvent::MessageDelta(
            "_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro".to_string(),
        ));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert_eq!(state.messages.len(), 3);
        match &state.messages[0] {
            ChatMessage::Assistant(text) => assert_eq!(text, "Intro\n"),
            other => panic!("expected assistant preface, got {other:?}"),
        }
        match &state.messages[1] {
            ChatMessage::ProposedPlan(text) => assert_eq!(text, "# Plan\n- inspect\n"),
            other => panic!("expected proposed plan, got {other:?}"),
        }
        match &state.messages[2] {
            ChatMessage::Assistant(text) => assert_eq!(text, "\nOutro"),
            other => panic!("expected assistant postscript, got {other:?}"),
        }
    }

    #[test]
    fn failed_plan_update_marks_panel_stale_until_next_success() {
        let mut state = state();

        state.update(TuiEvent::PlanUpdated {
            explanation: None,
            plan: vec![PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::InProgress,
            }],
        });
        assert!(!state.plan_update_failed);

        state.update(TuiEvent::ToolCompleted {
            id: "tool-plan-2".to_string(),
            name: "update_plan".to_string(),
            status: "failed".to_string(),
            output: "tool arguments failed schema validation".to_string(),
            diff: None,
            kind: Some("error".to_string()),
        });
        assert!(
            state.plan_update_failed,
            "failed update must mark the panel stale"
        );
        assert!(state.current_plan.is_some(), "the stale plan stays visible");

        state.update(TuiEvent::PlanUpdated {
            explanation: None,
            plan: vec![PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::Completed,
            }],
        });
        assert!(
            !state.plan_update_failed,
            "a successful update clears the stale marker"
        );
    }

    #[test]
    fn turn_completion_clears_plan_stale_marker() {
        let mut state = state();
        state.update(TuiEvent::PlanUpdated {
            explanation: None,
            plan: vec![PlanItem {
                step: "Inspect".to_string(),
                status: PlanStatus::Pending,
            }],
        });
        state.update(TuiEvent::ToolCompleted {
            id: "tool-plan".to_string(),
            name: "update_plan".to_string(),
            status: "failed".to_string(),
            output: "schema validation".to_string(),
            diff: None,
            kind: Some("error".to_string()),
        });
        assert!(state.plan_update_failed);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(!state.plan_update_failed);
    }

    #[test]
    fn session_completion_finalizes_the_turn_and_freezes_it() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::MessageDelta("answer".to_string()));

        // Mid-turn nothing is finalized: the whole transcript is still live.
        assert_eq!(state.finalized_count, 0);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        // After completion every message is frozen.
        assert_eq!(state.finalized_count, state.messages.len());
        assert!(state.finalized_count > 0);
    }

    #[test]
    fn session_completion_without_receiving_tools_preserves_populated_render_cache() {
        let mut state = state();
        state.push_message(ChatMessage::Assistant("stable markdown".to_string()));
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        state.transcript_render_cache.prepare(
            &state.messages,
            &state.message_revisions,
            40,
            &theme,
            0,
            false,
            |message, _, _, _, _| match message {
                ChatMessage::Assistant(text) => vec![ratatui::text::Line::from(text.clone())],
                _ => unreachable!(),
            },
        );
        assert_eq!(state.transcript_render_cache.populated_len(), 1);

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert_eq!(state.transcript_render_cache.populated_len(), 1);
    }

    #[test]
    fn expand_toggle_only_affects_live_tools_not_flushed_ones() {
        let mut state = state();

        // Turn 1: a tool call that gets completed.
        state.update(TuiEvent::ToolRequested {
            id: "t1".to_string(),
            name: "grep".to_string(),
            target: Some("a".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "t1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "hit".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        // Simulate the render loop flushing the settled prefix into scrollback: once
        // `flushed_count` covers the tool it is committed to the immutable scrollback.
        state.flushed_count = state.messages.len();

        // The flushed tool is frozen: `e` finds nothing in the (empty) live pane.
        assert!(!state.toggle_latest_tool_output());
        let ChatMessage::ToolCall { expanded, .. } = &state.messages[0] else {
            panic!("expected flushed tool call");
        };
        assert!(!expanded, "flushed tool must stay collapsed");

        // Turn 2: a new live tool call (beyond `flushed_count`) can be expanded.
        state.update(TuiEvent::ToolRequested {
            id: "t2".to_string(),
            name: "grep".to_string(),
            target: Some("b".to_string()),
        });
        assert!(state.toggle_latest_tool_output());
        let ChatMessage::ToolCall { expanded, .. } = state.messages.last().unwrap() else {
            panic!("expected live tool call");
        };
        assert!(expanded, "live tool should toggle expanded");
    }

    #[test]
    fn clearing_messages_resets_the_finalized_watermark() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert!(state.finalized_count > 0);

        state.messages.clear();
        state.finalized_count = 0;

        // Watermark must never dangle past the (now empty) message list.
        assert_eq!(state.finalized_count, 0);
        assert!(state.messages.is_empty());
    }

    #[test]
    fn backtrack_clamps_watermark_into_remaining_messages() {
        let mut state = state();
        state.messages.push(ChatMessage::User("first".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("reply".to_string()));
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        let finalized_before = state.finalized_count;
        assert_eq!(finalized_before, 2);

        // A second user prompt starts a new live turn, then we backtrack it away.
        state.messages.push(ChatMessage::User("second".to_string()));
        state.remove_after_last_user();

        // Everything from the last user prompt onward is gone, and the watermark is
        // clamped so it can never exceed the remaining message count.
        assert!(state.finalized_count <= state.messages.len());
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn flushable_prefix_stops_at_a_running_tool_call() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::ToolRequested {
            id: "t1".to_string(),
            name: "grep".to_string(),
            target: Some("a".to_string()),
        });
        // User is settled, the running tool blocks everything after it.
        assert_eq!(state.flushable_prefix_end(false), 1);

        state.update(TuiEvent::ToolCompleted {
            id: "t1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "hit".to_string(),
            diff: None,
            kind: Some("success".to_string()),
        });
        // Now the completed tool can flush too.
        assert_eq!(state.flushable_prefix_end(false), 2);
    }

    #[test]
    fn flushable_prefix_stops_at_a_receiving_tool_call() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::ToolCallProgress {
            id: "t1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 1024,
        });

        assert_eq!(state.flushable_prefix_end(false), 1);
    }

    #[test]
    fn flushable_prefix_holds_back_the_streaming_tail_until_turn_end() {
        let mut state = state();
        state.messages.push(ChatMessage::User("hi".to_string()));
        state.update(TuiEvent::MessageDelta("partial".to_string()));

        // The trailing assistant block is still growing, so mid-turn only the user
        // prompt is flushable.
        assert_eq!(state.flushable_prefix_end(false), 1);
        // When the turn ends the tail is settled and the whole prefix can flush.
        assert_eq!(state.flushable_prefix_end(true), 2);
    }

    #[test]
    fn flushable_prefix_releases_an_assistant_block_once_a_newer_message_follows() {
        let mut state = state();
        state.update(TuiEvent::MessageDelta("first answer".to_string()));
        // While it is the last message it is still mutable.
        assert_eq!(state.flushable_prefix_end(false), 0);

        // A following tool call means the assistant block will never grow again.
        state.update(TuiEvent::ToolRequested {
            id: "t1".to_string(),
            name: "grep".to_string(),
            target: None,
        });
        state.update(TuiEvent::ToolCompleted {
            id: "t1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "out".to_string(),
            diff: None,
            kind: None,
        });
        assert_eq!(state.flushable_prefix_end(false), 2);
    }

    #[test]
    fn flushable_prefix_is_bounded_by_already_flushed_count() {
        let mut state = state();
        state.messages.push(ChatMessage::User("a".to_string()));
        state.messages.push(ChatMessage::System("b".to_string()));
        state.flushed_count = 1;
        // Counts the contiguous settled run starting from flushed_count, not from 0.
        assert_eq!(state.flushable_prefix_end(false), 2);

        state.flushed_count = 2;
        assert_eq!(state.flushable_prefix_end(false), 2);
    }

    #[test]
    fn session_completion_re_pins_to_bottom_after_incidental_scroll() {
        let mut state = state();
        state.enter_running();
        state.total_lines = 100;
        state.visible_height = 20;
        state.scroll_offset = 60;
        state.auto_scroll = false;
        state
            .messages
            .push(ChatMessage::Assistant("final answer".to_string()));

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert!(
            state.auto_scroll,
            "finished turns should leave the final answer pinned above the composer"
        );
        assert_eq!(state.scroll_offset, 80);
    }

    #[test]
    fn scroll_up_with_content_shorter_than_pane_keeps_auto_follow() {
        // First screen: everything fits, nothing to scroll. A stray wheel-up (trackpad
        // inertia, accidental touch) must not disarm auto-follow, or the transcript
        // stops tracking new streamed content once it grows past one screen and the
        // user is forced to scroll down by hand.
        let mut state = state();
        state.total_lines = 10;
        state.visible_height = 24;
        state.auto_scroll = true;

        state.scroll_up(3);

        assert!(
            state.auto_scroll,
            "wheel-up on a not-yet-overflowing transcript must keep auto-follow armed"
        );
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn scroll_up_with_overflow_disarms_auto_follow() {
        let mut state = state();
        state.total_lines = 100;
        state.visible_height = 24;
        state.scroll_offset = 76;
        state.auto_scroll = true;

        state.scroll_up(3);

        assert!(
            !state.auto_scroll,
            "wheel-up on an overflowing transcript should still let the user break away"
        );
        assert_eq!(state.scroll_offset, 73);
    }

    #[test]
    fn scroll_navigation_preserves_offsets_above_u16_max() {
        let mut state = state();
        state.total_lines = 100_000;
        state.visible_height = 20;
        state.scroll_offset = 70_000;
        state.auto_scroll = false;

        state.scroll_down(5_000usize);
        assert_eq!(state.scroll_offset, 75_000);
        state.scroll_up(10_000usize);
        assert_eq!(state.scroll_offset, 65_000);
    }

    #[test]
    fn scroll_down_saturates_when_total_height_reaches_usize_max() {
        let mut state = state();
        state.total_lines = usize::MAX;
        state.visible_height = 0;
        state.scroll_offset = usize::MAX - 1;
        state.auto_scroll = false;

        state.scroll_down(10usize);

        assert_eq!(state.scroll_offset, usize::MAX);
        assert!(state.auto_scroll);
    }

    #[test]
    fn session_completion_temporarily_ignores_inertial_mouse_scroll() {
        let mut state = state();
        state.enter_running();
        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        let completed_at = state
            .last_completed_at
            .expect("session completion should record completion time");

        assert!(
            !state.accepts_mouse_scroll_at(completed_at),
            "trackpad inertia immediately after completion must not undo bottom pinning"
        );
        assert!(
            state.accepts_mouse_scroll_at(completed_at + std::time::Duration::from_millis(900)),
            "manual mouse scrolling should work again after the completion grace period"
        );
    }

    #[test]
    fn backtrack_clamps_flushed_watermark_too() {
        let mut state = state();
        state.messages.push(ChatMessage::User("first".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("reply".to_string()));
        state.flushed_count = 2;
        state.finalized_count = 2;

        state.messages.push(ChatMessage::User("second".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("reply2".to_string()));
        state.remove_after_last_user();

        assert!(state.flushed_count <= state.messages.len());
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn completed_tool_event_preserves_result_kind() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "grep-1".to_string(),
            name: "grep".to_string(),
            target: Some("needle".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            id: "grep-1".to_string(),
            name: "grep".to_string(),
            status: "completed".to_string(),
            output: "(no matches)".to_string(),
            diff: None,
            kind: Some("no_matches".to_string()),
        });

        match &state.messages[0] {
            ChatMessage::ToolCall { kind, .. } => {
                assert_eq!(kind.as_deref(), Some("no_matches"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn tool_output_delta_updates_matching_tool_id() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "a".to_string(),
            name: "bash".to_string(),
            target: Some("first".to_string()),
        });
        state.update(TuiEvent::ToolRequested {
            id: "b".to_string(),
            name: "bash".to_string(),
            target: Some("second".to_string()),
        });
        state.update(TuiEvent::ToolOutputDelta {
            id: "a".to_string(),
            chunk: "one\n".to_string(),
        });

        match &state.messages[0] {
            ChatMessage::ToolCall { output, .. } => {
                assert_eq!(output.as_deref(), Some("one\n"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        match &state.messages[1] {
            ChatMessage::ToolCall { output, .. } => assert!(output.is_none()),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_progress_creates_and_updates_running_row() {
        let mut state = state();

        state.update(TuiEvent::ToolCallProgress {
            id: "call_1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        });
        state.update(TuiEvent::ToolCallProgress {
            id: "call_1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 24_690,
        });
        state.update(TuiEvent::ToolRequested {
            id: "call_1".to_string(),
            name: "write_file".to_string(),
            target: Some("big.js".to_string()),
        });

        assert_eq!(state.messages.len(), 1);
        match &state.messages[0] {
            ChatMessage::ToolCall {
                name,
                target,
                status,
                output,
                ..
            } => {
                assert_eq!(name, "write_file");
                assert_eq!(target.as_deref(), Some("big.js"));
                assert_eq!(status, "running");
                assert_eq!(output.as_deref(), Some("receiving arguments... 24.1 KB"));
            }
            other => panic!("expected tool progress row, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_progress_ignores_panel_owned_tools() {
        let mut state = state();

        state.update(TuiEvent::ToolCallProgress {
            id: "plan-1".to_string(),
            name: Some("update_plan".to_string()),
            arguments_bytes: 1024,
        });
        state.update(TuiEvent::ToolCallProgress {
            id: "subagent-1".to_string(),
            name: Some("subagent".to_string()),
            arguments_bytes: 2048,
        });

        assert!(state.messages.is_empty());
    }

    #[test]
    fn terminal_events_remove_orphan_receiving_tool_progress() {
        let mut state = state();

        state.update(TuiEvent::ToolCallProgress {
            id: "call_1".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        });
        state.update(TuiEvent::Error("failed to parse tool call".to_string()));

        assert!(
            state.messages.iter().all(|message| {
                !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
            }),
            "error should clear orphan receiving rows: {:?}",
            state.messages
        );

        state.update(TuiEvent::ToolCallProgress {
            id: "call_2".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 24_690,
        });
        state.update(TuiEvent::SessionCompleted {
            status: "cancelled".to_string(),
        });

        assert!(
            state.messages.iter().all(|message| {
                !matches!(message, ChatMessage::ToolCall { status, .. } if status == "receiving")
            }),
            "completion should clear orphan receiving rows: {:?}",
            state.messages
        );
    }

    #[test]
    fn clearing_receiving_progress_preserves_finalized_prefix_boundaries() {
        let mut state = state();
        state.messages.push(ChatMessage::ToolCall {
            id: "frozen".to_string(),
            name: "write_file".to_string(),
            target: None,
            status: "receiving".to_string(),
            output: Some("receiving arguments... 1 KB".to_string()),
            diff: None,
            kind: None,
            expanded: false,
        });
        state.finalized_count = 1;
        state.flushed_count = 1;

        state.update(TuiEvent::ToolCallProgress {
            id: "live".to_string(),
            name: Some("write_file".to_string()),
            arguments_bytes: 24_690,
        });
        state.update(TuiEvent::Error("failed".to_string()));

        assert_eq!(state.finalized_count, 1);
        assert_eq!(state.flushed_count, 1);
        assert_eq!(state.messages.len(), 2);
        match &state.messages[0] {
            ChatMessage::ToolCall { id, status, .. } => {
                assert_eq!(id, "frozen");
                assert_eq!(status, "receiving");
            }
            other => panic!("finalized prefix should be preserved, got {other:?}"),
        }
        assert!(matches!(state.messages[1], ChatMessage::Error(_)));
    }

    #[test]
    fn toggle_latest_tool_output_flips_expanded_state() {
        let mut state = state();

        state.update(TuiEvent::ToolRequested {
            id: "tool-1".to_string(),
            name: "grep".to_string(),
            target: None,
        });

        assert!(state.toggle_latest_tool_output());
        match &state.messages[0] {
            ChatMessage::ToolCall { expanded, .. } => assert!(*expanded),
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn workflow_panel_state_defaults_to_empty() {
        let state = state();

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert_eq!(state.workflow_panel.selected, 0);
        assert!(state.workflow_panel.tasks.is_empty());
    }

    #[test]
    fn show_workflows_preserves_available_selection() {
        let mut state = state();
        state.workflow_panel.tasks = vec![BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Running,
            is_backgrounded: false,
            description: "demo".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
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
            last_activity_at_ms: None,
            result: None,
            error: None,
        }];
        state.workflow_panel.selected = 9;

        state.show_workflows();

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn workflow_panel_selection_moves_within_available_tasks() {
        let mut state = state();
        state.workflow_panel.tasks = vec![
            workflow_task_summary("task-1", "audit"),
            workflow_task_summary("task-2", "repair"),
        ];

        state.select_next_workflow_task();
        assert_eq!(state.workflow_panel.selected, 1);

        state.select_next_workflow_task();
        assert_eq!(state.workflow_panel.selected, 1);

        state.select_previous_workflow_task();
        assert_eq!(state.workflow_panel.selected, 0);

        state.workflow_panel.tasks.clear();
        state.select_next_workflow_task();
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn selected_background_approval_task_opens_approval_dialog() {
        let mut state = state();
        let mut task = workflow_task_summary("task-approval", "approval");
        task.task_type = TaskType::MainSession;
        task.status = TaskStatus::ApprovalRequired;
        task.is_backgrounded = true;
        task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "mock-tool-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: Some("background task".to_string()),
            arguments: "{\"limit\":1}".to_string(),
        });
        state.workflow_panel.tasks = vec![task];

        assert!(state.open_selected_background_approval_dialog());

        assert_eq!(state.status, AppStatus::WaitingApproval);
        let dialog = state.approval_dialog.as_ref().expect("approval dialog");
        assert_eq!(dialog.tool, "task_list");
        assert_eq!(dialog.target.as_deref(), Some("background task"));
        assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
        assert_eq!(dialog.diff.as_deref(), Some("{\"limit\":1}"));
    }

    #[test]
    fn show_agents_uses_dedicated_panel_mode() {
        let mut state = state();
        state.workflow_panel.selected = 9;

        state.show_agents();

        assert_eq!(state.panel_mode, PanelMode::Agents);
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn workflow_events_update_panel_and_queue_model_notification() {
        let mut state = state();
        state.workflow_panel.selected = 9;

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![BackgroundTaskSummary {
                id: "task-1".to_string(),
                task_type: TaskType::Workflow,
                status: TaskStatus::Completed,
                is_backgrounded: false,
                description: "demo".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                command: None,
                agent_type: None,
                server: None,
                tool: None,
                pending_tool_call: None,
                name: Some("audit".to_string()),
                workflow_run_id: Some("workflow-run-1".to_string()),
                phase_count: Some(2),
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
                last_activity_at_ms: None,
                result: None,
                error: None,
            }],
        });
        state.update(TuiEvent::WorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done".to_string(),
        });

        assert_eq!(state.workflow_panel.tasks.len(), 1);
        assert_eq!(state.workflow_panel.selected, 0);
        let notification = state
            .pending_workflow_notifications
            .pop_front()
            .expect("pending workflow notification");
        assert_eq!(notification.id, "notification-1");
        assert_eq!(
            notification.prompt,
            "<task-notification>done</task-notification>"
        );
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message)) if message.contains("Workflow completed. audit: done")
        ));
    }

    #[test]
    fn duplicate_workflow_notification_id_is_not_queued_twice() {
        let mut state = state();

        state.update(TuiEvent::WorkflowNotification {
            id: "workflow-run-1:task-1:tool-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done".to_string(),
        });
        state.update(TuiEvent::WorkflowNotification {
            id: "workflow-run-1:task-1:tool-1".to_string(),
            prompt: "<task-notification>done again</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done again".to_string(),
        });

        assert_eq!(state.pending_workflow_notifications.len(), 1);
        assert_eq!(
            state.pending_workflow_notifications[0].prompt,
            "<task-notification>done</task-notification>"
        );
        let workflow_messages = state
            .messages
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    ChatMessage::System(text) if text.starts_with("Workflow completed.")
                )
            })
            .count();
        assert_eq!(workflow_messages, 1);
    }

    #[test]
    fn pending_workflow_notification_queue_owns_unique_drain_and_notification_pop() {
        let queue = PendingWorkflowNotificationQueue::new();
        assert!(queue.push_unique(PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>one</task-notification>".to_string(),
        }));
        assert!(!queue.push_unique(PendingWorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>duplicate</task-notification>".to_string(),
        }));

        let mut pending = VecDeque::new();
        queue.drain_into(&mut pending);
        assert!(queue.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "notification-1");

        assert!(queue.push_unique(PendingWorkflowNotification {
            id: "notification-2".to_string(),
            prompt: "<task-notification>two</task-notification>".to_string(),
        }));
        assert_eq!(
            queue
                .pop_notification()
                .as_ref()
                .map(|notification| (notification.id.as_str(), notification.prompt.as_str())),
            Some((
                "notification-2",
                "<task-notification>two</task-notification>"
            ))
        );
        assert!(queue.pop_notification().is_none());
    }

    #[test]
    fn workflow_task_updates_sort_actionable_active_then_recent_terminal_tasks() {
        let mut state = state();
        let mut completed = workflow_task_summary("task-completed", "completed");
        completed.status = TaskStatus::Completed;
        completed.completed_at_ms = Some(9_000);
        completed.last_activity_at_ms = Some(9_000);

        let mut running = workflow_task_summary("task-running", "running");
        running.status = TaskStatus::Running;
        running.last_activity_at_ms = Some(5_000);

        let mut approval = workflow_task_summary("task-approval", "approval");
        approval.task_type = TaskType::MainSession;
        approval.status = TaskStatus::ApprovalRequired;
        approval.is_backgrounded = true;
        approval.last_activity_at_ms = Some(1_000);

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![completed, running, approval],
        });

        assert_eq!(
            state
                .workflow_panel
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-approval", "task-running", "task-completed"]
        );
    }

    #[test]
    fn workflow_task_updates_preserve_selected_task_id_after_sorting() {
        let mut state = state();
        let mut running = workflow_task_summary("task-running", "running");
        running.status = TaskStatus::Running;
        running.last_activity_at_ms = Some(5_000);
        let mut completed = workflow_task_summary("task-completed", "completed");
        completed.status = TaskStatus::Completed;
        completed.completed_at_ms = Some(9_000);
        completed.last_activity_at_ms = Some(9_000);
        state.workflow_panel.tasks = vec![running.clone(), completed.clone()];
        state.workflow_panel.selected = 1;

        running.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![completed, running],
        });

        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-completed"
        );
    }

    #[test]
    fn single_task_status_updates_merge_without_dropping_other_panel_tasks() {
        let mut state = state();
        let mut running = workflow_task_summary("task-running", "running");
        running.status = TaskStatus::Running;
        running.last_activity_at_ms = Some(5_000);
        let mut completed = workflow_task_summary("task-completed", "completed");
        completed.status = TaskStatus::Completed;
        completed.completed_at_ms = Some(9_000);
        completed.last_activity_at_ms = Some(9_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![running.clone(), completed.clone()],
        });
        state.workflow_panel.selected = state
            .workflow_panel
            .tasks
            .iter()
            .position(|task| task.id == "task-completed")
            .expect("completed task remains visible");

        running.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTaskUpdated { task: running });

        assert_eq!(
            state
                .workflow_panel
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-running", "task-completed"]
        );
        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-completed"
        );
    }

    #[test]
    fn backgrounded_main_session_update_reveals_and_selects_task_panel_once() {
        let mut state = state();
        let mut backgrounded = workflow_task_summary("task-main", "backgrounded");
        backgrounded.task_type = TaskType::MainSession;
        backgrounded.status = TaskStatus::Running;
        backgrounded.is_backgrounded = true;
        backgrounded.last_activity_at_ms = Some(8_000);
        let mut workflow = workflow_task_summary("task-workflow", "workflow");
        workflow.status = TaskStatus::Running;
        workflow.last_activity_at_ms = Some(9_000);

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow.clone(), backgrounded.clone()],
        });

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-main"
        );

        state.workflow_panel.selected = state
            .workflow_panel
            .tasks
            .iter()
            .position(|task| task.id == "task-workflow")
            .expect("workflow task remains visible");
        backgrounded.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow, backgrounded],
        });

        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-workflow"
        );
    }

    #[test]
    fn backgrounded_approval_update_reveals_and_selects_task_panel_once() {
        let mut state = state();
        let mut approval = workflow_task_summary("task-approval", "approval");
        approval.task_type = TaskType::MainSession;
        approval.status = TaskStatus::ApprovalRequired;
        approval.is_backgrounded = true;
        approval.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "approval-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            arguments: "{}".to_string(),
        });
        approval.last_activity_at_ms = Some(8_000);
        let mut workflow = workflow_task_summary("task-workflow", "workflow");
        workflow.status = TaskStatus::Running;
        workflow.last_activity_at_ms = Some(9_000);

        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow.clone(), approval.clone()],
        });

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-approval"
        );

        state.workflow_panel.selected = state
            .workflow_panel
            .tasks
            .iter()
            .position(|task| task.id == "task-workflow")
            .expect("workflow task remains visible");
        approval.last_activity_at_ms = Some(10_000);
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![workflow, approval],
        });

        assert_eq!(
            state.workflow_panel.tasks[state.workflow_panel.selected].id,
            "task-workflow"
        );
    }

    #[test]
    fn backgrounded_main_session_suppresses_foreground_output_until_completion() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![BackgroundTaskSummary {
                id: "task-main".to_string(),
                task_type: TaskType::MainSession,
                status: TaskStatus::Running,
                is_backgrounded: true,
                description: "long answer".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: None,
                command: None,
                agent_type: Some("main-session".to_string()),
                server: None,
                tool: None,
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
                last_activity_at_ms: None,
                result: None,
                error: None,
            }],
        });

        state.update(TuiEvent::MessageDelta(
            "hidden background output".to_string(),
        ));
        assert!(state.messages.is_empty());

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        state.update(TuiEvent::TurnStarted {
            turn: 2,
            task: None,
        });
        state.update(TuiEvent::MessageDelta(
            "visible foreground output".to_string(),
        ));

        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::Assistant(text)) if text == "visible foreground output"
        ));
    }

    #[test]
    fn foregrounded_main_session_task_update_clears_output_suppression() {
        let mut state = state();
        state.suppress_background_main_session_output = true;

        let mut task = workflow_task_summary("task-main", "foregrounded");
        task.task_type = TaskType::MainSession;
        task.status = TaskStatus::Running;
        task.is_backgrounded = false;
        state.update(TuiEvent::WorkflowTasksUpdated { tasks: vec![task] });

        assert!(!state.suppress_background_main_session_output);
    }

    #[test]
    fn foregrounded_selected_main_session_returns_to_conversation_panel() {
        let mut state = state();
        state.panel_mode = PanelMode::Workflows;
        state.suppress_background_main_session_output = true;

        let mut selected = workflow_task_summary("task-main", "selected");
        selected.task_type = TaskType::MainSession;
        selected.status = TaskStatus::Running;
        selected.is_backgrounded = true;
        let mut other = workflow_task_summary("task-other", "other");
        other.status = TaskStatus::Running;
        state.workflow_panel.tasks = vec![selected.clone(), other.clone()];
        state.workflow_panel.selected = 0;

        selected.is_backgrounded = false;
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![selected, other],
        });

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(!state.suppress_background_main_session_output);
    }

    #[test]
    fn backgrounded_main_session_completion_adds_system_notice() {
        let mut state = state();
        state.update(TuiEvent::WorkflowTasksUpdated {
            tasks: vec![BackgroundTaskSummary {
                id: "task-main".to_string(),
                task_type: TaskType::MainSession,
                status: TaskStatus::Running,
                is_backgrounded: true,
                description: "long answer".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: None,
                command: None,
                agent_type: Some("main-session".to_string()),
                server: None,
                tool: None,
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
                last_activity_at_ms: None,
                result: None,
                error: None,
            }],
        });

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });

        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message))
                if message == "Background session completed: success"
        ));
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn active_goal_updates_do_not_mark_running_app_idle() {
        let mut state = state();
        state.status = AppStatus::Running;
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "keep going".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal.clone())));
        assert_eq!(state.status, AppStatus::Running);

        state.update(TuiEvent::GoalUpdated(goal));
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn goal_status_messages_compact_long_objectives() {
        let mut state = state();
        let objective = "目标内容很长".repeat(100);
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: objective.clone(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: Some(2_000),
            tokens_used: 1_500,
            time_used_seconds: 120,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal)));

        let Some(ChatMessage::System(message)) = state.messages.last() else {
            panic!("goal status should add a system message");
        };
        assert!(message.starts_with("Goal active · 目标内容"));
        assert!(message.contains('…'));
        assert!(message.ends_with("2m · 1.5K/2K tok"));
        assert!(!message.contains(&objective));
    }

    #[test]
    fn running_goal_does_not_repeat_unchanged_status_notice() {
        let mut state = state();
        state.status = AppStatus::Running;
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "keep going".to_string(),
            status: orca_core::goal_types::ThreadGoalStatus::Active,
            token_budget: None,
            tokens_used: 10,
            time_used_seconds: 120,
            created_at: 1,
            updated_at: 1,
        };

        state.update(TuiEvent::GoalStatus(Some(goal.clone())));
        state.update(TuiEvent::GoalStatus(Some(goal)));

        assert_eq!(
            state
                .messages
                .iter()
                .filter(|message| matches!(message, ChatMessage::System(text) if text.starts_with("Goal active")))
                .count(),
            1
        );
    }

    #[test]
    fn compacted_event_explains_runtime_recovery_reason() {
        let mut state = state();
        state.status = AppStatus::Compacting;

        state.update(TuiEvent::Compacted {
            before_messages: 12,
            after_messages: 5,
            reason: "prompt_too_long_recovery".to_string(),
            strategy: "remote_summary".to_string(),
            collapsed_messages: 7,
            status_text: "compacted context after prompt-too-long".to_string(),
        });

        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message))
                if message == "Compacted conversation context after prompt-too-long: 12 -> 5 messages (collapsed 7, remote_summary)."
        ));
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn compaction_lifecycle_sets_compacting_until_completion() {
        let mut state = state();

        state.update(TuiEvent::CompactionStarted);
        assert_eq!(state.status, AppStatus::Compacting);

        state.update(TuiEvent::Compacted {
            before_messages: 12,
            after_messages: 5,
            reason: "manual".to_string(),
            strategy: "manual".to_string(),
            collapsed_messages: 7,
            status_text: "compacted context manually".to_string(),
        });
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn running_timer_starts_and_stops_with_running_status() {
        let mut state = state();
        assert!(state.running_started_at.is_none());

        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        assert!(state.running_started_at.is_some());

        state.update(TuiEvent::SessionCompleted {
            status: "success".to_string(),
        });
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.running_started_at.is_none());
    }

    #[test]
    fn approval_round_trip_preserves_running_timer() {
        let mut state = state();
        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let started_at = Instant::now() - std::time::Duration::from_secs(65);
        state.running_started_at = Some(started_at);

        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-1".to_string(),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        });
        assert_eq!(state.status, AppStatus::WaitingApproval);
        assert_eq!(state.running_started_at, Some(started_at));

        state.enter_running();
        assert_eq!(state.status, AppStatus::Running);
        assert_eq!(state.running_started_at, Some(started_at));
    }
}
