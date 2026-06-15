use crate::provider::conversation::{Conversation, Message};

const DEFAULT_MAX_TOKENS: usize = 128_000;
const COMPACTION_THRESHOLD: f64 = 0.80;
const RESERVED_FOR_RESPONSE: usize = 4096;

pub struct ContextConfig {
    pub max_tokens: usize,
    pub compaction_threshold: f64,
    pub reserved_for_response: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            compaction_threshold: COMPACTION_THRESHOLD,
            reserved_for_response: RESERVED_FOR_RESPONSE,
        }
    }
}

pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

pub fn message_tokens(msg: &Message) -> usize {
    match msg {
        Message::System(content) => estimate_tokens(content) + 4,
        Message::User(content) => estimate_tokens(content) + 4,
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            let mut tokens = 4;
            if let Some(c) = content {
                tokens += estimate_tokens(c);
            }
            if let Some(r) = reasoning_content {
                tokens += estimate_tokens(r);
            }
            for tc in tool_calls {
                tokens += estimate_tokens(&tc.function_name);
                tokens += estimate_tokens(&tc.arguments);
                tokens += 8;
            }
            tokens
        }
        Message::Tool { content, .. } => estimate_tokens(content) + 4,
    }
}

pub fn conversation_tokens(conversation: &Conversation) -> usize {
    conversation.messages.iter().map(message_tokens).sum()
}

pub fn needs_compaction(conversation: &Conversation, config: &ContextConfig) -> bool {
    let total = conversation_tokens(conversation);
    let effective_limit = (config.max_tokens as f64 * config.compaction_threshold) as usize
        - config.reserved_for_response;
    total > effective_limit
}

pub fn compact(conversation: &Conversation, config: &ContextConfig) -> Conversation {
    let messages = &conversation.messages;
    let target_tokens = (config.max_tokens as f64 * config.compaction_threshold) as usize
        - config.reserved_for_response;

    let system_msg = messages.first().cloned();
    let system_tokens = system_msg.as_ref().map(message_tokens).unwrap_or(0);

    let non_system: Vec<&Message> = messages.iter().skip(1).collect();

    let mut kept: Vec<Message> = Vec::new();
    let mut budget = system_tokens
        + estimate_tokens("[Earlier conversation history was truncated to fit context window]")
        + 4;

    for msg in non_system.iter().rev() {
        let msg_tokens = message_tokens(msg);
        if budget + msg_tokens > target_tokens {
            break;
        }
        budget += msg_tokens;
        kept.push((*msg).clone());
    }
    kept.reverse();

    let mut result = Conversation::new();
    if let Some(sys) = system_msg {
        result.messages.push(sys);
    }
    if kept.len() < non_system.len() {
        result.messages.push(Message::System(
            "[Earlier conversation history was truncated to fit context window]".to_string(),
        ));
    }
    result.messages.extend(kept);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_approximates() {
        assert_eq!(estimate_tokens("hello world"), 2); // 11 chars / 4 = 2
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdefgh"), 2); // 8 / 4 = 2
    }

    #[test]
    fn needs_compaction_false_for_small_conversation() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello".to_string());

        let config = ContextConfig::default();
        assert!(!needs_compaction(&conv, &config));
    }

    #[test]
    fn compact_preserves_system_and_recent_messages() {
        let config = ContextConfig {
            max_tokens: 60,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
        };
        // budget = 60 tokens

        let mut conv = Conversation::new();
        // system: "s" → 0 chars/4 + 4 = 4 tokens
        conv.add_system("s".to_string());
        // user: 80 chars → 20 + 4 = 24 tokens
        conv.add_user("aaaa".repeat(20));
        // assistant: 80 chars → 20 + 4 = 24 tokens
        conv.add_assistant(Some("bbbb".repeat(20)), None, vec![]);
        // user: 20 chars → 5 + 4 = 9 tokens
        conv.add_user("cccc".repeat(5));
        // assistant: 20 chars → 5 + 4 = 9 tokens
        conv.add_assistant(Some("dddd".repeat(5)), None, vec![]);
        // user: "end" → 0 + 4 = 4 tokens
        conv.add_user("end".to_string());
        // total non-system: 24+24+9+9+4 = 70, exceeds budget of 60-4(system) = 56

        let compacted = compact(&conv, &config);

        // system should be first
        assert!(matches!(&compacted.messages[0], Message::System(s) if s == "s"));
        // should have dropped some messages
        assert!(compacted.messages.len() < conv.messages.len());
        // last message should be "end"
        let last = compacted.messages.last().unwrap();
        assert!(matches!(last, Message::User(s) if s == "end"));
    }
}
