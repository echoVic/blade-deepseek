use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::Instant;

use orca_core::cost_types::UsageTotals;
use orca_core::goal_types::ThreadGoal;
use orca_core::plan_types::PlanItem;
use orca_core::task_types::BackgroundTaskSummary;
use orca_runtime::history::SessionSummary;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TuiEvent {
    TurnStarted {
        turn: u32,
    },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolRequested {
        id: String,
        name: String,
        target: Option<String>,
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
    WorkflowTasksUpdated {
        tasks: Vec<BackgroundTaskSummary>,
    },
    WorkflowNotification {
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
    UserInputRequested {
        id: String,
        question: String,
        choices: Vec<String>,
    },
    Notice(String),
    Error(String),
    UsageUpdated(UsageTotals),
    ContextUpdated {
        used_tokens: usize,
        limit_tokens: usize,
    },
    SessionCompleted {
        status: String,
    },
    Compacted {
        before_messages: usize,
        after_messages: usize,
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
    SetModel(String),
    Remember(String),
    Compact,
    GoalShow,
    GoalSet(String),
    GoalEdit(String),
    GoalClear,
    GoalPause,
    GoalResume,
    Approve(bool),
    RespondToUserInput(String),
    Backtrack,
    Interrupt,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    Setup,
    SessionPicker,
    Idle,
    Running,
    WaitingApproval,
    WaitingUserInput,
}

#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    Reasoning(String),
    Assistant(String),
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
            ApprovalOption::Once => 'y',
            ApprovalOption::AlwaysTool => 'a',
            ApprovalOption::AlwaysTarget => 'A',
            ApprovalOption::Deny => 'n',
        }
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
    pub tool: String,
    pub target: Option<String>,
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
                ApprovalOption::AlwaysTool,
                ApprovalOption::AlwaysTarget,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    Conversation,
    Workflows,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowPanelState {
    pub selected: usize,
    pub tasks: Vec<BackgroundTaskSummary>,
}

#[derive(Debug, Clone)]
pub struct SlashMenuItem {
    pub command: &'static str,
    pub description: &'static str,
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
}

pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub status: AppStatus,
    pub running_started_at: Option<Instant>,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub total_lines: u16,
    pub visible_height: u16,
    pub app_version: String,
    pub model_name: String,
    pub cwd: String,
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<UserAction>,
    pub approval_dialog: Option<ApprovalDialog>,
    /// Tool / "tool\u{0}target" keys the user chose to always allow this
    /// session. Checked when a new approval arrives so the dialog is skipped.
    pub approval_allowlist: std::collections::HashSet<String>,
    pub setup_step: u8,
    pub show_shortcuts: bool,
    pub input_history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub draft_before_history: Option<String>,
    pub last_ctrl_c: Option<Instant>,
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
    pub current_goal: Option<ThreadGoal>,
    pub panel_mode: PanelMode,
    pub workflow_panel: WorkflowPanelState,
    pub pending_workflow_notifications: VecDeque<String>,
    pub tick: u64,
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
            status: AppStatus::Idle,
            running_started_at: None,
            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,
            visible_height: 0,
            app_version,
            model_name,
            cwd,
            event_tx,
            approval_dialog: None,
            approval_allowlist: std::collections::HashSet::new(),
            setup_step: 0,
            show_shortcuts: false,
            input_history: Vec::new(),
            history_cursor: None,
            draft_before_history: None,
            last_ctrl_c: None,
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
            current_goal: None,
            panel_mode: PanelMode::Conversation,
            workflow_panel: WorkflowPanelState::default(),
            pending_workflow_notifications: VecDeque::new(),
            tick: 0,
        }
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
            AppStatus::WaitingApproval | AppStatus::WaitingUserInput
        ) {
            self.status = status;
        } else {
            self.status = status;
            self.running_started_at = None;
        }
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, lines: u16) {
        let max_scroll = self.total_lines.saturating_sub(self.visible_height);
        self.scroll_offset = (self.scroll_offset + lines).min(max_scroll);
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

    pub fn toggle_shortcuts(&mut self) {
        self.show_shortcuts = !self.show_shortcuts;
    }

    pub fn show_workflows(&mut self) {
        self.panel_mode = PanelMode::Workflows;
        if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
            self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
        }
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
        for message in self.messages.iter_mut().rev() {
            if let ChatMessage::ToolCall { expanded, .. } = message {
                *expanded = !*expanded;
                return true;
            }
        }
        false
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
                self.enter_running();
            }
            TuiEvent::ReasoningDelta(text) => {
                if let Some(ChatMessage::Reasoning(existing)) = self.messages.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.messages.push(ChatMessage::Reasoning(text));
                }
            }
            TuiEvent::MessageDelta(text) => {
                if let Some(ChatMessage::Assistant(existing)) = self.messages.last_mut() {
                    existing.push_str(&text);
                } else {
                    self.messages.push(ChatMessage::Assistant(text));
                }
            }
            TuiEvent::ToolRequested { id, name, target } => {
                if name == "subagent" || name == "update_plan" {
                    return;
                }
                self.messages.push(ChatMessage::ToolCall {
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
            TuiEvent::ToolOutputDelta { id, chunk } => {
                if let Some(ChatMessage::ToolCall { output, .. }) =
                    self.messages.iter_mut().rev().find(|message| {
                        matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == &id)
                    })
                {
                    output.get_or_insert_with(String::new).push_str(&chunk);
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
                if name == "subagent" || name == "update_plan" {
                    return;
                }
                let updated = if let Some(ChatMessage::ToolCall {
                    id: existing_id,
                    name: existing_name,
                    status: s,
                    output: o,
                    diff: d,
                    kind: k,
                    ..
                }) = self.messages.iter_mut().rev().find(|message| {
                    matches!(message, ChatMessage::ToolCall { id: existing_id, .. } if existing_id == &id)
                })
                {
                    *existing_id = id.clone();
                    *existing_name = name.clone();
                    *s = status.clone();
                    if o.is_none() || output.is_empty() {
                        *o = if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        };
                    }
                    *d = diff.clone();
                    *k = kind.clone();
                    true
                } else {
                    false
                };
                if !updated {
                    self.messages.push(ChatMessage::ToolCall {
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
                self.current_plan = if plan.is_empty() {
                    None
                } else {
                    Some((explanation.clone(), plan.clone()))
                };
                self.messages
                    .push(ChatMessage::PlanUpdate { explanation, plan });
            }
            TuiEvent::SubagentStarted { id, description } => {
                self.messages.push(ChatMessage::Subagent {
                    id,
                    description,
                    status: "running".to_string(),
                    output: None,
                    error: None,
                });
            }
            TuiEvent::SubagentCompleted {
                id,
                description,
                status,
                output,
                error,
            } => {
                let updated = self.messages.iter_mut().rev().find_map(|msg| {
                    if let ChatMessage::Subagent {
                        id: existing_id,
                        status: existing_status,
                        output: existing_output,
                        error: existing_error,
                        ..
                    } = msg
                    {
                        if existing_id == &id {
                            *existing_status = status.clone();
                            *existing_output = output.clone();
                            *existing_error = error.clone();
                            return Some(());
                        }
                    }
                    None
                });

                if updated.is_none() {
                    self.messages.push(ChatMessage::Subagent {
                        id,
                        description,
                        status,
                        output,
                        error,
                    });
                }
            }
            TuiEvent::WorkflowTasksUpdated { tasks } => {
                self.workflow_panel.tasks = tasks;
                if self.workflow_panel.selected >= self.workflow_panel.tasks.len() {
                    self.workflow_panel.selected = self.workflow_panel.tasks.len().saturating_sub(1);
                }
            }
            TuiEvent::WorkflowNotification {
                prompt,
                status,
                summary,
            } => {
                self.pending_workflow_notifications.push_back(prompt);
                self.messages.push(ChatMessage::System(format!(
                    "Workflow {status}. {summary}"
                )));
            }
            TuiEvent::ApprovalNeeded {
                tool,
                target,
                preview,
                ..
            } => {
                self.set_status(AppStatus::WaitingApproval);
                let options = ApprovalDialog::options_for(&tool, target.as_deref());
                self.approval_dialog = Some(ApprovalDialog {
                    tool,
                    target,
                    selected: 0,
                    options,
                    diff: preview,
                });
            }
            TuiEvent::UserInputRequested {
                question, choices, ..
            } => {
                self.set_status(AppStatus::WaitingUserInput);
                let mut message = question;
                if !choices.is_empty() {
                    message.push_str("\nChoices: ");
                    message.push_str(&choices.join(", "));
                }
                self.messages.push(ChatMessage::System(message));
            }
            TuiEvent::Error(msg) => {
                self.messages.push(ChatMessage::Error(msg));
            }
            TuiEvent::Notice(msg) => {
                self.messages.push(ChatMessage::System(msg));
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
            TuiEvent::SessionCompleted { .. } => {
                self.promote_trailing_reasoning();
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::Compacted {
                before_messages,
                after_messages,
            } => {
                self.messages.push(ChatMessage::System(format!(
                    "Compacted conversation context: {before_messages} -> {after_messages} messages."
                )));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::GoalUpdated(goal) => {
                let summary = orca_core::goal_types::goal_usage_summary(&goal);
                let label = orca_core::goal_types::goal_status_label(goal.status);
                let should_keep_running =
                    self.status == AppStatus::Running && goal.status.should_continue();
                self.current_goal = Some(goal);
                self.messages
                    .push(ChatMessage::System(format!("Goal {label}. {summary}")));
                if !should_keep_running {
                    self.set_status(AppStatus::Idle);
                }
            }
            TuiEvent::GoalCleared => {
                self.current_goal = None;
                self.messages
                    .push(ChatMessage::System("Goal cleared.".to_string()));
                self.set_status(AppStatus::Idle);
            }
            TuiEvent::GoalStatus(goal) => {
                self.current_goal = goal.clone();
                let mut should_keep_running = false;
                match goal {
                    Some(goal) => {
                        should_keep_running =
                            self.status == AppStatus::Running && goal.status.should_continue();
                        let label = orca_core::goal_types::goal_status_label(goal.status);
                        let summary = orca_core::goal_types::goal_usage_summary(&goal);
                        self.messages
                            .push(ChatMessage::System(format!("Goal {label}. {summary}")));
                    }
                    None => self
                        .messages
                        .push(ChatMessage::System("No goal is currently set.".to_string())),
                }
                if !should_keep_running {
                    self.set_status(AppStatus::Idle);
                }
            }
            TuiEvent::Backtracked { prompt } => {
                self.remove_after_last_user();
                self.messages.push(ChatMessage::System(format!(
                    "Backtracked to previous prompt: {}",
                    prompt.trim()
                )));
                self.set_status(AppStatus::Idle);
            }
        }
    }

    fn promote_trailing_reasoning(&mut self) {
        if let Some(ChatMessage::Reasoning(text)) = self.messages.last() {
            let text = text.clone();
            self.messages.pop();
            self.messages.push(ChatMessage::Assistant(text));
        }
    }

    pub fn remove_after_last_user(&mut self) {
        if let Some(index) = self
            .messages
            .iter()
            .rposition(|message| matches!(message, ChatMessage::User(_)))
        {
            self.messages.truncate(index);
        }
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
        }
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
    fn approval_dialog_has_four_options_with_target_and_three_without() {
        // Static-target tool (like read_file) shows AlwaysTarget option.
        let with_target = ApprovalDialog::options_for("read_file", Some("src/auth/token.rs"));
        assert_eq!(
            with_target,
            vec![
                ApprovalOption::Once,
                ApprovalOption::AlwaysTool,
                ApprovalOption::AlwaysTarget,
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
        assert_eq!(dialog.options.len(), 4);
        assert!(dialog.diff.is_some());
        assert_eq!(dialog.current(), ApprovalOption::Once);
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
    fn update_plan_events_create_plan_message_without_tool_rows() {
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
            description: "demo".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            usage: None,
        }];
        state.workflow_panel.selected = 9;

        state.show_workflows();

        assert_eq!(state.panel_mode, PanelMode::Workflows);
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
                description: "demo".to_string(),
                created_at_ms: 1_000,
                started_at_ms: Some(1_000),
                completed_at_ms: Some(2_000),
                command: None,
                agent_type: None,
                server: None,
                tool: None,
                name: Some("audit".to_string()),
                workflow_run_id: Some("workflow-run-1".to_string()),
                phase_count: Some(2),
                workflow_progress: None,
                workflow_phases: Vec::new(),
                workflow_agents: Vec::new(),
                usage: None,
            }],
        });
        state.update(TuiEvent::WorkflowNotification {
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "audit: done".to_string(),
        });

        assert_eq!(state.workflow_panel.tasks.len(), 1);
        assert_eq!(state.workflow_panel.selected, 0);
        assert_eq!(
            state.pending_workflow_notifications.pop_front().as_deref(),
            Some("<task-notification>done</task-notification>")
        );
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(message)) if message.contains("Workflow completed. audit: done")
        ));
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
    fn running_timer_starts_and_stops_with_running_status() {
        let mut state = state();
        assert!(state.running_started_at.is_none());

        state.update(TuiEvent::TurnStarted { turn: 1 });
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
        state.update(TuiEvent::TurnStarted { turn: 1 });
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
