# TUI Scroll Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make fullscreen Orca TUI scrolling responsive with event-aware frame scheduling, per-message render caching, and a virtualized transcript viewport.

**Architecture:** A pure frame scheduler prevents input from starving draws and suppresses idle frames. A transcript view cache stores styled message lines and wrapped heights by message revision and width, then selects only the messages intersecting the current viewport.

**Tech Stack:** Rust 2024, crossterm 0.28, ratatui 0.29, pulldown-cmark 0.12, existing `orca-tui` unit tests and `TestBackend`.

## Global Constraints

- Preserve fullscreen in-application history scrolling; do not restore native-scrollback flushing.
- Scroll-only frames must not parse unchanged Markdown or build off-screen message lines.
- Runtime events and terminal input must both be serviced while scrolling is continuous.
- Scroll metrics must support more than 65,535 visual lines.
- Completion requires automated tests and a real DeepSeek API TUI run in a PTY.

---

### Task 1: Frame Scheduler And Input Batching

**Files:**
- Create: `crates/orca-tui/src/frame_scheduler.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`

**Interfaces:**
- Produces: `FrameScheduler::{new, mark_dirty, poll_timeout, animation_due, should_draw, did_draw}`.
- Produces: bounded terminal-event batching and coalesced adjacent wheel deltas.

- [x] Write scheduler and wheel-coalescing tests that fail because the new APIs do not exist.
- [x] Run `cargo test -p orca-tui frame_scheduler -- --nocapture` and confirm the expected compile/test failure.
- [x] Implement the scheduler and event batch representation with a 16 ms frame interval and a slower animation interval.
- [x] Refactor the main loop so no normal terminal-event path skips runtime draining or the draw decision.
- [x] Run focused scheduler, input-action, and existing scroll tests until green.

### Task 2: Message Revisions And Render Cache

**Files:**
- Create: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`

**Interfaces:**
- Produces: `TranscriptRenderCache` containing one `CachedMessage` per retained message.
- Produces: explicit message revision/invalidation methods on `AppState`.
- Consumes: existing `build_lines_for_messages` behavior through a single-message rendering boundary.

- [x] Write failing tests proving a scroll-only second frame performs no message or Markdown rebuilds.
- [x] Run the focused cache tests and confirm failure for missing cache behavior.
- [x] Add message revisions and ensure every mutable projection path invalidates only changed messages.
- [x] Cache per-message styled lines and visual heights by revision, width, tick-dependency, and theme.
- [x] Reconcile cache entries on session replacement, clear, backtrack, and transcript truncation.
- [x] Run focused projection, cache, Markdown, tool expansion, and session-resume tests until green.

### Task 3: Virtualized Transcript And Large Scroll Metrics

**Files:**
- Modify: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/idle_navigation_actions.rs`
- Modify: `crates/orca-tui/src/running_actions.rs`

**Interfaces:**
- Produces: a viewport selection containing total height, first/last message indices, local scroll, and bounded render lines.
- Changes: transcript offsets/heights use `usize`; ratatui receives a clamped local `u16` offset only.

- [x] Write failing tests for a bounded window over thousands of messages and offsets above 65,535.
- [x] Run focused tests and confirm the old full-paragraph/u16 behavior fails them.
- [x] Add cumulative message heights and binary-search viewport selection with one-message overscan.
- [x] Render only selected cached entries while preserving exact current output and auto-follow behavior.
- [x] Convert scroll calculations and shortcuts to `usize` and update existing assertions.
- [x] Run all existing `ui` and `types` tests until green.

### Task 4: Dirty Rendering And Performance Regression Harness

**Files:**
- Modify: `crates/orca-tui/src/frame_scheduler.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/ui.rs`

**Interfaces:**
- Produces: test-only render/cache counters and deterministic scheduler timing hooks.

- [x] Write failing tests proving idle iterations do not draw and streaming invalidates only the live tail.
- [x] Implement dirty marking for input, runtime events, resize, animation, and modal/status changes.
- [x] Add a deterministic large-transcript regression test with counters rather than a flaky wall-clock threshold.
- [x] Run focused performance regression tests and inspect counters.

### Task 5: Automated And Real-Environment Verification

**Files:**
- Modify only if verification exposes a defect in the preceding implementation.

**Interfaces:**
- Consumes: repository test commands, built `target/debug/orca`, authenticated local Orca configuration, and a PTY terminal driver.

- [x] Run touched-file `rustfmt --check`, `cargo check -p orca-tui`, and `cargo test -p orca-tui`.
- [x] Run `cargo test --workspace` and separate pre-existing failures from regressions if any.
- [x] Build `target/debug/orca` and confirm a real DeepSeek API request succeeds without exposing credentials.
- [x] Launch the real TUI in a PTY, generate long streaming Markdown content, inject sustained scroll input, and capture frame/cache timing evidence.
- [x] Exercise PageUp/PageDown, top/bottom navigation, streaming auto-follow, resize, and terminal cleanup.
- [x] Review `git diff`, requirement coverage, and fresh verification output before reporting completion.
