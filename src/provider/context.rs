use crate::provider::conversation::{Conversation, Message};
use tiktoken_rs::cl100k_base_singleton;

const DEFAULT_MAX_TOKENS: usize = 128_000;
const COMPACTION_THRESHOLD: f64 = 0.80;
const RESERVED_FOR_RESPONSE: usize = 4096;

pub trait TokenCounter {
    fn count_text(&self, text: &str) -> usize;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultTokenCounter;

impl TokenCounter for DefaultTokenCounter {
    fn count_text(&self, text: &str) -> usize {
        // DeepSeek uses a custom BPE vocabulary. cl100k_base (GPT-4) is an approximation
        // with ~10-15% variance. Acceptable for compaction decisions; actual billing uses
        // the API-reported usage field.
        cl100k_base_singleton().encode_ordinary(text).len()
    }
}

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

pub fn message_tokens_with_counter(msg: &Message, counter: &impl TokenCounter) -> usize {
    match msg {
        Message::System(content) => counter.count_text(content) + 4,
        Message::User(content) => counter.count_text(content) + 4,
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            let mut tokens = 4;
            if let Some(c) = content {
                tokens += counter.count_text(c);
            }
            if let Some(r) = reasoning_content {
                tokens += counter.count_text(r);
            }
            for tc in tool_calls {
                tokens += counter.count_text(&tc.function_name);
                tokens += counter.count_text(&tc.arguments);
                tokens += 8;
            }
            tokens
        }
        Message::Tool { content, .. } => counter.count_text(content) + 4,
    }
}

pub fn message_tokens(msg: &Message) -> usize {
    message_tokens_with_counter(msg, &DefaultTokenCounter)
}

pub fn conversation_tokens(conversation: &Conversation) -> usize {
    conversation.messages.iter().map(message_tokens).sum()
}

#[cfg(test)]
fn conversation_tokens_with_counter(
    conversation: &Conversation,
    counter: &impl TokenCounter,
) -> usize {
    conversation
        .messages
        .iter()
        .map(|message| message_tokens_with_counter(message, counter))
        .sum()
}

pub fn needs_compaction(conversation: &Conversation, config: &ContextConfig) -> bool {
    let total = conversation_tokens(conversation);
    let effective_limit = (config.max_tokens as f64 * config.compaction_threshold) as usize
        - config.reserved_for_response;
    total > effective_limit
}

pub fn compact(conversation: &Conversation, config: &ContextConfig) -> Conversation {
    compact_with_counter(conversation, config, &DefaultTokenCounter)
}

pub fn compact_with_counter(
    conversation: &Conversation,
    config: &ContextConfig,
    counter: &impl TokenCounter,
) -> Conversation {
    let messages = &conversation.messages;
    let target_tokens = (config.max_tokens as f64 * config.compaction_threshold) as usize
        - config.reserved_for_response;

    let system_msg = messages.first().cloned();
    let system_tokens = system_msg
        .as_ref()
        .map(|message| message_tokens_with_counter(message, counter))
        .unwrap_or(0);

    let non_system: Vec<&Message> = messages.iter().skip(1).collect();

    let mut kept: Vec<Message> = Vec::new();
    let mut budget = system_tokens
        + counter.count_text("[Earlier conversation history was truncated to fit context window]")
        + 4;

    for msg in non_system.iter().rev() {
        let msg_tokens = message_tokens_with_counter(msg, counter);
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

    struct FixedCounter;

    impl TokenCounter for FixedCounter {
        fn count_text(&self, text: &str) -> usize {
            if text.is_empty() { 0 } else { 1 }
        }
    }

    #[test]
    fn default_token_counter_counts_text_without_chars_div_four_api() {
        let counter = DefaultTokenCounter;
        assert_eq!(counter.count_text("hello world"), 2);
        assert_eq!(counter.count_text(""), 0);
        assert_eq!(counter.count_text("hello, world!"), 4);
    }

    #[test]
    fn default_token_counter_uses_bpe_token_boundaries() {
        let counter = DefaultTokenCounter;

        assert_eq!(counter.count_text("hellohellohello"), 3);
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
    fn conversation_tokens_can_use_custom_counter() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello world".to_string());

        assert_eq!(conversation_tokens_with_counter(&conv, &FixedCounter), 10);
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
        conv.add_system("s".to_string());
        conv.add_user("aaaa".repeat(20));
        conv.add_assistant(Some("bbbb".repeat(20)), None, vec![]);
        conv.add_user("cccc".repeat(5));
        conv.add_assistant(Some("dddd".repeat(5)), None, vec![]);
        conv.add_user("end".to_string());

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
