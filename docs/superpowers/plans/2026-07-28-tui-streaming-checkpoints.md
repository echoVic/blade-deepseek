# TUI Streaming Markdown Checkpoints Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound repeated assistant Markdown parsing and wrapping by freezing stable streaming blocks into immutable transcript chunks while gating partial lines and holding incomplete pipe tables.

**Architecture:** A pure `StreamingMarkdownAssembler` consumes assistant segments after proposed-plan parsing and emits deterministic tail/freeze actions. `AppState` applies those actions as immutable `AssistantChunk` messages plus one mutable `Assistant` tail, allowing the existing message-revision and `TranscriptRenderCache` system to reuse all frozen work. Rendering adds no new cache and keeps ordinary replayed assistant behavior unchanged.

**Tech Stack:** Rust 2024, pulldown-cmark 0.12, existing `ProposedPlanStreamParser`, ratatui 0.29, existing `TranscriptRenderCache`.

---

## File Map

- Create `crates/orca-tui/src/streaming_markdown.rs`
  - Pure newline gate, block/fence state, table holdback, finish actions, and
    reconstruction tests.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the focused module.
- Modify `crates/orca-tui/src/types.rs`
  - Add `AssistantChunk`, assembler/tail ownership, event-boundary finishing,
    settled-state behavior, reset logic, and state tests.
- Modify `crates/orca-tui/src/ui.rs`
  - Render chunks with exact blank-row semantics and add completed-frame tests.
- Modify `crates/orca-tui/src/transcript_view.rs`
  - Add checkpoint-cache performance tests and exhaustive message matches only.
- Modify `crates/orca-tui/src/app.rs`
  - Add frame-level newline/table integration tests; production app loop should
    remain unchanged.

Baseline for final scope audit:

```text
be8ff507b0ca791732b05f216c790d2cd0aac9ef
```

---

### Task 1: Build the Newline-Gated Pure Assembler

**Files:**
- Create: `crates/orca-tui/src/streaming_markdown.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Register an empty test module**

In `crates/orca-tui/src/lib.rs`, add:

```rust
mod streaming_markdown;
```

Create `streaming_markdown.rs` with test-only intended types:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StreamingMarkdownAction {
    UpdateTail(String),
    FreezeTail {
        text: String,
        trailing_blank: bool,
    },
    AppendFrozen {
        text: String,
        trailing_blank: bool,
    },
    ClearTail,
    FinishTail(String),
}

#[derive(Default)]
pub(crate) struct StreamingMarkdownAssembler;
```

- [ ] **Step 2: Write failing newline-gate tests**

Add:

```rust
#[test]
fn partial_source_line_stays_hidden_until_newline_or_finish() {
    let mut assembler = StreamingMarkdownAssembler::default();
    assert!(assembler.push("hello").is_empty());
    assert_eq!(assembler.held_text_for_test(), "hello");
    assert_eq!(
        assembler.push(" world\n"),
        vec![StreamingMarkdownAction::UpdateTail(
            "hello world\n".to_string()
        )]
    );
    assert_eq!(assembler.held_text_for_test(), "");

    assert!(assembler.push("final").is_empty());
    assert_eq!(
        assembler.finish(),
        vec![StreamingMarkdownAction::FinishTail("final".to_string())]
    );
    assert!(assembler.finish().is_empty());
}
```

Add Unicode split coverage:

```rust
#[test]
fn newline_gate_reconstructs_cjk_emoji_and_combining_text_exactly() {
    let mut assembler = StreamingMarkdownAssembler::default();
    let input = ["中", "文👍🏽e\u{301}", "\n尾", "行"];
    let mut actions = Vec::new();
    for piece in input {
        actions.extend(assembler.push(piece));
    }
    actions.extend(assembler.finish());
    assert_eq!(
        reconstructed_action_text(&actions),
        "中文👍🏽e\u{301}\n尾行"
    );
}
```

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui partial_source_line_stays_hidden --lib
cargo test -p orca-tui newline_gate_reconstructs --lib
```

Expected: methods and state are missing.

- [ ] **Step 4: Implement only newline extraction**

Use:

```rust
#[derive(Default)]
pub(crate) struct StreamingMarkdownAssembler {
    partial_line: String,
    current_block: String,
    finished: bool,
}

impl StreamingMarkdownAssembler {
    pub(crate) fn push(&mut self, text: &str) -> Vec<StreamingMarkdownAction> {
        if self.finished || text.is_empty() {
            return Vec::new();
        }
        self.partial_line.push_str(text);
        let Some(last_newline) = self.partial_line.rfind('\n') else {
            return Vec::new();
        };
        let complete = self.partial_line[..=last_newline].to_owned();
        self.partial_line.drain(..=last_newline);
        self.current_block.push_str(&complete);
        vec![StreamingMarkdownAction::UpdateTail(
            self.current_block.clone(),
        )]
    }

    pub(crate) fn finish(&mut self) -> Vec<StreamingMarkdownAction> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.current_block.is_empty() && self.partial_line.is_empty() {
            Vec::new()
        } else {
            self.current_block.clear();
            vec![StreamingMarkdownAction::FinishTail(std::mem::take(
                &mut self.partial_line,
            ))]
        }
    }

    #[cfg(test)]
    fn held_text_for_test(&self) -> &str {
        &self.partial_line
    }
}
```

`reconstructed_action_text` is test-only and concatenates only action text
without duplicating repeated `UpdateTail` snapshots. It tracks the current tail
and replaces it on `UpdateTail`; `FinishTail` appends the final hidden suffix
to that visible snapshot.

Task 2 may refactor newline processing from one aggregate `UpdateTail` into
line-by-line actions. Task 1 tests pin only newline visibility and exact
reconstruction; Task 2 tests become authoritative for the final action
sequence around checkpoints.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui streaming_markdown --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/lib.rs crates/orca-tui/src/streaming_markdown.rs
git commit -m "feat(tui): gate streaming markdown by newline" \
  -m "Hold partial assistant source lines until a newline or semantic finish boundary." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Freeze Blank-Delimited and Fenced Blocks

**Files:**
- Modify: `crates/orca-tui/src/streaming_markdown.rs`

- [ ] **Step 1: Write failing blank-boundary tests**

Add:

```rust
#[test]
fn blank_line_freezes_the_visible_tail_and_starts_a_fresh_block() {
    let mut assembler = StreamingMarkdownAssembler::default();
    assert_eq!(
        assembler.push("first paragraph\n\n"),
        vec![
            StreamingMarkdownAction::UpdateTail(
                "first paragraph\n\n".to_string()
            ),
            StreamingMarkdownAction::FreezeTail {
                text: "first paragraph\n\n".to_string(),
                trailing_blank: true,
            },
            StreamingMarkdownAction::ClearTail,
        ]
    );
    assert_eq!(
        assembler.push("second paragraph\n"),
        vec![StreamingMarkdownAction::UpdateTail(
            "second paragraph\n".to_string()
        )]
    );
}
```

Add a test asserting two consecutive blank lines remain exact source text but
produce only one `trailing_blank` display flag for the frozen chunk.

- [ ] **Step 2: Write failing fence tests**

Add:

```rust
#[test]
fn fenced_block_freezes_only_after_matching_close() {
    let mut assembler = StreamingMarkdownAssembler::default();
    assert_eq!(
        assembler.push("before\n\n```rust\nfn main() {\n"),
        vec![
            StreamingMarkdownAction::UpdateTail("before\n\n".to_string()),
            StreamingMarkdownAction::FreezeTail {
                text: "before\n\n".to_string(),
                trailing_blank: true,
            },
            StreamingMarkdownAction::ClearTail,
            StreamingMarkdownAction::UpdateTail(
                "```rust\nfn main() {\n".to_string()
            ),
        ]
    );
    assert_eq!(
        assembler.push("}\n```\n"),
        vec![
            StreamingMarkdownAction::UpdateTail(
                "```rust\nfn main() {\n}\n```\n".to_string()
            ),
            StreamingMarkdownAction::FreezeTail {
                text: "```rust\nfn main() {\n}\n```\n".to_string(),
                trailing_blank: false,
            },
            StreamingMarkdownAction::ClearTail,
        ]
    );
}
```

Cover:

- tilde fences;
- up to three leading spaces;
- closing run shorter than opening does not close;
- different fence character does not close;
- unfinished fence is emitted by `finish`.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui blank_line_freezes --lib
cargo test -p orca-tui fenced_block_freezes --lib
```

- [ ] **Step 4: Implement line-by-line block scanning**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FenceState {
    marker: char,
    run_len: usize,
}
```

Process every extracted complete line in source order. Helpers:

```rust
fn fence_open(line: &str) -> Option<FenceState>;
fn fence_closes(line: &str, fence: FenceState) -> bool;
fn line_is_blank(line: &str) -> bool;
```

The action invariant is:

- `UpdateTail` always replaces the current visible tail snapshot;
- `FreezeTail` always refers to that exact snapshot;
- `ClearTail` follows every freeze;
- `AppendFrozen` is reserved for hidden content such as tables that was never
  displayed as a tail.

Do not split a fenced block at blank lines.

- [ ] **Step 5: Add reconstruction/property-style tests**

For fixtures including paragraphs, lists, headings, blank lines, backticks,
tildes, CJK, and combining text:

```rust
assert_eq!(
    reconstructed_action_text(&all_actions_and_finish),
    original_input
);
```

Also assert `held_text_for_test()` plus already frozen text equals all input
after every individual `push`.

- [ ] **Step 6: Run GREEN and commit**

```sh
cargo test -p orca-tui blank_line_freezes --lib
cargo test -p orca-tui fence --lib
cargo test -p orca-tui streaming_markdown --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/streaming_markdown.rs
git commit -m "feat(tui): freeze stable streaming markdown blocks" \
  -m "Checkpoint blank-delimited prose and complete fenced code while preserving exact source text." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Hold Pipe Tables Until Complete

**Files:**
- Modify: `crates/orca-tui/src/streaming_markdown.rs`

- [ ] **Step 1: Write failing table-candidate tests**

Add:

```rust
#[test]
fn pipe_header_candidate_is_hidden_until_confirmed_or_rejected() {
    let mut confirmed = StreamingMarkdownAssembler::default();
    assert!(confirmed.push("| Name | Value |\n").is_empty());
    assert_eq!(confirmed.held_text_for_test(), "| Name | Value |\n");
    assert!(confirmed.push("|---|---|\n").is_empty());
    assert_eq!(
        confirmed.held_text_for_test(),
        "| Name | Value |\n|---|---|\n"
    );

    let mut rejected = StreamingMarkdownAssembler::default();
    assert!(rejected.push("A | B\n").is_empty());
    assert_eq!(
        rejected.push("ordinary next line\n"),
        vec![StreamingMarkdownAction::UpdateTail(
            "A | B\nordinary next line\n".to_string()
        )]
    );
}
```

Cover escaped `\|`, no-content pipes, and delimiter alignment forms.

- [ ] **Step 2: Write failing whole-table release tests**

Add:

```rust
#[test]
fn confirmed_table_remains_hidden_until_boundary_then_emits_once() {
    let mut assembler = StreamingMarkdownAssembler::default();
    assert!(assembler.push("| Name | Value |\n|---|---|\n").is_empty());
    assert!(assembler.push("| A | 1 |\n").is_empty());
    assert_eq!(
        assembler.push("\n"),
        vec![StreamingMarkdownAction::AppendFrozen {
            text: "| Name | Value |\n|---|---|\n| A | 1 |\n\n".to_string(),
            trailing_blank: true,
        }]
    );
    assert!(assembler.finish().is_empty());
}
```

Add non-table terminator and `finish()` variants. Assert no table prefix appears
in any earlier action.

- [ ] **Step 3: Run RED**

```sh
cargo test -p orca-tui pipe_header_candidate --lib
cargo test -p orca-tui confirmed_table_remains_hidden --lib
```

- [ ] **Step 4: Implement conservative table helpers**

Add:

```rust
fn unescaped_pipe_cells(line: &str) -> Option<Vec<&str>>;
fn plausible_table_header(line: &str) -> bool;
fn table_delimiter(line: &str) -> bool;
fn table_row(line: &str) -> bool;
```

Parser rules:

- ignore a leading/trailing structural pipe;
- split only unescaped pipes;
- trim cells for validation but preserve the original raw line;
- delimiter cells accept optional leading/trailing `:` and at least one `-`;
- an active table row must have an unescaped pipe and at least one non-empty
  cell;
- code fences bypass table detection.

- [ ] **Step 5: Integrate candidate/table state**

Add:

```rust
pipe_candidate: Option<String>,
active_table: Option<String>,
```

Candidate rejection returns the raw line to `current_block` before the next line
is processed.

Table confirmation freezes any existing ordinary tail before hiding the table.
The table itself is emitted through `AppendFrozen` because it was never visible
as an `Assistant` tail.

- [ ] **Step 6: Run GREEN and commit**

```sh
cargo test -p orca-tui table --lib
cargo test -p orca-tui streaming_markdown --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/streaming_markdown.rs
git commit -m "feat(tui): hold streaming markdown tables" \
  -m "Delay pipe-table headers and rows until a complete table boundary prevents layout churn." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Add Immutable Assistant Chunks to App State

**Files:**
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Write failing action-application tests**

Add the variant:

```rust
AssistantChunk {
    text: String,
    trailing_blank: bool,
},
```

Before production integration, add tests:

```rust
#[test]
fn complete_lines_mutate_only_the_active_assistant_tail_revision() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("first line\n".to_string()));
    let first_revision = state.message_revisions[0];
    state.update(TuiEvent::MessageDelta("second line\n".to_string()));
    assert_eq!(state.messages.len(), 1);
    assert_ne!(state.message_revisions[0], first_revision);

    let revisions = state.message_revisions.clone();
    state.update(TuiEvent::MessageDelta("hidden half".to_string()));
    assert_eq!(state.message_revisions, revisions);
}
```

Add:

```rust
#[test]
fn blank_boundary_freezes_tail_revision_and_new_block_uses_new_tail() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("first\n\n".to_string()));
    assert!(matches!(
        &state.messages[..],
        [ChatMessage::AssistantChunk {
            text,
            trailing_blank: true,
        }] if text == "first\n\n"
    ));
    let frozen_revision = state.message_revisions[0];

    state.update(TuiEvent::MessageDelta("second\n".to_string()));
    assert!(matches!(
        state.messages.last(),
        Some(ChatMessage::Assistant(text)) if text == "second\n"
    ));
    assert_eq!(state.message_revisions[0], frozen_revision);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui complete_lines_mutate_only --lib
cargo test -p orca-tui blank_boundary_freezes_tail_revision --lib
```

- [ ] **Step 3: Add assembler ownership**

Add to `AppState`:

```rust
assistant_stream: StreamingMarkdownAssembler,
assistant_stream_tail: Option<usize>,
```

Initialize both in `AppState::new`.

Add:

```rust
fn apply_streaming_markdown_actions(
    &mut self,
    actions: Vec<StreamingMarkdownAction>,
);
```

Semantics:

- `UpdateTail(text)`
  - mutate the indexed active `Assistant`, or push a new `Assistant`;
- `FreezeTail { text, trailing_blank }`
  - replace the active `Assistant` at the exact index with `AssistantChunk`;
- `AppendFrozen`
  - push a new `AssistantChunk`;
- `ClearTail`
  - clear `assistant_stream_tail`;
- `FinishTail`
  - append the final hidden suffix to the active `Assistant`, or push a new
    ordinary final `Assistant`, then clear the tail index;

Every action must use existing `push_message`, `replace_message`, or
`mutate_message` so revision/cache invalidation stays canonical.

- [ ] **Step 4: Route assistant segments through assembler**

Change:

```rust
ProposedPlanSegment::Agent(text)
```

to:

```rust
let actions = self.assistant_stream.push(&text);
self.apply_streaming_markdown_actions(actions);
```

Keep proposed-plan parsing first.

- [ ] **Step 5: Finish on proposed-plan and turn boundaries**

Before pushing `Plan(text)`:

```rust
self.finish_assistant_stream();
```

On `SessionCompleted`, after `flush_proposed_plan_parser` and before reasoning
promotion:

```rust
self.finish_assistant_stream();
```

`finish_assistant_stream` applies `assembler.finish()` exactly once and resets
the assembler to default for the next semantic segment.

After applying finish actions:

- if an ordinary final `Assistant` exists, its existing renderer owns the one
  terminal response separator;
- otherwise mutate only the last chunk's `trailing_blank` to true through
  `mutate_message`.

- [ ] **Step 6: Add exact source reconstruction tests**

Provide a test helper:

```rust
fn assistant_projection_text(messages: &[ChatMessage]) -> String
```

It concatenates `AssistantChunk.text` and `Assistant` text, resetting at plan or
other semantic boundaries as appropriate.

Tests:

- deltas with CJK/emoji/combining text reconstruct exactly;
- completion flushes one partial line once;
- proposed plan split across deltas remains:
  `AssistantChunk/Assistant`, `ProposedPlan`, `Assistant`;
- concatenated assistant source around each plan equals parser-emitted agent
  source;
- repeated `SessionCompleted` does not duplicate held text.

- [ ] **Step 7: Make chunk settled and reset state safely**

Update `message_is_settled`:

```rust
ChatMessage::AssistantChunk { .. } => true,
```

Reset assembler and tail index in:

- `replace_messages`;
- `clear_messages`;
- `truncate_messages` when the tail index is removed;
- `remove_after_last_user`;
- submission/operation rejection before adding an error;
- resume/bootstrap state replacement.

Do not auto-finish inside `push_message`.

- [ ] **Step 8: Run GREEN and commit**

```sh
cargo test -p orca-tui MessageDelta --lib
cargo test -p orca-tui assistant_stream --lib
cargo test -p orca-tui proposed_plan --lib
cargo test -p orca-tui flushable_prefix --lib
cargo test -p orca-tui revision --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/types.rs
git commit -m "feat(tui): checkpoint streaming assistant messages" \
  -m "Apply stable Markdown blocks as immutable assistant chunks while keeping one mutable newline-gated tail." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Render Assistant Chunks without Extra Spacing

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/transcript_view.rs`

- [ ] **Step 1: Write failing chunk rendering tests**

Add:

```rust
#[test]
fn adjacent_assistant_chunks_preserve_only_source_blank_rows() {
    let theme = Theme::named(ThemeName::Dark);
    let first = ChatMessage::AssistantChunk {
        text: "first paragraph\n\n".to_string(),
        trailing_blank: true,
    };
    let second = ChatMessage::AssistantChunk {
        text: "```rust\nfn main() {}\n```\n".to_string(),
        trailing_blank: false,
    };
    let tail = ChatMessage::Assistant("tail".to_string());

    let first_lines = build_lines_for_message(&first, &theme, 80, 0, false, None);
    let second_lines = build_lines_for_message(&second, &theme, 80, 0, false, None);
    let tail_lines = build_lines_for_message(&tail, &theme, 80, 0, false, None);

    assert_eq!(first_lines.last().map(ToString::to_string), Some(String::new()));
    assert_ne!(second_lines.last().map(ToString::to_string), Some(String::new()));
    assert_eq!(tail_lines.last().map(ToString::to_string), Some(String::new()));
}
```

Add a frozen fenced-code test asserting syntax foreground diversity.

- [ ] **Step 2: Run RED**

```sh
cargo test -p orca-tui adjacent_assistant_chunks --lib
cargo test -p orca-tui frozen_fenced_code --lib
```

- [ ] **Step 3: Render the new variant**

Extract assistant rendering:

```rust
fn append_assistant_markdown(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    theme: &Theme,
    trailing_blank: bool,
);
```

Behavior:

- append `render_markdown(text, width, theme)`;
- append one blank `Line` only when `trailing_blank` is true.

Call with:

```rust
ChatMessage::Assistant(text) => {
    append_assistant_markdown(lines, text, width, theme, true);
}
ChatMessage::AssistantChunk {
    text,
    trailing_blank,
} => {
    append_assistant_markdown(lines, text, width, theme, *trailing_blank);
}
```

- [ ] **Step 4: Update exhaustive cache matches**

Where transcript tests or production spinner logic distinguish assistant
messages, treat `AssistantChunk` as non-spinner immutable Markdown.

Do not change `TranscriptRenderCache` matching, wrapping, cumulative-height, or
viewport algorithms.

- [ ] **Step 5: Add selection extraction coverage**

Build a cache from two chunks and a tail. Select across the chunk boundary and
assert copied text preserves:

- paragraph content;
- one intended blank line;
- fenced-code source;
- tail text;
- no synthetic extra blank line.

- [ ] **Step 6: Run GREEN and commit**

```sh
cargo test -p orca-tui assistant_chunk --lib
cargo test -p orca-tui frozen_fenced_code --lib
cargo test -p orca-tui selection --lib
cargo test -p orca-tui transcript_view --lib
cargo test -p orca-tui ui::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/ui.rs crates/orca-tui/src/transcript_view.rs
git commit -m "feat(tui): render frozen assistant chunks" \
  -m "Reuse Markdown and transcript wrapping while preserving checkpoint-owned blank row semantics." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Prove Cache Work Is Bounded by the Active Tail

**Files:**
- Modify: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Write a failing 1,000-block cache test**

Construct `AppState`, feed 1,000 deltas:

```rust
for index in 0..1_000 {
    state.update(TuiEvent::MessageDelta(format!(
        "block {index}\n\n"
    )));
}
```

Assert:

```rust
assert_eq!(state.messages.len(), 1_000);
assert!(state.messages.iter().all(
    |message| matches!(message, ChatMessage::AssistantChunk { .. })
));
```

Capture all revisions and build the cache. Then append:

```rust
state.update(TuiEvent::MessageDelta("live tail\n".to_string()));
```

Prepare again with a builder that records indexes. Assert:

```rust
assert_eq!(built_indices, vec![1_000]);
assert_eq!(cache.last_prepare_visited(), 1);
assert_eq!(&state.message_revisions[..1_000], &frozen_revisions[..]);
```

- [ ] **Step 2: Add mutable-tail input-size evidence**

Instrument the test builder to record the byte length of assistant/chunk text it
receives.

After 1,000 frozen blocks plus one tail, assert:

```rust
assert_eq!(rebuilt_text_bytes, "live tail\n".len());
```

This proves the rebuilt input excludes frozen content.

- [ ] **Step 3: Add steady and scroll-only zero-visit assertions**

After the tail build:

```rust
cache.prepare(...same revisions/context...);
assert_eq!(cache.last_prepare_visited(), 0);
cache.viewport(500, 20, usize::MAX);
cache.prepare(...same revisions/context...);
assert_eq!(cache.last_prepare_visited(), 0);
```

- [ ] **Step 4: Verify global invalidations remain correct**

Change width, theme identity, syntax theme revision, and force-expand in
separate prepares. Assert every entry is visited exactly once for each global
identity change.

- [ ] **Step 5: Run GREEN and commit**

```sh
cargo test -p orca-tui thousand_streaming_blocks --lib
cargo test -p orca-tui rebuilt_text_bytes --lib
cargo test -p orca-tui steady --lib
cargo test -p orca-tui syntax_theme_revision --lib
cargo test -p orca-tui transcript_view --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/transcript_view.rs crates/orca-tui/src/types.rs
git commit -m "perf(tui): bound streaming markdown rebuilds" \
  -m "Prove frozen assistant blocks keep stable revisions and only the active tail re-enters transcript layout." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

If Task 4 already satisfies these tests without additional production changes,
commit only the tests.

---

### Task 7: Add Frame-Level Newline and Table Holdback Tests

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Write failing completed-frame newline-gate test**

Use `TestBackend` with an editable conversation state:

```rust
state.update(TuiEvent::MessageDelta("visible line\nhidden half".to_string()));
terminal.draw(|frame| render(frame, &mut state, &textarea, &theme))?;
```

Assert:

- buffer contains `visible line`;
- buffer does not contain `hidden half`;
- completing the turn makes `hidden half` visible;
- exactly one final assistant blank separator exists.

- [ ] **Step 2: Write failing table-holdback frame test**

Feed:

```text
| Name | Value |
|---|---|
| A | 1 |
```

one complete line per delta.

After every delta before termination, assert the completed frame contains none
of the table source/cell values.

Feed a blank line and assert the whole formatted table appears together.

Repeat with `SessionCompleted` instead of the blank line.

- [ ] **Step 3: Extend auto-follow coverage**

Stream 100 blank-delimited blocks plus a hidden partial tail. Assert:

- the latest complete block remains visible;
- hidden partial text is absent;
- after completion the partial text is visible;
- `auto_scroll` remains true.

- [ ] **Step 4: Run GREEN and commit**

```sh
cargo test -p orca-tui streaming_newline_gate --lib
cargo test -p orca-tui streaming_table_holdback --lib
cargo test -p orca-tui streaming_auto_follow --lib
cargo test -p orca-tui ui::tests --lib
cargo test -p orca-tui app::tests --lib
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
git add crates/orca-tui/src/app.rs crates/orca-tui/src/ui.rs
git commit -m "test(tui): cover streaming checkpoint frames" \
  -m "Verify partial lines and tables stay hidden while completed checkpoints continue auto-following." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 8: Final Review, Audit, and Delivery

**Files:**
- Verify every file above.

- [ ] **Step 1: Run focused requirement filters**

```sh
cargo test -p orca-tui streaming_markdown --lib
cargo test -p orca-tui assistant_stream --lib
cargo test -p orca-tui assistant_chunk --lib
cargo test -p orca-tui proposed_plan --lib
cargo test -p orca-tui MessageDelta --lib
cargo test -p orca-tui flushable_prefix --lib
cargo test -p orca-tui transcript_view --lib
cargo test -p orca-tui streaming_newline_gate --lib
cargo test -p orca-tui streaming_table_holdback --lib
cargo test -p orca-tui streaming_auto_follow --lib
```

- [ ] **Step 2: Run package and workspace gates**

```sh
cargo test -p orca-tui ui::tests --lib
cargo test -p orca-tui app::tests --lib
cargo test -p orca-tui -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

If the unchanged external-process timeout test exhibits its proven timing
flake:

1. prove its source is unchanged from the P0 #8 baseline;
2. run its exact serialized filter five times;
3. run the workspace suite with only that exact test skipped;
4. do not modify unrelated process code.

- [ ] **Step 3: Prompt-to-artifact audit**

| Requirement | Direct evidence |
|---|---|
| Message-internal checkpointing | frozen chunk/revision tests |
| Only mutable tail rebuilds | 1,000-block cache visit test |
| Complete-line gate | state and completed-frame tests |
| Half-line absent | completed TestBackend buffer |
| Final partial line exact | finish and canonical reconstruction tests |
| Stable block boundaries | blank/fence assembler tests |
| Fenced code integrity | matching-close and syntax rendering tests |
| Whole-table holdback | candidate, active table, frame tests |
| Table rejection exact | candidate rejection test |
| Proposed plan compatibility | split-tag semantic ordering test |
| Unicode exactness | CJK/emoji/combining reconstruction |
| Existing history unchanged | runtime canonical persistence source audit |
| Existing cache reused | no production cache algorithm changes |
| Selection preserved | cross-chunk selection test |
| Auto-follow preserved | 100-block completed-frame test |
| Reset paths safe | replace/clear/truncate/backtrack tests |
| No P2 leakage | changed-file and symbol audit |

Treat missing evidence as incomplete.

- [ ] **Step 4: Request independent specification and quality reviews**

Review:

```text
docs/superpowers/specs/2026-07-28-tui-streaming-checkpoints-design.md
```

Specification review checks every prompt-to-artifact row.

Quality review checks:

- source-text loss or duplication;
- UTF-8 and newline slicing;
- false table detection;
- CommonMark fence edge cases;
- recursive event/message boundary bugs;
- stale tail indexes after mutation/reset;
- revision/cache ownership;
- selection and scrollback ordering;
- hidden-state memory growth;
- actual performance evidence rather than proxy test counts;
- no Critical or Important findings.

Fix every Critical/Important finding with RED/GREEN evidence and rerun all
gates.

- [ ] **Step 5: Audit commits, trailers, and scope**

```sh
git status --short
git log --format='%H%n%s%n%(trailers:key=Co-authored-by,valueonly)%n---' \
  be8ff507b0ca791732b05f216c790d2cd0aac9ef..HEAD
git diff --check be8ff507b0ca791732b05f216c790d2cd0aac9ef..HEAD
git diff --name-status be8ff507b0ca791732b05f216c790d2cd0aac9ef..HEAD
git diff --stat be8ff507b0ca791732b05f216c790d2cd0aac9ef..HEAD
```

Every commit must contain exactly one final:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

- [ ] **Step 6: Push and verify**

```sh
git push origin feature/tui-syntax-highlighting
local_sha=$(git rev-parse HEAD)
remote_sha=$(git ls-remote --heads origin feature/tui-syntax-highlighting | awk '{print $1}')
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
git status --short --branch
```

Keep the branch for the P2 roadmap.
