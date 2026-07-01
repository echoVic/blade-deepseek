# Orca Production Roadmap

> Goal: evolve Orca into a production-grade DeepSeek-native agent runtime.
> Reference implementations: Codex CLI, Claude Code, and the current Orca codebase.

Last updated: 2026-07-01
Current baseline: v0.1.84 focused protocol facade with separate command_exec, events, permissions, shell, thread, turn, and wire modules, focused ThreadStore storage facade with separate types, local JSONL, writer, projection, pagination, and live-thread modules, managed permission-profile network domain enforcement for command/exec through a loopback HTTP proxy with Codex-style `blocked-by-denylist` / `blocked-by-allowlist` diagnostics and default local/private target blocking unless explicitly allowlisted, DNS-resolved non-public target blocking before connect, macOS command/exec Unix socket allowlists for configured permission profiles without broad network access, configurable permission-profile filesystem glob scan depth with inherited profile defaults and child-profile overrides, bounded permission-profile filesystem glob expansion for read/write/read-write command sandbox roots, dedicated runtime compaction module for prompt-budget hooks, summary persistence, and prompt-too-long recovery, inline TUI scrollback/viewport split for native terminal history plus live bottom-pane rendering, dedicated runtime tool-turn module for cursoring, batching, execution, and result folding, TUI tool approval gate owned by the runtime interaction adapter, dedicated TUI runtime interaction adapter module, dedicated TUI runtime event projection module, runtime-owned `RuntimeThread` boundary that groups `InteractiveSession` and `RuntimeSessionLifecycle` and now backs server-mode `ServerThread`, headless controller agent state, and the TUI conversation-session wrapper, site prerender server entry for crawler-visible HTML, shared tool item started/completed/status/error projection helpers, shared persisted commandExecution and fileChange history projection helpers, shared agent-message/plan/reasoning/commandExecution/fileChange/workflow lifecycle item builders for realtime server streams, tag release-gate Rust tests serialized for server-heavy contracts, stdio MCP fixture hardening for Linux release runners, active-writer JSONL polling hardening for server background-turn tests, realtime tool item error exit-code projection, MCP resource capability caching, MCP resource/template discovery with registry-level startup errors in all-server listings, MCP resource listing error aggregation, MCP resource list/read tools, server JSONL test harness hardening, structured hook action validation, tool argument schema composition validation, fuzzy model-facing file discovery, runtime-owned agent turn loop orchestration, workflow parity loop, process timeout hardening, runtime/TUI task-turn lifecycle seed, and configured permission-profile write/deny/network/special-root sandboxing

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
| Runtime lifecycle | Headless, server-mode, and TUI agent runs now seed an agent task lifecycle through a runtime turn runner; `RuntimeThread` groups runtime-owned interactive session state with lifecycle state, and server-mode `ServerThread`, the headless controller, and the TUI conversation-session wrapper now keep long-lived agent state behind `RuntimeThread` instead of directly assembling session/lifecycle/executor pieces; workflow runs, sync/async subagent boundaries, workflow child agents, and shell tool calls also carry task metadata; tool approval/hooks/normal fallback now share a runtime tool actor context | Codex `Session -> Task -> Turn` model | Seeded; deeper TUI loop delegation and runtime-special dispatch still open |
| TUI | Markdown-ish rendering, themes, Vim mode, diff preview, slash commands, workflow panel, elapsed timers, and clearer approval dialogs | Codex/Claude richer terminal UX | Partial |
| History | JSONL transcripts, resume/fork/search/archive/compress with a dedicated `SessionStore` boundary | Codex thread store with queryable metadata | Partial |
| Release | GitHub release + npm alias distribution scripts, retrying post-publish GitHub/npm/npm-exec verification, and a reusable real API e2e release gate | Codex npm/native release model | Implemented |
| Skills | Markdown skill discovery, `list_skills`/`read_skill`, and explicit `$skill` prompt injection | Codex skills and plugin-provided skill bundles | Partial |

---

## Patch Release Plan

The next work should land as independent patch releases. Each release must be
verified before the next phase starts.

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
   deltas with `command/exec/write` stdin support, and `command/exec` TTY
   initial size/resize support.

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
   and TTY initial size/resize request fields. Server shell reads now emit
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
   automatically replaying the failed call. Next, expand cross-platform PTY
   support.
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
   still rejected before scanning. Interactive network ask flows remain a later
   expansion.
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
   event/handler boundaries. Realtime and persisted tool item projections now
   share MCP tool name parsing, JSON argument parsing, MCP/dynamic started-item
   builders, MCP result shaping, camelCase tool error object helpers, exit-code
   normalization from runtime payloads or persisted result metadata, and completed-status checks. Realtime
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
  runtime-special: it accepts `permissions.fileSystem.read/write` plus
  `permissions.network.enabled`, grants `fileSystem.write` roots as a
  turn-scoped overlay for later bash execution, deliberately avoids persisting
  those temporary roots into thread metadata, and server-mode `permission/respond`
  now completes the request / resolved round trip before continuing the turn.
  `session`-scoped permission grants now persist approved filesystem roots into
  thread metadata and live server thread state so later turns inherit the
  directory scope. Codex-style `fileSystem.entries` with `read`, `write`, and
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
construction, and hook-modified request validation now flow through
`orca_runtime::tool_invocation` for normal controller execution, readonly
batches, subagent batches, and TUI approval prompts.

**Goal:** reduce the remaining divergence between built-in tools, MCP tools,
external tools, approvals, and future plugin-provided tools.

**Scope:**

1. Normalize tool invocation records across all tool sources. Done in v0.1.33
   for built-in, MCP, and TOML external tools.
2. Move approval classification and validation result shaping into a shared
   runtime path. Done in v0.1.33.
3. Prepare for long-running shell sessions, worktree automation, and async
   subagents without adding them in the same patch. Still open after v0.1.33.

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
| P1 | Runtime Task/Turn actor | Turn-start, model routing, pre/post model hooks, provider streaming, shell tool event shaping, pre/post tool hooks, non-interactive and interactive approval resolution, request-user-input handling, normal tool execution fallback, one tool actor context, runtime-special dispatch classification including `request_permissions`, workflow IPC execution, SubagentStatus execution, package-3-style `task_list` / `task_stop`, WorkflowDraft preview creation, workflow/subagent execution modules, active server-turn interrupt/resume, active steer item streaming/context injection including multi-text inputs, shell session/list/update controls, package-3-style incremental permission updates including additional directory roots, usage accounting, immutable turn-entry snapshotting through `RuntimeTurnConfig`, read-only service grouping through `RuntimeTurnDeps`, mutable runtime handle grouping through `RuntimeTurnState`, execution/lifecycle grouping through `RuntimeTurnExecution`, lifecycle-owned agent-loop result shape and terminal constructors, lifecycle-owned runtime conversation bootstrap step composing session-owned bootstrap and initial history recording, lifecycle-owned runtime turn setup step composing context config, tool approval policy, and provider config construction, lifecycle-owned runtime turn opening step composing compaction, turn start, turn-start result folding, model routing, and steer application, lifecycle-owned runtime provider cycle step composing provider turn, provider turn result folding, provider error handling/result folding, and provider response/result folding, lifecycle-owned runtime turn iteration step composing turn opening, provider cycle execution, and provider-cycle result folding, lifecycle-owned runtime turn loop step owning iteration retry/return folding, lifecycle-owned runtime turn loop input/executor grouping to shrink the agent-loop call surface, lifecycle-owned runtime compaction step handling budget warning hooks, pre/post compact hooks, prompt-too-long reactive compaction, and history persistence, lifecycle-owned turn-start step handling first-turn prompt selection, turn start errors, and started event emission, lifecycle-owned turn-start result folding into continuation or agent-loop results, lifecycle-owned model-route step handling model routing, cost model updates, per-turn provider config selection, and `model.routed` event emission, lifecycle-owned provider-error step handling reactive prompt-too-long retry state, compaction retry decisions, and provider error failures, lifecycle-owned provider-error result folding into turn continuation, loop continuation, or agent-loop results, lifecycle-owned provider-turn result step handling response/terminal folding and cancelled-error event suppression, lifecycle-owned provider-turn result folding from response/failure outcomes into response continuation or agent-loop results, lifecycle-owned provider-turn step handling pre/post model hooks, provider streaming deltas, provider replay updates, provider error handling including prompt-too-long retry decisions, cancellation checks, usage accounting, and usage history persistence, lifecycle-owned provider-response step handling assistant response recording, final-response memory extraction, provider turn terminal folding, provider tool request extraction, and tool-turn dispatch, lifecycle-owned provider-response result step folding continue/success/terminal outcomes into agent-loop results, lifecycle-owned steer step draining multi-text inputs into conversation/history, tool-execution-owned approval policy construction, tool-execution-owned normal tool execution entrypoint, tool-invocation-owned provider tool schema override, tool-invocation-owned provider config construction, tool-invocation-owned provider tool request extraction, tool-invocation-owned child tool policy gate, tool-turn-owned cursor state, tool-turn outcome state, dispatch runner, normal/readonly tool-turn runners, read-only batch planning/execution/result recording, and normal result recording/status folding, subagent-execution-owned batch result recording and status folding, subagent-execution-owned batch tool-turn runner, memory-owned final response auto-memory extraction, session-owned system prompt construction for agent conversation bootstrap, session-owned conversation bootstrap, session-owned initial history recording, session-owned assistant response recording, session-owned tool result recording for model content plus history persistence, and session-owned plan-state recording for conversation plus history persistence are seeded; next continue shrinking lifecycle/tool-turn call surfaces against the Codex/package 3 priority list | Medium/High |
| P1 | Storage-neutral ThreadStore | Codex keeps thread persistence behind a dedicated `thread-store` crate; Orca now exposes a `thread_store` module that owns the storage-neutral `ThreadStore` trait, the `JsonlThreadStore` backend type, the `ThreadStore` implementation, live thread handle, session metadata, summary, transcript, and writer API/behavior, JSONL record shape and stored-message conversion, append writing/redaction/locking, JSONL record reading/rewrite helpers, session metadata/transcript read models, thread-record lookup/path helpers, session list/load/read-summary/search/mutation operations, storage-neutral thread projection/page/filter types, message/turn/item projections, next-turn id calculation, pagination, filters, and protocol-visible thread types, with `SessionStore` retained as a compatibility alias; live server message/turn/item/search projection, next persisted turn id calculation, protocol thread shapes, session production wiring, agent-loop resume wiring, pagination, thread-record materialization, session list/load materialization, session search, delete/archive/rename/compress session mutations, and metadata/read/list/search/turn/item trait paths now go through the boundary without bridging projection helpers back through `history`; next consider a storage backend split only after the runtime/session protocol boundaries settle | Medium |
| P1 | Permission profiles and directory scope | Codex app-server has named active permission profiles, request-permissions approval round trips, `turn` / `session` grant scopes, filesystem entry semantics, special workspace-root labels, runtime workspace-root rebinding, and strict auto-review, while package 3 tracks update destinations, sources, and additional directories; Orca now has thread-scoped mode/rule snapshots, active permission profile metadata, built-in `permissionProfile` execution semantics for `command/exec`, configured profile `extends` chains plus filesystem `read` roots enforced as strict read allow-lists for custom read-only command sandbox profiles, filesystem `write` / `read-write` roots, filesystem `deny` read/write overrides, startup-time expansion of bounded configured filesystem globs for read/write/read-write/deny access, configurable glob scan depth with inherited profile defaults and child-profile overrides, `[network].enabled` command sandbox resolution, command/exec domain allow/deny policy enforcement through a managed loopback HTTP proxy with Codex-style denylist/allowlist block reasons plus default local/private literal blocking unless explicitly allowlisted and DNS-resolved non-public target blocking before connect, configured Unix socket allowlists materialized into macOS command/exec Seatbelt rules while non-macOS builds accept the config without path-level enforcement, configured `:workspace_roots` / `:workspace_roots/<subpath>` materialization against thread runtime roots, TOML scoped filesystem table normalization, trailing `/**` subtree normalization, configured `:tmpdir` / `:slash_tmp` materialization for command sandbox roots, configured `:root` materialization, configured `:minimal` platform-default read-root materialization, inherited thread active profiles for thread-bound command execution, incremental rule updates with destination metadata, persisted additional working directories with source-aware replacement/removal, protocol projections, bash sandbox roots, turn-scoped `request_permissions` write-root overlays, server-mediated permission approvals, session-scope grant persistence, Codex-style `fileSystem.entries` normalization including `project_roots` / `:workspace_roots` special paths, explicit `runtimeWorkspaceRoots` thread/turn overrides, TUI session-picker labels for workspace-root-scoped directory grants, `strictAutoReview` propagation that re-prompts later same-turn tools, and thread-bound shell tasks that can be stopped through model-visible `task_stop`; next expand toward interactive network ask flows while reducing remaining TUI/runtime protocol drift | Medium |
| P1 | TUI event and interaction adapters | Assistant deltas, usage, model routing notices, errors, session completion, tool requested/completed, plan updated, subagent started/completed, approval prompts/resolution notices, request-user-input prompts/results, verification started/completed notices, workflow terminal notifications, workflow lifecycle notices, and workflow task-list/progress refreshes now flow through runtime `EventFactory`/handler boundaries; TUI runtime approval and request-user-input handlers now live in a dedicated interaction adapter module; TUI tool approval request construction, preview generation, and interactive wait handling are now delegated to that adapter instead of `bridge`; next extract the remaining renderer-owned orchestration on the prioritized path | Medium |
| P2 | Unified tool invocation records | First-class MCP and external/dynamic app-server stream and history items are seeded, including failed/denied/not-implemented status plus error/exit-code/truncation restoration in history projections and legacy realtime `tool_completed` exit-code/result-kind preservation; MCP resource list/read/template tools now share the registry path, all-server resource/template discovery surfaces registry startup failures plus per-server list failures, and resource-capability caching avoids probing tools-only servers during all-server discovery; next reduce remaining TUI/runtime protocol drift | Medium |
| P2 | Shared approval/result shaping | Historical first-class tool item completion preserves explicit non-success statuses, realtime MCP/dynamic/file-change item helpers avoid success result/content payloads for non-completed statuses, realtime MCP/dynamic item errors now carry `exitCode` when tool completion reports one, command-execution items keep failed-command output as diagnostic aggregated output by contract, and realtime/persisted tool item projections now share MCP parsing/started/result/error/status helpers; next consolidate completed item construction to reduce remaining schema drift | Medium |
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
