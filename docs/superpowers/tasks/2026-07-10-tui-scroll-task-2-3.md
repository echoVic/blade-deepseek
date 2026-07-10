# Tasks 2-3: Cached And Virtualized Transcript

Implement Tasks 2 and 3 from `docs/superpowers/plans/2026-07-10-tui-scroll-performance.md` as one cohesive rendering change.

## Required Behavior

- Add `crates/orca-tui/src/transcript_view.rs` with a cache containing one entry per retained message.
- Eliminate `live_message_signature` and the full-content hashing performed on every frame.
- Cache each message's styled logical lines and ratatui-compatible wrapped visual height by explicit message revision, terminal width, theme identity, `force_expand`, and a spinner-phase key used only by running or receiving tools.
- Scrolling alone must rebuild zero messages and reparse zero Markdown messages.
- Streaming an assistant delta must invalidate and rebuild only that final message.
- Width changes must rebuild layout and preserve clamped scroll behavior.
- Maintain cumulative visual heights using `usize`, binary-search the visible message range, and submit only visible messages plus at most one message of overscan on each side to the final `Paragraph`.
- Convert `scroll_offset`, `total_lines`, and `visible_height` plus scroll APIs to `usize`; only clamp the final local ratatui scroll value to `u16`.
- Preserve exact current Markdown/CJK/table/tool expansion/auto-follow behavior and all existing UI tests.
- Do not advance `flushed_count` or restore native terminal scrollback.

## Mutation Contract

Use explicit revisions. Production mutation paths are append, indexed in-place mutation, whole transcript replacement/clear, retain of receiving tool progress, and truncate during backtrack. Centralize helpers on `AppState` such as push, replace, clear, truncate, replace-at, and mutate-at/touch. Migrate production direct writes in the files you must touch. It is acceptable for rare whole replacement/retain paths to reset the complete render cache; streaming and tool/subagent updates must touch only the affected index.

Do not assume finalized messages never change: tool/subagent expansion can mutate an old message. Tick changes only the running/receiving tool spinner and must not invalidate other entries.

## TDD Requirements

Before production implementation, add and run failing tests that prove:

1. a second scroll-only frame has zero message builds and zero Markdown parses;
2. a delta on the final assistant rebuilds only one message;
3. thousands of messages produce a bounded rendered window independent of transcript length;
4. offsets above 65,535 remain representable and navigable;
5. a terminal width change recomputes layout.

Record exact RED and GREEN commands/output in `/tmp/tui-scroll-cache-implementer-report.md`. Run `cargo test -p orca-tui ui::tests -- --nocapture`, relevant `types` tests, then `cargo test -p orca-tui` before reporting. Do not commit because the primary agent already has uncommitted Task 1 changes in the shared worktree. Do not modify the Task 1 event-loop files except `lib.rs` module registration if needed.
