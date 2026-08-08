# Goal Interaction Settlement Spec

## Problem and Evidence

Typed surface Goal execution already models `GoalOuterTurnStatus::ApprovalRequired`, and the
runtime maps an approval-required operation to `FailureClass::LegacyApprovalRequired`. Three
boundary inconsistencies were found while exercising the complete interaction lifecycle:

- Goal finalization collapsed that failure class into `GoalOuterTurnStatus::Failed`.
- durable Goal usage truncated fractional cost micros while the operation surface rounded them,
  allowing the recorded turn and finalization preview to disagree;
- `GoalPatch::OuterTurnFinished` replaced cumulative surface Goal usage with the latest turn delta,
  so an explicit resume diverged from the SQLite Goal record.

Existing tests covered ordinary Goal success, budget exhaustion, cancellation, and restart recovery,
but did not drive a live Goal through typed Allow, Deny, and explicit resume while comparing durable
and surface settlement.

## User and Architecture Value

For a TUI user, denying a tool approval must visibly stop the Goal and make it resumable without
replaying the denied tool or leaving a live run behind. Each submitted run must keep one durable
outer-turn record, one terminal operation result, and one usage event owned by `GoalActor`/`GoalStore`.

## Scope

In scope:

- Preserve `LegacyApprovalRequired` as `GoalOuterTurnStatus::ApprovalRequired` during Goal
  finalization.
- Exercise a real typed surface Goal operation that requests tool approval, then test both Allow and
  Deny outcomes.
- Use one shared USD-to-micros conversion for operation and durable Goal usage.
- Accumulate each settled outer-turn delta in the surface Goal projection, matching SQLite.
- Assert one outer turn and terminal operation per submitted run, exact usage, no in-flight run, and
  a fresh run/fence after explicit resume for the denied case.
- Update the production roadmap and this Spec/Plan with the final deletion and compatibility status.

Out of scope:

- Adding a new public pause-reason enum or changing CLI, TUI, server JSONL, or persistence schemas.
- Removing the source-compatible pending-interaction shim; that requires its separate API migration
  gate.
- Changing provider/tool execution policy or approval UI rendering.

## Lifecycle Semantics

- Allow: a real `write_file` approval resolves through the typed runtime surface, the tool executes,
  the operation reaches terminal success, and the Goal outer turn settles once with usage.
- Deny: the interaction resolves as `LegacyApprovalRequired`, the operation reaches a terminal
  failed-with-approval classification, and Goal state is paused through the typed
  `ApprovalRequired` outer-turn status. No continuation is admitted automatically.
- Cancel, timeout, crash, and restart semantics remain owned by existing interaction recovery and
  operation finalization paths; this slice must not add a second waiter or cancellation owner.
- Explicit resume starts a new Goal run and a new generation fence; the denied generation is never
  re-executed.

## Ownership and Boundaries

`RuntimeHost` owns the live operation and joins it. The typed runtime surface owns the interaction
request/response and operation terminal. `GoalActor`/`GoalStore` owns outer-turn state, usage, run
terminalization, and resume identity. No process-local pending map is consulted for Goal admission.

## Compatibility

No external protocol, CLI argument, TUI flow, or persistence shape changes. The existing
`FailureClass::LegacyApprovalRequired` terminal remains compatible; only its typed Goal projection is
corrected.

## Acceptance Criteria

1. A live typed Goal Allow test observes one terminal operation, one outer turn, the real tool side
   effect, and the exact usage delta, with no in-flight run.
2. A live typed Goal Deny test observes the typed approval-required stop, one terminal operation,
   exact usage, no in-flight run, and zero tool side effects.
3. After Deny, explicit resume creates a different Goal run, operation, and interaction fence. Allow
   then executes the tool once; both the surface and SQLite Goal report cumulative `14/6` usage.
4. The behavior tests pass on the rebased feature branch; focused runtime-surface and Goal lifecycle
   tests, formatting, and diff checks pass.
5. No old interaction store or compatibility path is added.

## Verification Commands

```bash
cargo test -p orca-runtime --lib runtime_host::tests::surface_goal_tool_approval
cargo test -p orca-runtime --lib runtime_host::tests::surface_goal_approval_denial
cargo test -p orca-runtime --lib thread::tests::goal_usage_delta_rounds_cost_micros_like_surface_projection
cargo test -p orca-runtime --lib runtime_host::tests::surface_goal
cargo test --test runtime_lifecycle_contract -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

After the semantic commit, rebase `origin/main` and rerun the focused tests plus the required
runtime/server contract gates for the final branch.

## Migration and Removal

This is a settlement/data-consistency correction, not a compatibility layer. The incorrect mapping,
duplicate cost conversion, and per-turn usage replacement are removed in the same commit. The
pending-interaction shim remains governed by the previous slice's versioned API migration gate and
is not part of this work.
