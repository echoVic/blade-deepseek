use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::approval_types::ActionKind;
use crate::tool_types::{ToolName, ToolRequest, ToolResult, ToolTerminal, ToolTerminalSource};

pub const MISSING_TOOL_TERMINAL_ERROR: &str = "Tool invocation outcome is indeterminate because its terminal result was missing from recovered history. Inspect external state before retrying.";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
        terminal: Option<ToolTerminal>,
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
    pub runtime: Option<String>,
    pub plan: Option<String>,
    pub goal: Option<String>,
    pub skill: Option<String>,
    /// Assistant turns since the pinned plan was last successfully updated.
    /// Once this passes [`PLAN_REMINDER_AFTER_TURNS`] with open steps left,
    /// `render` appends a reconciliation reminder. Guards against the model
    /// finishing (or abandoning) work while the pinned plan silently goes
    /// stale — e.g. after failed update_plan calls it never retried.
    pub plan_age_turns: u32,
}

/// Assistant turns a plan with open steps may go without an update before the
/// rendered context nudges the model to reconcile it.
pub const PLAN_REMINDER_AFTER_TURNS: u32 = 10;

const PLAN_REMINDER: &str = "[Plan reminder] The pinned plan above has open steps but has not been updated for a while. If steps were finished, call update_plan now with every step's current status. If the plan no longer matches the work, replace or clear it via update_plan. Do not announce completion while the plan still shows open steps.";

impl VolatileContext {
    pub fn is_empty(&self) -> bool {
        self.runtime.is_none() && self.plan.is_none() && self.goal.is_none() && self.skill.is_none()
    }

    pub fn note_assistant_turn(&mut self) {
        if self.plan.is_some() {
            self.plan_age_turns = self.plan_age_turns.saturating_add(1);
        }
    }

    fn plan_reminder_due(&self) -> bool {
        self.plan_age_turns >= PLAN_REMINDER_AFTER_TURNS
            && self
                .plan
                .as_deref()
                .is_some_and(|plan| plan.contains("[in_progress]") || plan.contains("[pending]"))
    }

    pub fn render(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(runtime) = &self.runtime {
            parts.push(runtime.as_str());
        }
        if let Some(goal) = &self.goal {
            parts.push(goal.as_str());
        }
        if let Some(plan) = &self.plan {
            parts.push(plan.as_str());
            if self.plan_reminder_due() {
                parts.push(PLAN_REMINDER);
            }
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
        self.volatile.plan_age_turns = 0;
    }

    pub fn replace_goal_state(&mut self, content: String) {
        self.volatile.goal = Some(format!("[Goal state]\n{content}"));
    }

    pub fn replace_runtime_context(&mut self, content: Option<String>) {
        self.volatile.runtime = content
            .filter(|text| !text.trim().is_empty())
            .map(|text| format!("[Runtime context]\n{text}"));
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
        self.volatile.note_assistant_turn();
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
            terminal: None,
            pinned: false,
        });
    }

    pub fn add_tool_result_with_terminal(&mut self, result: &ToolResult, content: String) {
        self.messages.push(Message::Tool {
            tool_call_id: result.id.clone(),
            content,
            terminal: Some(result.terminal().clone()),
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
            .rposition(|message| matches!(message, Message::User { pinned: false, .. }))?;
        let prompt = match &self.messages[index] {
            Message::User { content, .. } => content.clone(),
            _ => unreachable!("rposition only matches user messages"),
        };
        self.messages.truncate(index);
        Some(prompt)
    }
}

pub fn assistant_message_has_payload(content: Option<&str>, tool_calls: &[RawToolCall]) -> bool {
    content.is_some_and(|text| !text.trim().is_empty()) || !tool_calls.is_empty()
}

pub fn normalize_tool_boundaries(messages: &mut Vec<Message>) {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut index = 0usize;

    while index < messages.len() {
        match &messages[index] {
            Message::Tool { .. } => {
                index += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } if !assistant_message_has_payload(content.as_deref(), tool_calls) => {
                index += 1;
            }
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } if !tool_calls.is_empty() => {
                let mut seen_call_ids = HashSet::new();
                let unique_tool_calls = tool_calls
                    .iter()
                    .filter(|tool_call| seen_call_ids.insert(tool_call.id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                let expected = unique_tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.as_str())
                    .collect::<HashSet<_>>();
                let mut collected_tools = HashMap::new();
                let mut next = index + 1;

                while next < messages.len() {
                    let Message::Tool { tool_call_id, .. } = &messages[next] else {
                        break;
                    };
                    if expected.contains(tool_call_id.as_str()) {
                        collected_tools
                            .entry(tool_call_id.clone())
                            .or_insert_with(|| messages[next].clone());
                    }
                    next += 1;
                }

                normalized.push(Message::Assistant {
                    content: content.clone(),
                    reasoning_content: reasoning_content.clone(),
                    tool_calls: unique_tool_calls.clone(),
                    pinned: *pinned,
                });
                for tool_call in &unique_tool_calls {
                    normalized.push(
                        collected_tools
                            .remove(&tool_call.id)
                            .unwrap_or_else(|| repaired_missing_tool_result(tool_call)),
                    );
                }
                index = next.max(index + 1);
            }
            message => {
                normalized.push(message.clone());
                index += 1;
            }
        }
    }

    *messages = normalized;
}

pub fn repaired_missing_tool_result(tool_call: &RawToolCall) -> Message {
    let request = ToolRequest {
        id: tool_call.id.clone(),
        name: ToolName::from_str(&tool_call.function_name)
            .unwrap_or_else(|| ToolName::plain(&tool_call.function_name)),
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(tool_call.arguments.clone()),
    };
    let result = ToolResult::indeterminate(&request, MISSING_TOOL_TERMINAL_ERROR)
        .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
    Message::Tool {
        tool_call_id: tool_call.id.clone(),
        content: format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
        terminal: Some(result.terminal().clone()),
        pinned: false,
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
    fn add_tool_result_with_terminal_preserves_canonical_terminal() {
        let request = ToolRequest {
            id: "call_1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let result = ToolResult::indeterminate(&request, "terminal missing")
            .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
        let mut conv = Conversation::new();

        conv.add_tool_result_with_terminal(&result, "ERROR: terminal missing".to_string());

        assert!(matches!(
            &conv.messages[0],
            Message::Tool { terminal: Some(terminal), .. }
                if terminal == result.terminal()
        ));
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

    #[test]
    fn backtrack_last_user_skips_pinned_user_context() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("first".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        conv.add_user_pinned("workflow notification".to_string());
        conv.add_assistant(Some("notification reply".to_string()), None, vec![]);

        assert_eq!(conv.backtrack_last_user(), Some("first".to_string()));
        assert_eq!(conv.messages.len(), 1);
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
                    terminal,
                    pinned,
                } => format!("tool|{pinned}|{tool_call_id}|{content}|{terminal:?}"),
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
    fn stale_plan_with_open_steps_gets_reminder_in_render() {
        let mut conv = Conversation::new();
        conv.replace_plan_state(
            "[Pinned plan state]\n[completed] a\n[in_progress] b\n[pending] c".to_string(),
        );

        for _ in 0..PLAN_REMINDER_AFTER_TURNS - 1 {
            conv.add_assistant(Some("working".to_string()), None, vec![]);
        }
        assert!(
            !conv.volatile.render().contains("[Plan reminder]"),
            "reminder must not fire before the threshold"
        );

        conv.add_assistant(Some("working".to_string()), None, vec![]);
        assert!(
            conv.volatile.render().contains("[Plan reminder]"),
            "reminder must fire once the plan has gone stale"
        );
    }

    #[test]
    fn plan_update_resets_staleness_reminder() {
        let mut conv = Conversation::new();
        conv.replace_plan_state("[Pinned plan state]\n[pending] a".to_string());
        for _ in 0..PLAN_REMINDER_AFTER_TURNS {
            conv.add_assistant(None, None, vec![]);
        }
        assert!(conv.volatile.render().contains("[Plan reminder]"));

        conv.replace_plan_state("[Pinned plan state]\n[in_progress] a".to_string());
        assert!(
            !conv.volatile.render().contains("[Plan reminder]"),
            "a successful update must clear the reminder"
        );
    }

    #[test]
    fn fully_completed_plan_never_reminds() {
        let mut conv = Conversation::new();
        conv.replace_plan_state("[Pinned plan state]\n[completed] a\n[completed] b".to_string());
        for _ in 0..PLAN_REMINDER_AFTER_TURNS * 2 {
            conv.add_assistant(None, None, vec![]);
        }
        assert!(!conv.volatile.render().contains("[Plan reminder]"));
    }

    #[test]
    fn assistant_turns_without_plan_do_not_accumulate_age() {
        let mut conv = Conversation::new();
        for _ in 0..PLAN_REMINDER_AFTER_TURNS * 2 {
            conv.add_assistant(None, None, vec![]);
        }
        assert_eq!(conv.volatile.plan_age_turns, 0);

        conv.replace_plan_state("[Pinned plan state]\n[pending] a".to_string());
        assert!(!conv.volatile.render().contains("[Plan reminder]"));
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

    #[test]
    fn normalize_tool_boundaries_repairs_missing_results() {
        let mut messages = vec![
            Message::user("before".to_string()),
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
                pinned: false,
            },
            Message::user("after".to_string()),
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[0], Message::User { content, .. } if content == "before"));
        assert!(
            matches!(&messages[1], Message::Assistant { tool_calls, .. } if tool_calls.len() == 1)
        );
        assert!(matches!(
            &messages[2],
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } if tool_call_id == "call_1"
                && terminal.status == crate::tool_types::ToolStatus::Indeterminate
                && terminal.source == crate::tool_types::ToolTerminalSource::CompatibilityRepair
        ));
        assert!(matches!(&messages[3], Message::User { content, .. } if content == "after"));
    }

    #[test]
    fn normalize_tool_boundaries_drops_reasoning_only_assistant() {
        let mut messages = vec![
            Message::user("before".to_string()),
            Message::Assistant {
                content: None,
                reasoning_content: Some("private reasoning".to_string()),
                tool_calls: vec![],
                pinned: false,
            },
            Message::user("after".to_string()),
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::User { content, .. } if content == "before"));
        assert!(matches!(&messages[1], Message::User { content, .. } if content == "after"));
    }

    #[test]
    fn assistant_message_payload_requires_content_or_tool_calls() {
        assert!(!assistant_message_has_payload(None, &[]));
        assert!(!assistant_message_has_payload(Some("  \n"), &[]));
        assert!(assistant_message_has_payload(Some("answer"), &[]));
        assert!(assistant_message_has_payload(
            None,
            &[RawToolCall {
                id: "call_1".to_string(),
                function_name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
        ));
    }

    #[test]
    fn normalize_tool_boundaries_keeps_complete_assistant_tool_call() {
        let mut messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "ok".to_string(),
                terminal: None,
                pinned: false,
            },
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::Assistant { tool_calls, .. } if tool_calls.len() == 1
        ));
        assert!(
            matches!(&messages[1], Message::Tool { tool_call_id, .. } if tool_call_id == "call_1")
        );
    }

    #[test]
    fn normalize_tool_boundaries_orders_results_and_discards_duplicate_orphans() {
        let mut messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    },
                    RawToolCall {
                        id: "call_2".to_string(),
                        function_name: "grep".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "orphan".to_string(),
                content: "discard me".to_string(),
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "existing second".to_string(),
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "duplicate second".to_string(),
                terminal: None,
                pinned: false,
            },
            Message::user("after".to_string()),
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 4);
        assert!(
            matches!(&messages[1], Message::Tool { tool_call_id, terminal: Some(_), .. } if tool_call_id == "call_1")
        );
        assert!(
            matches!(&messages[2], Message::Tool { tool_call_id, content, terminal: None, .. }
            if tool_call_id == "call_2" && content == "existing second")
        );
        assert!(matches!(&messages[3], Message::User { content, .. } if content == "after"));

        let once = format!("{messages:?}");
        normalize_tool_boundaries(&mut messages);
        assert_eq!(format!("{messages:?}"), once);
    }

    #[test]
    fn normalize_tool_boundaries_keeps_first_duplicate_assistant_call_id() {
        let mut messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: r#"{"path":"first"}"#.to_string(),
                    },
                    RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "write_file".to_string(),
                        arguments: r#"{"path":"second"}"#.to_string(),
                    },
                ],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "first result".to_string(),
                terminal: None,
                pinned: false,
            },
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::Assistant { tool_calls, .. }
                if tool_calls.len() == 1
                    && tool_calls[0].function_name == "read_file"
                    && tool_calls[0].arguments == r#"{"path":"first"}"#
        ));
        assert!(matches!(
            &messages[1],
            Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "call_1" && content == "first result"
        ));
    }
}
