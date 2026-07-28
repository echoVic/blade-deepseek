# Thin CLI Library Boundaries Design

Date: 2026-07-28
Status: Approved direction
Selected approach: typed requests with library-owned command facades

## Objective

Reduce `src/cli.rs` from a 3,093-line application implementation to a thin
binary adapter that owns only:

- Clap argument declarations and parsing;
- conversion from parsed arguments into typed library requests;
- dispatch to public library entry points; and
- the final process exit code.

Application behavior must move to the crate that owns the capability. In
particular, workflow launch, worker handoff, persistence, inspection, and
control belong to `orca-runtime`; update discovery and installation-method
execution belong to `orca-runtime`; and terminal rendering for the update
prompt belongs to `orca-tui`. This is an ownership refactor, not a mechanical
move into another binary-only source file.

The command surface, output formats, exit codes, environment precedence,
history behavior, workflow persistence, worker compatibility, and credential
handling must remain compatible.

## Current State

`src/cli.rs` currently contains 3,093 lines with these approximate regions:

| Responsibility | Lines | Current owner |
| --- | ---: | --- |
| Clap arguments and command models | 620 | root binary |
| general dispatch, config, exec, and history | 415 | root binary |
| workflow commands, worker launch, persistence, and control | 1,118 | root binary |
| interactive launch and update flow | 348 | root binary |
| server and ACP launch | 142 | root binary |
| unit tests for extracted behavior | 450 | root binary |

The root binary also declares many modules that merely re-export library
crates. This lets application logic continue referring to `crate::runtime`,
`crate::config`, and similar compatibility shims, obscuring the actual crate
boundary.

The repository already contains two useful precedents:

1. `orca-runtime::subagent_async_worker` owns the async worker implementation
   and leaves the CLI with a small forwarding call.
2. `orca-runtime::update_check` already owns network discovery and prompt
   suppression, but installation detection, command construction, execution,
   and prompt coordination remain in `src/cli.rs`.

## Considered Approaches

### 1. Typed library requests and capability-owned facades (selected)

Clap models remain private to the binary. Each command converts its parsed
arguments into a request type defined by a library crate and calls one public
entry point. The library owns validation, configuration resolution, state,
process management, and result construction. Presentation-specific terminal
code lives in `orca-tui`.

This establishes enforceable ownership and makes the same operations reusable
without invoking the executable or depending on Clap. The cost is introducing
explicit request/result types and moving tests to their real owners.

### 2. Move functions into root `src/cli/*` modules

This would quickly reduce the line count of `src/cli.rs`, but the binary would
still own workflow state, update installation, config assembly, and worker
processes. It improves navigation without fixing reuse or dependency
direction, so it is rejected.

### 3. Create a generic `orca-cli` library containing all command logic

This would make the binary small, but would create a second application layer
that owns capabilities already modeled by `orca-core`, `orca-runtime`, and
`orca-tui`. It would mostly relocate the monolith and encourage cross-domain
coupling, so it is rejected.

## Ownership Model

### Root binary

The root package remains the executable package. After the refactor it owns:

- `Cli`, subcommand, and argument structs with Clap derives;
- lossless conversion of those structs into library request enums/structs;
- one dispatch match; and
- `std::process::exit`.

It must not own:

- layered config loading or environment precedence;
- `RunConfig` construction;
- history storage operations or transcript rendering;
- workflow state discovery, launch records, control, or worker processes;
- API-key transport to worker processes;
- update network checks, cache decisions, installation detection, or installer
  process execution;
- raw terminal mode or update prompt rendering;
- server, ACP, exec, or TUI runtime setup; or
- business tests for any of the above.

The re-export-only root modules are removed when no binary code needs them.
`src/main.rs` should declare only the thin CLI module and call it. Root package
dependencies are reduced to crates and libraries actually required by parsing
and forwarding.

### `orca-core`

`orca-core` owns transport-neutral launch inputs and configuration concepts.
It exposes request fields using existing shared types such as `ProviderKind`,
`ApprovalMode`, `ReasoningEffort`, `HistoryMode`, and `RunConfig`. It continues
to own layered file configuration and folder trust persistence.

Environment parsing that is purely configuration precedence moves beside the
layering implementation instead of remaining in the binary. The API accepts
an explicit override set and returns either an effective configuration or a
typed/displayable error. This keeps environment/file/CLI precedence usable by
all launch surfaces.

`orca-core` does not acquire dependencies on `orca-runtime`, `orca-tui`,
process execution, or terminal rendering.

### `orca-runtime`

`orca-runtime` owns non-visual command use cases through focused modules rather
than one replacement monolith:

- `command::exec` validates exec-only flags, resolves stdin through an injected
  input boundary, builds the effective `RunConfig`, and invokes the controller;
- `command::history` executes list/show/archive/delete/rename/search/compress
  operations and writes their existing textual representation through an
  injected writer;
- `command::trust` resolves and mutates folder trust and writes existing
  diagnostics through injected writers;
- `command::launch` builds interactive, server, ACP, and hidden-worker runtime
  configurations and invokes runtime-owned surfaces where no UI crate is
  required; and
- `workflow::command` owns workflow run/list/show/source/stop/pause/resume/clone/
  restart behavior, durable launch-record migration, workflow state discovery,
  worker spawning, bounded credential handoff, and worker execution.

The command facade request types do not depend on Clap. Output is written via
`std::io::Write` or returned as typed data so tests can invoke the library
directly. Public convenience entry points may bind standard input/output for
the executable, but the underlying behavior remains injectable and testable.

The hidden workflow worker protocol remains compatible: the same subcommand
and flags launch it, credentials still travel through bounded stdin rather
than argv or persisted launch records, and the first stdout line remains the
launch result. Only its implementation owner changes.

### `orca-runtime::update_check`

The existing module expands from release discovery/cache into the complete
non-visual update service. It owns:

- development-build suppression;
- `UpdateInfo` and preflight decisions;
- npm-managed versus standalone installation detection;
- safe standalone installer command construction;
- human-readable command description data;
- process execution and exit interpretation; and
- skip-until-next-version persistence.

Command construction remains separately testable without starting a process.
The runtime API returns typed progress/outcome information; it does not enable
raw terminal mode or read keyboard events.

### `orca-tui`

`orca-tui` owns interactive terminal presentation. A focused update preflight
module uses `orca-runtime::update_check`, renders the selector, reads keys,
restores terminal state through RAII, and returns a typed decision/outcome.
The default interactive entry point coordinates update preflight and normal
TUI startup so the binary needs only one forwarding call.

This avoids a dependency cycle: `orca-tui` may depend on `orca-runtime`, while
`orca-runtime` never depends on `orca-tui`.

## Request and Dispatch Flow

The top-level flow is:

```text
argv -> Clap structs in src/cli.rs
     -> typed request conversion
     -> capability-owned library entry point
     -> typed result / writer output / exit code
     -> process exit
```

Examples:

```text
orca workflow run ...
  -> WorkflowCommandRequest::Run
  -> orca_runtime::workflow::command::run(...)

orca exec ...
  -> ExecCommandRequest
  -> orca_runtime::command::exec::run(...)

orca
  -> InteractiveLaunchRequest
  -> orca_runtime prepares RunConfig
  -> orca_tui performs update preflight and runs TUI
```

Library requests contain owned values so the CLI can destructure once without
maintaining parallel references into Clap structs. Library errors preserve the
existing user-facing `orca:` diagnostics and exit-code semantics at the
facade boundary. Protocol modes never emit update text on stdout.

## Compatibility Requirements

The refactor must preserve all current observable contracts:

1. `orca exec` supports argument prompts, `-`, omitted prompt with piped stdin,
   appended stdin context, history options, output formats, verifier, budget,
   provider, model, API key, base URL, cwd, and config precedence.
2. `orca history` preserves list/show/archive/delete/rename/search/compress
   output and errors.
3. Every workflow subcommand preserves JSON shapes, filtering, ordering,
   asynchronous return behavior, status transitions, launch-record migration,
   and hidden worker behavior.
4. Workflow credentials never appear in argv or launch records, remain bounded
   to 64 KiB, and use the resolved effective key rather than only the CLI flag.
5. Default TUI startup preserves resume/fork/continue/session-picker semantics,
   prompt handling, update suppression for development builds, prompt choices,
   dismissal, upgrade execution, and quit behavior.
6. `--mode=server` and `--mode=acp` preserve exclusivity validation, config
   construction, protocol-clean stdout, and exit codes.
7. Folder trust and async subagent worker commands preserve their public and
   hidden CLI contracts.
8. No migration changes the persisted history, task, workflow, trust, or update
   file formats.

## Testing Strategy

The implementation follows red-green-refactor at each boundary. Existing
end-to-end tests are retained as behavior oracles, and new library tests are
written before moving each behavior.

### Structural tests

A new architecture contract reads `src/cli.rs` and asserts that it:

- stays below 1,000 lines, with a target near the current 620-line Clap model;
- contains Clap parsing and request conversion but no filesystem persistence,
  process spawning, raw terminal handling, workflow stores/runners, update
  network checks, or `RunConfig` assembly;
- has no `#[cfg(test)]` module for extracted application behavior; and
- calls the public library facades for every top-level command.

The same contract verifies `src/main.rs` no longer declares re-export shim
modules and checks that workflow and update implementations exist under their
owning library modules. This prevents a line-count-only refactor.

### Library tests

- effective config/environment precedence and invalid mode/reasoning values;
- stdin prompt resolution using injected readers and terminal-state flags;
- history and trust result formatting through in-memory writers;
- workflow input resolution, listing/filtering, launch-record migration, worker
  command construction, bounded credential transport, and restart/control;
- update action detection, installer command construction, preflight decisions,
  dismissal, and process-outcome mapping; and
- update selector navigation/rendering/terminal cleanup in `orca-tui`.

### Integration tests

Existing command-level contracts remain the final parity layer, including:

- `tests/exec_jsonl.rs`;
- `tests/history_contract.rs`;
- `tests/workflow_cli_contract.rs`;
- `tests/session_server_contract.rs`;
- `tests/subagent_contract.rs`; and
- `tests/tui_pty_contract.rs`.

Targeted tests run serially where the existing suite shares process-global
environment or persistent state. The clean `origin/main` baseline currently
passes the root CLI tests and workflow integration tests, while the highly
parallel `orca-runtime --lib` run has 29 pre-existing shared-state failures
(1,003 of 1,032 pass). Completion therefore requires both:

- all refactor-specific and command contract tests passing; and
- no new failures relative to a fresh, identically configured baseline.

The final verification also runs formatting, workspace checking/building, and
the broadest feasible workspace test command with test debug information and
incremental compilation disabled to fit the machine's current disk budget.

## Implementation Sequence

The refactor proceeds in behavior-preserving slices:

1. Add the architecture contract and typed configuration/launch request
   foundations.
2. Move update service behavior into `orca-runtime` and terminal prompt behavior
   into `orca-tui`; replace CLI code with forwarding.
3. Move workflow command, process-worker, durable record, and control behavior
   into `orca-runtime::workflow::command`; replace CLI code with conversion and
   forwarding.
4. Move exec and shared launch/config assembly into runtime/core boundaries.
5. Move history and trust command use cases into library facades.
6. Replace server, ACP, subagent worker, and interactive setup with typed launch
   calls.
7. Remove root re-export shims and unused root dependencies.
8. Move unit tests to owning crates, tighten the architecture contract, format,
   and run the full parity matrix.

Each slice begins with a failing library or architecture test, preserves the
existing integration tests, and is committed separately.

## Acceptance Criteria

The work is complete only when all of the following are true:

- the requested isolated worktree and dedicated branch contain the changes;
- `src/cli.rs` is below 1,000 lines and contains only Clap models, conversion,
  dispatch, and trivial exit-code forwarding;
- workflow startup/control/worker/persistence logic is owned by
  `orca-runtime`;
- update discovery, preflight policy, installation selection, and execution are
  owned by `orca-runtime`, while terminal interaction is owned by `orca-tui`;
- other command business logic no longer remains in `src/cli.rs`;
- root re-export shims and dependencies that existed only to support the fat
  binary are removed;
- architecture tests prove ownership instead of checking line count alone;
- command and protocol compatibility tests pass;
- credential and persisted-format safety contracts pass;
- formatting and workspace compilation pass; and
- the final prompt-to-artifact audit maps every requirement above to current
  files and fresh command output with no uncovered requirement.
