# TUI Streaming Markdown Checkpoints Design

## Goal

Bound the amount of Markdown that Orca reparses and rewraps for each assistant
delta by freezing complete, stable Markdown blocks into immutable transcript
messages.

The implementation must also:

- commit only complete newline-terminated source while a turn is streaming;
- hide a detected Markdown pipe table until the whole table is complete;
- preserve fenced code blocks, Unicode, proposed-plan tags, transcript
  selection, auto-follow, history replay, and final assistant text exactly;
- reuse the existing message-revision and `TranscriptRenderCache` machinery
  instead of introducing a second rendering cache.

This is P0 item 8 only. It does not include transcript search, queue previews,
status-bar cwd/branch data, Vim expansion, keybinding configuration,
diagnostics, or onboarding.

## Current State

`TuiEvent::MessageDelta` is projected from the runtime and passed to
`AppState::handle_message_delta`.

The current path is:

```text
runtime assistant delta
  -> ProposedPlanStreamParser
  -> push_assistant_delta
  -> append to the final ChatMessage::Assistant
  -> allocate a new message revision
  -> invalidate the final TranscriptRenderCache entry
  -> render_markdown over the complete accumulated assistant text
  -> wrap every rendered logical line again
```

The cache already limits invalidation to one message. However, the invalidated
message grows with every delta, so a long answer reparses and rewraps its full
prefix repeatedly.

The runtime records the canonical completed assistant response independently
through `record_assistant_response_for_agent`. TUI `ChatMessage` values are a
projection and are not the authority for persisted conversation history.
Therefore the TUI may split one visible assistant response into frozen chunks
without altering runtime history.

## Chosen Architecture

Add a pure `StreamingMarkdownAssembler` that converts assistant text into:

- immutable frozen chunks at stable Markdown boundaries;
- one mutable visible tail containing only complete lines after the last
  checkpoint;
- one hidden partial source line;
- one hidden pipe-table candidate or active table.

Frozen chunks become separate `ChatMessage::AssistantChunk` values. The active
tail remains the existing `ChatMessage::Assistant` variant.

This reuses the current cache contract:

- each frozen chunk has its own revision and cache entry;
- its revision never changes after freezing;
- only the active `Assistant` tail changes revision on later complete lines;
- a stable boundary replaces the active tail once with an immutable chunk;
- future deltas create or mutate a new final tail.

No width, Markdown parser state, or checkpoint identity is added to
`TranscriptRenderCache`.

## Alternative Designs Rejected

### Incremental Markdown State Inside `TranscriptRenderCache`

This would keep a single assistant message, but the cache would need to own raw
Markdown, parser state, code-fence state, table state, style state, and wrapped
row checkpoints. It would duplicate domain logic from `ui.rs` and couple the
cache to Markdown semantics.

### Split into Ordinary `ChatMessage::Assistant` Values

The current assistant renderer appends one visual blank row after every
assistant message. Using ordinary assistant messages for checkpoints would
insert extra blank rows between chunks and change the rendered answer.

### Freeze Every Complete Line

A future line can change an earlier Markdown line:

- a Setext underline changes the previous paragraph into a heading;
- list continuation indentation changes the current list item;
- a pipe delimiter changes the previous pipe-containing line into a table
  header;
- an opening fence changes following lines into code.

Only stable block boundaries may freeze.

## New Module

Create:

```text
crates/orca-tui/src/streaming_markdown.rs
```

It owns a deterministic state machine and contains no ratatui types.

```rust
pub(crate) struct StreamingMarkdownAssembler {
    partial_line: String,
    current_block: String,
    pipe_candidate: Option<String>,
    active_table: Option<String>,
    fence: Option<FenceState>,
}
```

The module emits:

```rust
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
```

Exact names may be refined in the implementation plan, but the actions must
retain these semantics.

The assembler does not store a second full copy of the assistant response.
Tests reconstruct received text from emitted chunks, visible tail, and hidden
partial state. The module exposes a test-only reconstruction helper that
concatenates committed action text with the currently held candidate, table,
block, and partial line in source order.

## Message Model

Add one projection-only variant:

```rust
ChatMessage::AssistantChunk {
    text: String,
    trailing_blank: bool,
}
```

`AssistantChunk` is immutable after insertion.

The active streaming tail remains:

```rust
ChatMessage::Assistant(String)
```

`AppState` tracks the current tail index:

```rust
assistant_stream_tail: Option<usize>
```

The index is cleared whenever the tail freezes, is finalized, is removed, or
another semantic message boundary starts.

### Rendering

`AssistantChunk` renders through the same `render_markdown` function as
`Assistant`.

Unlike ordinary `Assistant`, it does not unconditionally append a blank line.
It appends one blank display row only when `trailing_blank` is true.

The raw `text` stored in a chunk remains byte-for-byte source text, including
its structural newlines. `trailing_blank` is presentation metadata because
pulldown-cmark does not emit a visible row for a trailing blank Markdown line.

At semantic finish:

- an ordinary final `Assistant` supplies the response's one terminal blank
  separator through its existing renderer;
- if there is no final `Assistant`, `AppState` changes only the last
  `AssistantChunk.trailing_blank` to true;
- earlier chunks keep only checkpoint-owned blank rows;
- one assistant semantic segment therefore ends with exactly one display
  separator.

Ordinary `Assistant` behavior is unchanged for:

- history replay;
- preloaded messages;
- static tests;
- the final mutable/finished tail.

### Settled and Flushable State

`AssistantChunk` is always settled. Once it is before the active tail, the
existing `flushable_prefix_end` may commit it to immutable scrollback.

`Assistant` remains unsettled while it is the final streaming message and
becomes settled when a newer message follows or the turn ends.

This allows long answers to leave the mutable live pane block by block rather
than retaining one ever-growing mutable message.

## Newline Gate

Every assistant delta is appended to `partial_line`.

The assembler finds the last newline in the accumulated input and extracts
only prefixes ending in `\n`. Text after that last newline remains hidden in
`partial_line`.

Consequences:

- a half-written word or Markdown delimiter never appears;
- no revision changes when a delta contains no newline;
- the current visible tail changes only after at least one complete line;
- `finish()` appends the hidden partial line exactly once.

The gate applies after `ProposedPlanStreamParser`, not before it. This preserves
the parser's ability to recognize a proposed-plan tag split across deltas.

## Stable Checkpoint Boundaries

Outside a table or fence, an ordinary block freezes when a complete blank line
is received.

The frozen raw chunk includes the blank line and sets:

```text
trailing_blank = true
```

An opening fenced-code line first freezes any preceding ordinary block, then
starts a fenced block. Complete lines inside the fence remain the mutable tail.
The fenced block freezes only after a matching closing fence line.

Fence rules:

- backtick and tilde fences are supported;
- up to three leading spaces are allowed;
- a closing fence uses the same character;
- its run length is at least the opening run length;
- text after a valid closing run follows CommonMark-compatible whitespace
  rules;
- an incomplete fence is force-finished at the turn boundary.

A closed fence does not imply a visual blank row. `trailing_blank` is false
unless a following blank line is also consumed.

Conservative boundaries deliberately exclude arbitrary line breaks, Setext
headings, and unfinished list items.

## Pipe Table Holdback

Pipe tables require one-line lookbehind.

### Candidate

A complete line containing a plausible pipe-table header is held as
`pipe_candidate` instead of being displayed immediately.

A candidate must:

- contain at least one unescaped `|`;
- contain non-whitespace cell content;
- not be inside a fenced code block.

### Confirmation

The next complete line confirms a table only when every delimiter cell matches
the Markdown alignment-row shape:

```text
:---:
---:
:---
---
```

with at least one hyphen per cell.

When confirmed:

- any preceding ordinary block freezes first;
- the candidate and delimiter enter `active_table`;
- no table line appears in the transcript yet.

### Rejection

If the next line is not a delimiter, the candidate is released into the
ordinary block before that next line is processed. A normal sentence
containing `|` is delayed by at most one complete line and is never lost.

### Completion

While a table is active, a complete pipe row is appended to the hidden table.

The table completes when:

- a blank line arrives;
- a non-table line arrives;
- the assistant semantic segment ends;
- the turn completes.

The whole table is then emitted once. A terminating blank line is included and
sets `trailing_blank = true`; a non-table terminator is processed as the first
line of the next ordinary block.

This prevents table column widths from changing on every row delta.

## Proposed Plan Boundaries

`ProposedPlanStreamParser` remains the first consumer of raw assistant deltas.

For each emitted segment:

- `Agent(text)` enters `StreamingMarkdownAssembler`;
- before `Plan(text)` is pushed, the current assistant assembler is
  semantically finished, including its hidden partial text and any held table;
- the plan is pushed through the existing `ProposedPlan` path;
- a later `Agent(text)` starts a fresh assistant assembler.

This guarantees:

- checkpointing never freezes a partial `<proposed_plan>` tag;
- plan Markdown is not treated as ordinary assistant text;
- assistant text before and after a plan remains in source order.

## Other Semantic Boundaries

Before a non-assistant transcript item is inserted after assistant output, the
assistant assembler is finished.

Required boundaries include:

- tool requested;
- subagent/tool projection that appends a row;
- error or notice row;
- turn completion;
- backtrack, clear, replace, or transcript truncation.

Usage/context updates that do not append transcript rows do not finish the
assembler.

Reasoning before the first assistant segment is unchanged. If reasoning appears
after assistant text, the assistant assembler finishes before the reasoning row
is inserted.

This boundary logic is explicit at event-handling call sites. The generic
`push_message` primitive must not automatically finish streaming state:

- assembler actions themselves use `push_message`;
- automatic finishing there would recurse;
- many callers push already-replayed or internal immutable messages;
- usage/context events do not use `push_message` and must remain non-boundaries.

## Turn Completion

On `SessionCompleted`:

1. finish `ProposedPlanStreamParser`;
2. finish `StreamingMarkdownAssembler`;
3. append the hidden partial line exactly once;
4. release a pipe candidate or active table;
5. convert remaining visible text to an ordinary final `Assistant`, or mark the
   final frozen chunk to provide the normal terminal blank separator;
6. promote trailing reasoning;
7. archive the live plan;
8. finalize the turn.

Concatenating the raw `text` of every assistant chunk and final assistant tail
for a semantic assistant segment must equal the exact `Agent` text emitted by
`ProposedPlanStreamParser`.

Runtime history remains unchanged and is independently checked against the
provider response.

## Cache and Performance Contract

Frozen chunks rely on normal message cache entries.

For each stable checkpoint:

- the old tail changes revision once when converted to `AssistantChunk`;
- that revision never changes again;
- only a new final tail changes on later complete lines;
- `TranscriptRenderCache::prepare` visits only the changed/new entry after the
  initial build;
- scrolling and steady frames visit zero entries;
- theme, width, syntax-theme, and force-expand invalidation still rebuild all
  affected entries through existing rules.

The design bounds repeated work by the size of the current unstable Markdown
block, not the full assistant answer.

No absolute byte bound is imposed on one unclosed paragraph or code fence
because freezing at an unsafe arbitrary offset could change Markdown
semantics. Existing syntax and long-content guardrails remain in force.

## Selection and Auto-Follow

Freezing the final tail replaces only the final message. Existing selection
logic already preserves a selection above tail rewrites.

Adding a new tail below frozen chunks behaves like an ordinary appended
message.

Auto-follow continues to call `scroll_to_bottom` after event processing. Tests
must verify that:

- complete committed lines remain visible;
- hidden partial lines do not create phantom height;
- table holdback does not disarm follow;
- table release and turn completion reveal the final tail.

## State Reset and Recovery

`StreamingMarkdownAssembler` and `assistant_stream_tail` must reset on:

- `replace_messages`;
- `clear_messages`;
- truncation removing the active tail;
- backtrack;
- session resume/bootstrap;
- operation rejection;
- turn completion.

Replay data arrives as ordinary `ChatMessage::Assistant` values and never
re-enters streaming assembly.

## Files

### Create `crates/orca-tui/src/streaming_markdown.rs`

Pure newline gate, block/fence scanner, table holdback, finish logic, and unit
tests.

### Modify `crates/orca-tui/src/lib.rs`

Register the focused module.

### Modify `crates/orca-tui/src/types.rs`

Add `AssistantChunk`, assembler ownership, tail-index ownership, action
application, semantic-boundary finishing, settled-state behavior, reset logic,
and state-level tests.

### Modify `crates/orca-tui/src/ui.rs`

Render `AssistantChunk` through existing Markdown semantics without adding an
unconditional blank row.

### Modify `crates/orca-tui/src/transcript_view.rs`

Only tests and exhaustive pattern matches should change unless implementation
evidence proves a production change is required. The cache algorithm itself
must remain generic and unaware of Markdown checkpoints.

### Modify Tests in `crates/orca-tui/src/app.rs`

Add completed-frame and cache-visit integration coverage. Production app-loop
behavior should not require a new streaming subsystem.

## Test Matrix

### Pure Assembler

- a delta without `\n` emits nothing and remains hidden;
- multiple deltas reconstruct one UTF-8/CJK/combining line exactly;
- a final partial line is emitted once by `finish`;
- blank lines freeze ordinary blocks;
- an open fence stays mutable;
- a matching close freezes the complete fence;
- a mismatched or shorter fence does not close;
- a table candidate is hidden for one line;
- a rejected candidate is released exactly;
- a confirmed table remains entirely hidden while rows arrive;
- blank/non-table/finish boundaries emit the whole table once;
- escaped pipes do not create false table cells;
- emitted text plus hidden state reconstructs all input exactly.

### App State

- only the active tail revision advances for complete lines;
- deltas without newline do not advance any revision;
- frozen chunk revisions never advance;
- a blank boundary converts the tail to `AssistantChunk`;
- a new block creates a new tail;
- proposed-plan tags split across deltas remain three ordered semantic
  messages;
- a tool boundary finishes assistant pending text first;
- completion flushes partial text and held tables;
- reset/backtrack clears hidden pending text;
- `AssistantChunk` is settled while the live tail is not.

### Rendering

- adjacent chunks do not gain unconditional extra blank rows;
- `trailing_blank` produces exactly one intended blank display row;
- fenced code remains syntax highlighted after freezing;
- table output appears only after release and uses existing table layout;
- ordinary replayed `Assistant` rendering is byte-for-byte unchanged;
- selection extraction preserves frozen chunk content.

### Performance

- after 1,000 blank-delimited assistant blocks, every frozen revision remains
  unchanged;
- after the initial build, each next block visits only the prior tail/new tail
  cache entry, never all earlier chunks;
- steady and scroll-only cache prepares visit zero entries;
- the mutable tail build input excludes every frozen block;
- a long answer with many blocks avoids full-prefix Markdown rebuilds.

### Integration

- existing streaming auto-follow tests pass;
- a new newline-gate test proves half-lines are absent from completed frames;
- a table-holdback test proves partial tables never appear;
- final assistant source equals canonical emitted agent text;
- existing proposed plan, history replay, flushable-prefix, selection,
  transcript-cache, Markdown, syntax, diff, and hardware-cursor suites pass.

## Delivery Gates

Before delivery:

1. run pure assembler tests;
2. run `MessageDelta`, proposed-plan, flushable-prefix, and revision tests;
3. run transcript-cache and Markdown tests;
4. run `ui::tests`;
5. run the complete `orca-tui` package serially;
6. run the workspace all-targets suite serially;
7. run `cargo check -p orca-tui`, formatting, and `git diff --check`;
8. request independent specification and quality reviews;
9. audit prompt-to-artifact coverage, commit trailers, and changed-file scope;
10. push `feature/tui-syntax-highlighting` and compare local/remote SHAs.
