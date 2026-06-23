use orca_core::config::{ModelRuntimeConfig, ProviderKind};
use orca_core::conversation::{Conversation, Message, SummaryState};
use orca_core::provider_types::ProviderStep;
use tiktoken_rs::cl100k_base_singleton;

use crate::ProviderConfig;

const DEFAULT_MAX_TOKENS: usize = 1_000_000;
const COMPACTION_THRESHOLD: f64 = 0.80;
const RESERVED_FOR_RESPONSE: usize = 4096;
const STALE_TOOL_OUTPUT_BYTES: usize = 2048;

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
    pub auto_compact_token_limit: Option<usize>,
}

impl ContextConfig {
    pub fn for_model(model: Option<&str>) -> Self {
        Self {
            max_tokens: orca_core::model::max_context_tokens(model),
            compaction_threshold: COMPACTION_THRESHOLD,
            reserved_for_response: RESERVED_FOR_RESPONSE,
            auto_compact_token_limit: None,
        }
    }

    pub fn for_model_with_runtime(model: Option<&str>, runtime: &ModelRuntimeConfig) -> Self {
        let mut config = Self::for_model(model);
        if let Some(context_window) = runtime.context_window {
            config.max_tokens = context_window.max(1);
        }
        config.auto_compact_token_limit = runtime.auto_compact_token_limit;
        config
    }

    pub fn effective_limit(&self) -> usize {
        if let Some(limit) = self.auto_compact_token_limit {
            return limit.min(self.max_tokens).max(1);
        }
        let threshold = self.compaction_threshold.clamp(0.1, 1.0);
        ((self.max_tokens as f64 * threshold) as usize).saturating_sub(self.reserved_for_response)
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: DEFAULT_MAX_TOKENS,
            compaction_threshold: COMPACTION_THRESHOLD,
            reserved_for_response: RESERVED_FOR_RESPONSE,
            auto_compact_token_limit: None,
        }
    }
}

pub fn message_tokens_with_counter(msg: &Message, counter: &impl TokenCounter) -> usize {
    match msg {
        Message::System { content, .. } => counter.count_text(content) + 4,
        Message::User { content, .. } => counter.count_text(content) + 4,
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
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

fn conversation_tokens_with_counter(
    conversation: &Conversation,
    counter: &impl TokenCounter,
) -> usize {
    conversation
        .messages
        .iter()
        .map(|message| message_tokens_with_counter(message, counter))
        .sum::<usize>()
        + volatile_tokens_with_counter(conversation, counter)
        + summary_state_tokens(conversation, counter)
}

fn volatile_tokens_with_counter(conversation: &Conversation, counter: &impl TokenCounter) -> usize {
    if conversation.messages.is_empty() || conversation.volatile.is_empty() {
        return 0;
    }
    counter.count_text(&conversation.volatile.render())
}

pub fn needs_compaction(conversation: &Conversation, config: &ContextConfig) -> bool {
    let total = conversation_tokens(conversation);
    total > config.effective_limit()
}

pub fn is_prompt_too_long_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("prompt_too_long")
        || normalized.contains("maximum context length")
        || normalized.contains("context length exceeded")
        || normalized.contains("context_length_exceeded")
}

pub fn compact(conversation: &Conversation, config: &ContextConfig) -> Conversation {
    compact_with_counter(conversation, config, &DefaultTokenCounter)
}

#[derive(Clone, Debug)]
pub enum CompactionKind {
    LocalTruncation,
    RemoteSummary(String),
}

#[derive(Clone, Debug)]
pub struct CompactionResult {
    pub conversation: Conversation,
    pub kind: CompactionKind,
}

pub fn compact_with_summary(
    provider_kind: ProviderKind,
    conversation: &Conversation,
    context_config: &ContextConfig,
    provider_config: &ProviderConfig,
) -> CompactionResult {
    let conversation = micro_compact_stale_tool_outputs(conversation);
    if !needs_compaction(&conversation, context_config) {
        return CompactionResult {
            conversation,
            kind: CompactionKind::LocalTruncation,
        };
    }
    match summarize_collapsed_messages(
        provider_kind,
        &conversation,
        context_config,
        provider_config,
    ) {
        Some((conversation, summary)) => CompactionResult {
            conversation,
            kind: CompactionKind::RemoteSummary(summary),
        },
        None => CompactionResult {
            conversation: compact(&conversation, context_config),
            kind: CompactionKind::LocalTruncation,
        },
    }
}

const SMALL_DELTA_TOKEN_THRESHOLD: usize = 200;
const MAX_SUMMARY_DELTAS: usize = 5;
const BASELINE_REBUILD_TOKEN_THRESHOLD: usize = 2000;

fn summarize_collapsed_messages(
    provider_kind: ProviderKind,
    conversation: &Conversation,
    context_config: &ContextConfig,
    provider_config: &ProviderConfig,
) -> Option<(Conversation, String)> {
    let (system_msg, pinned, collapsed, kept) =
        partition_for_compaction(conversation, context_config, &DefaultTokenCounter)?;
    if collapsed.is_empty() || kept.is_empty() {
        return None;
    }

    let delta_text = format_messages(&local_extractive_compaction(&collapsed));
    let delta_tokens = DefaultTokenCounter.count_text(&delta_text);

    let has_existing_summary =
        conversation.rolling_summary.is_some() || !conversation.summary.is_empty();
    let new_delta = if has_existing_summary {
        if delta_tokens < SMALL_DELTA_TOKEN_THRESHOLD {
            delta_text.trim().to_string()
        } else {
            request_summary(provider_kind, provider_config, None, &delta_text)?
        }
    } else {
        request_summary(provider_kind, provider_config, None, &delta_text)?
    };

    let mut result = Conversation::new();
    if let Some(system) = system_msg {
        result.messages.push(system);
    }
    result.messages.extend(pinned);
    result.messages.extend(kept);
    result.volatile = conversation.volatile.clone();
    result.rolling_summary = Some(new_delta.clone());

    let mut summary = conversation.summary.clone();
    if summary.baseline.is_none() {
        summary.baseline = Some(new_delta.clone());
    } else {
        summary.deltas.push(new_delta.clone());
        let needs_rebuild = summary.deltas.len() > MAX_SUMMARY_DELTAS
            || summary_total_delta_tokens(&summary) > BASELINE_REBUILD_TOKEN_THRESHOLD;
        if needs_rebuild {
            let merged = rebuild_baseline(provider_kind, provider_config, &summary);
            summary.baseline = Some(merged);
            summary.deltas.clear();
        }
    }
    result.summary = summary;

    Some((result, new_delta))
}

fn summary_total_delta_tokens(summary: &SummaryState) -> usize {
    summary
        .deltas
        .iter()
        .map(|delta| DefaultTokenCounter.count_text(delta))
        .sum()
}

fn summary_state_tokens(conversation: &Conversation, counter: &impl TokenCounter) -> usize {
    let mut tokens = 0;
    if let Some(baseline) = &conversation.summary.baseline {
        tokens += counter.count_text(baseline) + counter.count_text("[Summary baseline]") + 4;
    }
    for (i, delta) in conversation.summary.deltas.iter().enumerate() {
        tokens += counter.count_text(delta)
            + counter.count_text(&format!("[Summary update {}]", i + 1))
            + 4;
    }
    tokens
}

fn rebuild_baseline(
    provider_kind: ProviderKind,
    provider_config: &ProviderConfig,
    summary: &SummaryState,
) -> String {
    let mut combined = String::new();
    if let Some(baseline) = &summary.baseline {
        combined.push_str(baseline);
    }
    for delta in &summary.deltas {
        combined.push_str("\n\n");
        combined.push_str(delta);
    }
    request_summary(provider_kind, provider_config, None, &combined).unwrap_or(combined)
}

fn partition_for_compaction(
    conversation: &Conversation,
    config: &ContextConfig,
    counter: &impl TokenCounter,
) -> Option<(Option<Message>, Vec<Message>, Vec<Message>, Vec<Message>)> {
    let messages = &conversation.messages;
    let target_tokens = config.effective_limit();
    let system_msg = messages.first().cloned();
    let system_tokens = system_msg
        .as_ref()
        .map(|message| message_tokens_with_counter(message, counter))
        .unwrap_or(0);
    let summary_tokens = if conversation.summary.is_empty() {
        counter.count_text("[Summary baseline]") + 256
    } else {
        summary_state_tokens(conversation, counter) + 256
    };
    let non_system: Vec<&Message> = messages.iter().skip(1).collect();
    let pinned: Vec<Message> = non_system
        .iter()
        .filter(|message| message.is_pinned())
        .map(|message| (*message).clone())
        .collect();
    let droppable: Vec<&Message> = non_system
        .iter()
        .copied()
        .filter(|message| !message.is_pinned())
        .collect();

    let mut kept: Vec<Message> = Vec::new();
    let pinned_tokens: usize = pinned
        .iter()
        .map(|message| message_tokens_with_counter(message, counter))
        .sum();
    let volatile_tokens = volatile_tokens_with_counter(conversation, counter);
    let mut budget = system_tokens + pinned_tokens + summary_tokens + volatile_tokens + 4;
    for msg in droppable.iter().rev() {
        let msg_tokens = message_tokens_with_counter(msg, counter);
        if budget + msg_tokens > target_tokens {
            break;
        }
        budget += msg_tokens;
        kept.push((*msg).clone());
    }
    kept.reverse();
    normalize_tool_boundaries(&mut kept);

    let collapsed_len = droppable.len().saturating_sub(kept.len());
    if collapsed_len == 0 {
        return None;
    }
    let collapsed = droppable
        .iter()
        .take(collapsed_len)
        .map(|message| (*message).clone())
        .collect();
    Some((system_msg, pinned, collapsed, kept))
}

fn request_summary(
    provider_kind: ProviderKind,
    provider_config: &ProviderConfig,
    previous_summary: Option<&str>,
    collapsed_text: &str,
) -> Option<String> {
    let cache_scope = summary_cache_scope(provider_kind, provider_config);
    let cache_key =
        crate::summary_cache::summary_key(&cache_scope, previous_summary, collapsed_text);
    if let Some(cached) = crate::summary_cache::lookup(&cache_key) {
        return Some(cached);
    }

    let summary_model = orca_core::model::auxiliary_model().to_string();
    let summary_config = ProviderConfig {
        api_key: provider_config.api_key.clone(),
        base_url: provider_config.base_url.clone(),
        model: Some(summary_model),
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };

    let user_prompt = match previous_summary {
        Some(prev) => format!(
            "You have a previous summary of older conversation history:\n\n{prev}\n\nNow summarize the following newly collapsed segment and merge it with the previous summary into one coherent updated summary:\n\n{collapsed_text}"
        ),
        None => format!("Summarize this collapsed conversation segment:\n\n{collapsed_text}"),
    };

    let mut summary_conversation = Conversation::new();
    summary_conversation.add_system(SUMMARY_SYSTEM_PROMPT.to_string());
    summary_conversation.add_user(user_prompt);

    let response = crate::call(provider_kind, &summary_conversation, &summary_config);
    if response
        .steps
        .iter()
        .any(|step| matches!(step, ProviderStep::Error(_)))
    {
        return None;
    }
    let summary = response
        .assistant_content
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())?;
    crate::summary_cache::store(&cache_key, &summary);
    Some(summary)
}

const EXTRACTIVE_TOOL_OUTPUT_BYTES: usize = 1024;
const EXTRACTIVE_HEAD_LINES: usize = 12;
const EXTRACTIVE_TAIL_LINES: usize = 8;
const EXTRACTIVE_HEAD_CHARS: usize = 384;
const EXTRACTIVE_TAIL_CHARS: usize = 384;
const SUMMARY_PROMPT_VERSION: &str = "summary-prompt-v1";
const SUMMARY_SYSTEM_PROMPT: &str = "Summarize old agent conversation context for future continuation. Preserve user goals, decisions, file paths, tool results, blockers, and exact constraints. Be concise and factual.";

fn summary_cache_scope(provider_kind: ProviderKind, provider_config: &ProviderConfig) -> String {
    format!(
        "provider={};base_url={};model={};prompt_version={};prompt={}",
        provider_kind.as_str(),
        provider_config.base_url.as_deref().unwrap_or("<default>"),
        orca_core::model::auxiliary_model(),
        SUMMARY_PROMPT_VERSION,
        SUMMARY_SYSTEM_PROMPT
    )
}

/// Deterministically shrink the collapsed delta before it reaches the remote
/// summary model. Tool outputs (file reads, bash output, grep dumps) are the
/// bulk of collapsed tokens and are highly compressible without an LLM call:
/// we keep a head/tail extract plus a size marker. Natural-language turns
/// (user/assistant) are left untouched so the remote summarizer keeps full
/// fidelity on intent and decisions. This is purely local and deterministic,
/// so identical inputs always yield identical output (which also stabilizes
/// the summary hash cache).
fn local_extractive_compaction(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|message| match message {
            Message::Tool {
                tool_call_id,
                content,
                pinned,
            } if content.len() > EXTRACTIVE_TOOL_OUTPUT_BYTES => Message::Tool {
                tool_call_id: tool_call_id.clone(),
                content: extractive_summarize_output(content),
                pinned: *pinned,
            },
            other => other.clone(),
        })
        .collect()
}

fn extractive_summarize_output(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= EXTRACTIVE_HEAD_LINES + EXTRACTIVE_TAIL_LINES {
        return extractive_summarize_single_span(content);
    }
    let head = lines[..EXTRACTIVE_HEAD_LINES].join("\n");
    let tail = lines[lines.len() - EXTRACTIVE_TAIL_LINES..].join("\n");
    let omitted = lines.len() - EXTRACTIVE_HEAD_LINES - EXTRACTIVE_TAIL_LINES;
    format!(
        "[extractive-compact] original_bytes={} original_lines={} omitted_lines={}\n{}\n... [{} lines omitted] ...\n{}",
        content.len(),
        lines.len(),
        omitted,
        head.trim_end(),
        omitted,
        tail.trim_start()
    )
}

fn extractive_summarize_single_span(content: &str) -> String {
    let char_count = content.chars().count();
    if char_count <= EXTRACTIVE_HEAD_CHARS + EXTRACTIVE_TAIL_CHARS {
        return content.to_string();
    }
    let head: String = content.chars().take(EXTRACTIVE_HEAD_CHARS).collect();
    let tail_vec: Vec<char> = content.chars().rev().take(EXTRACTIVE_TAIL_CHARS).collect();
    let tail: String = tail_vec.into_iter().rev().collect();
    let omitted = char_count - EXTRACTIVE_HEAD_CHARS - EXTRACTIVE_TAIL_CHARS;
    format!(
        "[extractive-compact] original_bytes={} original_chars={} omitted_chars={}\n{}\n... [{} chars omitted] ...\n{}",
        content.len(),
        char_count,
        omitted,
        head.trim_end(),
        omitted,
        tail.trim_start()
    )
}

fn format_messages(messages: &[Message]) -> String {
    let mut output = String::new();
    for message in messages {
        match message {
            Message::System { content, .. } => {
                output.push_str("[system]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
            Message::User { content, .. } => {
                output.push_str("[user]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                output.push_str("[assistant]\n");
                if let Some(reasoning) = reasoning_content
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                {
                    output.push_str("reasoning: ");
                    output.push_str(reasoning.trim());
                    output.push('\n');
                }
                if let Some(content) = content.as_deref().filter(|text| !text.trim().is_empty()) {
                    output.push_str(content.trim());
                    output.push('\n');
                }
                for tool_call in tool_calls {
                    output.push_str("tool_call ");
                    output.push_str(&tool_call.function_name);
                    output.push(' ');
                    output.push_str(&tool_call.arguments);
                    output.push('\n');
                }
                output.push('\n');
            }
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                output.push_str("[tool ");
                output.push_str(tool_call_id);
                output.push_str("]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
        }
    }
    output
}

pub fn compact_with_counter(
    conversation: &Conversation,
    config: &ContextConfig,
    counter: &impl TokenCounter,
) -> Conversation {
    let micro_compacted = micro_compact_stale_tool_outputs(conversation);
    if conversation_tokens_with_counter(&micro_compacted, counter) <= config.effective_limit() {
        return normalize_compacted_conversation(micro_compacted);
    }

    let messages = &micro_compacted.messages;
    let target_tokens = config.effective_limit();

    let system_msg = messages.first().cloned();
    let system_tokens = system_msg
        .as_ref()
        .map(|message| message_tokens_with_counter(message, counter))
        .unwrap_or(0);

    let non_system: Vec<&Message> = messages.iter().skip(1).collect();
    let mut pinned: Vec<Message> = non_system
        .iter()
        .filter(|message| message.is_pinned())
        .map(|message| (*message).clone())
        .collect();
    let droppable: Vec<&Message> = non_system
        .iter()
        .copied()
        .filter(|message| !message.is_pinned())
        .collect();

    let mut kept: Vec<Message> = Vec::new();
    let pinned_budget_limit = target_tokens / 2;
    let mut pinned_tokens: usize = pinned
        .iter()
        .map(|message| message_tokens_with_counter(message, counter))
        .sum();

    if pinned_tokens > pinned_budget_limit {
        eprintln!(
            "orca: warning: pinned messages use {pinned_tokens} tokens (>{pinned_budget_limit} limit), demoting oldest"
        );
        while pinned_tokens > pinned_budget_limit && pinned.len() > 1 {
            let is_plan = pinned[0]
                .content_str()
                .map_or(false, |c| c.starts_with("[Pinned plan state]"));
            if is_plan {
                break;
            }
            pinned_tokens -= message_tokens_with_counter(&pinned[0], counter);
            pinned.remove(0);
        }
    }

    let mut budget = system_tokens
        + pinned_tokens
        + summary_state_tokens(&micro_compacted, counter)
        + volatile_tokens_with_counter(&micro_compacted, counter)
        + counter.count_text("[Earlier conversation history was truncated to fit context window]")
        + 4;

    for msg in droppable.iter().rev() {
        let msg_tokens = message_tokens_with_counter(msg, counter);
        if budget + msg_tokens > target_tokens {
            break;
        }
        budget += msg_tokens;
        kept.push((*msg).clone());
    }
    kept.reverse();

    normalize_tool_boundaries(&mut kept);

    let mut result = Conversation::new();
    if let Some(sys) = system_msg {
        result.messages.push(sys);
    }
    if kept.len() < droppable.len() {
        result.messages.push(Message::system(
            "[Earlier conversation history was truncated to fit context window]".to_string(),
        ));
    }
    result.messages.extend(pinned);
    result.messages.extend(kept);
    result.volatile = conversation.volatile.clone();
    result.rolling_summary = conversation.rolling_summary.clone();
    result.summary = conversation.summary.clone();
    result
}

fn normalize_compacted_conversation(mut conversation: Conversation) -> Conversation {
    if conversation.messages.len() <= 1 {
        return conversation;
    }
    let volatile = conversation.volatile.clone();
    let rolling_summary = conversation.rolling_summary.clone();
    let summary = conversation.summary.clone();
    let system = conversation.messages.remove(0);
    normalize_tool_boundaries(&mut conversation.messages);
    let mut result = Conversation::new();
    result.messages.push(system);
    result.messages.extend(conversation.messages);
    result.volatile = volatile;
    result.rolling_summary = rolling_summary;
    result.summary = summary;
    result
}

fn micro_compact_stale_tool_outputs(conversation: &Conversation) -> Conversation {
    let mut result = Conversation::new();
    result.volatile = conversation.volatile.clone();
    result.rolling_summary = conversation.rolling_summary.clone();
    result.summary = conversation.summary.clone();
    let last_user_index = conversation
        .messages
        .iter()
        .rposition(|message| matches!(message, Message::User { .. }))
        .unwrap_or(conversation.messages.len());

    for (index, message) in conversation.messages.iter().enumerate() {
        let compacted = match message {
            Message::Tool {
                tool_call_id,
                content,
                pinned,
            } if index < last_user_index && !*pinned && content.len() > STALE_TOOL_OUTPUT_BYTES => {
                Message::Tool {
                    tool_call_id: tool_call_id.clone(),
                    content: micro_compact_tool_output(content),
                    pinned: false,
                }
            }
            _ => message.clone(),
        };
        result.messages.push(compacted);
    }
    result
}

fn micro_compact_tool_output(content: &str) -> String {
    let head: String = content.chars().take(320).collect();
    let tail_vec: Vec<char> = content.chars().rev().take(320).collect();
    let tail: String = tail_vec.into_iter().rev().collect();
    format!(
        "[tool output micro-compact]\noriginal_bytes: {}\nhead:\n{}\n\ntail:\n{}",
        content.len(),
        head.trim_end(),
        tail.trim_start()
    )
}

fn normalize_tool_boundaries(messages: &mut Vec<Message>) {
    let leading_tools = messages
        .iter()
        .take_while(|msg| matches!(msg, Message::Tool { .. }))
        .count();
    if leading_tools > 0 {
        messages.drain(..leading_tools);
    }
    if let Some(Message::Assistant { tool_calls, .. }) = messages.last()
        && !tool_calls.is_empty()
    {
        messages.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::conversation::RawToolCall;

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
    fn context_config_uses_model_specific_token_limit() {
        assert_eq!(
            ContextConfig::for_model(Some("deepseek-chat")).max_tokens,
            1_000_000
        );
        assert_eq!(
            ContextConfig::for_model(Some("deepseek-reasoner")).max_tokens,
            1_000_000
        );
        assert_eq!(
            ContextConfig::for_model(Some(orca_core::model::PRO_MODEL)).max_tokens,
            1_000_000
        );
        assert_eq!(ContextConfig::default().max_tokens, 1_000_000);
    }

    #[test]
    fn context_config_uses_model_runtime_overrides() {
        let runtime = ModelRuntimeConfig {
            context_window: Some(128_000),
            auto_compact_token_limit: Some(96_000),
        };

        let config = ContextConfig::for_model_with_runtime(Some("deepseek-v4-pro"), &runtime);

        assert_eq!(config.max_tokens, 128_000);
        assert_eq!(config.effective_limit(), 96_000);
    }

    #[test]
    fn conversation_tokens_can_use_custom_counter() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello world".to_string());

        assert_eq!(conversation_tokens_with_counter(&conv, &FixedCounter), 10);
    }

    #[test]
    fn conversation_tokens_include_volatile_overlay() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello world".to_string());
        conv.replace_plan_state("plan".to_string());
        conv.replace_goal_state("goal".to_string());

        assert_eq!(conversation_tokens_with_counter(&conv, &FixedCounter), 11);
    }

    #[test]
    fn no_message_is_annotated_with_a_context_budget_hint() {
        // Budget/remaining context is local observability only; it must never be
        // injected into upstream messages, which would break DeepSeek prefix cache.
        let config = ContextConfig {
            max_tokens: 1_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(42),
        };

        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("active request".to_string());
        conv.add_assistant(Some("answer".to_string()), None, vec![]);
        conv.add_tool_result("tc1".to_string(), "tool output".to_string());

        // Exercise compaction paths that rebuild the conversation; none should add a hint.
        let compacted = compact(&conv, &config);

        for conversation in [&conv, &compacted] {
            for message in &conversation.messages {
                if let Some(text) = message.content_str() {
                    assert!(
                        !text.contains("[context: ~"),
                        "no message may carry a context budget hint, found: {text:?}"
                    );
                    assert!(
                        !text.contains("tokens remaining"),
                        "no message may carry a context budget hint, found: {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn compact_preserves_system_and_recent_messages() {
        let config = ContextConfig {
            max_tokens: 60,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
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
        assert!(
            matches!(&compacted.messages[0], Message::System { content, .. } if content == "s")
        );
        // should have dropped some messages
        assert!(compacted.messages.len() < conv.messages.len());
        // last message should be "end"
        let last = compacted.messages.last().unwrap();
        assert!(matches!(last, Message::User { content, .. } if content == "end"));
    }

    #[test]
    fn compact_preserves_pinned_messages_outside_recent_window() {
        let config = ContextConfig {
            max_tokens: 42,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("s".to_string());
        conv.add_user_pinned("core constraint".to_string());
        conv.add_user("old filler".repeat(40));
        conv.add_assistant(Some("old answer".repeat(40)), None, vec![]);
        conv.add_user("newest request".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        assert!(compacted.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == "core constraint")
                && message.is_pinned()
        }));
        assert!(
            matches!(compacted.messages.last(), Some(Message::User { content, .. }) if content == "newest request")
        );
    }

    #[test]
    fn compact_micro_compacts_stale_tool_output_before_dropping_messages() {
        let config = ContextConfig {
            max_tokens: 80,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("s".to_string());
        conv.add_user("inspect".to_string());
        conv.add_assistant(
            Some("calling read_file".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"large.log"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "line\n".repeat(500));
        conv.add_user("newest request".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        let tool_output = compacted.messages.iter().find_map(|message| match message {
            Message::Tool { content, .. } => Some(content.as_str()),
            _ => None,
        });
        assert!(matches!(
            tool_output,
            Some(content)
                if content.contains("[tool output micro-compact]")
                    && !content.contains(&"line\n".repeat(100))
        ));
    }

    #[test]
    fn effective_limit_does_not_underflow_when_reserved_exceeds_threshold() {
        let config = ContextConfig {
            max_tokens: 100,
            compaction_threshold: 0.5,
            reserved_for_response: 9999,
            auto_compact_token_limit: None,
        };
        // 100 * 0.5 = 50, saturating_sub(9999) = 0 (not panic)
        assert_eq!(config.effective_limit(), 0);
    }

    #[test]
    fn effective_limit_clamps_invalid_threshold() {
        let below = ContextConfig {
            max_tokens: 1000,
            compaction_threshold: 0.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };
        // 0.0 clamped to 0.1 → 1000 * 0.1 = 100
        assert_eq!(below.effective_limit(), 100);

        let above = ContextConfig {
            max_tokens: 1000,
            compaction_threshold: 2.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };
        // 2.0 clamped to 1.0 → 1000 * 1.0 = 1000
        assert_eq!(above.effective_limit(), 1000);
    }

    #[test]
    fn compact_trims_orphaned_tool_messages_at_front() {
        let config = ContextConfig {
            max_tokens: 200,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("s".to_string());
        // Old assistant with tool_call (will be dropped due to budget)
        conv.add_user("filler".repeat(50));
        conv.add_assistant(
            Some("calling tool".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file content".to_string());
        // Recent messages that fit in budget
        conv.add_user("recent question".to_string());
        conv.add_assistant(Some("recent answer".to_string()), None, vec![]);

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        // Should not start with an orphaned Tool message
        for msg in &compacted.messages {
            if matches!(msg, Message::Tool { .. }) {
                panic!("orphaned Tool message should have been trimmed");
            }
            if matches!(msg, Message::User { .. } | Message::Assistant { .. }) {
                break;
            }
        }
    }

    #[test]
    fn compact_trims_trailing_assistant_with_pending_tool_calls() {
        let config = ContextConfig {
            max_tokens: 50,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("s".to_string());
        conv.add_user("question".to_string());
        conv.add_assistant(
            Some("let me call a tool".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "bash".to_string(),
                arguments: "{}".to_string(),
            }],
        );

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        // Last message should NOT be an Assistant with pending tool_calls
        if let Some(Message::Assistant { tool_calls, .. }) = compacted.messages.last() {
            assert!(
                tool_calls.is_empty(),
                "trailing Assistant with pending tool_calls should be trimmed"
            );
        }
    }

    #[test]
    fn compact_with_summary_falls_back_to_local_when_provider_errors() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("old ".repeat(100));
        conv.add_assistant(Some("older answer".to_string()), None, vec![]);
        conv.add_user("newest request".to_string());

        let config = ContextConfig {
            max_tokens: 40,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let result = compact_with_summary(ProviderKind::DeepSeek, &conv, &config, &provider_config);

        assert!(matches!(result.kind, CompactionKind::LocalTruncation));
        assert!(result.conversation.messages.iter().any(|message| {
            matches!(message, Message::System { content, .. } if content.contains("truncated to fit context window"))
        }));
    }

    #[test]
    fn compact_with_existing_summary_falls_back_to_local_when_large_delta_summary_fails() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("old ".repeat(400));
        conv.add_assistant(Some("older answer ".repeat(400)), None, vec![]);
        conv.add_user("newest request".to_string());
        conv.rolling_summary = Some("previous summary only".to_string());

        let config = ContextConfig {
            max_tokens: 500,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let result = compact_with_summary(ProviderKind::DeepSeek, &conv, &config, &provider_config);

        assert!(
            matches!(result.kind, CompactionKind::LocalTruncation),
            "large deltas must not be dropped behind a stale rolling summary when summary fails"
        );
        assert!(result.conversation.messages.iter().any(|message| {
            matches!(message, Message::System { content, .. } if content.contains("truncated to fit context window"))
        }));
    }

    #[test]
    fn detects_prompt_too_long_provider_errors() {
        assert!(is_prompt_too_long_error(
            "DeepSeek provider error: prompt_too_long: context length exceeded"
        ));
        assert!(is_prompt_too_long_error(
            "This model's maximum context length is 64000 tokens."
        ));
        assert!(!is_prompt_too_long_error(
            "Response blocked by content filter"
        ));
    }

    /// The system prompt is the token-0 prefix that anchors the entire DeepSeek
    /// prefix cache. Local truncation compaction must keep it byte-identical and
    /// in position 0, otherwise every subsequent turn misses the cache wholesale.
    #[test]
    fn compaction_preserves_system_prompt_as_byte_identical_token_zero_prefix() {
        let system = "you are orca, a precise coding agent";
        // FixedCounter scores every non-empty message as 5 tokens (content 1 + 4
        // overhead). Four messages = 20 tokens; a 16-token budget forces the
        // truncation rebuild path while keeping the system prompt + newest turn.
        let config = ContextConfig {
            max_tokens: 16,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system(system.to_string());
        conv.add_user("old ".repeat(40));
        conv.add_assistant(Some("old answer ".repeat(40)), None, vec![]);
        conv.add_user("newest request".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        match &compacted.messages[0] {
            Message::System { content, pinned } => {
                assert_eq!(content, system, "system prompt bytes must be unchanged");
                assert!(!pinned);
            }
            other => panic!("expected system prompt at position 0, found {other:?}"),
        }
        // Truncation must have happened (proves we exercised the rebuild path).
        assert!(compacted.messages.len() < conv.messages.len());
    }

    /// Remote-summary compaction must *insert a new summary message* right after
    /// the system prompt rather than rewriting any retained message in place.
    /// Retained recent messages must stay byte-identical so the cache survives
    /// from the summary boundary onward.
    #[test]
    fn summary_is_inserted_after_system_without_rewriting_kept_messages() {
        // partition_for_compaction is the pure splitting step used by the remote
        // summary path; it must not mutate the messages it keeps.
        //
        // FixedCounter scores each message as 5 tokens (content 1 + overhead 4).
        // The partition budget starts at system(5) + summary reserve(257) + 4 =
        // 266, then keeps recent messages until the limit. A 272-token effective
        // limit keeps exactly the newest message and collapses the two before it.
        let config = ContextConfig {
            max_tokens: 1000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(272),
        };

        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("oldest".to_string());
        conv.add_assistant(Some("older".to_string()), None, vec![]);
        conv.add_user("keep me verbatim".to_string());

        let (system_msg, _pinned, collapsed, kept) =
            partition_for_compaction(&conv, &config, &FixedCounter)
                .expect("partition should split this conversation");

        // System prompt is carried through untouched.
        assert!(
            matches!(&system_msg, Some(Message::System { content, .. }) if content == "system prompt")
        );
        // The most recent message is kept verbatim, not rewritten.
        assert!(
            matches!(kept.last(), Some(Message::User { content, .. }) if content == "keep me verbatim")
        );
        // Something was actually collapsed (so the summary path is meaningful).
        assert!(!collapsed.is_empty());

        // Now assemble the summarized conversation the way summarize_collapsed_messages
        // does, and confirm the layout: system, then a NEW summary system message,
        // then the kept tail unchanged.
        let mut result = Conversation::new();
        result.messages.push(system_msg.unwrap());
        result.messages.push(Message::system(
            "[Summary of earlier conversation]\nX".to_string(),
        ));
        result.messages.extend(kept);

        assert!(
            matches!(&result.messages[0], Message::System { content, .. } if content == "system prompt")
        );
        assert!(
            matches!(&result.messages[1], Message::System { content, .. } if content.starts_with("[Summary of earlier conversation]"))
        );
        assert!(
            matches!(result.messages.last(), Some(Message::User { content, .. }) if content == "keep me verbatim")
        );
    }

    #[test]
    fn compaction_inherits_volatile_state() {
        let config = ContextConfig {
            max_tokens: 16,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("old ".repeat(40));
        conv.add_assistant(Some("old answer ".repeat(40)), None, vec![]);
        conv.add_user("newest".to_string());
        conv.replace_plan_state("active plan".to_string());
        conv.replace_goal_state("active goal".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        assert!(compacted.messages.len() < conv.messages.len());
        assert_eq!(compacted.volatile.plan.as_deref(), Some("active plan"));
        assert!(
            compacted
                .volatile
                .goal
                .as_ref()
                .unwrap()
                .contains("active goal")
        );
    }

    #[test]
    fn micro_compaction_preserves_volatile_state_when_no_truncation_needed() {
        let config = ContextConfig {
            max_tokens: 1_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(1_000),
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("inspect".to_string());
        conv.add_assistant(
            Some("calling read_file".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"large.log"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "x".repeat(STALE_TOOL_OUTPUT_BYTES + 10));
        conv.add_user("newest".to_string());
        conv.replace_plan_state("active plan".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        assert_eq!(compacted.volatile.plan.as_deref(), Some("active plan"));
    }

    #[test]
    fn micro_compaction_preserves_rolling_summary_when_no_truncation_needed() {
        let config = ContextConfig {
            max_tokens: 1_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(1_000),
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("inspect".to_string());
        conv.add_assistant(
            Some("calling read_file".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"large.log"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "x".repeat(STALE_TOOL_OUTPUT_BYTES + 10));
        conv.add_user("newest".to_string());
        conv.rolling_summary = Some("existing rolling summary".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        assert_eq!(
            compacted.rolling_summary.as_deref(),
            Some("existing rolling summary")
        );
    }

    #[test]
    fn local_truncation_inherits_rolling_summary() {
        let config = ContextConfig {
            max_tokens: 16,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("old ".repeat(40));
        conv.add_assistant(Some("old ".repeat(40)), None, vec![]);
        conv.add_user("newest".to_string());
        conv.rolling_summary = Some("previously summarized context".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);
        assert_eq!(
            compacted.rolling_summary.as_deref(),
            Some("previously summarized context")
        );
    }

    #[test]
    fn no_truncation_normalization_preserves_rolling_summary() {
        let config = ContextConfig {
            max_tokens: 1_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(1_000),
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("newest".to_string());
        conv.rolling_summary = Some("existing rolling summary".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        assert_eq!(
            compacted.rolling_summary.as_deref(),
            Some("existing rolling summary")
        );
    }

    #[test]
    fn small_delta_token_threshold_is_reasonable() {
        assert!(
            SMALL_DELTA_TOKEN_THRESHOLD > 0 && SMALL_DELTA_TOKEN_THRESHOLD <= 500,
            "threshold should be in a reasonable range"
        );
    }

    #[test]
    fn summary_state_renders_baseline_then_deltas_in_api_messages() {
        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("hello".to_string());
        conv.summary.baseline = Some("baseline facts".to_string());
        conv.summary.deltas.push("delta 1 facts".to_string());
        conv.summary.deltas.push("delta 2 facts".to_string());

        let messages = crate::deepseek_http::conversation_to_api_messages(&conv);
        assert_eq!(messages[0].content.as_deref(), Some("system prompt"));
        assert!(
            messages[1]
                .content
                .as_deref()
                .unwrap()
                .starts_with("[Summary baseline]")
        );
        assert!(
            messages[2]
                .content
                .as_deref()
                .unwrap()
                .starts_with("[Summary update 1]")
        );
        assert!(
            messages[3]
                .content
                .as_deref()
                .unwrap()
                .starts_with("[Summary update 2]")
        );
        assert_eq!(messages[4].content.as_deref(), Some("hello"));
        assert_eq!(messages.len(), 5);
    }

    #[test]
    fn empty_summary_state_adds_no_api_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("hello".to_string());

        let messages = crate::deepseek_http::conversation_to_api_messages(&conv);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn summary_baseline_persists_through_local_truncation() {
        let config = ContextConfig {
            max_tokens: 16,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("old ".repeat(40));
        conv.add_assistant(Some("old ".repeat(40)), None, vec![]);
        conv.add_user("newest".to_string());
        conv.summary.baseline = Some("stable baseline".to_string());
        conv.summary.deltas.push("delta 1".to_string());

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);
        assert_eq!(
            compacted.summary.baseline.as_deref(),
            Some("stable baseline")
        );
        assert_eq!(compacted.summary.deltas.len(), 1);
    }

    #[test]
    fn local_truncation_budget_counts_summary_state() {
        let config = ContextConfig {
            max_tokens: 25,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: None,
        };

        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.summary.baseline = Some("stable baseline".to_string());
        for index in 0..5 {
            conv.add_user(format!("message {index}"));
        }

        let compacted = compact_with_counter(&conv, &config, &FixedCounter);

        assert!(
            conversation_tokens_with_counter(&compacted, &FixedCounter) <= config.effective_limit(),
            "local truncation must reserve budget for injected summary state"
        );
    }

    #[test]
    fn max_summary_deltas_is_bounded() {
        assert!(
            MAX_SUMMARY_DELTAS > 0 && MAX_SUMMARY_DELTAS <= 10,
            "deltas cap should be reasonable"
        );
    }

    #[test]
    fn extractive_compaction_shrinks_large_tool_output_with_head_and_tail() {
        let big_output = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let original_bytes = big_output.len();
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: big_output,
            pinned: false,
        }];

        let compacted = local_extractive_compaction(&messages);
        let content = compacted[0].content_str().unwrap();

        assert!(content.starts_with("[extractive-compact]"));
        assert!(content.contains(&format!("original_bytes={original_bytes}")));
        assert!(content.contains("line 0"));
        assert!(content.contains("line 199"));
        assert!(content.contains("lines omitted"));
        assert!(
            content.len() < original_bytes,
            "extractive output must be smaller than the original"
        );
    }

    #[test]
    fn extractive_compaction_shrinks_large_single_line_tool_output() {
        let big_output = format!(
            "{{\"status\":\"ok\",\"payload\":\"{}\",\"tail\":\"final-value\"}}",
            "x".repeat(8_000)
        );
        let original_bytes = big_output.len();
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: big_output,
            pinned: false,
        }];

        let compacted = local_extractive_compaction(&messages);
        let content = compacted[0].content_str().unwrap();

        assert!(content.starts_with("[extractive-compact]"));
        assert!(content.contains(&format!("original_bytes={original_bytes}")));
        assert!(content.contains("\"status\":\"ok\""));
        assert!(content.contains("final-value"));
        assert!(
            content.len() < original_bytes,
            "large single-line outputs must shrink before remote summary"
        );
    }

    #[test]
    fn extractive_compaction_leaves_small_tool_output_untouched() {
        let small = "short output".to_string();
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: small.clone(),
            pinned: false,
        }];

        let compacted = local_extractive_compaction(&messages);
        assert_eq!(compacted[0].content_str(), Some(small.as_str()));
    }

    #[test]
    fn extractive_compaction_preserves_natural_language_turns() {
        let long_user = "user intent ".repeat(200);
        let long_assistant = "assistant decision ".repeat(200);
        let messages = vec![
            Message::user(long_user.clone()),
            Message::Assistant {
                content: Some(long_assistant.clone()),
                reasoning_content: None,
                tool_calls: vec![],
                pinned: false,
            },
        ];

        let compacted = local_extractive_compaction(&messages);
        assert_eq!(compacted[0].content_str(), Some(long_user.as_str()));
        assert_eq!(compacted[1].content_str(), Some(long_assistant.as_str()));
    }

    #[test]
    fn extractive_compaction_is_deterministic() {
        let big_output = (0..200)
            .map(|i| format!("row {i} value"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: big_output,
            pinned: false,
        }];

        let first = local_extractive_compaction(&messages);
        let second = local_extractive_compaction(&messages);
        assert_eq!(first[0].content_str(), second[0].content_str());
    }
}
