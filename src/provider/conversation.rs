#[derive(Clone, Debug)]
pub struct RawToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Clone, Debug)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    pub fn add_system(&mut self, content: String) {
        self.messages.push(Message::System(content));
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message::User(content));
    }

    pub fn add_assistant(
        &mut self,
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
    ) {
        self.messages.push(Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        });
    }

    pub fn add_tool_result(&mut self, tool_call_id: String, content: String) {
        self.messages.push(Message::Tool {
            tool_call_id,
            content,
        });
    }

    pub fn last_user_message(&self) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match msg {
            Message::User(content) => Some(content.as_str()),
            _ => None,
        })
    }
}
