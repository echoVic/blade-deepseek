# DeepSeek History Replay Validity Design

## Summary

Orca must never persist or replay an assistant turn that contains only
`reasoning_content` and has neither visible assistant content nor tool calls.
DeepSeek can occasionally produce this shape after an interrupted, empty, or
otherwise incomplete response. If it enters the JSONL transcript, a later TUI
resume can send an invalid assistant message back to the provider and make an
otherwise healthy session impossible to continue.

This slice adds one shared assistant-payload invariant and enforces it at the
provider response, session persistence, and history resume boundaries. The
change is intentionally transparent in the TUI: the user-visible outcome is
that resumed sessions continue instead of failing because of a hidden malformed
turn.

## User Outcome

- Resuming an older TUI session that contains a reasoning-only assistant turn
  does not poison the next DeepSeek request.
- A new reasoning-only provider response is retried through the existing empty
  response policy and is never written to history.
- Valid DeepSeek reasoning attached to visible content or tool calls remains
  available for replay and TUI rendering.
- A malformed provider turn fails at the turn where it occurs instead of
  silently becoming a delayed resume failure.

## Assistant Payload Invariant

An assistant message has a replayable payload when at least one of these is
true:

1. `content` contains non-whitespace text.
2. `tool_calls` is non-empty.

`reasoning_content` alone is not a replayable assistant payload. It is provider
state associated with a visible answer or tool-call turn, not a standalone
conversation turn.

The invariant lives in `orca-core` so provider serialization, session
persistence, and history normalization cannot drift into different definitions
of a valid assistant message.

## Architecture

### Provider Response Boundary

Both DeepSeek streaming and non-streaming response paths apply the shared
payload invariant after response folding. A reasoning-only response is treated
the same as the existing empty response case:

- retry up to the existing empty-response retry limit;
- preserve real provider errors instead of relabeling them as empty responses;
- return the existing empty-response error after retries are exhausted.

This prevents malformed turns before they enter runtime state while preserving
the current retry semantics and user-facing error path.

### Session Persistence Boundary

The runtime assistant-response recorder checks the invariant before mutating
the conversation or appending JSONL history. Invalid responses return
`InvalidData` and leave the conversation unchanged.

This is a defensive boundary for provider implementations and future runtime
callers. Provider validation remains the normal first line of defense, but
history correctness must not depend on every provider path remembering the
same rule.

### Resume Sanitization Boundary

Conversation normalization drops assistant messages that fail the invariant
when reconstructing a conversation from stored history. The existing tool-call
boundary normalization continues to remove orphaned tool results and incomplete
tool-call groups.

Resume sanitization is required for legacy transcripts written before this
invariant existed. It operates in memory and does not rewrite the source JSONL
file, so session recovery is safe and reversible.

### TUI Boundary

No new modal, notice, or transcript card is introduced in this slice. The
failure is hidden historical corruption, so the best TUI behavior is seamless
recovery. Valid reasoning remains projected through the existing TUI reasoning
path when it belongs to a content or tool-call turn.

## Data Flow

1. DeepSeek returns streaming or non-streaming response data.
2. Provider folding extracts visible content, reasoning, and raw tool calls.
3. The provider validates content/tool-call payload presence.
4. The runtime recorder validates the same invariant before persistence.
5. JSONL history stores only replayable assistant turns.
6. On resume, normalization removes any legacy reasoning-only assistant turn
   before the next provider request is assembled.

## Error Handling

- Reasoning-only provider responses use the existing empty-response retry and
  terminal error behavior.
- Session recording rejects invalid payloads before conversation mutation.
- Resume sanitization skips invalid legacy turns and keeps later user messages.
- Valid assistant tool-call turns remain subject to the existing tool-result
  completeness normalization.

## Alternatives Considered

### Sanitize Only During Resume

This repairs old sessions but continues writing malformed new history and lets
the active turn appear successful until the next resume. Rejected because it
delays the failure and preserves the source of corruption.

### Reject Only At The Provider Boundary

This protects new DeepSeek responses but leaves legacy transcripts broken and
trusts every future provider implementation to enforce history validity.
Rejected because persistence needs its own invariant.

### Shared Invariant At Three Boundaries

Recommended and selected. Provider validation gives immediate retry behavior,
session validation protects persistence, and resume normalization repairs old
data without rewriting it.

## Non-Goals

- Do not remove reasoning from valid content or tool-call turns.
- Do not rewrite or migrate existing JSONL transcript files.
- Do not add a new TUI warning for silently repaired legacy turns in this
  release.
- Do not include the separate `context.compaction.started` work currently
  present in another checkout.
- Do not change tool-call replay ordering or DeepSeek reasoning replay rules for
  complete tool-call turns.

## Acceptance Criteria

1. `Conversation` normalization drops reasoning-only and whitespace-only
   assistant messages.
2. DeepSeek streaming and non-streaming reasoning-only responses retry and then
   return the existing empty-response error.
3. Provider API message construction omits reasoning-only assistant messages
   loaded from conversation state.
4. Runtime assistant response recording rejects invalid payloads without
   mutating conversation or history.
5. Resume drops legacy reasoning-only assistant turns while retaining later
   user turns.
6. Valid reasoning plus tool calls still replays with the existing DeepSeek
   ordering and tool-call ids.
7. Focused core, provider, runtime, history, and TUI resume tests pass.
8. Full workspace tests, clippy, formatting, release staging, site checks, and
   real DeepSeek API smoke pass before tagging.
9. The change ships as an isolated patch release with roadmap/release notes and
   public GitHub Release/npm verification.

## Release Scope

Target release: `v0.2.15`.

The release note should describe this as a TUI session-resume reliability fix:
Orca rejects incomplete DeepSeek assistant turns at creation time and repairs
legacy reasoning-only turns in memory when a session resumes.
