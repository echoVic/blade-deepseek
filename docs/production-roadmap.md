# Orca Production Roadmap

> Goal: evolve Orca into a production-grade DeepSeek-native agent runtime.
> Reference implementations: Codex CLI, Claude Code, and the current Orca codebase.

Last updated: 2026-07-08
Current baseline: current main after v0.1.191 and the follow-on TUI bridge
slice owns approved background provider-response continuation execution in
`orca-runtime`: the runtime consumes the pending provider response from
`TaskRegistry` and derives the single preapproved tool-call id before the TUI
resumes a backgrounded turn. Runtime provider-cycle, turn-loop, and agent-loop
inputs carry a typed `RuntimeTurnContinuation` instead of a bare
`ProviderResponse`; the provider cycle consumes the continuation once, seeds the
turn permission overlay with the preapproved tool-call id, and the approval gate
consumes that id exactly once for the matching tool call. The TUI bridge now
converts approved background continuations into runtime `ThreadTurnRequest`
continuations and projects runtime JSONL events back into TUI events, retiring
the renderer-owned preapproved provider/tool loop. The follow-on TUI
notification slice now preserves workflow terminal notification ids across the
pending queue, action channel, and turn-result continuation boundary instead of
recasting queued workflow continuations as plain user prompts. The TUI agent
loop now also routes submitted turns through a named source boundary: human
submits still get user-authored `@file` mention expansion, while workflow
notification continuations are forwarded as typed follow-ups without user
prompt preprocessing; the same source boundary now also supplies the TUI task
label, so workflow follow-up turns show a stable notification label instead of
raw notification payload text, uses that label as the session title seed when a
workflow follow-up creates the first recorded history thread, and records
workflow follow-ups as non-backtrackable context so the TUI backtrack command
still targets the user's last real submit. The TUI goal-turn loop now also
receives that submitted-turn boundary directly, so task-label and backtrack
presentation metadata stay grouped with the submitted turn instead of crossing
the loop as parallel ad hoc fields. Earlier v0.1.191 makes
`RuntimeProviderResponseStep` consume the
named `RuntimeProviderResponseInput` directly and carries child-agent executors
through `RuntimeProviderResponseExecutors`. Provider final-message handling and
tool-turn dispatch now keep one response handoff instead of re-expanding the
kernel-assembled response input into a long argument list. Earlier v0.1.190
carries provider-turn execution through a named `RuntimeProviderTurnInput` and
groups provider-call I/O refs behind `RuntimeProviderTurnIo`.
`RuntimeProviderTurnStep` now receives actor, provider, runtime system messages,
hook/cancel refs, budget policy, steering handle, and the grouped
conversation/history/event/sink/cost refs as one call-boundary object, while
provider behavior remains unchanged. Earlier v0.1.189 carries provider-response
I/O refs through a named `RuntimeProviderResponseIo` bundle.
`RuntimeTurnKernel` now assembles events, sink, conversation, history writer,
cost tracker, and background workflow refs as one response handoff, while
provider response handling destructures that bundle only at the execution
boundary before dispatching final-message or tool turn work. This continues the
same direction seen in Codex turn/step contexts and Claude Code query/tool
contexts: wide execution state is handed across named runtime boundaries rather
than expanded at every call site. Earlier v0.1.188 carries provider-cycle capability refs through the
same `RuntimeStepCapabilitySnapshot` used by step context. `RuntimeProviderCycleInput`
keeps provider-cycle execution fields separate from the request capability
bundle, `runtime_turn_iteration` assembles that bundle once from turn input, and
`provider_turn` passes it into `RuntimeStepContext` without expanding the flat
instructions, memory, MCP, hook, cancellation, task, workflow IPC, or interaction
handler refs. Earlier v0.1.187 moved request-scoped capability refs inside a named
`RuntimeStepCapabilitySnapshot`. `RuntimeStepSnapshot` now keeps the immediate
request execution fields while routing instructions, memory, MCP registry,
hooks, cancellation, task registry, workflow IPC, and turn interaction handlers
through that capability bundle; `tool_turn` consumes the bundle through
`RuntimeStepSnapshot::capabilities()` before dispatching readonly, subagent, and
normal tool turns. Earlier v0.1.186 routes `request_user_input` through the same
turn-scoped interaction boundary as permission requests. `RuntimeTurnInteractionState`
now carries both the permission handler and the runtime user-input handler,
`ThreadTurnRequest` can install a user-input handler for a turn, and
`runtime_turn_iteration`, `RuntimeStepSnapshot`, `ToolExecutionContext`, and
`RuntimeToolRouter` pass that handler to the point where `request_user_input`
is dispatched as a runtime special tool instead of a normal-tool fallback.
Earlier v0.1.185 grouped turn-scoped interaction handlers behind a named
`RuntimeTurnInteractionState`. `AgentLoopContext`, `runtime_turn_loop`, and
`runtime_turn_iteration` carry the permission-request handler through that
grouped turn interaction boundary before provider/tool dispatch needs it,
leaving the existing approval and permission behavior unchanged while making
room for later elicitation and dynamic-tool waiters to share the same
turn-owned interaction surface. Earlier v0.1.184 gave provider and
tool-dispatch steps a named request-scoped runtime snapshot.
`RuntimeStepSnapshot` now owns the stable per-request runtime inputs that had
been spread across `RuntimeStepContext`, while `RuntimeStepContext` carries that
snapshot plus the kernel-bound extension context. Provider final-response
handling reads settings through the snapshot, and tool dispatch splits the step
context into snapshot plus extension binding before routing normal, readonly,
workflow, and subagent tool turns. Earlier v0.1.183 gave runtime capability
changes a named snapshot contract.
`RuntimeCapabilityPatch` and `RuntimeCapabilitySnapshot` own model overrides,
allowed-tool replacements, runtime system-message injection, and transition
reasons behind directive state, while `RuntimeDirectiveState` applies patches
and exposes that shared snapshot for future skill, hook, MCP, and tool-policy
paths. Earlier v0.1.182 moved turn-loop state assembly onto a
`RuntimeTurnKernel` instance. `RuntimeTurnState` creates the kernel from the
thread and turn extension stores, then asks that instance to assemble
`RuntimeTurnLoopState`; the loop state keeps shared scoped extension stores so
the kernel can borrow the same state it hands forward. Earlier v0.1.181 let
`RuntimeTurnKernel` assemble the lifecycle-owned `RuntimeTurnLoopState` that
carries directive state, mutable runtime refs, and scoped extension state into
the turn loop. `RuntimeTurnState` no longer expands loop runtime and
extension-state fields itself, preserving behavior while moving the Codex-style
turn-state handoff through the named kernel boundary. Earlier v0.1.180 let
`RuntimeTurnKernel` assemble the
provider-response input object that carries the bound `RuntimeStepContext`,
kernel-owned sampling state, event/sink refs, conversation/history refs, cost
tracker, and background workflow handles. Provider response handling no longer
exposes kernel-owned sampling state or step-context binding as separate fields,
preserving behavior while tightening the Codex-style turn-state handoff. Earlier
v0.1.179 let `RuntimeTurnKernel` retain the runtime extension stores used by its
reducer and bind provider-response `RuntimeStepContext` extensions through the
same kernel. Provider response handling no longer wires step-context extension
stores directly, preserving behavior while tightening the Codex-style
turn-state boundary around sampling state, reducer state, and extension context.
Earlier v0.1.178 introduced a `RuntimeTurnKernel` that owns the per-sampling
request state together with the runtime turn reducer. Provider response handling
now constructs tool-dispatch state through that kernel before passing it into
tool turns, preserving behavior while giving the next Codex-style turn state
consolidation a named runtime boundary. Earlier v0.1.177
enriches server-mode `command/exec/list` snapshots with the backing `shellId`,
`taskId`, requested terminal mode, and effective terminal mode, so reconnecting
app-server clients can recover the same task identity and PTY/pipe semantics
exposed by `shell/list`. Earlier v0.1.176 added
server-mode `command/exec/list` so app-server clients can recover active
`command/exec` process handles by listing `processId`, original command argv,
`cwd`, running status, stream-output settings, output cap, and stdout/stderr
sent-byte counters; completed processes are drained before the next list
response and disappear from the active snapshot. Earlier v0.1.175 let
server-mode `command/exec/read` requests apply an `outputBytesCap` byte budget
to active streaming `command/exec` processes, tightening the process output cap
before the server's normal pre-dispatch drain and returning UTF-8-safe
`command_exec_output_delta` events with `capReached` metadata. Earlier v0.1.174 added server-mode
`command/exec/read` so app-server clients can actively drain long-running
streaming `command/exec` process handles by `processId`, receive a
`command_exec_read` acknowledgment, and reuse the existing
`command_exec_output_delta` / `command_exec_completed` stream. Earlier v0.1.173 let server-mode `shell/read` requests apply an
`outputBytesCap` byte budget to incremental shell stdout/stderr, returning
truncated UTF-8-safe deltas plus `capReached` metadata on
`shell_output_delta`, `shell_updated`, and `shell_completed` events. Earlier
v0.1.172 exposed a server-mode `shell/capabilities` operation so app-server
clients can query the current platform, native PTY and PTY resize availability,
accepted terminal modes, pipe fallback behavior, and the `processId`
requirement for streaming `command/exec` sessions before launching terminal
work. Earlier v0.1.171 fixed two sandbox/task-state rough edges:
pathless macOS sandbox denials such as GitHub HTTPS credential prompts can now
escalate through runtime, JSONL `command/exec`, and TUI approval flows to
re-run the command without the filesystem sandbox, while shell task session
state now lives under `ORCA_HOME/task-sessions` (or `~/.orca/task-sessions`)
with migration from legacy project `.orca/task-sessions` directories. Earlier v0.1.170 let
`RuntimeSamplingRequestState` record normal tool results and own the
approval-required plus subagent-failure terminal folding for single-tool turns.
Normal tool execution now borrows its permission overlay and records its result
through the same request state, leaving `tool_turn` to delegate the
per-sampling state boundary. Earlier v0.1.169 let
`RuntimeSamplingRequestState` produce clamped `RuntimeToolDispatchWindow` values
for readonly and subagent batch dispatch. Tool turns no longer read raw cursor
positions or slice batch windows directly, and the dispatch-window API
guarantees forward progress over the current request even if a batch collector
returns the current cursor. Earlier v0.1.168 let
`RuntimeSamplingRequestState` own the tool-dispatch cursor as well as the
per-sampling permission overlay. Tool turns now read and advance the current
request through sampling state instead of keeping a separate `ToolRequestCursor`,
so the Codex-style request-scoped runtime state boundary has one clearer owner.
Earlier v0.1.167 introduced
`RuntimeSamplingRequestState` as the first per-sampling request-state home and
routes normal tool turns through its permission overlay instead of allocating
local permission state inside `tool_turn`. Provider response handling now
creates that sampling state before tool dispatch, giving later Codex-style
request snapshots a concrete runtime boundary. Earlier v0.1.166 moved direct
`RuntimeTurnLoopInput` construction out of `agent_loop` and behind the focused
`run_agent_turn_loop` entrypoint. `agent_loop` passes a
`RuntimeAgentTurnLoopInput` launch object while `runtime_turn_loop` owns the
internal wide handoff to the iteration boundary. Earlier v0.1.165 let
`RuntimeTurnLoopState` own the directive-resolved loop policy surface:
`agent_loop` no longer destructures loop state or reads directive accessors
directly; lifecycle resolves tool policy, runtime system messages, model
override, cost/cancel/task refs, and grouped extension context for each
turn-loop iteration. Earlier v0.1.164 let
`RuntimeTurnState` hand `agent_loop` a lifecycle-owned `RuntimeTurnLoopState`
and moved extension context derivation to the iteration boundary, v0.1.163 moved
grouped runtime extension-context composition into the state boundary, v0.1.162 moved grouped
runtime extension routing up to the turn-loop, turn-iteration, and provider-cycle inputs,
v0.1.161 moved the grouped context into `RuntimeStepContext` and
`RuntimeNormalToolTurnContext`, v0.1.160 moved grouped extension-store routing up
to `ToolExecutionContext`, v0.1.159 grouped permission-sensitive turn/thread
extension references behind `RuntimeExtensionStores`, v0.1.158 made permission
reduction consistently instance-owned by `RuntimeTurnReducer`, v0.1.157 routed
permission overlay mutation through the reducer, v0.1.156 routed runtime
directive application through the reducer, and v0.1.155 introduced the reducer
for completed-tool goal progress.

---

## Current State

Orca has moved beyond the original MVP roadmap. The table below is the current
working baseline used to prioritize the next patch releases.

| Area | Current Orca State | Codex/Claude Reference | Status |
|------|--------------------|------------------------|--------|
| Tool registry | Built-ins, MCP tools, and TOML external tools share `ToolSpec` metadata; runtime argument validation covers common object keywords plus `oneOf` / `anyOf` composition | Codex-style spec/capability registry | Implemented |
| Tool approval | Action kind is derived from tool capabilities, with TOML allow/deny rules | Capability/policy driven approvals | Implemented |
| File discovery | `glob` is model-facing with normal glob patterns plus `mode: "fuzzy"` path queries; `list_files` remains a compatibility alias | Claude `Glob`, Codex file search | Implemented |
| Shell execution | `bash` and server shell ops route through a runtime shell-session manager with task ids, stdin, kill, nonblocking incremental reads, optional Unix PTY mode, PTY resize, stdout/stderr collection, macOS Seatbelt path, configurable timeout, and observable requested/effective terminal modes with pipe fallback where PTY is unavailable | Codex `exec_command` sessions, PTY, stdin, timeout | Seeded; richer shell controls still open |
| Context management | BPE token counting, local compaction, persisted collapse/summary records | Multi-level local/remote compaction | Partial |
| Tool output control | Fixed byte truncation helper on tool output | Codex truncation policies by bytes/tokens with explicit warnings | Partial |
| Model metadata | `ModelSelection` plus DeepSeek defaults | Codex `models-manager` with model capability metadata | Partial |
| MCP | stdio/SSE config surface, tool routing, and read-only resource list/read/template tools | Codex MCP client/server ecosystem | Partial; resource access seeded |
| Hooks | Lifecycle hooks with JSON stdout actions; structured outputs that declare `action` now validate supported actions and required string fields | Codex hooks runtime and schema validation | Implemented; schema docs/validation improved |
| Project instructions | User/project/rules files with includes | `AGENTS.md` style layered instructions | Implemented |
| Memory | Manual `/remember` plus optional project extraction | Codex memories extension | Partial |
| Persistent goals | `/goal` with persisted state plus goal-scoped `get_goal`, `create_goal`, and narrow `update_goal` | Codex goal extension | Implemented |
| Workflows | JavaScript workflow DSL, generated drafts, edit/save/run controls, reusable workflow commands, task state, notifications, runtime status events, evidence-bound reports, and worktree-isolated/recoverable agent runs | Codex/Claude workflow orchestration concepts | Implemented |
| Runtime lifecycle | Headless, server-mode, and TUI agent runs now seed an agent task lifecycle through a runtime turn runner; `RuntimeThread` groups runtime-owned interactive session state with lifecycle state, and server-mode `ServerThread`, the headless controller, and the TUI conversation-session wrapper now keep long-lived agent state behind `RuntimeThread` instead of directly assembling session/lifecycle/executor pieces; workflow runs, sync/async subagent boundaries, workflow child agents, and shell tool calls also carry task metadata; tool approval/hooks/normal fallback now share a runtime tool actor context, while workflow, subagent, task, permission, workflow IPC, and normal-tool dispatch route through `RuntimeToolRouter`; server stdio decoding now delegates operation dispatch through a focused router boundary, with synchronous thread query/metadata, turn-control, shell session, command/exec compatibility, permission response, user-input response, and submit-family dispatch moved into focused processor modules; command/exec active process state and pending server user-input state now live in focused server manager modules; runtime-special tool classification and small executors now live in `runtime_special.rs`; approved background provider-response continuations now convert into typed `RuntimeTurnContinuation` values, and the runtime consumes the single preapproved tool-call id exactly once through the turn permission overlay | Codex `Session -> Task -> Turn`, app-server request processors; package 3 pending permission maps | Seeded; deeper TUI loop delegation still open |
| TUI | Markdown-ish rendering, themes, Vim mode, diff preview, slash commands, workflow panel, elapsed timers, and clearer approval dialogs | Codex/Claude richer terminal UX | Partial |
| History | JSONL transcripts, resume/fork/search/archive/compress with a dedicated `SessionStore` boundary | Codex thread store with queryable metadata | Partial |
| Release | GitHub release + npm alias distribution scripts, retrying post-publish GitHub/npm/npm-exec verification, and a reusable real API e2e release gate | Codex npm/native release model | Implemented |
| Skills | Markdown skill discovery, `list_skills`/`read_skill`, and explicit `$skill` prompt injection | Codex skills and plugin-provided skill bundles | Partial |

---

## Patch Release Plan

The next work should land as independent patch releases. Each release must be
verified before the next phase starts.

### Current Refactor Priorities

The July 2026 Codex and package 3 reference pass ranks the remaining
architecture work as follows. Codex is the stronger reference for ownership:
core `SessionTask` implementations run turns against a frozen `TurnContext`,
while the TUI mostly replays protocol state. Package 3 is useful for product
surface and pending-request UX, but its broad `ToolUseContext` should not be
copied into Orca.

1. **P0: Runtime-owned background approval continuation execution.** Done on
   current main: the TUI now resumes approved background turns by converting the
   stored provider response into a typed runtime continuation and running a
   `ThreadTurnRequest` through the runtime bridge. The renderer-owned
   preapproved provider/tool loop has been removed, and the TUI only projects
   runtime events plus final task status.
2. **P1: Pending interactive request boundary.** Seeded: runtime now owns a
   focused `RuntimePendingInteractionRecord` shape for tool approvals,
   `request_permissions`, and `request_user_input`, and the TUI interaction
   adapter projects those runtime records into existing dialogs/prompts instead
   of hand-building separate payloads. Runtime also owns the shared pending
   interaction store, and the TUI session passes that store through tool
   approvals, `request_permissions`, `request_user_input`, and child-agent tool
   paths. TUI approval and user-input responses now carry the runtime
   interaction id back through the action channel, so handlers resolve only the
   matching pending request. The server protocol now also rejects
   `permission/respond` submissions that omit `requestId`, keeping cross-surface
   permission responses tied to a concrete pending request. Runtime and server
   pending-request maps now reject duplicate ids instead of silently replacing
   an existing waiter, and TUI interaction adapters now fail before prompting
   when a duplicate pending id would otherwise create an unrouteable second
   dialog. Server-mode `request_user_input` now follows the same Codex/package
   3-style pending map: the runtime emits `user_input_request`, clients answer
   with `user_input/respond` plus `requestId`, and the server resolves only the
   matching waiter with `user_input_resolved`. Background main-session approval
   actions now also carry the pending tool approval request id through the TUI
   action channel; the runtime task registry validates that id, rejects
   duplicate responses, and returns the owning task id only after the request
   has been matched. Workflow terminal notifications now carry a stable
   notification id derived from runtime workflow ids through the TUI queues, so
   batch-boundary reconciliation no longer identifies pending continuations by
   prompt text; both AppState and batch-boundary queues now reject duplicate
   notification ids before creating duplicate model continuations or user-visible
   notices. The cross-thread TUI notification queue is now a focused
   `PendingWorkflowNotificationQueue` boundary instead of an exposed
   `Arc<Mutex<VecDeque<_>>>`, so queue insert/drain/pop behavior stays behind
   named methods. Queued workflow continuations also keep their notification id
   when they cross the TUI action channel or return from a turn-result
   continuation; human prompts remain plain `Submit` actions, while workflow
   follow-ups use a typed notification action/result. The workflow-notification
   action channel now carries `PendingWorkflowNotification` directly instead of
   splitting the id and prompt into separate action fields, and workflow
   notifications enter `SubmittedTurn` through the same typed notification
   boundary. The TUI agent loop now applies prompt preprocessing through a named
   submitted-turn source boundary, so `@file` mention expansion remains
   user-input behavior and workflow notifications are not dropped because
   generated notification text happens to look like a local file mention. That
   source boundary also carries the TUI task description for workflow follow-up
   turns, keeping the workflows panel focused on a stable notification label
   instead of raw XML/diagnostic payload text. The same submitted-turn boundary
   now also gives first-turn workflow notification sessions a stable title seed,
   so recorded history/search does not name the thread after raw notification
   XML. Workflow follow-up turns remain model-visible user-role context, but are
   no longer treated as the user's last backtrack target. That submitted-turn
   value now enters the TUI goal-turn loop as one boundary object, with
   `SubmittedTurnPresentation` owning the task label and backtrack policy that
   had been passed as parallel fields. `SubmittedTurnKind` now owns the prompt
   and source-specific workflow notification state, leaving presentation metadata
   as a display/backtrack policy layer instead of a third parallel source of
   turn identity; the goal loop now reads that policy through submitted-turn
   accessors instead of reaching into presentation fields. That boundary now
   lives in a focused `submitted_turn` module instead of the app event loop
   file, and its presentation metadata type is private behind the submitted-turn
   accessors. Turn results now expose a typed `TuiAgentTurnContinuation`
   boundary instead of a workflow-notification-specific result field, so
   workflow follow-ups are one continuation variant and future continuation
   kinds do not need more parallel ad hoc result slots. Approved background
   turns also cross from the TUI approval handler into the continuation runner
   as a typed `TuiBackgroundTurnContinuationRequest`, so the runner no longer
   exposes a naked task id as its continuation boundary and denied approvals do
   not manufacture continuation requests. The TUI background approval response
   submission path now also lives in a focused module, keeping request-id
   matching, denied-task finishing, task-list refreshes, and typed continuation
   request creation behind one named boundary instead of embedding that state
   transition in the app event loop. Workflow terminal notification queueing,
   cross-thread pending notification draining, pending notification submission,
   and by-id removal now also live in a focused TUI workflow notification
   module, so the app event loop coordinates notification turn boundaries
   without owning the pending-continuation queue mechanics. Foreground and
   background approval option resolution now also lives in a focused TUI
   approval action module, keeping session allowlist updates and request-id
   action dispatch out of the app event loop while preserving the runtime
   pending-interaction ids. Next, move the same id discipline into
   remaining turn/item continuation ownership so continuations stop depending on
   separate ad hoc task fields plus TUI-local queues.
3. **P2: Frozen per-turn context boundary.** Continue shrinking wide call
   surfaces into `RuntimeTurnConfig`, `RuntimeTurnDeps`,
   `RuntimeTurnState`, and request snapshots. Runtime turn continuations now
   live with the other immutable turn inputs inside `RuntimeTurnConfig`, and
   runtime steering handles now enter the turn through the same config
   boundary, so `AgentLoopContext` no longer carries either as a separate ad
   hoc field. Turn-scoped permission and user-input handlers now live with the
   other injected services in `RuntimeTurnDeps`, keeping TUI interaction
   routing on the same dependency boundary as server/headless turns. Turn-loop
   workflow refs now pass through `RuntimeTurnWorkflowContext` instead of
   parallel background-workflow and workflow-IPC fields, and event output refs
   now pass through `RuntimeTurnOutputContext` instead of parallel
   `EventFactory`/`EventSink` fields. Turn-loop provider/model refs now pass
   through `RuntimeTurnProviderContext` instead of parallel provider,
   provider-config, model, and budget fields, and immutable request inputs now
   pass through `RuntimeTurnRequestContext` instead of parallel cwd, prompt,
   continuation, steering, and subagent fields. `RuntimeAgentTurnLoopInput`
   now enters the loop through those same provider/request contexts instead of
   rebuilding parallel fields at the loop boundary, and turn-loop stages now
   pass injected services through `RuntimeTurnDeps` instead of repeating hooks,
   instruction, memory, MCP, and interaction fields. Turn-loop policy/config
   refs now pass through `RuntimeTurnPolicyContext` instead of repeating run
   config, directive-resolved tool policy, and approval policy fields.
   Iteration stages now keep lifecycle-owned `RuntimeTurnLoopIterationState`
   grouped instead of unpacking runtime system messages, model overrides,
   cost/cancel/task refs, and extension refs into the iteration input. Keep
   borrowing package 3's explicit loop-local `State` idea, but avoid a single
   giant context object.
4. **P3: Protocolized task/thread/interactive status.** Push background task,
   approval-needed, needs-input, foregrounded/backgrounded, and completed
   status through runtime protocol events so TUI, server, and future app
   clients stop inferring state from surface-specific structs. The runtime
   event schema now has a single-task `task.status.updated` event, and TUI
   main-session task start/background/finish and background provider-completion
   updates, plus approved background-turn continuation refreshes, route through
   it instead of borrowing the workflow task-list event for each one-task status
   change. TUI subagent task creation, progress, and terminal status updates
   now use the same single-task event path. Workflow launch/startup terminal
   updates and background terminal updates now also use that single-task event
   path when a concrete workflow task id is known, while workflow progress
   polling keeps the aggregate workflow task-list event for full-list progress
   refreshes.
   Server protocol event mapping also preserves that single-task status event
   as `task_status_updated` for non-TUI clients. The TUI projection now keeps
   that path as a single-task update and merges it into the panel by task id, so
   one status event cannot drop unrelated visible tasks.
5. **P4: Persistence policy for pending background continuations.** Seeded:
   approval-required background main-session tasks now persist a compact
   provider-response continuation record through `TaskRegistry`, so a restarted
   TUI session can recover the pending tool approval, accept the approval
   response, and resume through the runtime-owned continuation path instead of
   losing the provider response. TUI session initialization now also refreshes
   recovered approval-required background tasks and emits a user-visible notice
   naming the pending tool. Invalid or future-incompatible continuation records
   now fail closed at task-registry load time: the affected background task is
   marked failed, pending approval state is cleared, and the sanitized record is
   written back instead of blocking the whole session restore.
6. **P5: Package-3-style task UX polish.** Borrow the visible task panel ideas:
   sorted task list, detail view, foreground/stop actions, and notifications.
   Keep implementation behind Orca runtime task/protocol types rather than
   importing package 3's UI-state coupling. Seeded: the TUI task panel can now
   request a stop for the selected non-terminal task through the runtime
   `TaskRegistry`, refreshing the panel after the status changes. Stop,
   foreground, and recovered-background-approval task actions now live in a
   focused TUI background task module, keeping package 3-style task controls
   behind Orca runtime task summaries instead of app-loop state mutation. The
   workflows panel key handler now also lives in a focused TUI panel action
   module, so task selection, approval opening, stop dispatch, and foreground
   dispatch are grouped with the panel UX instead of the app event loop.
   Running-state shortcuts now also execute through a focused TUI running
   action module, keeping background-current-turn, interrupt, and live-scroll
   behavior grouped with the running UX instead of the app event loop.
   Composer textarea construction, prefilled text restoration, text extraction,
   setup input masking, and paste insertion now live in a focused TUI composer
   module, giving slash/mention/menu input flows one shared input boundary.
   Mention candidate refresh and mention menu key handling now live in a
   focused TUI mention action module, keeping @file completion state changes out
   of the app event loop.
   Slash command execution now lives in a focused TUI slash command action
   module, so direct command submission and menu completion share one
   configuration/state mutation boundary.
   Slash menu candidate refresh, menu key handling, selected command dispatch,
   and model/reasoning submenu flow now live in a focused TUI slash menu action
   module, leaving the app loop to route input events rather than own menu
   mechanics.
   Composer input editing now lives in a focused TUI composer input action
   module, covering slash/mention refresh after edits, newline handling,
   history recall, Tab file mention completion, and plain key input.
   Idle submit handling now also lives in a focused TUI idle submit action
   module, covering slash-command short-circuit submission, pending
   user-input answers, normal prompt submission, prompt-history recording, and
   composer reset after accepted submissions.
   Idle navigation/control shortcuts now live in a focused TUI idle navigation
   action module, covering scroll/page movement, backtrack dispatch, and
   expand-latest-tool-output fallback into normal composer editing.
   Global TUI shortcuts now live in a focused global action module, keeping
   Ctrl-C interrupt/exit flow, shortcut overlay toggling, transcript top/bottom
   scrolling, and clear-screen terminal cleanup out of the app event loop.
   Runtime task summaries now also expose terminal `result`/`error` fields so
   the selected task row can show completion output or failure details in the
   panel. The
   panel now renders contextual action hints for selection, approval, stop, and
   closing so TUI users can discover task controls in-place. Selected task
   result/error details now render as bounded multi-line summaries, keeping
   longer terminal output readable without letting one task consume the panel.
   Task refreshes now sort the panel by attention priority (approval-required,
   active, then terminal with recent activity first) while preserving the
   selected task by id across refreshes. Backgrounded running main-session
   tasks can now be returned to the foreground from the panel with `f`, clearing
   foreground-output suppression and refreshing the task list through
   `TaskRegistry`; the detached background provider worker now replays buffered
   visible reasoning/message/tool-progress deltas generated while hidden,
   forwards future deltas after foregrounding, and emits the normal foreground
   session-completed event when that turn finishes. When a main-session turn is
   first backgrounded, the TUI now opens the task panel and selects that
   backgrounded session once, making the foreground/stop controls discoverable
   without stealing selection on later refreshes. When that selected
   backgrounded session is returned to the foreground, the TUI closes the task
   panel so replayed and future assistant output is visible immediately.
   Backgrounded main-session approvals now also reveal and select their task
   once, so an approval wait is visible without clobbering later manual
   selection.

### P0: Session Runtime Unification

**Release target:** v0.1.31

**Current status:** done in v0.1.31.

**Goal:** move long-lived interactive session state from the TUI bridge into
`orca-runtime`, creating the runtime boundary needed for a Codex-style protocol
layer.

**Deliverables:**

- Add `orca_runtime::session::InteractiveSession`.
- Centralize conversation, history writer, session id, project instructions,
  memory, hooks, MCP registry, cost tracking, and workflow task registry in
  runtime.
- Keep `TuiConversationSession` as a compatibility wrapper that delegates to the
  runtime session.
- Preserve current TUI event names, JSONL behavior, workflows, goals, backtrack,
  compaction, and request-user-input continuation.
- Document the boundary in
  `docs/superpowers/specs/2026-06-25-session-runtime-unification-design.md`.

**Verification:**

- `cargo fmt -- --check`
- `cargo test --workspace --all-targets`
- `npm --prefix site run build`
- `npm --prefix site run check:seo`
- `node scripts/release/test-stage-npm.mjs`
- `git diff --check`
- Post-publish `scripts/release/verify-published.mjs` for GitHub Release, npm,
  and `npm exec` smoke verification.

### P1: Protocol And Event Boundary

**Release target:** v0.1.32

**Current status:** server-mode submissions and server-facing events now flow
through `orca_runtime::protocol` with typed `Submission`, `ClientOp`, and
`ServerEvent` values while preserving the legacy flat JSON wire format. The
server accepts the original `{"op":"submit"}` wire shape plus Codex-style
`thread/start` and `turn/start` method requests for the first app-server-shaped
thread/turn lifecycle entry points. Server-mode `turn/start` now parses
`params.threadId` and rejects unknown in-process thread ids, while persistent
ThreadStore-backed materialization remains a follow-up.

**Goal:** introduce a runtime protocol boundary so TUI/headless clients can send
commands and consume versioned events without owning turn execution details.

**Scope:**

1. Define an `orca-runtime` protocol module inspired by Codex protocol types. Done in v0.1.32 for server mode.
   - User input, approval responses, cancel/backtrack, goal operations, and
     workflow controls should be commands.
   - Session lifecycle, assistant deltas, reasoning, tool calls, workflow/task
     updates, approvals, errors, and completion should be events.
2. Add a runtime event adapter. Server-mode adapter done in v0.1.32; TUI
   assistant deltas, usage, errors, session completion, tool-call
   requested/completed, plan-updated, and subagent started/completed events now
   adapt from runtime `EventFactory` payloads instead of hand-built TUI structs.
   - Preserve existing display behavior while sourcing events from runtime where practical.
   - Runtime approval events now carry concrete tool name, target, and preview
     metadata needed by TUI prompts, and interactive approval prompts flow
     through the adapter without losing UI fidelity.
   - Workflow terminal notifications now flow through runtime
     `workflow.result.available` / `workflow.failed` events with workflow name,
     tool-use id, status, and summary metadata. Workflow task-list/progress
     refreshes now flow through a runtime `workflow.tasks.updated` event before
     adapting back to `TuiEvent::WorkflowTasksUpdated`. Declared workflow
     lifecycle events for resume, phase start/completion, agent start/cache/
     completion/failure, pause, and stop now have `EventFactory`, server
     protocol, and TUI notice coverage.
   - Keep JSONL output names stable for this release unless explicitly versioned.
3. Move more turn-loop orchestration behind runtime-owned APIs. Seeded after v0.1.42.
   - The TUI may still render and request approvals.
   - Runtime should own command handling and event emission. The current
     `RuntimeTaskActor` seed owns turn budget checks, turn advancement,
     `turn.started` event construction, model routing, pre/post model hook
     orchestration, provider streaming calls, and usage/budget accounting for
     controller turns. It also owns shell tool lifecycle event shaping so
     controller call sites no longer construct shell task payloads directly,
     owns pre/post tool hook context and warning/error formatting, and resolves
     non-interactive tool approval decisions. Normal built-in/external/MCP tool
     execution fallback also flows through the actor now. Tool approval,
     pre/post tool hooks, and normal fallback execution share one
     `RuntimeToolActorContext` instead of constructing ad hoc controller-owned
     lifecycles. Runtime-special tool dispatch classification for workflow,
     subagent, workflow IPC, and normal tool paths also now lives on that
     actor. `AgentLoopContext` now delegates immutable turn entry values to
     `RuntimeTurnConfig` and read-only agent-loop services to
     `RuntimeTurnDeps`, the first package-3 `QueryConfig` / `QueryDeps`-style
     split, with mutable per-turn runtime handles grouped behind
     `RuntimeTurnState` and execution/lifecycle refs grouped behind
     `RuntimeTurnExecution`. Workflow IPC execution now flows through a runtime IPC trait on
     the context, SubagentStatus execution now flows through a runtime status
     lookup trait, and WorkflowDraft preview creation now lives on the runtime
     context. Workflow draft actions and launch now live in
     `workflow_execution`, subagent sync/async launch and worker entrypoints now
     live in `subagent_execution`, and the controller no longer owns those
     execution bodies.
4. Seed a first runtime-owned task/turn lifecycle. Done after v0.1.42 for
   headless agent runs, server-mode submissions, and TUI bridge turns:
   `turn.started` JSONL events, legacy server `turn_started` events, and
   `TuiEvent::TurnStarted` now carry task metadata. Workflow lifecycle events
   and synchronous `subagent.started`/`subagent.completed` events also carry
   task metadata.
5. Add a runtime-owned `RuntimeTurnRunner` seed. Done after v0.1.42 for
   headless controller turns and TUI bridge turns: turn advancement and
   `turn.started` task payload construction now live in `orca-runtime`.
   Async subagent workers now persist task lifecycle metadata through
   `subagent_status` results; workflow child agent evidence and shell tool
   call events now carry task lifecycle metadata too. A `RuntimeTaskActor`
   seed now owns controller turn starts, max-turn exhaustion, model routing,
   pre/post model hook orchestration, provider streaming calls, and
   usage/budget accounting, plus shell tool requested/completed event shaping
   and pre/post tool hook orchestration. Non-interactive tool approval
   resolution and normal tool execution fallback now flow through the actor
   too; these controller tool phases now reuse a single `RuntimeToolActorContext`.
   Runtime-special workflow/subagent/workflow-IPC dispatch is classified by the
   runtime context, workflow IPC execution now lives behind a
   `RuntimeWorkflowIpc` trait on that context, and SubagentStatus execution now
   lives behind a runtime status lookup trait. WorkflowDraft preview creation
   also now lives on the runtime context. Workflow draft actions, workflow
   launch, subagent launch, and async subagent worker entrypoints have been
   extracted from the controller into focused execution modules. Interactive
   approval resolution now flows through runtime approval handlers, so
   headless `tool_execution` and TUI tool execution share the runtime approval
   boundary while each surface supplies its own user-action adapter. TUI
   `request_user_input` continuations now use a runtime user-input handler
   boundary as well, leaving the TUI responsible for presenting and collecting
   user actions while runtime owns argument parsing and tool-result shaping.
   A `RuntimeShellSessionManager` seed can now spawn shell tasks with stdin,
   collect stdout/stderr, kill the process group, and keep `TaskRegistry`
   shell records in sync. Model-facing bash execution and server protocol
   operations now route through that shell-session boundary. Server
   `shell/read` now returns a running snapshot with available stdout/stderr
   without waiting for process completion. `shell/start` now accepts explicit
   `terminalMode: "pipe" | "pty"` configuration, preserves legacy `pty: true`,
   and can seed Unix PTY window size with initial `cols` / `rows`; `shell/resize`
   can still update Unix PTY window size after start. Shell reads now also emit
   Codex-style `shell_output_delta` notifications
   before the legacy `shell_updated` / `shell_completed` responses, and
   terminal shell reads/kills emit `shell_exited` with normalized process exit
   codes, including Unix signal exits as `128 + signal`. Active MCP tool waits now
   observe server-turn cancellation and let interrupted turns complete without
   waiting for the MCP transport's default request timeout. MCP stdio/SSE
   transports now accept configurable startup/tool request timeouts, and stdio
   requests use a reader-thread boundary so slow `tools/call` responses time out
   without blocking on `read_line`. SSE timeout behavior now has transport
   contract coverage, and legacy app-server `tool_completed` events preserve
   MCP timeout errors plus runtime `exit_code` and `kind` metadata. MCP clients
   now refresh the underlying transport after
   timeout/connection failures so later calls can recover without silently
   replaying the failed tool call. Shell sessions now expose requested and
   effective terminal modes so non-PTY platforms can fall back to pipe mode
   without making the session untestable. Server `shell/list` now returns
   active shell snapshots with task ids, commands, status, terminal modes, and
   descriptions, while `shell/update` can rename an active shell description
   and have the updated metadata reflected in later list snapshots. Codex-style
   `command/exec` and `command/exec/terminate` are now compatibility entries
   on top of the runtime shell-session manager for buffered commands and
   killable process ids, including request-scoped `cwd`, env override / unset
   handling, Codex `tty` field parsing, Codex-style validation for mutually
   exclusive timeout/output-cap and sandbox/profile options plus
   streaming-without-process-id requests, buffered and streaming output-cap
   truncation with streamed `capReached` metadata, streamed stdout/stderr
   deltas with `command/exec/write` stdin support,
   client-driven `command/exec/read` drains for active process handles, and
   `command/exec` TTY initial size/resize support, and read-time
   `outputBytesCap` tightening for active streaming process drains.

**Refreshed reference-driven priority order (Codex + package 3):**

1. **Shell/PTY task sessions:** Codex exposes long-running exec flows and
   package 3 models shell work as `LocalShellTask`; Orca now has a runtime
   shell-session seed with task ids, stdin, kill, output collection, nonblocking
   incremental reads, explicit pipe/PTY terminal modes, initial PTY sizing,
   PTY resize, bash-tool routing, and server
   `shell/start|write|close|resize|read|kill|list` operations. Shell start now
   reports requested/effective terminal modes, PTY requests fall back to pipe
   mode on platforms without PTY support, `shell/list` returns active shell
   snapshots for reconnecting clients, and `shell/update` can refresh the
   user-facing shell description metadata. Codex-style `command/exec` buffered
   execution and `command/exec/terminate` process-id cancellation now reuse the
   same manager rather than a second process runner, and `command/exec` now
   honors Codex-style `cwd`, env override/unset, `tty`, invalid option
   validation, output caps, streamed stdout/stderr delta, `command/exec/write`,
   explicit `command/exec/read` drains, and TTY initial size/resize request
   fields. Server shell reads now emit
   `shell_output_delta` and `shell_exited` notifications alongside legacy
   shell responses, giving clients a Codex-shaped process stream seed. The
   model-visible package-3-inspired `task_list` / `task_stop` tools now expose
   session tasks through `TaskRegistry` with `subject`, `status`, `owner`, and
   `blockedBy` fields plus Orca task type/command metadata, and `task_stop`
   accepts the deprecated `shell_id` alias while validating missing, unknown,
   and terminal tasks. MCP and TOML external tools now have first-class
   app-server item streams, and historical projections preserve failed
   MCP/dynamic tool status, error message, exit code, and truncation metadata.
2. **App-server turn controls:** Codex SDK tests cover steer/interrupt/resume
   at the turn handle level. Orca now accepts server `turn/interrupt`,
   `turn/resume`, and `turn/steer` commands, returns a stable
   `turn_controlled` event for idle/no-active-turn requests, runs
   thread-bound server turns in the background, and lets `turn/interrupt`
   cancel an active server turn so it completes as cancelled. Completed turn
   controls now return structured errors, and active controls can reject a
   mismatched `threadId` precondition. `turn/resume` can now reset a cancelled
   active token before the cancellation checkpoint observes it, and active
   `turn/steer` now emits an observable user `item_started` event and injects
   the steer input into the active turn's model context before the provider
   call. Active server turn handles now use the same user-visible persisted
   `turnId` that `thread/turns/list` exposes, with system messages excluded
   from user turn numbering. Pre/post model and tool hook subprocess waits now
   observe active turn cancellation, while provider streaming already receives
   the same cancel token. Bash shell-session tool waits and TOML external tool
   process waits now also observe the active turn cancel token, kill the child
   process group, and let interrupted turns complete as cancelled without
   waiting for the shell timeout. TUI-local streaming bash now shares the same
   cancel-aware process wait, preserving partial output while interrupting
   promptly. MCP tool execution now also observes the active turn cancel token
   and returns a cancelled tool result promptly instead of blocking the turn on
   the MCP request timeout. MCP server config now supports
   `startup_timeout_ms` and `tool_timeout_ms`; stdio/SSE `tools/call` timeouts
   are enforced at the transport boundary, and server-mode turns surface
   timeout details in legacy `tool_completed.error`. After timeout/connection
   failures, MCP clients rebuild the transport for future calls without
   automatically replaying the failed call. `shell/capabilities` now exposes
   the platform/runtime capability surface that clients need before requesting
   PTY sessions or resize operations, and `shell/read` can now cap incremental
   stdout/stderr with `outputBytesCap` plus `capReached` metadata for clients
   that need bounded reads, and `command/exec/read` now gives server clients a
   request/ack boundary for draining active streaming process output, with
   read-time `outputBytesCap` support for bounded polling. Next, use
   those boundaries for deeper
   cross-platform PTY support.
3. **ThreadStore-backed app-server materialization:** Codex treats threads as
   resumable/forkable SDK objects. Orca has `SessionStore` and an in-process
   server path; `thread/start` is now immediately visible through the
   persistent `SessionStore`, and server `thread/resume` / `thread/fork` can
   materialize live thread handles from persisted transcripts. Server
   `thread/resume` now reopens the same persisted thread id and appends future
   turn items to the original transcript, while `thread/fork` still creates a
   child transcript.
4. **Permission profile persistence:** Codex preserves approval mode across
   thread resume/fork/turn overrides, while package 3 keeps permission modes and
   rule sources as pure types. Orca now snapshots approval mode and permission
   rules into thread metadata and exposes `approvalMode` /
   `permissionRuleCount` through thread summaries. Server `thread/resume` and
   `thread/fork` now inherit the stored approval mode and permission-rule
   snapshot when materializing a live thread, and explicit app-server
   `approvalMode` / `permissionRules` resume/fork parameters override that
   snapshot when supplied. Codex-style `approvalPolicy` is now accepted as an
   app-server alias for thread resume/fork and turn/start requests, mapping
   `never` to Orca `full-auto` and `on-request` / `untrusted` to `suggest`;
   thread-bound `turn/start` permission overrides are applied to the active
   turn, persisted back to thread metadata, and visible in later thread
   summaries. Package-3-style `permissionUpdates` now decode on app-server
   `turn/start` and apply as ordered incremental updates after any whole-profile
   override: `setMode`, `addRules`, `removeRules`, and `replaceRules` map to
   Orca approval mode and permission-rule metadata, including package 3
   `Bash` / `Write` tool-name normalization and `ask` -> `prompt` behavior
   mapping. `addDirectories` / `removeDirectories` now persist package-3-style
   additional working directories on thread metadata, expose
   `additionalWorkingDirectories` / `additionalWorkingDirectoryCount` through
   `thread/read` and `thread/list`, and feed those roots into the bash
   seatbelt profile so the metadata changes real shell sandbox behavior.
   Codex-style built-in `permissionProfile` names (`read-only`, `workspace`,
   and `danger-full-access`, with or without the `:` prefix) now also drive
   `command/exec` sandbox selection. Thread-scoped `activePermissionProfile`
   is inherited by thread-bound `command/exec` when no request-level sandbox
   override is supplied, while explicit request `sandboxPolicy` still wins.
   Configured `[permission_profiles.<name>]` entries can now define
   Codex-style `extends` chains to those built-in profiles, and `command/exec`
   resolves the configured chain before choosing its sandbox. Configured
   `[permission_profiles.<name>.filesystem]` entries with `read` access are
   preserved as additional readable roots and now make custom read-only
   permission profiles use a strict read allow-list plus platform minimal
   runtime roots; entries with `write` or `read-write` access now compile into
   additional writable roots for `command/exec`, and `deny` entries compile
   into read/write deny rules that can override broader readable and writable
   roots.
   `[permission_profiles.<name>.network]`
   `enabled = true|false` now overrides the inherited built-in sandbox network
   default, Codex-style domain policy fields enforce through the managed
   command/exec network proxy, and Unix socket `allow` entries now materialize
   into macOS Seatbelt rules without enabling broad network access. Linux
   accepts the same Unix socket config for compatibility but cannot enforce
   path-level socket filters. Configured `:workspace_roots` /
   `:workspace_roots/<subpath>`
   filesystem entries now materialize against the owning thread's
   `runtimeWorkspaceRoots` before command execution, and TOML scoped
   filesystem tables such as
   `[permission_profiles.docs.filesystem.":workspace_roots"]` normalize into
   the same command sandbox roots. Configured `:tmpdir` / `:slash_tmp` entries
   now materialize to the current command environment's temp directory and
   `/tmp`, configured `:root` materializes to `/`, and configured `:minimal`
   materializes to platform default read roots needed by shell runtimes. Trailing
   `/**` filesystem entries now normalize to subtree roots, and bounded
   filesystem glob entries such as `*.env` or `docs/**/*.md` are expanded before
   command sandbox startup into concrete read, write, read-write, or deny roots.
   `glob_scan_max_depth` / `globScanMaxDepth` controls the bounded filesystem
   walk depth per profile, inheriting from extended profiles unless a child
   profile overrides it. Over-broad globs without a static parent directory are
   still rejected before scanning. Session-scoped `request_permissions` network
   domain grants now persist on server threads and feed later `command/exec`
   proxy policy; automatic ask-on-block remains a later expansion.
5. **Protocol item stream:** Codex SDK emits `thread.started`, `turn.started`,
   `item.started/updated/completed`, and terminal turn events. Orca now keeps
   legacy JSONL names stable while the server adapter emits user steer
   `item_started` events plus agent-message `item_started`,
   `item_message_delta`, and `item_completed` lifecycle events. Tool call
   server streams now also emit command-execution `item_started` /
   `item_completed` events while preserving legacy `tool_requested` /
   `tool_completed` events. Reasoning streams now have a Codex-shaped
   `reasoning` item lifecycle with `summary` accumulation,
   `item_reasoning_delta`, and `item_completed`, while preserving legacy
   `reasoning_delta`. Structured `plan.updated` runtime events now surface as
   app-server-style `turn_plan_updated` notifications for update-plan tool
   changes. Codex plan-mode `<proposed_plan>` blocks are now split out of
   assistant message deltas into `plan` item lifecycle events with
   `item_plan_delta`, including split-tag and incomplete-block handling.
   Workflow runtime streams now emit `workflow` item lifecycle events with
   launch metadata, result summaries, and failed/completed terminal states while
   keeping legacy `workflow_*` events. Server JSONL submit now waits for
   background workflow observation so workflow item completion is testable and
   visible to clients. Edit and write-file tool calls now also emit
   Codex-schema `fileChange` item lifecycles with change path/kind,
   terminal status, output/error details, and preserved legacy tool events.
   These paths have writer-level, provider-mock, and server-mode contract
   coverage. Server resume now reopens the same thread id, resume/fork
   preserve stored permission snapshots when materializing live threads, and
   explicit resume/fork permission overrides are parsed, persisted, and exposed
   through thread summaries. Active server turns now use persisted `turnId`
   handles for control and event payloads, hook subprocess waits are
   cancel-aware for active turn interrupts, bash/external process waits observe
   active turn cancellation, TUI-local streaming bash can be interrupted
   without waiting for the shell timeout, and `shell/start` now supports
   explicit terminal mode plus initial PTY sizing. Active MCP tool waits now
   also observe turn cancellation, MCP stdio/SSE transports expose configurable
   startup/tool request timeouts, SSE timeout behavior has transport coverage,
   and app-server MCP failure payloads surface timeout details in legacy
   `tool_completed` events as well as model-visible tool results. Timed-out or
   disconnected MCP transports now refresh for subsequent tool calls without
   replaying the failed call. Shell start now reports requested/effective
   terminal modes and can fall back to pipe mode when PTY is unavailable.
   Server `shell/list` exposes active shell snapshots for client recovery,
   including description metadata that can be updated through `shell/update`.
   Codex-style `command/exec` / `command/exec/terminate` now provide an
   app-server-compatible command execution entrypoint over the same runtime
   shell-session manager, including buffered `cwd`, env override/unset, `tty`
   field parsing, invalid option validation, buffered/streaming output caps,
   streamed stdout/stderr output, stdin write compatibility, and PTY initial
   size/resize support.
   Background `turn/start` output now reuses the same stateful server writer as
   submit-mode output, and MCP calls now stream first-class `mcpToolCall`
   items with server/tool/arguments/result/error fields instead of being
   flattened into generic command items. Persisted thread turn/item projections
   now also merge assistant MCP tool calls with their tool-result messages into
   first-class `mcpToolCall` history items. TOML external tools now stream
   first-class `dynamicToolCall` items with tool/arguments/content/error
   fields in realtime server output, including an end-to-end server-mode
   contract through a real descriptor, and persisted thread turn/item
   projections merge external tool calls with results into
   `dynamicToolCall` history items. Stored tool-result messages now retain
   status/error/exit-code/truncation metadata for app-server history projection
   without changing the model-visible tool-result text, so failed, denied,
   not-implemented, or truncated tool calls are restored as first-class items
   without collapsing explicit non-success statuses into completed. TUI
   plan/subagent/approval events, workflow terminal notifications, workflow
   task-list/progress refreshes, interactive approval decisions, and
   `request_user_input` continuations now also flow through runtime
   event/handler boundaries. Realtime server item streaming now uses a shared
   `RuntimeEventProjector` reducer for assistant message, plan, reasoning,
   tool, file-change, and workflow item lifecycles instead of keeping those
   runtime-event state machines inside `ServerRequestWriter`. Realtime and
   persisted tool item projections now
   share MCP tool name parsing, JSON argument parsing, MCP/dynamic started-item
   builders, MCP result shaping, camelCase tool error object helpers, exit-code
   normalization from runtime payloads or persisted result metadata, and
   completed-status checks. Realtime
   MCP/dynamic tool item helpers also use the shared status check before emitting success
   result/content items, and realtime file-change item helpers now share the
   same success-output / error-detail split. Non-success output is surfaced as
   error detail without also being rendered as successful content.
   Command-execution items
   intentionally keep aggregated output for failed commands as diagnostic
   context, matching Codex `CommandExecution.aggregatedOutput`, and realtime
   command items now expose that field instead of an `output` alias. Public
   realtime and persisted app-server tool/file/command item types now use
   Codex-style camelCase names (`commandExecution`, `fileChange`,
   `mcpToolCall`, and `dynamicToolCall`) while keeping runtime event payload
   metadata stable. Realtime `fileChange` items now use Codex-style
   `inProgress` status, string `changes[].diff`, and no Orca-specific top-level
   `tool` / `output` / `error` fields; legacy `tool_completed` still carries
   diagnostic details for compatibility. Persisted bash tool calls now project as
   `commandExecution` history items with aggregated output/truncation metadata
   instead of Orca's generic `tool_call` shape, and those persisted command
   items now use shared projection helpers that preserve history-only metadata
   such as cwd/process/source/action/duration placeholders while keeping failed
   command aggregated output empty. Remaining persisted
   non-MCP/non-bash tool calls now use `dynamicToolCall` so public thread items
   no longer expose the legacy `tool_call` item type. Active steer injection
   now has server-mode coverage for multi-text input, proving both the
   user-item stream and the running model context preserve the full steered
   content. Package-3-style `task_list` / `task_stop` model tools now expose
   the runtime task registry directly, Codex-style app-server `approvalPolicy`
   aliases now flow through resume/fork/turn-start permission overrides, and
  package-3-style `permissionUpdates` now give server clients an incremental
  permission reducer for session-scoped rule/mode changes and additional
  working-directory roots, and Codex-style `activePermissionProfile` now
  persists through thread metadata and projects through `thread/read` /
  `thread/list`. Codex-style `request_permissions` is now model-visible and
  runtime-special: it accepts `permissions.fileSystem.read/write`,
  `permissions.network.enabled`, and permission-profile-style
  `permissions.network.domains`; it grants `fileSystem.write` roots as a
  turn-scoped overlay for later bash execution, deliberately avoids persisting
  those temporary roots into thread metadata, and server-mode `permission/respond`
  now completes the request / resolved round trip before continuing the turn.
  `session`-scoped permission grants now persist approved filesystem roots and
  network domain entries into thread metadata and live server thread state so
  later turns inherit the directory scope and later `command/exec` calls inherit
  the managed proxy allowlist/denylist. Codex-style `fileSystem.entries` with
  `read`, `write`, and
  `readWrite` access now normalizes into Orca read/write roots in both protocol
  `permission/respond` handling and model-visible `request_permissions`
  arguments. `strictAutoReview` now propagates through the permission response,
  `permission_resolved` server event, and model-visible tool output, then
  forces later approval-requiring tools in the same turn back through Ask even
  when the active mode is otherwise full-auto. Thread-bound server
  `shell/start` sessions can now share the owning thread's task registry, so
  model-visible `task_stop` can request a stop for the same shell task and
  later `shell/read` / `shell/list` reaps the process through the runtime
  shell-session kill path instead of only marking registry state.
  Package-3-style permission update `destination` now survives protocol
  decoding for mode/rule/directory updates, directory updates preserve their
  source through thread metadata, add-directory updates follow path-keyed
  replacement semantics, and remove-directory updates use the destination when
  applying Orca's persisted source metadata. Codex-style special filesystem
  entries now accept `project_roots` / legacy `current_working_directory`
  labels, normalize them to `:workspace_roots` paths at the protocol boundary,
  and materialize session-scope grants against runtime workspace roots before
  persisting additional working-directory metadata. Explicit Codex-style
  `runtimeWorkspaceRoots` thread/turn overrides now decode through the
  app-server protocol, persist in thread metadata, project through
  `thread/read` / `thread/list`, and rebind later `:workspace_roots` grants.
  TUI session picker/profile metadata now surfaces additional directory grants
  with Codex-style `:workspace_roots` labels instead of only materialized paths.
  Next, keep reducing remaining TUI/runtime protocol drift.

**Out of scope for P1:**

- Full app-server transport.
- Remote UI clients.
- Tool-system rewrite.
- Background shell/PTTY sessions.

### P2: Tool System Convergence

**Release target:** v0.1.33

**Current status:** runtime tool invocation preparation, approval request
construction, and hook-modified request validation flow through
`orca_runtime::tool_invocation` for normal controller execution, readonly
batches, subagent batches, and TUI approval prompts. Runtime tool dispatch now
routes through `RuntimeToolRouter`, keeping `ToolExecutionActor` focused on
invocation prep, approval, hooks, and result finalization while the router owns
workflow, subagent, task, permission, workflow IPC, and normal-tool routing.
Normal tool execution now delegates through `RuntimeNormalToolExecutor`, which
owns the shell-session bash branch and the MCP/external/built-in fallback path
outside `lifecycle.rs`; router-driven normal tools now pass grouped
`RuntimeNormalToolInvocation` state into lifecycle actors instead of calling
the long roots/cancel method directly. Historical projected tool completion now
funnels through shared `tool_item_projection::complete_projected_tool_item`, so
`thread_store/projection.rs` no longer owns MCP, dynamic, commandExecution, or
fileChange completed-item reconstruction.

**Goal:** reduce the remaining divergence between built-in tools, MCP tools,
external tools, approvals, and future plugin-provided tools.

**Scope:**

1. Normalize tool invocation records across all tool sources. Done in v0.1.33
   for built-in, MCP, and TOML external tools.
2. Move approval classification and validation result shaping into a shared
   runtime path. Done in v0.1.33.
3. Split runtime tool dispatch behind a focused router boundary. Done in
   v0.1.104.
4. Prepare for long-running shell sessions, worktree automation, and async
   subagents without adding them in the same patch. The normal-tool executor
   boundary landed in v0.1.105, and the injectable fallback boundary landed in
   v0.1.106; tool-call argument progress landed in v0.1.107,
   lifecycle-to-normal-tool invocation now funnels through a single
   runtime_normal_tool helper in v0.1.108, router-to-lifecycle normal-tool
   routing now uses a grouped `RuntimeNormalToolInvocation` in v0.1.109, and
   historical projected tool completion uses the shared
   `complete_projected_tool_item` helper in v0.1.110, and
   `ToolExecutionActor::handle_approval` now takes a grouped
   `ToolApprovalGateContext` in v0.1.111, and normal tool-turn execution now
   takes a grouped `RuntimeNormalToolTurnContext` in v0.1.112, and
   provider-to-tool-turn dispatch now takes a grouped `RuntimeToolTurnsContext`
   in v0.1.113, and filesystem sandbox-denial recovery now shares diagnostics
   and permission-request retry behavior across command/exec and model-visible
   bash in v0.1.114, and bash shell-session invocation now takes a grouped
   `RuntimeBashInvocationContext` in v0.1.115, runtime turn-loop
   orchestration moved from `lifecycle.rs` into `runtime_turn_loop` in
   v0.1.116, and runtime turn-iteration orchestration moved into
   `runtime_turn_iteration` in v0.1.117, runtime turn-opening orchestration moved into
   `runtime_turn_opening` in v0.1.118, runtime turn-start orchestration
   moved into `runtime_turn_start` in v0.1.119, runtime model-route
   orchestration moved into `runtime_model_route` in v0.1.120, runtime
   steer application moved into `runtime_steer` in v0.1.121, and runtime
   conversation bootstrap moved into `runtime_conversation_bootstrap` in
   v0.1.122, and runtime turn setup moved into `runtime_turn_setup` in
   v0.1.123, runtime lifecycle state machine types moved into
   `runtime_lifecycle` in v0.1.124, `RuntimeToolActorContext` moved into
   `runtime_tool_actor` in v0.1.125, and server command/exec active process
   state moved into `server/command_exec_manager.rs` in v0.1.126, server
   active-turn lifecycle state moved into `server/active_turn_manager.rs` in
   v0.1.127, server pending-permission request state moved into
   `server/permission_manager.rs` in v0.1.128, server shell-session state
   moved into `server/shell_manager.rs` in v0.1.129, and async subagent worker
   launch/completion ownership moved into `subagent_async_worker.rs` in
   v0.1.130, and readonly tool-turn batch execution moved into
   `runtime_readonly_tool_turn.rs` with grouped readonly contexts in
   v0.1.131. The feature work remains open after v0.1.131 for deeper
   reducer-style runtime convergence.

### P3: Shell Timeout Hardening

**Release target:** v0.1.37

**Current status:** synchronous shell and external tool execution now honor the
configurable `[tools].shell_timeout_secs` setting, default to 120 seconds, and
normalize values into the 1..3600 second range.

**Goal:** keep shell execution bounded without widening the PTY/session model in
the same patch.

**Scope:**

1. Add a shared child-process wait helper with timeout handling.
2. Thread the configured timeout from `RunConfig` into `orca-tools`.
3. Preserve current `bash` and external tool semantics for non-timeout cases.

**Verification:** covered by the release patch checks and the Rust checks for
`orca-core`, `orca-tools`, and `orca-runtime`.

### P4: History Store Boundary

**Release target:** v0.1.38

**Current status:** history/session persistence now flows through a dedicated
`SessionStore` boundary, with runtime session/controller call sites aligned to
the same entry point.

**Goal:** separate session history persistence from orchestration so the
runtime can evolve toward a Codex-style thread store without keeping
everything in one history module.

**Scope:**

1. Add a dedicated history store object that owns session list/load/archive/
   delete/search/compress helpers.
2. Route runtime session/controller code through the store instead of direct
   helper calls.
3. Keep the existing JSONL format and user-facing history commands stable.

**Verification:** Rust tests for `orca-runtime`, plus release staging and
public publish verification.

### P5: Claude Code Workflow Parity

**Release target:** v0.1.42

**Current status:** generated workflow drafts, draft edit/save/cancel actions,
launch from draft, saved workflow slash invocation, argument schema validation,
pause/resume/clone/restart controls, and evidence-bound final reporting are
implemented.

**Goal:** make workflow a first-class reviewable artifact rather than only a
JavaScript runner.

**Scope:**

1. Generate workflow drafts from model tool calls and expose preview metadata.
2. Let users edit, save, cancel, run, clone, pause, resume, and restart
   workflow runs through durable state.
3. Treat saved project/user workflows as reusable command-like assets.
4. Ground final workflow status and reports in evidence, verifier contracts,
   and child tool events.

**Verification:** workflow CLI/runtime/script/tool/host/event contract tests,
release staging, site build/SEO checks, and public publish verification.

### P6: Process Timeout Cleanup

**Release target:** v0.1.42

**Current status:** shell, external tools, hook commands, sandbox helpers, and
verifier commands now share non-interactive child process setup and timeout
cleanup behavior.

**Goal:** prevent timed-out commands from leaving descendant processes behind
while keeping existing command surfaces stable.

**Scope:**

1. Add shared non-interactive process preparation.
2. Terminate the full child process tree on timeout.
3. Apply the timeout behavior consistently across bash, external tools, hooks,
   sandboxed commands, and verifier execution.

### Skills And Plugins

**Release target:** after the TUI runtime protocol adapter and shell
session/PTTY releases.

**Goal:** evolve the existing Markdown skill loading into a plugin-compatible
instruction and capability system.

**Scope:**

- Keep current `list_skills`, `read_skill`, and explicit `$skill` injection
  stable.
- Add richer skill manifests only after protocol/tool boundaries can carry
  plugin-provided capabilities cleanly.

---

## Priority Matrix

| Priority | Item | Why Now | Risk |
|----------|------|---------|------|
| P0 | Runtime-owned interactive session | Removes duplicated TUI/runtime state before deeper refactors | Medium |
| P0 | Published release verification | Prevents local tags from being mistaken for GitHub/npm releases | Low |
| P0 | Real API e2e release gate | Prevents local-only tests from being mistaken for provider/CLI/server readiness. Done in v0.1.34 | Low |
| P1 | Runtime protocol commands/events | Gives TUI/headless surfaces a shared contract | Medium |
| P1 | Runtime Task/Turn actor | Turn-start, model routing, pre/post model hooks, provider streaming, shell tool event shaping, pre/post tool hooks, non-interactive and interactive approval resolution, request-user-input handling, normal tool execution fallback, one tool actor context, runtime-special dispatch classification including `request_permissions`, workflow IPC execution, SubagentStatus execution, package-3-style `task_list` / `task_stop`, WorkflowDraft preview creation, workflow/subagent execution modules, active server-turn interrupt/resume, active steer item streaming/context injection including multi-text inputs, shell session/list/update controls, package-3-style incremental permission updates including additional directory roots, usage accounting, immutable turn-entry snapshotting through `RuntimeTurnConfig`, read-only service grouping through `RuntimeTurnDeps`, mutable runtime handle grouping through `RuntimeTurnState`, execution/lifecycle grouping through `RuntimeTurnExecution`, lifecycle-owned agent-loop result shape and terminal constructors, runtime-lifecycle-owned task/turn state machine types, runtime-conversation-bootstrap-owned step composing session-owned bootstrap and initial history recording, lifecycle-owned runtime turn setup step composing context config, tool approval policy, and provider config construction, lifecycle-owned runtime turn opening step composing compaction, turn start, turn-start result folding, model routing, and steer application, lifecycle-owned runtime provider cycle step composing provider turn, provider turn result folding, provider error handling/result folding, and provider response/result folding, lifecycle-owned runtime turn iteration step composing turn opening, provider cycle execution, and provider-cycle result folding, runtime-turn-loop-owned iteration retry/return folding plus grouped input/executor objects to shrink the agent-loop call surface, lifecycle-owned runtime compaction step handling budget warning hooks, pre/post compact hooks, prompt-too-long reactive compaction, and history persistence, lifecycle-owned turn-start step handling first-turn prompt selection, turn start errors, and started event emission, lifecycle-owned turn-start result folding into continuation or agent-loop results, lifecycle-owned model-route step handling model routing, cost model updates, per-turn provider config selection, and `model.routed` event emission, lifecycle-owned provider-error step handling reactive prompt-too-long retry state, compaction retry decisions, and provider error failures, lifecycle-owned provider-error result folding into turn continuation, loop continuation, or agent-loop results, lifecycle-owned provider-turn result step handling response/terminal folding and cancelled-error event suppression, lifecycle-owned provider-turn result folding from response/failure outcomes into response continuation or agent-loop results, lifecycle-owned provider-turn step handling pre/post model hooks, provider streaming deltas, provider replay updates, provider error handling including prompt-too-long retry decisions, cancellation checks, usage accounting, and usage history persistence, lifecycle-owned provider-response step handling assistant response recording, final-response memory extraction, provider turn terminal folding, provider tool request extraction, and tool-turn dispatch, lifecycle-owned provider-response result step folding continue/success/terminal outcomes into agent-loop results, runtime-steer-owned step draining multi-text inputs into conversation/history through grouped `RuntimeSteerInput`, tool-execution-owned approval policy construction, tool-execution-owned normal tool execution entrypoint, tool-invocation-owned provider tool schema override, tool-invocation-owned provider config construction, tool-invocation-owned provider tool request extraction, tool-invocation-owned child tool policy gate, tool-turn-owned cursor state, tool-turn outcome state, dispatch runner, normal/readonly tool-turn runners, read-only batch planning/execution/result recording, and normal result recording/status folding, subagent-execution-owned batch result recording and status folding, subagent-execution-owned batch tool-turn runner, memory-owned final response auto-memory extraction, session-owned system prompt construction for agent conversation bootstrap, session-owned conversation bootstrap, session-owned initial history recording, session-owned assistant response recording, session-owned tool result recording for model content plus history persistence, and session-owned plan-state recording for conversation plus history persistence are seeded; next continue shrinking lifecycle/tool-turn call surfaces against the Codex/package 3 priority list | Medium/High |
| P1 | Storage-neutral ThreadStore | Codex keeps thread persistence behind a dedicated `thread-store` crate; Orca now exposes a `thread_store` module that owns the storage-neutral `ThreadStore` trait, the `JsonlThreadStore` backend type, the `ThreadStore` implementation, live thread handle, session metadata, summary, transcript, and writer API/behavior, JSONL record shape and stored-message conversion, append writing/redaction/locking, JSONL record reading/rewrite helpers, session metadata/transcript read models, thread-record lookup/path helpers, session list/load/read-summary/search/mutation operations, storage-neutral thread projection/page/filter types, message/turn/item projections, next-turn id calculation, pagination, filters, and protocol-visible thread types, with `SessionStore` retained as a compatibility alias; live server message/turn/item/search projection, next persisted turn id calculation, protocol thread shapes, session production wiring, agent-loop resume wiring, pagination, thread-record materialization, session list/load materialization, session search, delete/archive/rename/compress session mutations, and metadata/read/list/search/turn/item trait paths now go through the boundary without bridging projection helpers back through `history`; next consider a storage backend split only after the runtime/session protocol boundaries settle | Medium |
| P1 | Permission profiles and directory scope | Codex app-server has named active permission profiles, request-permissions approval round trips, `turn` / `session` grant scopes, filesystem entry semantics, special workspace-root labels, runtime workspace-root rebinding, and strict auto-review, while package 3 tracks update destinations, sources, and additional directories; Orca now has thread-scoped mode/rule snapshots, active permission profile metadata, built-in `permissionProfile` execution semantics for `command/exec`, configured profile `extends` chains plus filesystem `read` roots enforced as strict read allow-lists for custom read-only command sandbox profiles, filesystem `write` / `read-write` roots, filesystem `deny` read/write overrides, startup-time expansion of bounded configured filesystem globs for read/write/read-write/deny access, configurable glob scan depth with inherited profile defaults and child-profile overrides, `[network].enabled` command sandbox resolution, command/exec domain allow/deny policy enforcement through a managed loopback HTTP proxy with Codex-style denylist/allowlist block reasons plus normalized blocked-host attribution, default local/private literal blocking unless explicitly allowlisted, and DNS-resolved non-public target blocking before connect, session-scoped `request_permissions` network domain grants persisted on server threads and inherited by later thread-bound `command/exec` calls, session-scoped network deny overlays that override permission-profile allows while session allows cannot bypass existing profile denies, configured Unix socket allowlists materialized into macOS command/exec Seatbelt rules while non-macOS builds accept the config without path-level enforcement, configured `:workspace_roots` / `:workspace_roots/<subpath>` materialization against thread runtime roots, TOML scoped filesystem table normalization, trailing `/**` subtree normalization, configured `:tmpdir` / `:slash_tmp` materialization for command sandbox roots, configured `:root` materialization, configured `:minimal` platform-default read-root materialization, inherited thread active profiles for thread-bound command execution, incremental rule updates with destination metadata, persisted additional working directories with source-aware replacement/removal, protocol projections, bash sandbox roots, turn-scoped `request_permissions` write-root overlays, server-mediated permission approvals, session-scope grant persistence, Codex-style `fileSystem.entries` normalization including `project_roots` / `:workspace_roots` special paths, explicit `runtimeWorkspaceRoots` thread/turn overrides, TUI session-picker labels for workspace-root-scoped directory grants, `strictAutoReview` propagation that re-prompts later same-turn tools, and thread-bound shell tasks that can be stopped through model-visible `task_stop`; next expand toward automatic ask-on-network-block flows while reducing remaining TUI/runtime protocol drift | Medium |
| P1 | TUI event and interaction adapters | Assistant deltas, usage, model routing notices, errors, session completion, tool requested/completed, plan updated, subagent started/completed, approval prompts/resolution notices, request-user-input prompts/results, verification started/completed notices, workflow terminal notifications, workflow lifecycle notices, and workflow task-list/progress refreshes now flow through runtime `EventFactory`/handler boundaries; TUI runtime approval and request-user-input handlers now live in a dedicated interaction adapter module; TUI tool approval request construction, preview generation, interactive wait handling, pending interaction storage, and id-carrying approval/user-input action routing are now delegated to runtime-backed adapter boundaries instead of `bridge`; next extract the remaining renderer-owned orchestration on the prioritized path | Medium |
| P2 | Unified tool invocation records | First-class MCP and external/dynamic app-server stream and history items are seeded, including failed/denied/not-implemented status plus error/exit-code/truncation restoration in history projections and legacy realtime `tool_completed` exit-code/result-kind preservation; MCP resource list/read/template tools now share the registry path, all-server resource/template discovery surfaces registry startup failures plus per-server list failures, and resource-capability caching avoids probing tools-only servers during all-server discovery; next reduce remaining TUI/runtime protocol drift | Medium |
| P2 | Shared approval/result shaping | Historical first-class tool item completion preserves explicit non-success statuses, realtime MCP/dynamic/file-change item helpers avoid success result/content payloads for non-completed statuses, realtime MCP/dynamic item errors now carry `exitCode` when tool completion reports one, command-execution items keep failed-command output as diagnostic aggregated output by contract, realtime/persisted tool item projections now share MCP parsing/started/result/error/status helpers, and `ToolExecutionActor::handle_approval` receives one grouped `ToolApprovalGateContext`; next continue moving approval/result shaping helpers behind focused runtime-owned context boundaries | Medium |
| Skills | Plugin-compatible skill manifests | Unlocks reusable instruction bundles after runtime contracts stabilize | Medium |
| Later | Cross-platform PTY depth | Shell session/list/update, command/exec compatibility, and shell output/exited stream notifications are seeded; deeper Windows PTY and richer terminal fidelity remain larger runtime work | High |
| Later | Remote compaction | High value, model-dependent behavior | Medium/High |
| Later | Worktree automation | High value, more filesystem/git risk | High |
| Later | Multi-format reading | Useful, but dependency and rendering heavy | Medium |

---

## Technical Decisions

| Decision | Current Choice | Notes |
|----------|----------------|-------|
| Tokenizer | `tiktoken-rs` BPE | Good enough for DeepSeek-compatible accounting until a DeepSeek-specific tokenizer is required |
| Config format | TOML | Keep user-facing config stable |
| Tool registry | `ToolSpec` capability registry | All built-ins, MCP, and external tools should flow through this path |
| Default truncation | Byte/token policy with compatibility defaults | Keep result budgets consistent as tool execution centralizes |
| MCP transport | stdio and SSE | Keep routing namespaced as `mcp__server__tool`; `startup_timeout_ms` and `tool_timeout_ms` bound startup/tool/resource requests, resource reads stay read-only, and failed transports refresh for later calls |
| Sandbox | macOS Seatbelt first, graceful fallback elsewhere | Add summaries before adding more platform sandboxes |
| History | JSONL transcript files | Runtime now owns interactive writer setup; introduce ThreadStore trait before considering SQLite metadata |
| Interactive session | `orca_runtime::session::InteractiveSession` plus `orca_runtime::lifecycle`/`RuntimeTaskActor` seed | TUI wrapper and shell/tool events now carry lifecycle metadata, but remain temporary while protocol/events and task/turn actor ownership are extracted |
| Skills | Markdown `SKILL.md` files | Keep instruction loading stable before adding plugin-provided capabilities |

---

## Completion Gates

Every patch phase must satisfy:

1. Version references are aligned across `Cargo.toml`, `Cargo.lock`, README, website metadata, and release notes.
2. Tests relevant to the touched surface pass fresh.
3. Release staging still validates with the current version.
4. `node scripts/release/real-api-e2e.mjs` passes with a real DeepSeek API key before tagging.
5. `git diff --check` is clean.
6. The release note describes user-visible changes and follow-up scope.
