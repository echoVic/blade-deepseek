use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
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

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        let index = self
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::User(_)))?;
        let prompt = match &self.messages[index] {
            Message::User(content) => content.clone(),
            _ => unreachable!("rposition only matches user messages"),
        };
        self.messages.truncate(index);
        Some(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_conversation_is_empty() {
        let conv = Conversation::new();
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn add_system_pushes_system_message() {
        let mut conv = Conversation::new();
        conv.add_system("you are helpful".to_string());
        assert!(matches!(&conv.messages[0], Message::System(s) if s == "you are helpful"));
    }

    #[test]
    fn add_user_pushes_user_message() {
        let mut conv = Conversation::new();
        conv.add_user("hello".to_string());
        assert!(matches!(&conv.messages[0], Message::User(s) if s == "hello"));
    }

    #[test]
    fn add_assistant_with_content_and_reasoning() {
        let mut conv = Conversation::new();
        conv.add_assistant(
            Some("answer".to_string()),
            Some("thinking".to_string()),
            vec![],
        );
        match &conv.messages[0] {
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("answer"));
                assert_eq!(reasoning_content.as_deref(), Some("thinking"));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn add_assistant_with_tool_calls() {
        let mut conv = Conversation::new();
        let tc = RawToolCall {
            id: "call_1".to_string(),
            function_name: "read_file".to_string(),
            arguments: r#"{"path":"x.rs"}"#.to_string(),
        };
        conv.add_assistant(None, None, vec![tc]);
        match &conv.messages[0] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].function_name, "read_file");
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn add_tool_result_pushes_tool_message() {
        let mut conv = Conversation::new();
        conv.add_tool_result("call_1".to_string(), "file contents".to_string());
        match &conv.messages[0] {
            Message::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(content, "file contents");
            }
            _ => panic!("expected Tool message"),
        }
    }

    #[test]
    fn last_user_message_returns_most_recent() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        conv.add_user("second".to_string());
        assert_eq!(conv.last_user_message(), Some("second"));
    }

    #[test]
    fn last_user_message_returns_none_when_empty() {
        let conv = Conversation::new();
        assert_eq!(conv.last_user_message(), None);
    }

    #[test]
    fn last_user_message_skips_non_user_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_assistant(Some("hi".to_string()), None, vec![]);
        assert_eq!(conv.last_user_message(), None);
    }

    #[test]
    fn backtrack_last_user_removes_user_and_later_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("first".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        conv.add_user("second".to_string());
        conv.add_assistant(Some("later".to_string()), None, vec![]);

        assert_eq!(conv.backtrack_last_user(), Some("second".to_string()));
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.last_user_message(), Some("first"));
    }
}
