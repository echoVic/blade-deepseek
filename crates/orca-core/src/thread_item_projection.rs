use serde::Serialize;
use serde_json::Value;

use crate::tool_types::{ToolInvocationStarted, ToolResultKind, ToolTerminal, ToolTerminalSource};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectedToolTerminalMetadata {
    #[serde(rename = "kind")]
    pub result_kind: ToolResultKind,
    #[serde(rename = "terminalSource", skip_serializing_if = "Option::is_none")]
    pub terminal_source: Option<ToolTerminalSource>,
    #[serde(rename = "invocationStarted", skip_serializing_if = "Option::is_none")]
    pub invocation_started: Option<ToolInvocationStarted>,
}

impl From<&ToolTerminal> for ProjectedToolTerminalMetadata {
    fn from(terminal: &ToolTerminal) -> Self {
        Self {
            result_kind: terminal.kind,
            terminal_source: (terminal.source != ToolTerminalSource::Observed)
                .then_some(terminal.source),
            invocation_started: (terminal.started != ToolInvocationStarted::Unknown)
                .then_some(terminal.started),
        }
    }
}

impl ProjectedToolTerminalMetadata {
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected tool terminal metadata serializes")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ProjectedUserMessageThreadItem {
    #[serde(rename = "user_message")]
    Started { role: &'static str, content: String },
}

impl ProjectedUserMessageThreadItem {
    pub fn new(content: impl Into<String>) -> Self {
        Self::Started {
            role: "user",
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ProjectedPersistedMessageThreadItem {
    System {
        role: &'static str,
        content: String,
    },
    User {
        role: &'static str,
        content: String,
    },
    Assistant {
        role: &'static str,
        content: Option<String>,
        #[serde(rename = "reasoningContent")]
        reasoning_content: Option<String>,
        #[serde(rename = "toolCalls")]
        tool_calls: Vec<Value>,
    },
    Tool {
        role: &'static str,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: String,
    },
}

impl ProjectedPersistedMessageThreadItem {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            role: "system",
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            role: "user",
            content: content.into(),
        }
    }

    pub fn assistant(
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<Value>,
    ) -> Self {
        Self::Assistant {
            role: "assistant",
            content,
            reasoning_content,
            tool_calls,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            role: "tool",
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ProjectedTextThreadItem {
    #[serde(rename = "agent_message")]
    AgentMessage { id: String, text: String },
    #[serde(rename = "plan")]
    Plan { id: String, text: String },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: String,
        content: String,
    },
}

impl ProjectedTextThreadItem {
    pub fn agent_message(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::AgentMessage {
            id: id.into(),
            text: text.into(),
        }
    }

    pub fn plan(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Plan {
            id: id.into(),
            text: text.into(),
        }
    }

    pub fn reasoning(id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::Reasoning {
            id: id.into(),
            summary: summary.into(),
            content: String::new(),
        }
    }

    pub fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected text thread item serializes")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectedTextItemKind {
    AgentMessage,
    Plan,
    Reasoning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedTextItem {
    kind: ProjectedTextItemKind,
    id: &'static str,
    text: String,
}

impl ProjectedTextItem {
    pub fn new(kind: ProjectedTextItemKind) -> Self {
        Self {
            kind,
            id: kind.id(),
            text: String::new(),
        }
    }

    pub fn id(&self) -> &str {
        self.id
    }

    pub fn push_delta(&mut self, delta: &str) {
        self.text.push_str(delta);
    }

    pub fn started_item(&self) -> Value {
        self.kind.item(self.id, "")
    }

    pub fn completed_item(self) -> Value {
        self.kind.item(self.id, self.text)
    }
}

impl ProjectedTextItemKind {
    fn id(self) -> &'static str {
        match self {
            Self::AgentMessage => "item-agent-message-1",
            Self::Plan => "item-plan-1",
            Self::Reasoning => "item-reasoning-1",
        }
    }

    fn item(self, id: impl Into<String>, text: impl Into<String>) -> Value {
        match self {
            Self::AgentMessage => ProjectedTextThreadItem::agent_message(id, text).into_value(),
            Self::Plan => ProjectedTextThreadItem::plan(id, text).into_value(),
            Self::Reasoning => ProjectedTextThreadItem::reasoning(id, text).into_value(),
        }
    }
}
