# Binding-Only `@` Mention Design

## Status

Approved in conversation on 2026-07-15. This document is awaiting final written-spec review before implementation planning.

## Problem

Orca currently treats any unbound token beginning with `@` as a possible legacy file mention. The parser consumes text through the next whitespace boundary, then attempts to resolve path-like values against the working directory. This makes ordinary text such as:

```text
@oai/sky还能逆向吗
```

behave like a file attachment request because the token contains `/`. Resolution fails with `No such file or directory` before the prompt reaches the model.

The failure also exposes a TUI lifecycle bug. The composer enters `Running` before mention expansion finishes. When expansion fails, the worker emits a generic error and returns without emitting a terminal turn event. The UI remains in `Running`, while Esc and Ctrl+C only send interrupts that the idle worker ignores.

The root design problem is that Orca currently uses one visible syntax for two different meanings:

- `@` as an autocomplete trigger;
- `@text` as proof that the user selected a concrete attachment.

Visible text is not sufficient identity. Orca already has `MentionBinding` and `MentionTarget` to carry an explicit, typed selection, so raw-text inference should no longer participate in mention expansion.

## Goals

1. Treat every unbound `@...` occurrence as ordinary prompt text.
2. Expand a Mention only when the client supplies a valid structured binding.
3. Apply the same binding-only rule to TUI, CLI, and app-server input.
4. Return the TUI to an editable idle state when submission preparation fails.
5. Preserve a force-exit path when a running-state terminal event is lost.
6. Keep selected file, Skill, Plugin, MCP Resource, and Resource Template expansion atomic and workspace-safe.

## Non-Goals

- Changing `$skill` plaintext compatibility in this work.
- Adding a new CLI flag for structured Mention input.
- Changing Mention search ranking, discovery, multi-root identity, or popup rendering.
- Changing the visible text inserted for a selected candidate.
- Making every generic runtime error terminal.

## Reference Behavior

The local Codex checkout separates autocomplete from selection:

- typing `@` opens a completion surface;
- submitting without a selected candidate sends literal text;
- selecting a structured tool Mention creates separate binding metadata;
- protocol `UserInput::Mention` is explicitly documented as a user-selected structured Mention.

Orca should adopt that semantic boundary while preserving its richer `MentionTarget` expansion for files, Skills, Plugins, and MCP Resources.

## Decision

### Binding-only `@` semantics

An `@` token may open and filter the Mention popup, but it has no attachment meaning by itself. Only selecting a final candidate creates a `MentionBinding`.

At submission time:

- a valid binding expands its exact `MentionTarget`;
- an unbound `@...` range remains unchanged;
- an invalid or stale binding rejects submission before a provider turn begins;
- the runtime never re-resolves visible `@name` text to guess a target.

This rule applies even when the raw text happens to name an existing file. For example, manually typing `@README.md` and pressing Enter without selecting the file sends the literal text `@README.md` to the model.

### Intentional compatibility break

Legacy unbound `@file` expansion is removed from every input surface.

Consequences:

- interactive TUI users must select the file candidate to attach it;
- CLI prompts containing `@file` no longer inject file contents;
- app-server text input containing `@file` remains text;
- app-server structured Mention input continues to expand;
- historical prompt text is not reinterpreted as a file attachment when replayed.

No replacement CLI attachment syntax is added in this change.

## Architecture

### Search and selection

The existing search pipeline remains responsible only for discovery and selection:

1. Composer text and cursor position identify an active `@` token.
2. Mention search returns typed candidates.
3. The popup may open, update, or close without changing prompt semantics.
4. Selecting a final candidate inserts visible text and records a hidden `MentionBinding`.
5. Submitting or dismissing the popup without a selection leaves the token as ordinary text.

Directory browsing remains non-attachable and does not create a binding.

### Expansion boundary

The runtime expansion API becomes binding-only. Its responsibilities are:

1. Reconcile bindings against the final prompt text.
2. Ignore bindings whose ranges or visible slices no longer match.
3. Deduplicate valid bindings by stable target id.
4. Revalidate every target against current workspace, plugin, Skill, and MCP state.
5. Append typed context blocks for valid targets.
6. Return the original prompt unchanged when no valid bindings exist.

The legacy scan of raw prompt text via `extract_mention_occurrences` and `file_mention_blocks` is removed from this expansion path.

If `expand_file_mentions` has no remaining production callers after CLI migration, remove it and any parser helpers used only by legacy expansion. Token parsing needed for popup discovery remains separate.

### Surface contracts

#### TUI

`SubmitWithMentions { prompt, bindings }` remains the submission action. The worker expands only the provided bindings.

#### CLI

The CLI stops preprocessing prompt text with `expand_file_mentions`. It sends the user's prompt unchanged. Because the CLI has no structured Mention argument, it cannot attach a Mention in this change.

#### App-server

Plain text input never creates a binding. Only structured Mention input constructs `MentionBinding` and reaches the shared expansion path. The server must not infer a Mention target from text input.

## Submission Failure Lifecycle

### Dedicated terminal event

Generic `TuiEvent::Error` must remain non-terminal because an error can occur during a turn that is still active. Submission-preparation failure therefore gets a dedicated event:

```rust
TuiEvent::SubmissionRejected {
    prompt: String,
    message: String,
}
```

This event is emitted when work performed after the optimistic TUI submit but before the provider turn fails, including:

- a bound file was deleted or moved;
- a bound root is no longer active;
- a bound Skill or Plugin is no longer discoverable;
- an MCP Resource read fails;
- conversation initialization fails before the turn begins.

### TUI recovery behavior

When `SubmissionRejected` arrives, the TUI:

1. removes the optimistic user message for the rejected submission;
2. restores the original visible prompt into the composer;
3. clears Mention bindings so a stale target cannot immediately fail again;
4. displays the rejection message as an error;
5. sets status to `Idle`;
6. clears the running timer and receiving-progress state;
7. scrolls to the error and restored composer.

The input-history entry may remain, because it is useful for recovery and already reflects text the user intentionally submitted.

No `session.completed` event is written for a provider turn that never began. The rejection event is the TUI terminal signal for this pre-turn lifecycle.

## Interrupt and Exit Semantics

Esc and Ctrl+G remain turn-interrupt shortcuts. They do not force process exit.

Ctrl+C becomes a two-stage fail-safe in `Running` and `Compacting`:

1. First Ctrl+C cancels the token, sends `UserAction::Interrupt`, records the timestamp, and shows that another Ctrl+C will exit.
2. A second Ctrl+C within two seconds sends `UserAction::Cancel` and exits with status 130, even if the UI still reports `Running`.

Outside `Running` and `Compacting`, the existing double-Ctrl+C exit behavior remains.

The UI should not optimistically switch a real provider turn to `Idle` on the first interrupt. Normal turn cancellation remains event-driven so late runtime output cannot appear under an incorrectly idle composer.

## Error Handling Rules

- Raw unbound `@` text cannot cause mention expansion failure.
- A stale structured binding is an explicit submission rejection, not a silent downgrade.
- Rejection restores the visible prompt but clears bindings.
- Generic runtime errors do not change turn status.
- The second Ctrl+C is allowed to terminate regardless of stale UI status.
- Existing workspace-boundary and target-integrity validation remains unchanged for selected bindings.

## Testing Strategy

### Runtime Mention tests

- An empty binding set preserves `@oai/sky还能逆向吗` exactly.
- An empty binding set preserves `@README.md` even when `README.md` exists.
- Emails, npm scopes, URLs, and punctuation around `@` remain unchanged.
- A selected file binding still expands the exact selected root and file.
- Selected Skill, Plugin, Resource, and Resource Template bindings still expand.
- Stale structured bindings still fail with target-specific errors.
- Legacy raw file-expansion tests are removed or rewritten as literal-text tests.

### TUI tests

- Enter with an open popup and no selected row submits literal text.
- Selecting a candidate creates one binding and submits it.
- A preparation failure emits `SubmissionRejected`.
- `SubmissionRejected` removes the optimistic user message, restores prompt text without bindings, and returns to `Idle`.
- Generic `TuiEvent::Error` remains non-terminal.
- First Ctrl+C while running interrupts; second Ctrl+C within two seconds exits 130.
- Esc still interrupts without exiting.

### CLI tests

- A mock-provider CLI request containing an existing `@file` sends the original prompt without a `<file>` block.
- `@oai/sky还能逆向吗` reaches the mock provider unchanged.

### App-server tests

- Plain text input containing an existing `@file` remains text in model history.
- Structured Mention input expands through its exact target.
- Same-name targets remain distinct through structured identity.
- A stale structured target returns a normal turn-start error without leaving an active turn registered.

### Regression scope

Run focused Mention, TUI shortcut, CLI prompt, and app-server contract tests first. Then run formatting, diff checks, workspace checks, and the broader test suite using the repository's established Node runtime path when required.

## Documentation Changes

Update the following source-of-truth surfaces with the implementation:

- ADR-0002: remove the compatibility statement for unbound `@file` and required invariant 8.
- Mention glossary: state that a Mention Token is only a search token until selection creates a binding.
- Harness contract: clarify that text input is never inferred as a Mention.
- README and CLI examples: remove any promise that typing raw `@file` attaches content.

## Acceptance Criteria

1. `@oai/sky还能逆向吗` reaches the model unchanged on every plain-text input surface.
2. Manually typed `@README.md` never injects file content, even when the file exists.
3. Selecting `README.md` in the Mention popup still injects the selected file.
4. Only structured Mention input can expand on app-server.
5. CLI no longer performs raw `@file` expansion.
6. A failed structured Mention preparation returns the TUI to `Idle` with editable text restored.
7. Two Ctrl+C presses within two seconds can always exit a stale running state.
8. Existing atomic identity, multi-root safety, target validation, and selected-candidate expansion tests remain green.
