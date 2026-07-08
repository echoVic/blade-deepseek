# Context Stability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Prevent long Orca coding sessions from appearing stuck by controlling file/tool output growth, stripping stale reasoning from provider replay, and compacting before latency becomes user-visible.

**Architecture:** Keep Orca's current synchronous runtime and transcript model. Add bounded read/search surfaces first, then a lightweight pressure pipeline before existing summarization compaction, then observability and regression coverage based on the reported session.

**Tech Stack:** Rust workspace, `serde_json`, existing `orca-provider::context`, existing `orca-tools` registry, current JSONL transcript tests.

## Global Constraints

- Preserve current event names: `context.collapsed`, `session.usage`, `tool.call.requested`, `tool.call.completed`.
- Do not remove transcript `reasoning_content`; change provider replay and compaction rendering first.
- Keep DeepSeek 1M model context support, but do not wait for 80 percent of 1M before soft compaction.
- Prefer deterministic tests with mock/deepseek-fixture; no network required.
- Follow current Rust formatting and run focused tests before broader checks.

---

## Source Findings To Carry Into Implementation

- Claude Code's `FileReadTool` supports strict `offset` and `limit` parameters and tells the model to use ranges for large files. It also deduplicates repeated identical reads.
- Claude Code's grep supports `head_limit` and `offset`; the default result cap is 250, with explicit `0` as the unlimited escape hatch.
- Claude Code runs staged pressure handling: tool-result budget, snip/microcompact, context-collapse projection, proactive autocompact, then reactive compact on prompt-too-long.
- Claude Code's canonical context measurement uses the last API response usage plus estimates for newly added messages, not cumulative billing totals.
- Codex tracks auto-compaction windows separately from total token spend, with window ids, prefill baselines, and `tokens_until_compaction`.
- Codex serializes reasoning summaries by default and avoids serializing raw reasoning text in normal model-visible paths.
- Orca currently has good summary compaction, but `read_file` lacks `offset/limit`, `read_file` schema does not reject unknown keys, stale assistant `reasoning_content` is replayed to DeepSeek, and proactive compaction is tied to the very large effective model limit.

---

### Task 1: Add Ranged `read_file`

**Files:**
- Modify: `crates/orca-tools/src/read_file.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Test: `crates/orca-tools/src/read_file.rs`
- Test: `crates/orca-tools/src/lib.rs`

**Interfaces:**
- Consumes: `ToolRequest.raw_arguments: Option<String>` and existing `ToolRequest.target`.
- Produces: `read_file` arguments `{ path: string, offset?: integer, limit?: integer }`, where `offset` is 1-based line number and `limit` is positive line count.

- [x] **Step 1: Write failing tests for ranged reads**

Add tests in `crates/orca-tools/src/read_file.rs`:

```rust
#[test]
fn read_file_respects_offset_and_limit() {
    let cwd = temp_dir("read-file-range");
    fs::create_dir_all(&cwd).expect("create temp workspace");
    fs::write(cwd.join("notes.txt"), "one\ntwo\nthree\nfour\n").expect("write fixture");
    let request = ToolRequest {
        id: "read-1".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Read,
        target: Some("notes.txt".to_string()),
        raw_arguments: Some(r#"{"path":"notes.txt","offset":2,"limit":2}"#.to_string()),
    };

    let result = execute(&request, &cwd, 1024);

    assert_eq!(result.status, ToolStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("2: two\n3: three"));
    assert!(!result.truncated);
}

#[test]
fn read_file_reports_short_file_when_offset_is_past_end() {
    let cwd = temp_dir("read-file-short");
    fs::create_dir_all(&cwd).expect("create temp workspace");
    fs::write(cwd.join("notes.txt"), "one\n").expect("write fixture");
    let request = ToolRequest {
        id: "read-1".to_string(),
        name: ToolName::ReadFile,
        action: ActionKind::Read,
        target: Some("notes.txt".to_string()),
        raw_arguments: Some(r#"{"path":"notes.txt","offset":5,"limit":2}"#.to_string()),
    };

    let result = execute(&request, &cwd, 1024);

    assert_eq!(result.status, ToolStatus::Completed);
    assert_eq!(
        result.output.as_deref(),
        Some("[file has 1 lines; requested offset 5 is past end]")
    );
}
```

- [x] **Step 2: Run tests and verify failure**

Run: `cargo test -p orca-tools read_file_respects_offset_and_limit read_file_reports_short_file_when_offset_is_past_end`

Expected: both tests fail because `read_file` currently ignores `offset` and `limit`.

- [x] **Step 3: Implement argument parsing and ranged output**

Add a local args struct and range renderer in `crates/orca-tools/src/read_file.rs`:

```rust
#[derive(Debug, Default, serde::Deserialize)]
struct ReadFileArgs {
    path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn parse_args(request: &ToolRequest) -> ReadFileArgs {
    request
        .raw_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ReadFileArgs>(raw).ok())
        .unwrap_or_default()
}

fn render_range(contents: &str, offset: usize, limit: Option<usize>) -> String {
    let total = contents.lines().count();
    if offset > total {
        return format!("[file has {total} lines; requested offset {offset} is past end]");
    }
    let take = limit.unwrap_or(usize::MAX);
    contents
        .lines()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(take)
        .map(|(idx, line)| format!("{}: {}", idx + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}
```

Change `execute()` so the resolved path comes from `args.path.or(request.target.clone())`, and when `offset` or `limit` is provided, run `render_range()` before `truncate_output`.

- [x] **Step 4: Make schema strict and advertise range params**

Update `read_file` schema in `crates/orca-tools/src/registry.rs`:

```rust
json!({
    "type": "object",
    "properties": {
        "path": {
            "type": "string",
            "description": "File path relative to workspace root"
        },
        "offset": {
            "type": "integer",
            "minimum": 1,
            "description": "1-based line number to start reading from. Use for large files."
        },
        "limit": {
            "type": "integer",
            "minimum": 1,
            "description": "Number of lines to read. Use with offset for large files."
        }
    },
    "required": ["path"],
    "additionalProperties": false
})
```

- [x] **Step 5: Run focused verification**

Run: `cargo test -p orca-tools read_file -- --test-threads=1`

Expected: all `read_file` tests pass.

---

### Task 2: Add Search Pagination Defaults

**Files:**
- Modify: `crates/orca-tools/src/grep.rs`
- Modify: `crates/orca-tools/src/glob.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Test: `crates/orca-tools/src/grep.rs`
- Test: `crates/orca-tools/src/glob.rs`

**Interfaces:**
- Produces optional `head_limit` and `offset` for grep and glob-like outputs.
- Default cap: `grep` 250 lines, `glob` 500 paths. `head_limit = 0` means unlimited.

- [x] **Step 1: Add failing tests for grep pagination**

Add a test that writes 300 matching lines, calls grep with no `head_limit`, and asserts 250 result lines plus a final pagination notice:

```rust
assert!(output.contains("[Showing first 250 results; use offset=250 to continue]"));
```

- [x] **Step 2: Add failing tests for explicit offset**

Call grep with:

```json
{"pattern":"needle","path":"notes.txt","head_limit":10,"offset":250}
```

Assert the first returned line is the 251st match and the notice mentions `offset=260`.

- [x] **Step 3: Implement shared pagination helper**

Add a small helper near grep/glob implementation:

```rust
fn paginate<T: Clone>(items: &[T], offset: usize, head_limit: Option<usize>, default_limit: usize) -> (Vec<T>, Option<usize>) {
    if head_limit == Some(0) {
        return (items.get(offset..).unwrap_or_default().to_vec(), None);
    }
    let limit = head_limit.unwrap_or(default_limit);
    let page = items
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let next_offset = (items.len().saturating_sub(offset) > limit).then_some(offset + limit);
    (page, next_offset)
}
```

- [x] **Step 4: Update schemas**

Add `head_limit` and `offset` to grep/glob schemas, and set `additionalProperties: false` if missing.

- [x] **Step 5: Run verification**

Run: `cargo test -p orca-tools grep glob -- --test-threads=1`

Expected: pagination and existing tests pass.

---

### Task 3: Stop Replaying Stale Raw Reasoning

**Files:**
- Modify: `crates/orca-provider/src/deepseek_http.rs`
- Modify: `crates/orca-provider/src/context.rs`
- Test: `crates/orca-provider/src/deepseek_http.rs`
- Test: `crates/orca-provider/src/context.rs`
- Test: `tests/provider_contract.rs`

**Interfaces:**
- Preserve transcript `Message::Assistant.reasoning_content`.
- Provider replay should send `reasoning_content` only for the newest unfinished assistant/tool boundary if DeepSeek requires it; otherwise omit older raw reasoning.
- Token budgeting for normal context should not count old reasoning as user-controllable context.

- [x] **Step 1: Write failing provider replay test**

In `deepseek_http.rs`, create a conversation with two prior assistant messages containing reasoning and assert `conversation_to_api_messages()` omits `reasoning_content` for the older one.

```rust
#[test]
fn api_replay_omits_stale_reasoning_content() {
    let mut conv = Conversation::new();
    conv.add_user("first".to_string());
    conv.add_assistant(Some("done".to_string()), Some("private thinking".to_string()), vec![]);
    conv.add_user("next".to_string());

    let messages = conversation_to_api_messages(&conv);

    assert!(messages.iter().all(|m| m.reasoning_content.is_none()));
}
```

- [x] **Step 2: Write failing context token test**

In `context.rs`, assert stale reasoning does not inflate `conversation_tokens()` or `wire_equivalent_tokens()` after a completed assistant turn.

- [x] **Step 3: Implement replay stripping**

Change `conversation_to_api_messages()` in the `Message::Assistant` branch:

```rust
let replay_reasoning = None;
ApiMessage {
    role: "assistant".to_string(),
    content: content.clone(),
    reasoning_content: replay_reasoning,
    tool_calls: api_tool_calls,
    tool_call_id: None,
}
```

If DeepSeek later proves it needs reasoning for assistant messages with tool calls, restrict preservation to the immediately preceding assistant message that has `tool_calls` and unresolved tool results. Do not preserve all historical reasoning.

- [x] **Step 4: Adjust token counting**

In `message_tokens_with_counter`, stop counting `reasoning_content` by default. If a helper is needed for transcript display, add `message_tokens_including_reasoning_for_debug()` and keep context compaction on the stripped version.

- [x] **Step 5: Run verification**

Run: `cargo test -p orca-provider api_replay_omits_stale_reasoning_content render_summary_delta_omits_assistant_reasoning_content -- --test-threads=1`

Expected: provider no longer replays stale raw reasoning; summary rendering still omits reasoning.

---

### Task 4: Add Soft Context Pressure Before Full Compaction

**Files:**
- Modify: `crates/orca-provider/src/context.rs`
- Modify: `crates/orca-runtime/src/compaction.rs`
- Modify: `crates/orca-tui/src/agent_runner.rs`
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `crates/orca-core/src/config/file.rs`
- Test: `crates/orca-provider/src/context.rs`
- Test: `tests/runtime_lifecycle_contract.rs`

**Interfaces:**
- Produces `ContextPressure` with fields:
  - `wire_tokens: usize`
  - `effective_limit: usize`
  - `soft_limit: usize`
  - `should_soft_compact: bool`
  - `should_hard_compact: bool`
- Default soft limit: `96_000` tokens, capped at existing `effective_limit`.
- Config override: `[model_runtime] soft_compact_token_limit = 96000`.

- [x] **Step 1: Add failing pressure tests**

In `context.rs`, add tests:

```rust
#[test]
fn pressure_triggers_soft_limit_before_model_limit() {
    let config = ContextConfig {
        max_tokens: 1_000_000,
        compaction_threshold: 0.80,
        reserved_for_response: 4096,
        auto_compact_token_limit: None,
        soft_compact_token_limit: Some(96_000),
    };
    let pressure = context_pressure_for_tokens(120_000, &config);
    assert!(pressure.should_soft_compact);
    assert!(!pressure.should_hard_compact);
}
```

- [x] **Step 2: Extend config**

Add `soft_compact_token_limit: Option<usize>` to runtime config normalization, TOML parsing, and display output. Keep default `Some(96_000)` for DeepSeek/auto.

- [x] **Step 3: Add pressure helper**

Implement:

```rust
pub fn context_pressure(conversation: &Conversation, config: &ContextConfig, provider_config: &ProviderConfig) -> ContextPressure {
    let wire_tokens = wire_equivalent_tokens(conversation, provider_config);
    context_pressure_for_tokens(wire_tokens, config)
}
```

- [x] **Step 4: Use soft pressure in runtime loops**

In `RuntimeCompactionStep::compact_if_needed()` and TUI `agent_runner`, replace direct `needs_compaction_wire()` calls with:

```rust
let pressure = context::context_pressure(conversation, self.context_config, self.provider_config);
if !pressure.should_soft_compact && !pressure.should_hard_compact {
    return Ok(false);
}
```

Use the existing `compact_with_summary()` path for now. This is intentionally conservative: it compacts earlier, but does not add a second compaction algorithm in this task.

- [x] **Step 5: Emit pressure metadata**

Add a lightweight status event or enrich existing context update with `soft_limit` and `tokens_until_compaction`. TUI should show context left based on soft limit when soft limit is lower than model limit.

- [x] **Step 6: Run verification**

Run: `cargo test -p orca-provider context_pressure -- --test-threads=1`

Run: `cargo test -p orca-runtime runtime_lifecycle_contract -- --test-threads=1`

Expected: soft pressure triggers before 1M-window hard limit and emits/persists the same compaction events.

---

### Task 5: Add Tool Result Budget Micro-Compaction

**Files:**
- Modify: `crates/orca-provider/src/context.rs`
- Modify: `crates/orca-core/src/conversation.rs`
- Test: `crates/orca-provider/src/context.rs`

**Interfaces:**
- Produces a pre-summary pass that replaces old tool outputs with:

```text
[old tool result content cleared; original_bytes=N; tool_call_id=...]
```

- Keep the newest 6 tool results verbatim by default.
- Keep any tool output younger than the current assistant trajectory.

- [x] **Step 1: Add failing micro-compaction test**

Create a conversation with 10 large tool outputs and assert old ones are replaced while the newest 6 remain.

- [x] **Step 2: Implement compactable tool identification**

Treat these tool names as compactable: `read_file`, `grep`, `glob`, `bash`, `web_search`, MCP tools. Use assistant `tool_calls` to map `tool_call_id` to tool name.

- [x] **Step 3: Apply before summary compaction**

Run this pass inside `compact_with_summary()` before `summarize_collapsed_messages()`, similar to the existing stale tool output micro-compaction but count-based rather than only byte-based.

- [x] **Step 4: Run verification**

Run: `cargo test -p orca-provider micro_compacts_old_tool_results -- --test-threads=1`

Expected: old tool outputs stop accumulating while recent context remains useful.

---

### Task 6: Add Reported Session Regression Fixture

**Files:**
- Create: `tests/fixtures/session_stuck_2026_07_08.min.jsonl`
- Modify: `tests/runtime_lifecycle_contract.rs`

**Interfaces:**
- Fixture is sanitized and minimized; no API keys or user private project content.
- Test reconstructs conversation shape: repeated full-file reads, long reasoning, edits, and a correction turn.

- [x] **Step 1: Create sanitized fixture**

Create a fixture that preserves:

```json
{"type":"session.usage","input_tokens":877257,"output_tokens":20902,"cache_tokens":824576}
{"type":"conversation.message","message":{"role":"assistant","reasoning_content":"<20k chars synthetic>","tool_calls":[{"id":"call_read","function_name":"read_file","arguments":"{\"path\":\"lib/meta.ts\",\"offset\":175,\"limit\":70}"}]}}
{"type":"conversation.message","message":{"role":"tool","tool_call_id":"call_read","content":"<8k chars synthetic full file output>"}}
```

- [x] **Step 2: Add regression test**

Test that after Task 1 and Task 3:
- `read_file` respects the range.
- stale reasoning is not replayed.
- context pressure crosses soft limit and triggers compaction before 800k.

- [x] **Step 3: Run regression**

Run: `cargo test --test runtime_lifecycle_contract reported_session_triggers_soft_compaction -- --test-threads=1`

Expected: the reported pattern is covered without requiring the original private JSONL.

---

### Task 7: User-Facing Recovery And Status

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/agent_runner.rs`
- Modify: `crates/orca-core/src/event_schema.rs`
- Test: `crates/orca-tui/src/runtime_event_projection.rs`

**Interfaces:**
- Show a concise status when soft compaction starts: `Compacting context...`
- Show `tokens until compact` or percent left from the soft window, not only the 1M model window.
- When user sends a correction after an interrupted turn, emit a non-blocking hint that previous edits remain in the worktree.

- [x] **Step 1: Add projection test**

Assert a compaction event updates TUI status and context metrics.

- [x] **Step 2: Add status updates around compaction**

Before `session.compact(config, &cwd)`, send a TUI event with the current pressure values. After compaction, send updated context.

- [x] **Step 3: Add interrupted-turn hint**

When the previous `session.completed` status was `interrupted` and the next user message is not `/exit`, append a small runtime note to volatile context:

```text
Previous turn was interrupted; existing workspace edits remain. Inspect git diff before continuing.
```

- [x] **Step 4: Run TUI focused tests**

Run: `cargo test -p orca-tui runtime_event_projection context -- --test-threads=1`

Expected: status projection remains stable.

---

### Task 8: Final Validation Bundle

**Files:**
- No new files unless tests expose missing coverage.

**Interfaces:**
- Must pass focused provider, tools, runtime, and TUI tests.

- [x] **Step 1: Format**

Run: `cargo fmt --check`

Expected: pass.

- [x] **Step 2: Diff hygiene**

Run: `git diff --check`

Expected: no whitespace errors.

- [x] **Step 3: Focused tests**

Run:

```bash
cargo test -p orca-tools read_file grep glob -- --test-threads=1
cargo test -p orca-provider context_pressure api_replay_omits_stale_reasoning_content -- --test-threads=1
cargo test -p orca-runtime runtime_lifecycle_contract -- --test-threads=1
cargo test -p orca-tui runtime_event_projection -- --test-threads=1
```

Expected: all pass.

- [x] **Step 4: Workspace check**

Run: `cargo check --workspace`

Expected: pass.

- [x] **Step 5: Manual smoke**

Run a mock/deepseek-fixture session with:
- a large file read using `offset/limit`
- repeated grep results
- a synthetic long reasoning turn
- a user correction after interruption

Expected:
- ranged reads stay small
- context pressure compacts before 96k
- `context.collapsed` appears
- stale reasoning is absent from provider replay
- no max-turn exhaustion or apparent stall

---

## Execution Order

1. Task 1 first: it directly fixes the reported `offset/limit` failure mode.
2. Task 3 second: it removes repeated raw reasoning bloat.
3. Task 4 third: it changes when compaction triggers, using existing compaction behavior.
4. Task 2 and Task 5 next: they reduce broad search/tool-result growth.
5. Task 6 and Task 7 last: lock the incident into regression coverage and make the behavior visible.

## Not In Scope

- Do not implement Claude Code cached microcompact/cache editing in this pass.
- Do not implement Codex remote compaction protocol in this pass.
- Do not rewrite Orca history/session persistence.
- Do not lower DeepSeek model context from 1M; add a soft operational limit instead.

## Self-Review

- Spec coverage: covers file read pagination, tool-output growth, reasoning replay, proactive compaction, prompt-too-long fallback, and user-visible status.
- Placeholder scan: no `TBD` or open-ended "add tests" steps remain.
- Type consistency: `ContextPressure`, `soft_compact_token_limit`, `offset`, `limit`, and `head_limit` are consistently named across tasks.
