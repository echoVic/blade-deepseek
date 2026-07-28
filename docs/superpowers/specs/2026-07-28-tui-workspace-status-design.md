# TUI Workspace Status Footer Design

## Goal

Add persistent workspace identity to the fullscreen TUI status line:

- show the current working directory in a compact form;
- show the current Git branch when the workspace is a Git checkout;
- show a short commit identity when the checkout is detached;
- preserve the existing model, approval-mode, context, usage, and shortcuts
  status information;
- perform no filesystem or process work from rendering.

This is the `cwd` plus Git-branch footer item from the P2 TUI roadmap. It does
not include Git dirty state, ahead/behind state, remote names, branch switching,
periodic refresh, configurable keybindings, Vim expansion, `/doctor`, FPS
telemetry, or onboarding changes.

## Current Behavior

`AppState` already stores `cwd`, initialized from the captured startup
workspace after replacing the home-directory prefix with `~`. The value appears
in the welcome screen but not in the persistent footer.

`ui::status_line` currently renders:

1. model and reasoning effort;
2. approval mode;
3. context percentage when known;
4. token and cost information when nonzero;
5. the `F1 shortcuts` hint when space permits.

The status renderer is synchronous and runs on every dirty frame. Git discovery
must therefore happen before the frame loop and be stored as immutable display
state.

## Chosen Approach

Capture one bounded workspace-status snapshot during TUI startup.

This is preferred over polling because it:

- keeps rendering and animation free of subprocesses;
- adds no timer, worker, event type, or shutdown path;
- matches the roadmap's low-cost scope;
- is sufficient to distinguish concurrently opened sessions.

If the user changes branches externally, the footer updates on the next TUI
start. No manual refresh command is added.

## Workspace Snapshot

Create `crates/orca-tui/src/workspace_status.rs`.

The module owns:

```rust
pub(crate) struct WorkspaceStatus {
    pub(crate) cwd: String,
    pub(crate) git: Option<GitIdentity>,
}

pub(crate) enum GitIdentity {
    Branch(String),
    Detached(String),
}
```

The public module boundary is:

```rust
pub(crate) fn snapshot(workspace: &Path) -> WorkspaceStatus;

impl GitIdentity {
    pub(crate) fn label(&self) -> String;
}

pub(crate) fn compact_cwd(cwd: &str, max_width: usize) -> String;
```

`snapshot` always returns a cwd value. Git discovery is best-effort and never
prevents TUI startup.

Home shortening is internally pure:

```rust
fn display_cwd(workspace: &Path, home: Option<&Path>) -> String;
```

Production resolves `dirs::home_dir()` once at the snapshot boundary. Tests
pass explicit paths and never mutate `HOME`.

## Cwd Display

The snapshot first converts the captured startup path to a user-facing value:

- an exact `$HOME` prefix becomes `~`;
- other paths remain absolute;
- no canonicalization is performed for display;
- control characters and line breaks are replaced with spaces.

The welcome screen continues to use this home-shortened cwd.

For the footer, `compact_cwd(max_width)` chooses the first representation that
fits by display width:

1. the complete home-shortened cwd;
2. `prefix/…/basename`, where `prefix` is `~` for a home path and `/` for an
   absolute non-home path;
3. the basename;
4. grapheme-safe ellipsis truncation of the basename.

Examples:

```text
~/Documents/GitHub/blade-deepseek
~/…/blade-deepseek
blade-deepseek
blade-deep…
```

The path separator and ellipsis are included in width calculations. Root paths
and paths without a normal basename fall back to their complete display value
and the shared grapheme-safe truncation helper.

## Git Identity Discovery

Git discovery uses `std::process::Command` directly, with the existing
`orca_tools::process` noninteractive and bounded-output helpers.

The first command is:

```text
git symbolic-ref --quiet --short HEAD
```

On success with a nonempty single-line value, the result is
`GitIdentity::Branch`.

When that command exits unsuccessfully, the fallback is:

```text
git rev-parse --short=8 HEAD
```

On success with a nonempty single-line value, the result is
`GitIdentity::Detached`.

Both commands:

- run with `current_dir(workspace)`;
- have a 500 ms timeout;
- retain at most 4 KiB across stdout and stderr;
- use no shell;
- inherit no stdin;
- trim surrounding whitespace;
- reject empty or multi-line/control-character output.

Spawn failure, timeout, non-Git directories, invalid output, and command
failure all produce `git: None`. An unborn repository may show the symbolic
branch returned by `symbolic-ref`, but it never fabricates a detached commit.
No error enters the transcript, status line, logs, or terminal notification
path.

Internally, discovery is split into:

```rust
struct GitCommandResult {
    success: bool,
    timed_out: bool,
    output_omitted: bool,
    stdout: String,
}

fn discover_git_identity(
    workspace: &Path,
    run: impl FnMut(&Path, &[&str]) -> io::Result<GitCommandResult>,
) -> Option<GitIdentity>;
```

Production passes the bounded process adapter. Unit tests pass a deterministic
closure to prove command order, timeout/spawn degradation, malformed output,
and oversized-output rejection without replacing the real `git` binary or
mutating process-global `PATH`.

The rendered labels are:

```text
git:feature/tui-syntax-highlighting
git:@5bbb60a
```

The detached marker prevents a commit hash from looking like a branch name.

## State Ownership

`AppState` keeps the existing `cwd: String` field and adds:

```rust
pub(crate) workspace_git: Option<GitIdentity>,
```

`AppState::new` remains convenient for existing tests and initializes
`workspace_git` to `None`. Startup captures `WorkspaceStatus`, passes its cwd
to `AppState::new`, then installs the optional Git identity before entering the
frame loop.

The state is immutable for the lifetime of the TUI session. Conversation
replacement, resume, fork, session picker transitions, and runtime events do
not alter it because they do not change the process workspace.

## Footer Layout

The footer retains one physical row. It does not change composer, queue,
search, transcript, or popup geometry.

Required cells, in order:

1. model and reasoning effort;
2. approval mode.

Optional cells, in priority order:

1. context percentage;
2. cwd;
3. Git identity;
4. tokens and cost;
5. `F1 shortcuts`.

The model may continue to truncate to preserve the approval mode. Optional
cells are admitted only when their complete rendered width fits.

Workspace fitting is adaptive:

1. reserve model, approval mode, and context;
2. try complete cwd plus Git identity;
3. compact cwd while retaining Git identity;
4. omit Git identity and retry cwd;
5. omit cwd if even its minimum representation cannot fit;
6. admit usage and shortcuts only from the remaining width.

This gives workspace identity priority over usage and shortcut hints without
breaking the existing invariant that approval mode and known context remain
visible in narrow terminals.

Both workspace cells use `theme.muted`. No new theme tokens are introduced.

## Rendering Contract

`ui::status_line` receives only `AppState`, `Theme`, and width. It may format
and truncate the captured strings but must not:

- invoke Git;
- read `.git`;
- inspect the filesystem;
- allocate a background task;
- mutate `AppState`.

Tests exercise the pure status-line builder repeatedly and verify identical
output. A source-scope audit confirms that `ui.rs` does not import
`std::process`, `orca_tools::process`, or `workspace_status::snapshot`. Git
command behavior is tested only through `workspace_status`.

## Error Handling

Workspace identity is supplemental metadata:

- cwd is always shown when width permits;
- Git failures are silent;
- malformed branch output is discarded;
- timeout cleanup waits for the spawned process tree to terminate through the
  existing process helper;
- no placeholder such as `git:?` is rendered.

If Git is unavailable, the TUI remains fully functional and the footer contains
only cwd.

## Files

### Create

- `crates/orca-tui/src/workspace_status.rs`
  - startup snapshot;
  - bounded Git identity discovery;
  - cwd home shortening and compact representations;
  - focused unit tests.

### Modify

- `crates/orca-tui/src/lib.rs`
  - register the focused module.
- `crates/orca-tui/src/types.rs`
  - store optional immutable Git identity.
- `crates/orca-tui/src/app.rs`
  - capture the snapshot exactly once before `AppState` construction.
- `crates/orca-tui/src/ui.rs`
  - add adaptive cwd and Git cells to the existing responsive footer;
  - preserve required-cell and geometry contracts.

No manifest or lockfile change is required.

## Test Strategy

### Workspace snapshot tests

Use real temporary directories and real temporary Git repositories:

- home-prefix shortening uses `~` only for an exact path-component prefix;
- complete, middle-compacted, basename-only, and truncated cwd forms obey
  Unicode display width;
- a normal repository reports its branch;
- a detached checkout reports `@` plus an eight-character commit;
- a non-Git directory returns no Git identity;
- an unborn repository returns no detached identity;
- malformed or oversized command output is rejected through the injected
  command-result closure;
- timeout and spawn failure return no Git identity.

Git-dependent tests skip only when `git --version` cannot execute.

### Footer tests

- wide footer shows full cwd and branch;
- medium footer compacts cwd but keeps branch;
- narrower footer drops branch before cwd;
- narrow footer preserves approval mode and context while omitting workspace
  metadata;
- usage and shortcuts yield before cwd and branch;
- detached identity uses the `git:@<sha>` label;
- Unicode and emoji path components are never split;
- repeated footer rendering is pure and deterministic.

### Integration and regression gates

Run:

```sh
cargo test -p orca-tui workspace_status --lib
cargo test -p orca-tui status_line --lib
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
```

Before push, run the workspace all-target gate. If the two unchanged
`orca-tools` 200 ms external-process timing tests reproduce their documented
macOS race, verify their source blobs still match the remote baseline and run
the all-target gate with only those exact tests skipped.

## Acceptance Criteria

1. A wide footer persistently identifies both cwd and current branch.
2. Detached HEAD renders as `git:@<eight-character-sha>`.
3. Non-Git and Git-failure startup remains silent and functional.
4. No Git command, filesystem read, worker, or timer runs from rendering.
5. Git discovery runs at most once per TUI startup.
6. Narrow layouts preserve approval mode and known context.
7. Workspace metadata yields predictably without changing footer height.
8. Cwd compaction is Unicode display-width and grapheme safe.
9. Existing transcript, queue, search, composer, popup, cursor, notification,
   theme, and terminal-capability contracts remain unchanged.
10. No Vim, configurable-keybinding, `/doctor`, FPS, or onboarding behavior is
    added.
11. The implementation is reviewed, committed with the required TRAE trailer,
    pushed to `feature/tui-syntax-highlighting`, and the remote SHA is verified
    equal to local `HEAD`.
