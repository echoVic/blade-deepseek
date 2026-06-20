use std::sync::mpsc;
use std::time::Instant;

use orca_core::cost_types::UsageTotals;
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
    ApprovalNeeded {
        id: String,
        tool: String,
        target: Option<String>,
    },
    Notice(String),
    Error(String),
    UsageUpdated(UsageTotals),
    SessionCompleted {
        status: String,
    },
    Compacted {
        before_messages: usize,
        after_messages: usize,
    },
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
    Approve(bool),
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

#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub tool: String,
    pub target: Option<String>,
    pub selected: usize,
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
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub total_lines: u16,
    pub visible_height: u16,
    pub model_name: String,
    pub cwd: String,
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<UserAction>,
    pub approval_dialog: Option<ApprovalDialog>,
    pub setup_step: u8,
    pub show_shortcuts: bool,
    pub input_history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub draft_before_history: Option<String>,
    pub last_ctrl_c: Option<Instant>,
    pub session_picker_sessions: Vec<SessionSummary>,
    pub session_picker_selected: usize,
    pub usage: UsageTotals,
    pub slash_menu: Option<SlashMenu>,
    pub mention_candidates: Vec<String>,
    pub mention_selected: usize,
    pub current_plan: Option<(Option<String>, Vec<PlanItem>)>,
    pub panel_mode: PanelMode,
    pub workflow_panel: WorkflowPanelState,
    pub tick: u64,
}

impl AppState {
    pub fn new(event_tx: mpsc::Sender<UserAction>, model_name: String, cwd: String) -> Self {
        Self {
            messages: Vec::new(),
            status: AppStatus::Idle,
            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,
            visible_height: 0,
            model_name,
            cwd,
            event_tx,
            approval_dialog: None,
            setup_step: 0,
            show_shortcuts: false,
            input_history: Vec::new(),
            history_cursor: None,
            draft_before_history: None,
            last_ctrl_c: None,
            session_picker_sessions: Vec::new(),
            session_picker_selected: 0,
            usage: UsageTotals::default(),
            slash_menu: None,
            mention_candidates: Vec::new(),
            mention_selected: 0,
            current_plan: None,
            panel_mode: PanelMode::Conversation,
            workflow_panel: WorkflowPanelState::default(),
            tick: 0,
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

    pub fn select_previous_session(&mut self) {
        self.session_picker_selected = self.session_picker_selected.saturating_sub(1);
    }

    pub fn select_next_session(&mut self) {
        if self.session_picker_selected + 1 < self.session_picker_sessions.len() {
            self.session_picker_selected += 1;
        }
    }

    pub fn selected_session_id(&self) -> Option<String> {
        self.session_picker_sessions
            .get(self.session_picker_selected)
            .map(|session| session.session_id.clone())
    }

    pub fn update(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::TurnStarted { .. } => {
                self.status = AppStatus::Running;
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
            TuiEvent::ApprovalNeeded { tool, target, .. } => {
                self.status = AppStatus::WaitingApproval;
                self.approval_dialog = Some(ApprovalDialog {
                    tool,
                    target,
                    selected: 0,
                });
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
            TuiEvent::SessionCompleted { .. } => {
                self.promote_trailing_reasoning();
                self.status = AppStatus::Idle;
            }
            TuiEvent::Compacted {
                before_messages,
                after_messages,
            } => {
                self.messages.push(ChatMessage::System(format!(
                    "Compacted conversation context: {before_messages} -> {after_messages} messages."
                )));
                self.status = AppStatus::Idle;
            }
            TuiEvent::Backtracked { prompt } => {
                self.remove_after_last_user();
                self.messages.push(ChatMessage::System(format!(
                    "Backtracked to previous prompt: {}",
                    prompt.trim()
                )));
                self.status = AppStatus::Idle;
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
        AppState::new(tx, "mock".to_string(), "/tmp".to_string())
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
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            name: Some("audit".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
        }];
        state.workflow_panel.selected = 9;

        state.show_workflows();

        assert_eq!(state.panel_mode, PanelMode::Workflows);
        assert_eq!(state.workflow_panel.selected, 0);
    }
}
