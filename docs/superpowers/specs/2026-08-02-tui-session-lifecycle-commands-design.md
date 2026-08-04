# TUI Session Lifecycle Commands Design

Date: 2026-08-02
Status: implemented; v0.3.3 release verification pending
Scope: complete Orca's interactive session lifecycle without restoring `/history`

## Objective

Make the current conversation inspectable, copyable, renameable, and forkable from the TUI while keeping saved-session discovery centered on `/resume`.

The public command surface added by this change is:

- `/fork [name]`
- `/rename [name]`
- `/status`
- `/copy [N]`

The existing `/resume` picker gains contextual actions for resume, fork, rename, archive, delete, and copy-session-ID. `/history` remains retired, `/clear` remains a hidden compatibility alias for `/new`, and `Ctrl+L` remains display-only terminal clearing.

## Evidence And Product Boundary

The local Codex, Grok Build, and Claude Code sources converge on four durable concepts:

1. A new conversation receives a new persisted identity.
2. Resume re-enters an existing identity; fork copies context into a new identity.
3. Rename changes saved-session metadata and all current-session projections.
4. Destructive saved-session actions require contextual confirmation.

They do not establish `/history` as a cross-session lifecycle command. Grok's `/history` searches prompts inside the current session, while cross-session discovery remains `/resume`. Orca therefore keeps one saved-session entry point.

## Command Semantics

### `/resume`

`/resume` always opens the saved-session picker. It never resumes a recoverable operation based on hidden state.

Recoverable operations remain explicit UI state. The recovery notice exposes a direct resume action and continues to expose `/cancel-operation` for abandonment. This avoids one command having two unrelated meanings.

### `/fork [name]`

`/fork` copies the current persisted conversation into a new session and switches the TUI to it. The old session remains unchanged and resumable.

- Optional `name` becomes the new session title after trimming.
- Without a name, the fork inherits a bounded title derived by the existing history layer.
- Fork is rejected while the current runtime snapshot contains foreground, queued, or background operations, non-terminal tasks or workflows, or an active goal.
- The fork starts through `RuntimeHostHandle` with `HistoryMode::Fork(current_session_id)` so the runtime remains the only history writer.
- The old runtime thread is shut down only after the new thread has started successfully.
- If new-thread creation or old-thread shutdown fails, the TUI remains attached to the original session. A newly started replacement is shut down on swap failure.
- Success reloads the copied transcript, resets transient UI state, and reports the new session identity.

### `/rename [name]`

`/rename <name>` trims and validates the title, updates the current runtime surface metadata, and persists the same title in the session store.

- An empty `/rename` does not mutate state. It returns the composer to `/rename ` so the user can enter a title.
- Titles containing only whitespace are rejected.
- Rename is allowed while model work is running because it changes metadata only.
- The runtime surface metadata patch uses its current revision as a precondition.
- Persistence and surface projection are treated as one user operation. If persistence fails, the visible title must not report success.
- Success updates `AppState.current_session_title`, the terminal title projection, and any matching row already loaded in the resume picker.

### `/status`

`/status` is a read-only snapshot assembled from existing `AppState` and `RunConfig` values. It performs no filesystem or runtime I/O and is available while work is running.

It reports:

- session ID and title;
- model and reasoning effort;
- approval mode;
- working directory and Git identity when known;
- context tokens used, limit, and percentage;
- input, output, cache tokens, and estimated cost;
- active goal state;
- visible background/workflow task counts; and
- whether a recoverable operation exists.

Unknown values render as `-`; the command never invents a persisted session identity before one exists.

### `/copy [N]`

`/copy` copies the latest assistant response as Markdown-compatible source text. `/copy N` copies the Nth-latest assistant response, where `N` starts at 1.

- Only finalized `ChatMessage::Assistant` values are candidates.
- A still-streaming `AssistantChunk` is excluded, so the clipboard never receives a partial response.
- `N = 0`, non-numeric arguments, missing responses, and out-of-range indices produce explicit errors without modifying the clipboard.
- Success reuses `AppState::stage_clipboard_copy`, preserving the existing OSC 52 and local clipboard fallback behavior and status-line feedback.

## Session Identity Projection

`AppState` gains `current_session_id: Option<String>` and `current_session_title: Option<String>`. The controller publishes `TuiEvent::SessionIdentityUpdated` whenever it creates, resumes, forks, or renames the active session.

The identity event is separate from `MentionRuntimeReady`: mention search receives a typed runtime handle, while lifecycle UI receives display metadata. Neither presentation path reads history storage directly.

## Resume Picker Interaction

The picker remains searchable by typing and keeps Enter as the primary Resume action.

```text
Up/Down  select
Enter    resume
Tab      actions
Esc      close or return from actions
```

The action menu contains:

```text
Resume
Fork
Rename
Archive
Delete
Copy session ID
```

Behavior:

- Resume and Fork switch through the hosted controller, not by mutating `RunConfig` and waiting for a later prompt.
- Rename enters an inline title-input phase scoped to the captured session ID.
- Archive and Delete enter a confirmation phase. The default is Cancel.
- Confirmation captures the session ID before opening. Async list refreshes or filter changes cannot redirect the operation to another row.
- Archive/Delete of the currently active session are not offered from an in-session `/resume` picker. They remain available for other saved sessions.
- Copy session ID uses the existing clipboard staging path and keeps the picker open.
- Successful metadata mutations refresh the picker list while preserving the query and selecting the nearest remaining row.
- Failed mutations keep the picker open and show an inline error.

## State Model

The picker uses an explicit phase rather than boolean combinations:

```rust
enum SessionPickerPhase {
    Browsing,
    Actions { session_id: String, selected: usize },
    Renaming { session_id: String, value: String, cursor: usize },
    ConfirmArchive { session_id: String, title: String },
    ConfirmDelete { session_id: String, title: String },
}
```

`Esc` moves one level toward Browsing; from Browsing it closes the picker. Successful resume or fork closes the picker through the normal session-switch event. Error text belongs to picker state and is cleared on the next edit or successful action.

## Failure And Concurrency Invariants

- New, fork, resume, archive, and delete never abandon active runtime work implicitly.
- Session swaps are start-new-then-shutdown-old; failure leaves the old session authoritative.
- Picker mutations operate on captured IDs, never the current selected index.
- Clipboard failures remain best-effort through the existing helper, but argument and missing-message failures are visible in the transcript.
- `/status`, `/copy`, and `/rename` are allowed during active work; `/fork` and session switching are not.
- No command directly edits transcript JSONL.

## Implemented Transaction Boundary

The shipped implementation binds every projected event to an immutable session
attachment. A switch activates the replacement attachment, resets session UI,
and replays its exact history before the source runtime is retired. Events
already queued by the source attachment are ignored after activation changes.

Rename uses the current runtime metadata revision as a precondition, then
persists the same title. A durable-write failure applies a second
revision-checked patch that restores the prior projection, so disk and screen
cannot silently disagree. Fork preserves the source history, creates a new
identity through `HistoryMode::Fork`, and projects the copied transcript only
after the replacement runtime is ready.

The picker derives available actions from both the selected and attached
session IDs. Archive and Delete are absent for the attached session, operate on
the captured target ID after confirmation, and refresh from durable storage
after settlement. Rendering tests cover every picker phase and bounded
`/status` layouts; history contracts cover archive and delete across reloads.

## Testing

Focused unit and integration tests must cover:

- command parsing, menu visibility, and hidden `/clear` compatibility;
- `/resume` opening the picker even with a recoverable operation;
- `/copy` indexing, partial-stream exclusion, and errors;
- `/status` complete and unknown-state formatting;
- fork identity, copied history, parent preservation, active-work rejection, and swap failure;
- rename persistence and current-state projection;
- every picker phase, captured-ID confirmation, cancel paths, and list refresh;
- archive/delete persistence in an isolated Orca home; and
- session identity resets across new, resume, and fork.

Final verification:

```text
cargo fmt --all -- --check
cargo test -p orca-tui --lib -- --test-threads=1
cargo test --test history_contract -- --test-threads=1
cargo test --test cli_architecture_contract -- --test-threads=1
cargo check --workspace
git diff --check
```
