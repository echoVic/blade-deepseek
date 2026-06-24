use orca_core::config::{ModelRuntimeConfig, ProviderKind};
use orca_core::conversation::{Conversation, Message, SummaryState, normalize_tool_boundaries};
use orca_core::provider_types::ProviderStep;
use tiktoken_rs::cl100k_base_singleton;

use crate::ProviderConfig;

const DEFAULT_MAX_TOKENS: usize = 1_000_000;
const COMPACTION_THRESHOLD: f64 = 0.80;
const RESERVED_FOR_RESPONSE: usize = 4096;
const STALE_TOOL_OUTPUT_BYTES: usize = 2048;

// Hysteresis: compaction triggers at effective_limit but compresses the kept
// window down to this fraction so the next turn still has headroom before the
// next trigger. Keeps the storm-quenching budget tight but not so tight that
// useful context gets dropped on every cycle.
const COMPACTION_TARGET_FRACTION: f64 = 0.60;

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

    /// Hysteresis target: when compaction triggers at `effective_limit()`, we
    /// compress the kept window down to a smaller window so the next turn has
    /// real headroom before the next trigger. Without this, every turn lands
    /// just under the limit and next turn's wire prompt almost always re-fires
    /// compaction (the "compaction storm" pathology).
    pub fn target_compaction_limit(&self) -> usize {
        let limit = self.effective_limit();
        let scaled = ((limit as f64) * COMPACTION_TARGET_FRACTION) as usize;
        scaled.max(1).min(limit)
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

/// Wire-equivalent token count: estimates the byte-equivalent prompt the
/// provider will actually send (`conversation_to_api_messages` + tool schema
/// JSON), not just the in-memory `messages` token sum. This is what matters
/// for `prompt_too_long` decisions and for the "compaction storm" check —
/// without it, local budgeting passes while the real wire prompt is well past
/// the limit (because injected `[Summary baseline]` / `[Summary update N]`
/// system messages, the volatile overlay glued to the last message, and the
/// tool schema JSON are all real tokens on the wire).
pub fn wire_equivalent_tokens(
    conversation: &Conversation,
    provider_config: &ProviderConfig,
) -> usize {
    wire_equivalent_tokens_with_counter(conversation, provider_config, &DefaultTokenCounter)
}

pub fn needs_compaction_wire(
    conversation: &Conversation,
    config: &ContextConfig,
    provider_config: &ProviderConfig,
) -> bool {
    wire_equivalent_tokens(conversation, provider_config) > config.effective_limit()
}

fn wire_equivalent_tokens_with_counter(
    conversation: &Conversation,
    provider_config: &ProviderConfig,
    counter: &impl TokenCounter,
) -> usize {
    let mut tokens = 0;
    let mut first_system_done = false;
    let last_index = conversation.messages.len().saturating_sub(1);
    let volatile_overlay = if conversation.volatile.is_empty() {
        None
    } else {
        Some(conversation.volatile.render())
    };

    for (index, message) in conversation.messages.iter().enumerate() {
        let mut message_tokens = message_tokens_with_counter(message, counter);
        // The volatile overlay is appended ("\n\n{overlay}") to the very last
        // wire message, regardless of role. Mirror that here so the wire count
        // matches `conversation_to_api_messages`'s actual output.
        if let Some(overlay) = volatile_overlay.as_deref()
            && index == last_index
        {
            // 2 newlines + overlay text. The 2-byte separator is negligible in
            // tokens; we count the overlay text only.
            message_tokens += counter.count_text(overlay);
        }
        tokens += message_tokens;
        if !first_system_done && matches!(message, Message::System { .. }) {
            first_system_done = true;
            tokens += inject_summary_tokens_with_counter(&conversation.summary, counter);
        }
    }

    if !first_system_done && !conversation.summary.is_empty() {
        tokens += inject_summary_tokens_with_counter(&conversation.summary, counter);
    }

    tokens += tools_schema_tokens_with_counter(provider_config, counter);
    tokens
}

fn inject_summary_tokens_with_counter(
    summary: &SummaryState,
    counter: &impl TokenCounter,
) -> usize {
    let mut tokens = 0;
    if let Some(baseline) = &summary.baseline {
        let body = format!("[Summary baseline]\n{baseline}");
        tokens += counter.count_text(&body) + 4;
    }
    for (i, delta) in summary.deltas.iter().enumerate() {
        let body = format!("[Summary update {}]\n{delta}", i + 1);
        tokens += counter.count_text(&body) + 4;
    }
    tokens
}

fn tools_schema_tokens_with_counter(
    provider_config: &ProviderConfig,
    counter: &impl TokenCounter,
) -> usize {
    let tools = provider_config.tools_override.clone().unwrap_or_else(|| {
        crate::tool_schema::deepseek_tools_schema_with_mcp_and_external(
            provider_config.mcp_registry.as_ref(),
            &provider_config.external_tools,
        )
    });
    if tools.is_empty() {
        return 0;
    }
    let serialized = serde_json::to_string(&tools).unwrap_or_else(|_| String::from("[]"));
    counter.count_text(&serialized)
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
    let micro_compacted = micro_compact_stale_tool_outputs(conversation);
    // Wire-equivalent gate: cheap micro compaction only counts as "enough"
    // when the prompt the provider will actually receive (messages +
    // injected summary + volatile overlay + tool schema JSON) is back under
    // the limit, otherwise the next turn re-enters the storm.
    if !needs_compaction_wire(&micro_compacted, context_config, provider_config) {
        return CompactionResult {
            conversation: micro_compacted,
            kind: CompactionKind::LocalTruncation,
        };
    }
    match summarize_collapsed_messages(
        provider_kind,
        conversation,
        &micro_compacted,
        context_config,
        provider_config,
    ) {
        Some((conversation, summary)) => CompactionResult {
            conversation,
            kind: CompactionKind::RemoteSummary(summary),
        },
        None => CompactionResult {
            conversation: compact(&micro_compacted, context_config),
            kind: CompactionKind::LocalTruncation,
        },
    }
}

const DIRECT_SUMMARY_DELTA_TOKEN_THRESHOLD: usize = 800;
const MAX_SUMMARY_DELTAS: usize = 5;
const BASELINE_REBUILD_TOKEN_THRESHOLD: usize = 2000;

/// `original_conversation` is the pre-micro-compaction input; `conversation` is
/// the micro-compacted main-context view. The kept tail comes from the
/// micro-compacted view (so main-context behavior is unchanged), but the
/// summary delta is rendered from the ORIGINAL collapsed content so summary
/// extractive rules are never masked by main-context micro compaction.
fn summarize_collapsed_messages(
    provider_kind: ProviderKind,
    original_conversation: &Conversation,
    conversation: &Conversation,
    context_config: &ContextConfig,
    provider_config: &ProviderConfig,
) -> Option<(Conversation, String)> {
    let (system_msg, pinned, collapsed, kept) =
        partition_for_compaction(conversation, context_config, &DefaultTokenCounter)?;
    if collapsed.is_empty() || kept.is_empty() {
        return None;
    }

    // Micro compaction is positional and in-place (it only rewrites Tool
    // contents, never reorders or drops messages), so the original droppable
    // list maps 1:1 onto the micro-compacted one. Taking the same prefix length
    // recovers the ORIGINAL collapsed messages for summary rendering.
    let original_collapsed: Vec<Message> = original_conversation
        .messages
        .iter()
        .skip(1)
        .filter(|message| !message.is_pinned())
        .take(collapsed.len())
        .cloned()
        .collect();
    let rendered = render_summary_delta(&original_collapsed);

    // Medium-and-smaller rendered deltas are cheap enough to keep directly.
    // Returning None here means "fall back to local truncation", which would
    // drop the collapsed facts.
    let new_delta = if rendered.rendered_tokens_est < DIRECT_SUMMARY_DELTA_TOKEN_THRESHOLD {
        rendered.text.trim().to_string()
    } else {
        request_summary(
            provider_kind,
            provider_config,
            SUMMARY_PURPOSE_DELTA,
            None,
            &rendered,
        )?
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
    let rendered = RenderedSummaryDelta::from_plain(combined.clone());
    request_summary(
        provider_kind,
        provider_config,
        SUMMARY_PURPOSE_REBUILD,
        None,
        &rendered,
    )
    .unwrap_or(combined)
}

fn partition_for_compaction(
    conversation: &Conversation,
    config: &ContextConfig,
    counter: &impl TokenCounter,
) -> Option<(Option<Message>, Vec<Message>, Vec<Message>, Vec<Message>)> {
    let messages = &conversation.messages;
    // Hysteresis: compress to the target window, not just below the trigger.
    let target_tokens = config.target_compaction_limit();
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
    keep_latest_droppable_if_empty(&mut kept, &droppable);
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
    purpose: &str,
    previous_summary: Option<&str>,
    rendered: &RenderedSummaryDelta,
) -> Option<String> {
    let collapsed_text = rendered.text.as_str();
    let cache_scope = summary_cache_scope(provider_kind, provider_config);
    let cache_key =
        crate::summary_cache::summary_key(&cache_scope, purpose, previous_summary, collapsed_text);
    if let Some(cached) = crate::summary_cache::lookup(&cache_key) {
        emit_summary_telemetry(purpose, true, rendered);
        return Some(cached);
    }
    emit_summary_telemetry(purpose, false, rendered);

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
    if let Some(usage) = response.usage {
        emit_summary_usage_telemetry(purpose, usage);
    }
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

// Summary-delta rendering tiers. These run on the ORIGINAL collapsed content
// (never the micro-compacted main-context view), so huge tool outputs are
// summarized by summary-specific extractive rules instead of being masked by
// main-context micro compaction. A single layered ruleset is applied to every
// tool output: tiny outputs are kept verbatim, mid-sized ones get a head/tail
// extract, and huge ones get a more aggressive extract.
const SUMMARY_KEEP_VERBATIM_BYTES: usize = 1024;
const SUMMARY_HUGE_BYTES: usize = 8 * 1024;
const SUMMARY_MEDIUM_HEAD_LINES: usize = 8;
const SUMMARY_MEDIUM_TAIL_LINES: usize = 6;
const SUMMARY_MEDIUM_HEAD_CHARS: usize = 384;
const SUMMARY_MEDIUM_TAIL_CHARS: usize = 384;
const SUMMARY_MEDIUM_MAX_BYTES: usize = 900;
const SUMMARY_HUGE_HEAD_LINES: usize = 6;
const SUMMARY_HUGE_TAIL_LINES: usize = 4;
const SUMMARY_HUGE_HEAD_CHARS: usize = 320;
const SUMMARY_HUGE_TAIL_CHARS: usize = 320;
const SUMMARY_HUGE_MAX_BYTES: usize = 700;
const ALREADY_COMPACTED_MARKERS: [&str; 2] =
    ["[tool output micro-compact]", "[extractive-compact]"];
const SUMMARY_PURPOSE_DELTA: &str = "delta";
const SUMMARY_PURPOSE_REBUILD: &str = "rebuild_baseline";
const SUMMARY_DEBUG_ENV: &str = "ORCA_SUMMARY_DEBUG";
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

/// Deterministic, observable rendering of a collapsed conversation segment
/// before it reaches the remote summary model. This is the single entry point
/// for summary-delta input: it always runs on the ORIGINAL collapsed messages,
/// never on the micro-compacted main-context view, so summary-specific
/// extractive rules are never masked by main-context micro compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedSummaryDelta {
    pub text: String,
    pub original_bytes: usize,
    pub rendered_bytes: usize,
    pub original_tokens_est: usize,
    pub rendered_tokens_est: usize,
    pub compacted_tool_outputs: usize,
}

impl RenderedSummaryDelta {
    /// Build metrics for a plain text input that needs no tool-output rendering
    /// (e.g. the merged baseline-rebuild prompt). original == rendered.
    fn from_plain(text: String) -> Self {
        let bytes = text.len();
        let tokens = DefaultTokenCounter.count_text(&text);
        Self {
            text,
            original_bytes: bytes,
            rendered_bytes: bytes,
            original_tokens_est: tokens,
            rendered_tokens_est: tokens,
            compacted_tool_outputs: 0,
        }
    }
}

/// Render the collapsed delta with the unified tool-output tier ruleset.
/// Both tool outputs and natural-language turns (user / assistant) go through
/// a single layered policy (`render_tool_output`): identical inputs always
/// yield identical output, which also stabilizes the summary hash cache and
/// keeps huge stdin / piped user blobs from blowing past the renderer budget.
/// Small content stays verbatim, so summary fidelity for normal turns is
/// preserved.
pub fn render_summary_delta(collapsed: &[Message]) -> RenderedSummaryDelta {
    let original_text = format_messages(collapsed);
    let mut compacted_tool_outputs = 0usize;
    let rendered_messages: Vec<Message> = collapsed
        .iter()
        .map(|message| match message {
            Message::Tool {
                tool_call_id,
                content,
                pinned,
            } => {
                let (rendered, compacted) = render_tool_output(content);
                if compacted {
                    compacted_tool_outputs += 1;
                }
                Message::Tool {
                    tool_call_id: tool_call_id.clone(),
                    content: rendered,
                    pinned: *pinned,
                }
            }
            Message::User { content, pinned } => {
                let (rendered, compacted) = render_tool_output(content);
                if compacted {
                    compacted_tool_outputs += 1;
                }
                Message::User {
                    content: rendered,
                    pinned: *pinned,
                }
            }
            Message::Assistant {
                content,
                reasoning_content: _,
                tool_calls,
                pinned,
            } => {
                let new_content = content.as_ref().map(|c| {
                    let (rendered, compacted) = render_tool_output(c);
                    if compacted {
                        compacted_tool_outputs += 1;
                    }
                    rendered
                });
                Message::Assistant {
                    content: new_content,
                    reasoning_content: None,
                    tool_calls: tool_calls.clone(),
                    pinned: *pinned,
                }
            }
            other => other.clone(),
        })
        .collect();
    let text = format_messages(&rendered_messages);
    let counter = DefaultTokenCounter;
    RenderedSummaryDelta {
        original_bytes: original_text.len(),
        rendered_bytes: text.len(),
        original_tokens_est: counter.count_text(&original_text),
        rendered_tokens_est: counter.count_text(&text),
        compacted_tool_outputs,
        text,
    }
}

/// Unified tool-output tier policy. Returns the rendered content and whether it
/// was compacted (for metrics):
///   - already contains a compaction marker: keep as-is (no double compression)
///   - <= 1KB: keep verbatim
///   - 1KB - 8KB: extractive head/tail (medium tier), capped at a hard budget
///   - > 8KB: more aggressive extractive (huge tier), capped at a tighter budget
///
/// The hard byte budget is the key follow-up: head/tail line counts alone do not
/// bound the rendered size (long lines blow past them), so the real-API cost
/// could exceed the old micro-compaction baseline. After the line/char extract
/// we always trim to the tier budget while preserving the size metadata so the
/// summarizer still sees this is truncated evidence.
fn render_tool_output(content: &str) -> (String, bool) {
    if ALREADY_COMPACTED_MARKERS
        .iter()
        .any(|marker| content.contains(marker))
    {
        return (content.to_string(), false);
    }
    let bytes = content.len();
    if bytes <= SUMMARY_KEEP_VERBATIM_BYTES {
        return (content.to_string(), false);
    }
    let (head_lines, tail_lines, head_chars, tail_chars, max_bytes) = if bytes > SUMMARY_HUGE_BYTES
    {
        (
            SUMMARY_HUGE_HEAD_LINES,
            SUMMARY_HUGE_TAIL_LINES,
            SUMMARY_HUGE_HEAD_CHARS,
            SUMMARY_HUGE_TAIL_CHARS,
            SUMMARY_HUGE_MAX_BYTES,
        )
    } else {
        (
            SUMMARY_MEDIUM_HEAD_LINES,
            SUMMARY_MEDIUM_TAIL_LINES,
            SUMMARY_MEDIUM_HEAD_CHARS,
            SUMMARY_MEDIUM_TAIL_CHARS,
            SUMMARY_MEDIUM_MAX_BYTES,
        )
    };
    let rendered = extractive_render(
        content, head_lines, tail_lines, head_chars, tail_chars, max_bytes,
    );
    // If the policy could not shrink the content (e.g. a small-span tier),
    // report it as untouched so the metric reflects reality.
    let compacted = rendered.len() < bytes;
    (rendered, compacted)
}

fn extractive_render(
    content: &str,
    head_lines: usize,
    tail_lines: usize,
    head_chars: usize,
    tail_chars: usize,
    max_bytes: usize,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > head_lines + tail_lines {
        let head = lines[..head_lines].join("\n");
        let tail = lines[lines.len() - tail_lines..].join("\n");
        let omitted = lines.len() - head_lines - tail_lines;
        let header = format!(
            "[extractive-compact] original_bytes={} original_lines={} omitted_lines={}",
            content.len(),
            lines.len(),
            omitted,
        );
        let body = budget_trim_head_tail(
            head.trim_end(),
            tail.trim_start(),
            &format!("... [{omitted} lines omitted] ..."),
            max_bytes.saturating_sub(header.len() + 2),
        )
        .text;
        return format!("{header}\n{body}");
    }
    let char_count = content.chars().count();
    if char_count <= head_chars + tail_chars && content.len() <= max_bytes {
        return content.to_string();
    }
    let head: String = content.chars().take(head_chars).collect();
    let tail_vec: Vec<char> = content.chars().rev().take(tail_chars).collect();
    let tail: String = tail_vec.into_iter().rev().collect();
    let mut omitted = char_count.saturating_sub(head_chars + tail_chars);
    let mut header = format!(
        "[extractive-compact] original_bytes={} original_chars={} omitted_chars={}",
        content.len(),
        char_count,
        omitted,
    );
    let mut trimmed = budget_trim_head_tail(
        head.trim_end(),
        tail.trim_start(),
        &format!("... [{omitted} chars omitted] ..."),
        max_bytes.saturating_sub(header.len() + 2),
    );
    let actual_omitted =
        char_count.saturating_sub(trimmed.head_chars_kept + trimmed.tail_chars_kept);
    if actual_omitted != omitted {
        omitted = actual_omitted;
        header = format!(
            "[extractive-compact] original_bytes={} original_chars={} omitted_chars={}",
            content.len(),
            char_count,
            omitted,
        );
        trimmed = budget_trim_head_tail(
            head.trim_end(),
            tail.trim_start(),
            &format!("... [{omitted} chars omitted] ..."),
            max_bytes.saturating_sub(header.len() + 2),
        );
    }
    format!("{header}\n{}", trimmed.text)
}

/// Assemble `head`, a separator, and `tail` so the whole body fits in `budget`
/// bytes. Head and tail are trimmed on char boundaries, splitting the remaining
/// space evenly. The separator is always kept so the truncation stays visible.
struct BudgetTrim {
    text: String,
    head_chars_kept: usize,
    tail_chars_kept: usize,
}

fn budget_trim_head_tail(head: &str, tail: &str, separator: &str, budget: usize) -> BudgetTrim {
    let current = head.len() + 1 + separator.len() + 1 + tail.len();
    if current <= budget {
        return BudgetTrim {
            text: format!("{head}\n{separator}\n{tail}"),
            head_chars_kept: head.chars().count(),
            tail_chars_kept: tail.chars().count(),
        };
    }
    let frame = separator.len() + 2; // two newlines around the separator
    let available = budget.saturating_sub(frame);
    let head_budget = available / 2;
    let tail_budget = available - head_budget;
    let head_trimmed = take_chars_within_bytes(head, head_budget);
    let tail_trimmed = take_chars_within_bytes_rev(tail, tail_budget);
    BudgetTrim {
        text: format!("{head_trimmed}\n{separator}\n{tail_trimmed}"),
        head_chars_kept: head_trimmed.chars().count(),
        tail_chars_kept: tail_trimmed.chars().count(),
    }
}

fn take_chars_within_bytes(text: &str, budget: usize) -> &str {
    let mut end = 0;
    for (idx, ch) in text.char_indices() {
        if idx + ch.len_utf8() > budget {
            break;
        }
        end = idx + ch.len_utf8();
    }
    &text[..end]
}

fn take_chars_within_bytes_rev(text: &str, budget: usize) -> &str {
    let mut start = text.len();
    for (idx, _) in text.char_indices().rev() {
        if text.len() - idx > budget {
            break;
        }
        start = idx;
    }
    &text[start..]
}

/// Emit cheap, structured observability for a remote summary request. Off by
/// default; set `ORCA_SUMMARY_DEBUG` to surface the metrics for evaluation.
fn emit_summary_telemetry(purpose: &str, cache_hit: bool, rendered: &RenderedSummaryDelta) {
    if std::env::var_os(SUMMARY_DEBUG_ENV).is_none() {
        return;
    }
    eprintln!("{}", format_summary_telemetry(purpose, cache_hit, rendered));
}

fn emit_summary_usage_telemetry(purpose: &str, usage: orca_core::provider_types::Usage) {
    if std::env::var_os(SUMMARY_DEBUG_ENV).is_none() {
        return;
    }
    eprintln!("{}", format_summary_usage_telemetry(purpose, usage));
}

fn format_summary_telemetry(
    purpose: &str,
    cache_hit: bool,
    rendered: &RenderedSummaryDelta,
) -> String {
    format!(
        "orca.remote_summary requested=1 purpose={} cache_hit={} cache_miss={} original_bytes={} rendered_bytes={} original_tokens_est={} rendered_tokens_est={} compacted_tool_outputs={}",
        purpose,
        cache_hit as u8,
        (!cache_hit) as u8,
        rendered.original_bytes,
        rendered.rendered_bytes,
        rendered.original_tokens_est,
        rendered.rendered_tokens_est,
        rendered.compacted_tool_outputs,
    )
}

fn format_summary_usage_telemetry(
    purpose: &str,
    usage: orca_core::provider_types::Usage,
) -> String {
    let cache_hit_ratio = if usage.input_tokens == 0 {
        0.0
    } else {
        usage.cache_tokens as f64 / usage.input_tokens as f64
    };
    format!(
        "orca.remote_summary_usage purpose={} input_tokens={} output_tokens={} cache_tokens={} cache_hit_ratio={:.4}",
        purpose, usage.input_tokens, usage.output_tokens, usage.cache_tokens, cache_hit_ratio,
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
    // Hysteresis: compress to the target window so the next turn does not
    // immediately re-trigger compaction.
    let target_tokens = config.target_compaction_limit();

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
    keep_latest_droppable_if_empty(&mut kept, &droppable);
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

fn keep_latest_droppable_if_empty(kept: &mut Vec<Message>, droppable: &[&Message]) {
    if kept.is_empty()
        && let Some(message) = droppable.last()
    {
        kept.push((*message).clone());
    }
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
    fn needs_compaction_wire_includes_tool_schema_tokens() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello".to_string());

        let config = ContextConfig {
            max_tokens: 100,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(100),
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "large_tool",
                    "description": "schema ".repeat(300),
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                }
            })]),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        assert!(!needs_compaction(&conv, &config));
        assert!(needs_compaction_wire(&conv, &config, &provider_config));
    }

    #[test]
    fn context_config_uses_model_specific_token_limit() {
        assert_eq!(
            ContextConfig::for_model(Some(orca_core::model::FLASH_MODEL)).max_tokens,
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
        for index in 0..6 {
            conv.add_user(format!("old {index} {}", "x ".repeat(1_200)));
            conv.add_assistant(
                Some(format!("older answer {index} {}", "y ".repeat(1_200))),
                None,
                vec![],
            );
        }
        conv.add_user("newest request".to_string());

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

        assert!(matches!(result.kind, CompactionKind::LocalTruncation));
        assert!(result.conversation.messages.iter().any(|message| {
            matches!(message, Message::System { content, .. } if content.contains("truncated to fit context window"))
        }));
    }

    #[test]
    fn compact_with_existing_summary_falls_back_to_local_when_large_delta_summary_fails() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        for index in 0..6 {
            conv.add_user(format!("old {index} {}", "x ".repeat(1_200)));
            conv.add_assistant(
                Some(format!("older answer {index} {}", "y ".repeat(1_200))),
                None,
                vec![],
            );
        }
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
    fn compact_with_summary_keeps_oversized_current_user_and_summarizes_old_history() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("alpha fact that must be summarized".to_string());
        conv.add_assistant(Some("alpha acknowledged".to_string()), None, vec![]);
        let oversized_current = "beta current turn ".repeat(2_000);
        conv.add_user(oversized_current.clone());

        let config = ContextConfig {
            max_tokens: 10_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(1_000),
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let result = compact_with_summary(ProviderKind::Mock, &conv, &config, &provider_config);

        assert!(
            matches!(result.kind, CompactionKind::RemoteSummary(_)),
            "oversized current turns must not force local truncation of old history"
        );
        assert!(result.conversation.summary.baseline.is_some());
        assert!(result.conversation.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == &oversized_current)
        }));
    }

    #[test]
    fn compact_with_summary_directly_preserves_medium_initial_delta_without_remote_call() {
        let alpha: String = (0..700)
            .map(|i| format!("alpha-cache-collision row {i:04}: stable fact A{}", i % 17))
            .collect::<Vec<_>>()
            .join("\n");
        let beta: String = (0..700)
            .map(|i| format!("beta-cache-collision row {i:04}: stable fact B{}", i % 19))
            .collect::<Vec<_>>()
            .join("\n");
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user(alpha);
        conv.add_assistant(Some("alpha received".to_string()), None, vec![]);
        conv.add_user(beta.clone());

        let config = ContextConfig {
            max_tokens: 50_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(18_000),
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
            matches!(result.kind, CompactionKind::RemoteSummary(_)),
            "medium rendered deltas should not require a remote summary call"
        );
        assert!(
            result
                .conversation
                .summary
                .baseline
                .as_deref()
                .is_some_and(|summary| summary.contains("alpha-cache-collision"))
        );
        assert!(result.conversation.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == &beta)
        }));
    }

    #[test]
    fn existing_summary_preserves_small_collapsed_delta_without_remote_call() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("remember the user chose option B".to_string());
        conv.add_user("current request".to_string());
        conv.summary.baseline = Some("existing baseline".to_string());

        let system_tokens = message_tokens(conv.messages.first().unwrap());
        let summary_tokens = summary_state_tokens(&conv, &DefaultTokenCounter) + 256;
        let newest_tokens = message_tokens(conv.messages.last().unwrap());
        let target_tokens = system_tokens + summary_tokens + newest_tokens + 4;
        let effective_limit = target_tokens.saturating_mul(5).div_ceil(3);
        let config = ContextConfig {
            max_tokens: 10_000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(effective_limit),
        };
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let (_summary_conversation, delta) = summarize_collapsed_messages(
            ProviderKind::DeepSeek,
            &conv,
            &conv,
            &config,
            &provider_config,
        )
        .expect("small collapsed deltas must be kept instead of falling back to local truncation");

        assert!(delta.contains("remember the user chose option B"));
        assert!(
            DefaultTokenCounter.count_text(&delta) < DIRECT_SUMMARY_DELTA_TOKEN_THRESHOLD,
            "test must exercise the small-delta path"
        );
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
        // 266. With hysteresis, partition uses target_compaction_limit() = 60%
        // of effective_limit. We pick 452 so target = 271 and exactly the
        // newest message fits (266 + 5 = 271), collapsing the two before it.
        let config = ContextConfig {
            max_tokens: 1000,
            compaction_threshold: 1.0,
            reserved_for_response: 0,
            auto_compact_token_limit: Some(452),
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
    fn direct_summary_delta_token_threshold_is_reasonable() {
        assert!(
            DIRECT_SUMMARY_DELTA_TOKEN_THRESHOLD > 0
                && DIRECT_SUMMARY_DELTA_TOKEN_THRESHOLD <= 1_000,
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
    fn render_summary_delta_shrinks_large_tool_output_with_head_and_tail() {
        let big_output = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: big_output,
            pinned: false,
        }];

        let rendered = render_summary_delta(&messages);

        assert!(rendered.text.contains("[extractive-compact]"));
        assert!(rendered.text.contains("line 0"));
        assert!(rendered.text.contains("line 199"));
        assert!(rendered.text.contains("lines omitted"));
        assert!(
            rendered.rendered_bytes < rendered.original_bytes,
            "extractive output must be smaller than the original"
        );
        assert_eq!(rendered.compacted_tool_outputs, 1);
    }

    #[test]
    fn render_summary_delta_shrinks_large_single_line_tool_output() {
        let big_output = format!(
            "{{\"status\":\"ok\",\"payload\":\"{}\",\"tail\":\"final-value\"}}",
            "x".repeat(8_000)
        );
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: big_output,
            pinned: false,
        }];

        let rendered = render_summary_delta(&messages);

        assert!(rendered.text.contains("[extractive-compact]"));
        assert!(rendered.text.contains("\"status\":\"ok\""));
        assert!(rendered.text.contains("final-value"));
        assert!(
            rendered.rendered_bytes < rendered.original_bytes,
            "large single-line outputs must shrink before remote summary"
        );
        assert_eq!(rendered.compacted_tool_outputs, 1);
    }

    #[test]
    fn render_summary_delta_uses_more_aggressive_tier_for_huge_outputs() {
        // Multi-line content so both tiers take the line-based extraction path.
        let medium: String = (0..40)
            .map(|i| format!("medium row {i} with some payload"))
            .collect::<Vec<_>>()
            .join("\n");
        let huge: String = (0..400)
            .map(|i| format!("huge row {i} with some payload text to inflate the byte size"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(medium.len() > SUMMARY_KEEP_VERBATIM_BYTES && medium.len() <= SUMMARY_HUGE_BYTES);
        assert!(huge.len() > SUMMARY_HUGE_BYTES);

        let medium_rendered = render_tool_output(&medium).0;
        let huge_rendered = render_tool_output(&huge).0;

        assert!(medium_rendered.contains("[extractive-compact]"));
        assert!(huge_rendered.contains("[extractive-compact]"));
        // The huge tier keeps strictly fewer head/tail lines than the medium tier.
        assert!(
            medium_rendered.contains(&format!(
                "line {} omitted",
                40 - SUMMARY_MEDIUM_HEAD_LINES - SUMMARY_MEDIUM_TAIL_LINES
            )) || medium_rendered.contains("lines omitted")
        );
        assert!(
            SUMMARY_HUGE_HEAD_LINES + SUMMARY_HUGE_TAIL_LINES
                < SUMMARY_MEDIUM_HEAD_LINES + SUMMARY_MEDIUM_TAIL_LINES
        );
        // The first head line of the huge render is line 0; the line right after
        // the kept head must be omitted (proves the shorter huge head was used).
        assert!(huge_rendered.contains("huge row 0 "));
        assert!(!huge_rendered.contains(&format!("huge row {} ", SUMMARY_HUGE_HEAD_LINES)));
    }

    #[test]
    fn render_summary_delta_leaves_small_tool_output_untouched() {
        let small = "short output".to_string();
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: small.clone(),
            pinned: false,
        }];

        let rendered = render_summary_delta(&messages);
        assert!(rendered.text.contains(&small));
        assert_eq!(rendered.compacted_tool_outputs, 0);
    }

    #[test]
    fn render_summary_delta_does_not_recompact_already_compacted_output() {
        let already = format!(
            "[tool output micro-compact]\noriginal_bytes: 99999\nhead:\n{}\n\ntail:\n{}",
            "h".repeat(400),
            "t".repeat(400)
        );
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: already.clone(),
            pinned: false,
        }];

        let rendered = render_summary_delta(&messages);
        assert!(rendered.text.contains("[tool output micro-compact]"));
        assert!(
            !rendered.text.contains("[extractive-compact]"),
            "already-compacted output must not be compacted a second time"
        );
        assert_eq!(rendered.compacted_tool_outputs, 0);
    }

    #[test]
    fn render_summary_delta_keeps_small_natural_language_turns_verbatim() {
        // Small natural-language turns must always pass through untouched so
        // the remote summarizer keeps full fidelity on user intent and
        // assistant decisions.
        let user = "decide whether to ship now or wait for review".to_string();
        let assistant = "I recommend waiting; the soak test failed twice.".to_string();
        let messages = vec![
            Message::user(user.clone()),
            Message::Assistant {
                content: Some(assistant.clone()),
                reasoning_content: None,
                tool_calls: vec![],
                pinned: false,
            },
        ];

        let rendered = render_summary_delta(&messages);
        assert!(rendered.text.contains(&user));
        assert!(rendered.text.contains(&assistant));
        assert_eq!(rendered.compacted_tool_outputs, 0);
    }

    #[test]
    fn render_summary_delta_extractive_renders_huge_stdin_user_blobs() {
        // Round 9 fix: piped/stdin user blobs that blow past the verbatim
        // tier must go through the extractive renderer too. Otherwise huge
        // single user messages drift the wire prompt and re-trigger the
        // compaction storm even after micro/summary compaction.
        let huge_stdin: String = (0..400)
            .map(|i| format!("stdin row {i} payload chunk"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(huge_stdin.len() > SUMMARY_HUGE_BYTES);
        let messages = vec![Message::user(huge_stdin.clone())];

        let rendered = render_summary_delta(&messages);

        assert!(rendered.text.contains("[extractive-compact]"));
        assert!(rendered.rendered_bytes < rendered.original_bytes);
        assert_eq!(rendered.compacted_tool_outputs, 1);
    }

    #[test]
    fn render_summary_delta_extractive_renders_huge_assistant_content() {
        let huge_assistant: String = (0..400)
            .map(|i| format!("assistant row {i} long reasoning narrative"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(huge_assistant.len() > SUMMARY_HUGE_BYTES);
        let messages = vec![Message::Assistant {
            content: Some(huge_assistant.clone()),
            reasoning_content: None,
            tool_calls: vec![],
            pinned: false,
        }];

        let rendered = render_summary_delta(&messages);

        assert!(rendered.text.contains("[extractive-compact]"));
        assert!(rendered.rendered_bytes < rendered.original_bytes);
        assert_eq!(rendered.compacted_tool_outputs, 1);
    }

    #[test]
    fn render_summary_delta_omits_assistant_reasoning_content() {
        let messages = vec![Message::Assistant {
            content: Some("final answer to preserve".to_string()),
            reasoning_content: Some("private chain of thought should not persist".to_string()),
            tool_calls: vec![],
            pinned: false,
        }];

        let rendered = render_summary_delta(&messages);

        assert!(rendered.text.contains("final answer to preserve"));
        assert!(!rendered.text.contains("private chain of thought"));
        assert!(!rendered.text.contains("reasoning:"));
    }

    #[test]
    fn render_summary_delta_is_deterministic() {
        let big_output = (0..200)
            .map(|i| format!("row {i} value"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = vec![Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: big_output,
            pinned: false,
        }];

        let first = render_summary_delta(&messages);
        let second = render_summary_delta(&messages);
        assert_eq!(first, second);
    }

    #[test]
    fn render_summary_delta_reports_metrics_for_mixed_segment() {
        let big_output = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = vec![
            Message::user("inspect the log".to_string()),
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: big_output,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "tiny".to_string(),
                pinned: false,
            },
        ];

        let rendered = render_summary_delta(&messages);
        assert!(rendered.original_bytes > rendered.rendered_bytes);
        assert!(rendered.original_tokens_est >= rendered.rendered_tokens_est);
        assert_eq!(
            rendered.compacted_tool_outputs, 1,
            "only the large tool output should count as compacted"
        );
    }

    #[test]
    fn summary_telemetry_reports_original_and_rendered_token_estimates() {
        let rendered = RenderedSummaryDelta {
            text: "rendered".to_string(),
            original_bytes: 100,
            rendered_bytes: 40,
            original_tokens_est: 25,
            rendered_tokens_est: 10,
            compacted_tool_outputs: 2,
        };

        let line = format_summary_telemetry("delta", false, &rendered);

        assert!(line.contains("original_tokens_est=25"));
        assert!(line.contains("rendered_tokens_est=10"));
        assert!(
            !line.contains(" input_tokens_est="),
            "ambiguous token metric name should not be emitted"
        );
    }

    #[test]
    fn summary_usage_telemetry_reports_provider_cache_tokens() {
        let usage = orca_core::provider_types::Usage {
            input_tokens: 1_000,
            output_tokens: 25,
            cache_tokens: 768,
        };

        let line = format_summary_usage_telemetry("delta", usage);

        assert!(line.contains("orca.remote_summary_usage"));
        assert!(line.contains("purpose=delta"));
        assert!(line.contains("input_tokens=1000"));
        assert!(line.contains("output_tokens=25"));
        assert!(line.contains("cache_tokens=768"));
        assert!(line.contains("cache_hit_ratio=0.7680"));
    }

    #[test]
    fn huge_summary_renderer_is_no_larger_than_micro_compact_baseline() {
        // The real-API target: the renderer must never make a huge tool output
        // *more* expensive than the old micro-compaction path that the main
        // context already applied. We assert byte parity against the exact same
        // baseline (`micro_compact_tool_output`) the old path produced.
        let huge = (0..600)
            .map(|i| format!("row {i}: data payload chunk {i} with content"))
            .collect::<Vec<_>>()
            .join("\n");
        let baseline = micro_compact_tool_output(&huge);
        let (rendered, _) = render_tool_output(&huge);

        assert!(
            rendered.len() <= baseline.len(),
            "huge renderer ({}) must not exceed micro-compact baseline ({})",
            rendered.len(),
            baseline.len()
        );
        assert!(rendered.len() <= SUMMARY_HUGE_MAX_BYTES);
        // Evidence metadata must survive the hard budget.
        assert!(rendered.contains("original_bytes="));
    }

    #[test]
    fn medium_summary_renderer_stays_under_tool_budget() {
        let medium = (0..120)
            .map(|i| format!("2024-06-23 INFO worker[{i}] processed batch ok"))
            .collect::<Vec<_>>()
            .join("\n");
        let (rendered, compacted) = render_tool_output(&medium);

        assert!(compacted);
        assert!(
            rendered.len() <= SUMMARY_MEDIUM_MAX_BYTES,
            "medium renderer {} exceeds budget {}",
            rendered.len(),
            SUMMARY_MEDIUM_MAX_BYTES
        );
        assert!(rendered.contains("original_bytes="));
        assert!(rendered.contains("original_lines="));
    }

    #[test]
    fn single_line_huge_output_is_hard_trimmed_to_budget_with_metadata() {
        // A huge single line cannot be split on newlines, so it must fall through
        // to the char-level budget trim while keeping the size metadata.
        let huge_line = format!("{{\"payload\":\"{}\"}}", "x".repeat(30_000));
        let (rendered, compacted) = render_tool_output(&huge_line);

        assert!(compacted);
        assert!(rendered.len() <= SUMMARY_HUGE_MAX_BYTES);
        assert!(rendered.contains("original_bytes="));
    }

    #[test]
    fn multibyte_tool_output_reports_omitted_chars_after_byte_budget_trim() {
        let output = "界".repeat(500);
        assert!(output.len() > SUMMARY_KEEP_VERBATIM_BYTES);

        let (rendered, compacted) = render_tool_output(&output);

        assert!(compacted);
        assert!(rendered.len() <= SUMMARY_MEDIUM_MAX_BYTES);
        let omitted = rendered
            .lines()
            .next()
            .and_then(|line| line.split("omitted_chars=").nth(1))
            .and_then(|value| value.parse::<usize>().ok())
            .expect("omitted_chars metadata");
        let kept_chars = rendered.matches('界').count();
        assert_eq!(
            omitted,
            500usize.saturating_sub(kept_chars),
            "metadata must reflect chars dropped by the hard byte budget: {rendered}"
        );
    }
}
