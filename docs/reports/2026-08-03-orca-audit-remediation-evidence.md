# Orca Audit Remediation Evidence

Date opened: 2026-08-03
Target release: v0.3.2
Source audit: `docs/reports/2026-08-03-architecture-performance-design-review.md`
Approved design: `docs/superpowers/specs/2026-08-03-orca-audit-remediation-v032-design.md`

This ledger is the release gate for the comprehensive Orca audit. A broad test
run does not replace a focused regression, latency bound, durability proof,
dependency check, or public artifact verification named below. `Not yet run`
is an explicit incomplete state, not evidence of success.

| ID | Finding | Baseline evidence | Regression or gate | Fix commit | Final evidence |
|---|---|---|---|---|---|
| C1 | Goal actor synchronous replies can block the single-thread host | `GoalRuntimeHandle::request` called unbounded `reply_rx.recv()` in `crates/orca-runtime/src/goal_actor.rs`; callers are inside the async `ThreadActor` loop | `goal_actor_request_times_out_with_typed_error`; `goal_store_wait_does_not_block_thread_actor` | `3c746a408` bounds replies with a typed timeout; lazy runtime initialization and derived receipt validation now run off-actor; remaining command/settlement completion migration is in progress | Timeout regression passed; all 15 goal-actor tests passed; locked runtime check passed; lazy runtime responsiveness and atomic stale-receipt regressions passed. Remaining actor-side goal request sites are not yet closed. |
| C2 | Host supervisor performs session store and unbounded history IO on its async loop | Direct store calls in `crates/orca-runtime/src/runtime_host.rs`; `list_threads` materializes the complete store | `session_listing_does_not_block_host_supervisor`; `bounded_session_page_does_not_materialize_all_transcripts` | This change dispatches thread preparation and every JSONL store operation to detached blocking workers with typed replies | FIFO-backed filesystem blocking test passed while an unrelated thread started; 2,000 metadata-only sessions returned a 25-row page without parsing deliberately invalid transcript bodies; locked runtime check passed. |
| C3 | Blocking web search ignores cooperative cancellation for up to its HTTP timeout | `crates/orca-tools/src/registry.rs` marks web search non-cooperative and `web_search.rs` uses blocking reqwest | `web_search_cancels_before_withheld_response_timeout` | Not yet run | Not yet run |
| C4 | Foreground cancellation does not stop spawned subagent task trees | Foreground Esc path cancels generation without requesting task-registry stop in `crates/orca-runtime/src/runtime_host.rs` | `foreground_cancel_stops_owned_subagent_tree` | Not yet run | Not yet run |
| C5 | Runtime-surface contract validation is failing, trusts commit trailers, omits closed inventory, and is absent from CI | Node validator currently reports missing `private-sha256`; its test reports an unterminated `cfg(test)` body; workflows do not invoke it | Node validator suite, live validator, Rust closed-inventory equality, and workflow fixture gates | Not yet run | Not yet run |
| S1 | Forking from the session picker leaves the previous transcript on screen | `ForkSavedSession` emits `SessionForked` without an atomic projection reset or history snapshot in `crates/orca-tui/src/app.rs` | `fork_saved_session_replaces_transcript_atomically`; `picker_fork_failure_preserves_old_projection` | Not yet run | Not yet run |
| S2 | `/rename` persists a title without a revision-checked runtime metadata patch | Rename path writes the session store while the runtime projection retains the old title | `rename_updates_store_and_runtime_metadata`; stale-revision rejection test | Not yet run | Not yet run |
| S3 | Picker exposes Archive/Delete for the attached current session until after confirmation | Action availability in `crates/orca-tui/src/session_picker_actions.rs` does not filter the current session | `current_session_has_no_destructive_picker_actions` | Not yet run | Not yet run |
| S4 | New picker phases and `/status` formatting lack render coverage | No focused render assertions cover all four picker phases or bounded status layouts | Phase render snapshots and `status_render_is_stable_and_bounded` | Not yet run | Not yet run |
| S5 | Archive/delete persistence lacks end-to-end history coverage | `tests/history_contract.rs` does not prove archive/delete durability and restore semantics | `session_archive_delete` history contract suite | Not yet run | Not yet run |
| S6 | Queued projection events from an old session can mutate a newly attached session | TUI event channel carries no session generation/attachment identity | `stale_session_events_are_discarded`; `current_attachment_events_are_applied` | Not yet run | Not yet run |
| A1 | ThreadActor retains capability and terminal state-machine ownership | Capability/terminal methods and pending fields remain in the large `ThreadActor` impl in `runtime_host.rs` | `capability_controller_trace_equivalence` | Not yet run | Not yet run |
| A2 | ThreadActor retains goal state-machine ownership | Goal methods, runtime handle, turn context, and pending operations remain in `runtime_host.rs` | `goal_controller_trace_equivalence` | Not yet run | Not yet run |
| A3 | ThreadActor retains background-task state-machine ownership | Background admission/completion/stop methods and pending fields remain in `runtime_host.rs` | `background_controller_trace_equivalence` | Not yet run | Not yet run |
| A4 | ThreadActor retains surface-commit state-machine ownership | Commit prepare/settle/retry/terminalization methods and pending fields remain in `runtime_host.rs` | `commit_controller_trace_equivalence` | Not yet run | Not yet run |
| A5 | Rust source-text assertions lock behavior tests to private syntax and file placement | 254 `include_str!` sites in runtime lib tests and 28 in TUI/workspace tests inspect source text | Zero private Rust source-layout assertions plus typed behavior and dependency gates | Not yet run | Not yet run |
| A6 | Provider transport depends on tools and special-cases concrete tool names | `orca-provider` depends on `orca-tools`; DeepSeek lowering normalizes named tools in `deepseek_http.rs` | `provider_does_not_depend_on_tools`; provider-neutral schema parity suite | Not yet run | Not yet run |
| A7 | Production server, ACP, and TUI projection consumers bypass the curated facade through `unstable_surface` | Production imports exist in two ACP modules, eight server modules, and TUI surface projection | Runtime contract zero-consumer gate and affected runtime/TUI suites | Not yet run | Not yet run |
| A8 | Runtime-surface modules use wildcard public exports and sibling imports | `crates/orca-runtime/src/runtime_surface/mod.rs` has eleven `pub use module::*` exports | Explicit-export contract fixture and runtime-surface type suite | Not yet run | Not yet run |
| A9 | TUI has an unused provider dependency and constructs/passes the root MCP registry | `crates/orca-tui/Cargo.toml` declares `orca-provider`; `app.rs` constructs `McpRegistry` and carries it through presentation setup | `tui_does_not_depend_on_provider`; `runtime_owns_root_mcp_registry` | Not yet run | Not yet run |
| P1 | Streaming reducer repeatedly copies accumulated assistant text, producing quadratic work | `runtime_surface/reducer.rs` rebuilds text with `format!("{}{}", accumulated, delta)` | Offset correctness, append allocation/work bound, and replay equivalence tests | Not yet run | Not yet run |
| P2 | Resize or theme change synchronously reflows the complete transcript | `transcript_view.rs` invalidates and rebuilds all layout entries | `reflow_budget_is_bounded_and_viewport_first` | Not yet run | Not yet run |
| P3 | Open search rescans the transcript on unchanged streaming frames | Search refresh in `transcript_search.rs`/`ui.rs` lacks per-entry generations | `unchanged_entries_are_not_rescanned_during_streaming` | Not yet run | Not yet run |
| P4 | ToolCall deduplication linearly scans all transcript messages | `crates/orca-tui/src/types.rs` searches the message vector by call id | Indexed-vs-linear differential mutation suite | Not yet run | Not yet run |
| P5 | Usage uses monotonic maxima, so compaction cannot reduce displayed context | Usage projection in `types.rs` applies `.max()` without event ordering | `compaction_usage_can_decrease`; `stale_usage_event_is_ignored` | Not yet run | Not yet run |
| P6 | Reducer state, TUI shadow fields, and AppState can silently diverge | Projection updates are manually duplicated without a shared invariant assertion | `surface_projection_consistency`; JSONL surface differential suite | Not yet run | Not yet run |

## Public Release Proof

The release is incomplete until all table rows are closed and the following are
independently visible:

- GitHub tag and non-draft release `v0.3.2` at the same commit.
- Six native release archives with checksums and signature assets required by
  the release workflow.
- `@blade-ai/orca@0.3.2` plus all six platform packages on the public npm
  registry.
- A clean install in an isolated directory where `orca --version` reports
  `0.3.2` and the binary provenance matches the published artifact.
