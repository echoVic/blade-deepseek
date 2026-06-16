use std::sync::mpsc;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TuiEvent {
    TurnStarted {
        turn: u32,
    },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolRequested {
        name: String,
        target: Option<String>,
    },
    ToolCompleted {
        name: String,
        status: String,
        output: String,
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
    Error(String),
    SessionCompleted {
        status: String,
    },
}

#[derive(Debug, Clone)]
pub enum UserAction {
    Submit(String),
    Approve(bool),
    Interrupt,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    Setup,
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
        name: String,
        target: Option<String>,
        status: String,
        output: Option<String>,
    },
    Subagent {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub tool: String,
    pub target: Option<String>,
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
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<UserAction>,
    pub approval_dialog: Option<ApprovalDialog>,
    pub setup_step: u8,
}

impl AppState {
    pub fn new(event_tx: mpsc::Sender<UserAction>, model_name: String) -> Self {
        Self {
            messages: Vec::new(),
            status: AppStatus::Idle,
            scroll_offset: 0,
            auto_scroll: true,
            total_lines: 0,
            visible_height: 0,
            model_name,
            event_tx,
            approval_dialog: None,
            setup_step: 0,
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
            TuiEvent::ToolRequested { name, target } => {
                if name == "subagent" {
                    return;
                }
                self.messages.push(ChatMessage::ToolCall {
                    name,
                    target,
                    status: "running".to_string(),
                    output: None,
                });
            }
            TuiEvent::ToolCompleted {
                name,
                status,
                output,
            } => {
                if name == "subagent" {
                    return;
                }
                let updated = if let Some(ChatMessage::ToolCall {
                    name: existing_name,
                    status: s,
                    output: o,
                    ..
                }) = self.messages.last_mut()
                {
                    if existing_name == &name {
                        *s = status.clone();
                        *o = if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        };
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !updated {
                    self.messages.push(ChatMessage::ToolCall {
                        name,
                        target: None,
                        status,
                        output: if output.is_empty() {
                            None
                        } else {
                            Some(output)
                        },
                    });
                }
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
            TuiEvent::SessionCompleted { .. } => {
                self.status = AppStatus::Idle;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let (tx, _rx) = mpsc::channel();
        AppState::new(tx, "mock".to_string())
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
            name: "subagent".to_string(),
            target: Some("inspect repo".to_string()),
        });
        state.update(TuiEvent::ToolCompleted {
            name: "subagent".to_string(),
            status: "completed".to_string(),
            output: "Subagent status: success".to_string(),
        });

        assert!(state.messages.is_empty());
    }
}
