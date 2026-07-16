# Harness Contract Refresh Design

Date: 2026-07-16

## Problem

`docs/harness-contract.md` began as the external contract for `orca exec` and
later accumulated the embedded server protocol, tool behavior, approval policy,
provider behavior, and Mention semantics. Several sections now lag behind the
implementation:

- the command section omits current resume, fork, continue, and history flags;
- the server section covers only part of the operations accepted by the wire
  decoder and router;
- the event list omits current usage, context, model-routing, plan, workflow,
  task, and progress events;
- the tool table describes subagents as synchronous and the agent-loop section
  says nested subagents are rejected, while the runtime supports sync and async
  modes and defaults to a maximum nesting depth of two.

This drift makes the document unreliable for client authors and weakens its use
as a compatibility review surface.

## Scope

Keep `docs/harness-contract.md` as the single entry point for Orca's external
headless and embedded-server contracts. Refresh it in place instead of splitting
it into multiple documents.

The document will cover:

- `orca exec` inputs, output modes, statuses, and deterministic exit codes;
- `orca --mode=server` framing, request ownership, method families, and compact
  server event projection;
- the versioned runtime event envelope and the complete current event type set;
- externally observable tool, approval, subagent, workflow, provider replay,
  configuration, file-search, and atomic Mention behavior.

The document will not attempt to describe internal actor ownership, private
Rust APIs, TUI-only implementation details, or experimental behavior that is
not exposed through the two documented entry points.

## Sources Of Truth

Every contract statement must be checked against the implementation surface
that owns it:

- CLI flags and defaults: Clap definitions and `README.md`;
- server methods and request shapes: `crates/orca-runtime/src/protocol/wire.rs`
  and `crates/orca-runtime/src/server/router.rs`;
- runtime events: `crates/orca-core/src/event_schema.rs`;
- built-in tools and capabilities: the canonical tool registry and tool types;
- subagent modes and limits: `subagent.rs`, `subagent_config.rs`, and
  `subagent_execution.rs`;
- observable protocol behavior: `exec_jsonl`, server runtime, and session server
  contract tests;
- Mention architecture: ADR 0002, while this document owns only the wire-level
  input and search semantics.

When prose and code disagree, this refresh follows current code and tests. It
does not change runtime behavior to preserve stale prose.

## Document Structure

The refreshed document will use this order:

1. purpose, compatibility boundary, and source-of-truth note;
2. headless `orca exec` command and output contract;
3. embedded server transport and method families;
4. file search, unified Mention search, and atomic Mention input;
5. runtime event envelope, event types, run status, and exit codes;
6. built-in and external tool behavior, hooks, subagents, workflows, and goals;
7. approval behavior;
8. DeepSeek provider and replay behavior;
9. configuration precedence and file locations.

Server methods will be grouped by capability rather than documenting every
request field inline. Representative examples will remain for integration-critical
flows. `wire.rs` and contract tests remain authoritative for exhaustive field
shapes.

## Compatibility Language

The opening section will distinguish normative and informational content:

- command names, method names, event names, statuses, exit codes, and documented
  request/response invariants are compatibility commitments;
- implementation topology, internal scheduling, and numeric defaults explicitly
  described as current defaults may evolve without creating a new protocol;
- legacy operations will be labelled as compatibility paths instead of being
  presented as the preferred API.

The event envelope version remains `1`; this documentation-only refresh does
not introduce a wire-format version change.

## Verification

No production code changes are required. Verification will include:

1. compare every documented runtime event with `EventType`;
2. compare every documented server method family with `ClientOp` and router
   dispatch;
3. compare CLI flags with the current command definitions and README;
4. compare tool and subagent statements with the canonical registry and runtime
   configuration;
5. run the focused JSONL and server protocol contract tests that exercise the
   documented surfaces;
6. scan the resulting Markdown for stale synchronous-only, no-nesting, and MVP
   claims.

## Acceptance Criteria

1. A client author can identify the supported headless and server integration
   surfaces without reading runtime internals.
2. The event list matches the current `EventType` set.
3. The server method families match the current wire decoder and router.
4. Subagent sync, async, concurrency, and nesting statements match current code.
5. Atomic Mention identity and binding rules remain consistent with ADR 0002.
6. The document clearly separates stable external behavior from implementation
   detail and current defaults.
7. Focused contract tests pass without any production behavior change.
