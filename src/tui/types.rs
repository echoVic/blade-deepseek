use std::sync::mpsc;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TuiEvent {
    TurnStarted { turn: u32 },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolRequested { name: String, target: Option<String> },
    ToolCompleted { name: String, status: String, output: String },
    ApprovalNeeded { id: String, tool: String, target: Option<String> },
    Error(String),
    SessionCompleted { status: String },
}

#[derive(Debug, Clone)]
pub enum UserAction {
    Submit(String),
    Approve(bool),
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
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
    Error(String),
}

pub struct AppState {
    pub messages: Vec<ChatMessage>,
    pub status: AppStatus,
    #[allow(dead_code)]
    pub scroll_offset: u16,
    pub model_name: String,
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<UserAction>,
    pub approval_info: Option<String>,
}

impl AppState {
    pub fn new(event_tx: mpsc::Sender<UserAction>, model_name: String) -> Self {
        Self {
            messages: Vec::new(),
            status: AppStatus::Idle,
            scroll_offset: 0,
            model_name,
            event_tx,
            approval_info: None,
        }
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
                        output: if output.is_empty() { None } else { Some(output) },
                    });
                }
            }
            TuiEvent::ApprovalNeeded { tool, target, .. } => {
                self.status = AppStatus::WaitingApproval;
                let info = match target {
                    Some(t) => format!("{tool}: {t}"),
                    None => tool,
                };
                self.approval_info = Some(info);
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
