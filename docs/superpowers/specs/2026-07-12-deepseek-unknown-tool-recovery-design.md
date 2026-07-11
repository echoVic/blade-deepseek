# DeepSeek Unknown Tool Recovery Design

## Summary

Orca must treat a model-generated unknown tool name as a recoverable tool
failure, not as a terminal provider failure. The production incident that
triggered this change occurred when DeepSeek emitted `function.name = "wc -l"`
instead of calling `bash` with the command in its arguments. Orca rejected the
name while parsing the provider response, converted the rejection into
`ProviderStep::Error`, failed the turn, and paused an otherwise healthy
persistent Goal.

This slice preserves the malformed call and its provider call id, routes it
through the existing tool-validation boundary, records a failed tool result,
and lets the model correct its call on the next turn. Unknown names never map
to shell execution automatically.

## Incident Evidence

The affected session is:

`~/.orca/sessions/2026/07/11/session-2026-07-11T15-33-39-b55d2465-5bf6-4fb9-91d6-2954aff6f5eb.jsonl`

At 2026-07-12 00:13:09 +0800, automatic Goal continuation #10 started. The
session failed 7.25 seconds later. The TUI displayed:

```text
ERROR: failed to parse tool call 'wc -l': unknown tool: wc -l
Goal paused because the last turn ended with status 'failed'.
```

No shell task was created. The session JSONL persisted the continuation, usage,
and terminal failed status, but not the malformed assistant call or diagnostic.
The corresponding task record contained only `error: "failed"`.

## User Outcome

- A single hallucinated or malformed tool name no longer pauses an active Goal.
- The TUI shows the failed tool call's original name and error.
- The call id is preserved internally and in history to pair the assistant call
  with its tool result.
- The failed result is recorded in conversation history and sent back to
  DeepSeek, which can correct the call on the next turn.
- Genuine provider, transport, quota, and context errors keep their existing
  terminal behavior.
- Arbitrary unknown names are never executed as shell commands.

## Safety Invariant

Provider parsing may classify a call, but it must not invent executable intent.

1. A name resolved by the configured built-in, MCP, or external-tool registry
   keeps its current canonical tool identity and action.
2. An unresolved `mcp__*` name remains `ToolName::Mcp(original_name)`; every
   other generic unresolved name becomes `ToolName::External(original_name)`.
   Both keep the raw arguments and original call id.
3. The unresolved request receives no shell, write, network, or agent
   capability. Its provisional action remains `Read` only so it can enter the
   validation path.
4. Registry validation rejects it before approval, hooks, task creation, or
   execution.
5. The resulting failed tool result is the only effect of the unknown call.

The implementation must not infer `bash` from whitespace, shell metacharacters,
or command-shaped names such as `wc -l`.

## Architecture

### Provider Response Boundary

`orca-provider` already preserves invalid JSON arguments for known tools so the
execution layer can return a corrective failure. Unknown names should use the
same boundary. `parse_tool_call` will return a `ToolRequest` for every provider
tool call:

- registered names use the resolved action and existing target extraction;
- unresolved `mcp__*` names use `ToolName::Mcp`; other generic unresolved names
  use `ToolName::External`. Both use `ActionKind::Read`, the original name as
  the display target, and the untouched argument string.

Streaming and non-streaming response folding will append
`ProviderStep::ToolCall` instead of `ProviderStep::Error` for unresolved names.
The raw assistant tool call remains unchanged for DeepSeek history replay.

### Tool Validation Boundary

No new executor is added. The existing shared validation path resolves the
request against the configured registry. An unresolved name returns
`unknown tool: <name>` before approval or execution. Runtime and TUI tool loops
already record ordinary non-subagent tool failures and continue to the next
provider turn.

### Goal Continuation Boundary

Because the provider response no longer contains `ProviderStep::Error`, the
turn does not take the terminal provider-error path. The model receives the
failed tool result, can issue a valid call, and can complete the current Goal
turn. Actual provider errors still fail the turn and preserve `/goal resume`
semantics.

### Persistence And Diagnostics

The malformed assistant call and matching failed tool result are recorded by
the normal conversation/session path. This closes the incident's diagnostic
gap without adding a second logging mechanism or rewriting old session files.

## Error Handling

- Unknown built-in-like names return a failed tool result and continue.
- Unknown MCP names already follow the recoverable tool-validation path and
  remain unchanged.
- Configured external tools continue to resolve and execute according to their
  declared action and schema.
- Invalid arguments for known tools continue to return schema failures.
- Provider transport, quota, content-filter, truncation, and stream-integrity
  errors remain terminal provider errors.
- Repeated unknown calls remain bounded by the existing agent turn limit.

## Alternatives Considered

### Convert Command-Shaped Names To Bash

This would make the incident's `wc -l` call run immediately, but it would also
turn arbitrary model-generated function names into executable shell input.
Rejected because it invents capability and bypasses registry identity.

### Retry The Provider Request Internally

The provider could discard the malformed response and retry with extra prompt
text. Rejected because it hides the original call, consumes another completion,
and does not use the model's standard tool-result correction protocol.

### Preserve The Call And Return A Tool Failure

Selected. It is deterministic, keeps call/result pairing, uses existing safety
and persistence boundaries, and gives the model explicit corrective feedback.

### Merge The Invocation Terminal Truth Branch

That branch changes roughly 30 files to introduce broader cancellation,
terminal, and replay semantics, but it still rejects unknown tools during
provider parsing. Rejected for this incident because it does not fix the root
cause and is not a finished release candidate.

## Test Strategy

1. Replace the parser test that expects an error with one that requires the
   exact `wc -l` call to become a non-bash external request.
2. Add a streaming fixture proving the response contains a `ToolCall`, no
   provider error, and an unchanged raw assistant call.
3. Extend the mock provider with an unknown-call-then-correct flow.
4. Add a TUI agent-loop regression proving the unknown call is rejected,
   recorded, returned to the model, and followed by a successful final answer.
5. Run provider and TUI focused tests, then the complete release gate and real
   DeepSeek API smoke before tagging.

## Documentation And Release

By execution time, `v0.2.17` and its cumulative Goal timing changes were already
public on GitHub and npm. The unknown-tool recovery therefore ships as the new
`v0.2.18` release. Update:

- `docs/goal-mode.md`
- `docs/production-roadmap.md`
- `docs/releases/v0.2.18.md`
- English and Chinese `v0.2.18` site summaries

The release workflow remains tag-driven and must publish the GitHub Release,
four native archives, npm wrapper/platform packages, and npm tarball assets.

## Acceptance Criteria

1. `wc -l` is preserved as `ToolName::External("wc -l")`, never `Bash`.
2. Streaming and non-streaming unknown calls produce `ProviderStep::ToolCall`,
   not `ProviderStep::Error`.
3. The registry rejects the request before approval, hooks, tasks, or execution.
4. The model receives a matching failed tool result and can complete the next
   turn.
5. The failed call and diagnostic are present in conversation history.
6. Actual provider errors remain terminal.
7. Focused provider/TUI regression tests pass.
8. Formatting, clippy, full workspace tests, site checks, release-script tests,
   npm staging, and the real DeepSeek release smoke pass.
9. `v0.2.18` is public on GitHub and npm, and `npm exec` reports `orca 0.2.18`.
