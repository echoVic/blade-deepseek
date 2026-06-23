use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Clone, Debug)]
pub enum Message {
    System {
        content: String,
        pinned: bool,
    },
    User {
        content: String,
        pinned: bool,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        pinned: bool,
    },
}

impl Message {
    pub fn system(content: String) -> Self {
        Self::System {
            content,
            pinned: false,
        }
    }

    pub fn pinned_system(content: String) -> Self {
        Self::System {
            content,
            pinned: true,
        }
    }

    pub fn user(content: String) -> Self {
        Self::User {
            content,
            pinned: false,
        }
    }

    pub fn pinned_user(content: String) -> Self {
        Self::User {
            content,
            pinned: true,
        }
    }

    pub fn is_pinned(&self) -> bool {
        match self {
            Self::System { pinned, .. }
            | Self::User { pinned, .. }
            | Self::Assistant { pinned, .. }
            | Self::Tool { pinned, .. } => *pinned,
        }
    }

    pub fn content_str(&self) -> Option<&str> {
        match self {
            Self::System { content, .. }
            | Self::User { content, .. }
            | Self::Tool { content, .. } => Some(content),
            Self::Assistant { content, .. } => content.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct VolatileContext {
    pub plan: Option<String>,
    pub goal: Option<String>,
    pub skill: Option<String>,
}

impl VolatileContext {
    pub fn is_empty(&self) -> bool {
        self.plan.is_none() && self.goal.is_none() && self.skill.is_none()
    }

    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        if let Some(goal) = &self.goal {
            parts.push(goal.as_str());
        }
        if let Some(plan) = &self.plan {
            parts.push(plan.as_str());
        }
        if let Some(skill) = &self.skill {
            parts.push(skill.as_str());
        }
        parts.join("\n\n")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SummaryState {
    pub baseline: Option<String>,
    pub deltas: Vec<String>,
}

impl SummaryState {
    pub fn is_empty(&self) -> bool {
        self.baseline.is_none() && self.deltas.is_empty()
    }

    pub fn latest_rolling(&self) -> Option<&str> {
        if let Some(last_delta) = self.deltas.last() {
            Some(last_delta.as_str())
        } else {
            self.baseline.as_deref()
        }
    }

    pub fn total_tokens(&self, counter: &impl TokenCountable) -> usize {
        let mut total = 0;
        if let Some(baseline) = &self.baseline {
            total += counter.count(baseline);
        }
        for delta in &self.deltas {
            total += counter.count(delta);
        }
        total
    }
}

pub trait TokenCountable {
    fn count(&self, text: &str) -> usize;
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub volatile: VolatileContext,
    pub rolling_summary: Option<String>,
    pub summary: SummaryState,
}

impl Conversation {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            volatile: VolatileContext::default(),
            rolling_summary: None,
            summary: SummaryState::default(),
        }
    }

    pub fn add_system(&mut self, content: String) {
        self.messages.push(Message::system(content));
    }

    pub fn add_system_pinned(&mut self, content: String) {
        self.messages.push(Message::pinned_system(content));
    }

    pub fn replace_plan_state(&mut self, content: String) {
        self.volatile.plan = Some(content);
    }

    pub fn replace_goal_state(&mut self, content: String) {
        self.volatile.goal = Some(format!("[Goal state]\n{content}"));
    }

    pub fn replace_skill_context(&mut self, content: Option<String>) {
        self.volatile.skill = content
            .filter(|text| !text.trim().is_empty())
            .map(|text| format!("[Skill context]\n{text}"));
    }

    pub fn strip_legacy_pinned_volatile(&mut self) {
        self.messages.retain(|msg| {
            if let Message::System {
                content,
                pinned: true,
            } = msg
            {
                !content.starts_with("[Pinned plan state]")
                    && !content.starts_with("[Pinned goal state]")
                    && !content.starts_with("[Pinned skill context]")
            } else {
                true
            }
        });
    }

    pub fn strip_legacy_summary_messages(&mut self) {
        self.messages.retain(|msg| {
            !matches!(
                msg,
                Message::System { content, pinned: false }
                if content.starts_with("[Summary of earlier conversation]")
                || content.starts_with("[Earlier conversation history was truncated")
            )
        });
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message::user(content));
    }

    pub fn add_user_pinned(&mut self, content: String) {
        self.messages.push(Message::pinned_user(content));
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
            pinned: false,
        });
    }

    pub fn add_tool_result(&mut self, tool_call_id: String, content: String) {
        self.messages.push(Message::Tool {
            tool_call_id,
            content,
            pinned: false,
        });
    }

    pub fn last_user_message(&self) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match msg {
            Message::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        let index = self
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::User { .. }))?;
        let prompt = match &self.messages[index] {
            Message::User { content, .. } => content.clone(),
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
        assert!(
            matches!(&conv.messages[0], Message::System { content, .. } if content == "you are helpful")
        );
    }

    #[test]
    fn add_user_pushes_user_message() {
        let mut conv = Conversation::new();
        conv.add_user("hello".to_string());
        assert!(matches!(&conv.messages[0], Message::User { content, .. } if content == "hello"));
    }

    #[test]
    fn add_pinned_user_marks_message_pinned() {
        let mut conv = Conversation::new();
        conv.add_user_pinned("keep this constraint".to_string());

        assert!(conv.messages[0].is_pinned());
    }

    #[test]
    fn replace_goal_state_keeps_single_volatile_goal() {
        let mut conv = Conversation::new();
        conv.replace_goal_state("first".to_string());
        conv.replace_goal_state("second".to_string());

        assert!(conv.messages.is_empty());
        assert!(conv.volatile.goal.as_ref().unwrap().contains("second"));
        assert!(!conv.volatile.goal.as_ref().unwrap().contains("first"));
    }

    #[test]
    fn replace_skill_context_updates_volatile_skill() {
        let mut conv = Conversation::new();
        conv.replace_skill_context(Some("first".to_string()));
        conv.replace_skill_context(Some("second".to_string()));

        assert!(conv.messages.is_empty());
        assert!(conv.volatile.skill.as_ref().unwrap().contains("second"));

        conv.replace_skill_context(None);
        assert!(conv.volatile.skill.is_none());
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
                ..
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
                ..
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

    /// DeepSeek prefix cache requires that a normal turn only *appends* to the
    /// conversation: the existing prefix (every earlier message) must stay
    /// byte-identical so the server-side cache keeps hitting. This locks that
    /// `add_*` never rewrites or reorders prior messages.
    #[test]
    fn turn_appends_never_mutate_the_existing_prefix() {
        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("first request".to_string());
        conv.add_assistant(
            Some("calling a tool".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x.rs"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());

        let prefix_snapshot = render_prefix(&conv);

        // A second turn: append a new user message, assistant reply, and tool result.
        conv.add_user("second request".to_string());
        conv.add_assistant(Some("answer".to_string()), None, vec![]);

        // The first four messages must be byte-identical to the snapshot.
        let prefix_after = render_prefix(&conv);
        assert_eq!(&prefix_after[..prefix_snapshot.len()], &prefix_snapshot[..]);
        assert_eq!(prefix_snapshot.len(), 4);
        assert_eq!(prefix_after.len(), 6);
    }

    /// Renders each message into a stable string so prefix equality can be
    /// asserted across mutations.
    fn render_prefix(conv: &Conversation) -> Vec<String> {
        conv.messages
            .iter()
            .map(|message| match message {
                Message::System { content, pinned } => format!("sys|{pinned}|{content}"),
                Message::User { content, pinned } => format!("usr|{pinned}|{content}"),
                Message::Assistant {
                    content,
                    reasoning_content,
                    tool_calls,
                    pinned,
                } => format!(
                    "ast|{pinned}|{content:?}|{reasoning_content:?}|{}",
                    tool_calls
                        .iter()
                        .map(|tc| format!("{}:{}:{}", tc.id, tc.function_name, tc.arguments))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                Message::Tool {
                    tool_call_id,
                    content,
                    pinned,
                } => format!("tool|{pinned}|{tool_call_id}|{content}"),
            })
            .collect()
    }

    #[test]
    fn volatile_updates_never_touch_messages() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);

        let snapshot = render_prefix(&conv);

        conv.replace_plan_state("step 1: do X".to_string());
        conv.replace_goal_state("build a widget".to_string());
        conv.replace_skill_context(Some("rust expertise".to_string()));

        assert_eq!(render_prefix(&conv), snapshot);
        assert_eq!(conv.messages.len(), 3);
        assert!(conv.volatile.plan.is_some());
        assert!(conv.volatile.goal.is_some());
        assert!(conv.volatile.skill.is_some());
    }

    #[test]
    fn multiple_plan_updates_only_change_volatile_not_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("do work".to_string());

        let snapshot = render_prefix(&conv);

        conv.replace_plan_state("plan v1".to_string());
        conv.replace_plan_state("plan v2".to_string());
        conv.replace_plan_state("plan v3".to_string());

        assert_eq!(render_prefix(&conv), snapshot);
        assert_eq!(conv.volatile.plan.as_deref(), Some("plan v3"));
    }

    #[test]
    fn strip_legacy_pinned_volatile_removes_old_format_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("hello".to_string());
        conv.messages.push(Message::pinned_system(
            "[Pinned plan state]\nold plan".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Pinned goal state]\nold goal".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Pinned skill context]\nold skill".to_string(),
        ));
        conv.add_assistant(Some("reply".to_string()), None, vec![]);

        assert_eq!(conv.messages.len(), 6);
        conv.strip_legacy_pinned_volatile();
        assert_eq!(conv.messages.len(), 3);
        assert!(
            matches!(&conv.messages[0], Message::System { content, pinned: false } if content == "sys")
        );
        assert!(matches!(&conv.messages[1], Message::User { content, .. } if content == "hello"));
        assert!(matches!(&conv.messages[2], Message::Assistant { .. }));
    }

    #[test]
    fn strip_legacy_preserves_non_volatile_pinned() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user_pinned("important constraint".to_string());
        conv.messages.push(Message::pinned_system(
            "[Pinned plan state]\nold".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Hook context]\nkeep this".to_string(),
        ));

        conv.strip_legacy_pinned_volatile();
        assert_eq!(conv.messages.len(), 3);
        assert!(conv.messages[1].is_pinned());
        assert!(
            matches!(&conv.messages[2], Message::System { content, pinned: true } if content.contains("Hook context"))
        );
    }
}
