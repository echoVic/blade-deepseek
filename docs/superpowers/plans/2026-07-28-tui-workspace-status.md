# TUI Workspace Status Footer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persistently show a compact startup cwd and best-effort Git branch or detached commit in the one-row TUI footer without running Git or reading the filesystem during rendering.

**Architecture:** A focused `workspace_status` module captures one bounded startup snapshot and owns pure cwd compaction plus Git-label formatting. `AppState` stores the immutable Git identity beside its existing cwd string. The footer remains a pure responsive formatter: required model/mode/context cells are preserved, workspace identity yields predictably, and lower-priority usage/shortcut cells consume only the remaining width.

**Tech Stack:** Rust 2024, ratatui 0.29, Unicode grapheme/display-width helpers, `std::process::Command`, existing `orca_tools::process` bounded process supervision, temporary real Git repositories in unit tests.

---

## Scope and Baseline

Implementation baseline:

```text
0a1d215 docs(tui): design workspace status footer
```

Design authority:

```text
docs/superpowers/specs/2026-07-28-tui-workspace-status-design.md
```

Do not implement:

- Git dirty, ahead/behind, remote, stash, tag, or worktree status;
- periodic, event-driven, or manual refresh;
- a worker thread, timer, runtime event, slash command, or configuration key;
- Vim counts, operators, registers, dot-repeat, or `jj`;
- configurable keybindings, `/doctor`, FPS telemetry, or onboarding;
- footer height or composer/transcript/search/queue geometry changes.

Every commit in this plan must end exactly once with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

## File Map

### Create

- `crates/orca-tui/src/workspace_status.rs`
  - immutable startup snapshot values;
  - home-safe cwd display and grapheme-safe compaction;
  - bounded Git command adapter;
  - deterministic injected discovery tests and real-repository tests.

### Modify

- `crates/orca-tui/src/lib.rs`
  - register `workspace_status`.
- `crates/orca-tui/src/types.rs`
  - store optional immutable `GitIdentity`.
- `crates/orca-tui/src/app.rs`
  - capture exactly one snapshot before `AppState` construction.
- `crates/orca-tui/src/ui.rs`
  - render responsive cwd/Git footer cells;
  - preserve existing required-cell and one-row contracts.

No manifest or lockfile changes are permitted.

---

### Task 1: Build the Pure Workspace Snapshot Module

**Files:**
- Create: `crates/orca-tui/src/workspace_status.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Register an empty module and write failing cwd/label tests**

Add to `crates/orca-tui/src/lib.rs` beside the other focused modules:

```rust
mod workspace_status;
```

Create `crates/orca-tui/src/workspace_status.rs` with only the target type
signatures and the following tests. The production functions may initially use
`unimplemented!()` so the file compiles and the assertions fail at runtime:

```rust
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitIdentity {
    Branch(String),
    Detached(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceStatus {
    pub(crate) cwd: String,
    pub(crate) git: Option<GitIdentity>,
}

impl GitIdentity {
    pub(crate) fn label(&self) -> String {
        unimplemented!()
    }
}

pub(crate) fn compact_cwd(_cwd: &str, _max_width: usize) -> String {
    unimplemented!()
}

fn display_cwd(_workspace: &Path, _home: Option<&Path>) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn display_cwd_shortens_only_a_component_safe_home_prefix() {
        assert_eq!(
            display_cwd(
                Path::new("/Users/alice/work/project"),
                Some(Path::new("/Users/alice")),
            ),
            "~/work/project"
        );
        assert_eq!(
            display_cwd(
                Path::new("/Users/alice-other/project"),
                Some(Path::new("/Users/alice")),
            ),
            "/Users/alice-other/project"
        );
        assert_eq!(
            display_cwd(Path::new("/Users/alice"), Some(Path::new("/Users/alice"))),
            "~"
        );
    }

    #[test]
    fn display_cwd_replaces_control_characters_without_changing_components() {
        assert_eq!(
            display_cwd(Path::new("/tmp/line\nbreak"), None),
            "/tmp/line break"
        );
    }

    #[test]
    fn compact_cwd_uses_full_middle_basename_then_grapheme_safe_truncation() {
        let cwd = "~/Documents/GitHub/blade-deepseek";
        assert_eq!(compact_cwd(cwd, 40), cwd);
        assert_eq!(compact_cwd(cwd, 22), "~/…/blade-deepseek");
        assert_eq!(compact_cwd(cwd, 14), "blade-deepseek");
        assert_eq!(compact_cwd(cwd, 10), "blade-dee…");
        assert_eq!(compact_cwd(cwd, 0), "");

        let unicode = "~/项目/👍🏽-workspace";
        for width in 0..=20 {
            let compact = compact_cwd(unicode, width);
            assert!(UnicodeWidthStr::width(compact.as_str()) <= width);
            assert!(!compact.contains('�'));
            assert_eq!(
                compact.contains('👍'),
                compact.contains('🏽'),
                "emoji modifier must remain attached: {compact:?}",
            );
        }
    }

    #[test]
    fn git_identity_labels_distinguish_branch_and_detached_head() {
        assert_eq!(
            GitIdentity::Branch("feature/footer".to_string()).label(),
            "git:feature/footer"
        );
        assert_eq!(
            GitIdentity::Detached("5bbb60aa".to_string()).label(),
            "git:@5bbb60aa"
        );
    }
}
```

- [ ] **Step 2: Run RED for the pure formatting surface**

Run:

```sh
cargo test -p orca-tui workspace_status::tests:: --lib -- --test-threads=1
```

Expected: FAIL because `display_cwd`, `compact_cwd`, and `GitIdentity::label`
are not implemented.

- [ ] **Step 3: Implement component-safe home shortening and cwd compaction**

Add imports:

```rust
use unicode_width::UnicodeWidthStr;

use crate::display_text::truncate_to_display_width;
```

Implement:

```rust
impl GitIdentity {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Branch(branch) => format!("git:{branch}"),
            Self::Detached(commit) => format!("git:@{commit}"),
        }
    }
}

pub(crate) fn compact_cwd(cwd: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(cwd) <= max_width {
        return cwd.to_string();
    }

    let basename = Path::new(cwd)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(cwd);
    let middle = if cwd.starts_with("~/") {
        format!("~/…/{basename}")
    } else if cwd.starts_with('/') {
        format!("/…/{basename}")
    } else {
        format!("…/{basename}")
    };
    if UnicodeWidthStr::width(middle.as_str()) <= max_width {
        return middle;
    }
    if UnicodeWidthStr::width(basename) <= max_width {
        return basename.to_string();
    }
    truncate_to_display_width(basename, max_width)
}

fn display_cwd(workspace: &Path, home: Option<&Path>) -> String {
    let display = home
        .and_then(|home| workspace.strip_prefix(home).ok())
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", relative.display())
            }
        })
        .unwrap_or_else(|| workspace.display().to_string());
    display
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}
```

Do not read `HOME` inside `display_cwd`.

- [ ] **Step 4: Run GREEN for formatting**

Run:

```sh
cargo test -p orca-tui workspace_status::tests:: --lib -- --test-threads=1
cargo test -p orca-tui display_text --lib -- --test-threads=1
```

Expected: all workspace formatting and existing grapheme truncation tests PASS.

- [ ] **Step 5: Write failing deterministic Git discovery tests**

Add below the value types:

```rust
use std::io;

#[derive(Debug)]
struct GitCommandResult {
    success: bool,
    timed_out: bool,
    output_omitted: bool,
    stdout: String,
}

fn discover_git_identity(
    _workspace: &Path,
    _run: impl FnMut(&Path, &[&str]) -> io::Result<GitCommandResult>,
) -> Option<GitIdentity> {
    None
}
```

Add tests:

```rust
#[test]
fn discovery_prefers_symbolic_branch_without_requesting_head() {
    let mut calls = Vec::new();
    let identity = discover_git_identity(Path::new("/workspace"), |cwd, args| {
        calls.push((cwd.to_path_buf(), args.join(" ")));
        Ok(GitCommandResult {
            success: true,
            timed_out: false,
            output_omitted: false,
            stdout: "feature/footer\n".to_string(),
        })
    });

    assert_eq!(
        identity,
        Some(GitIdentity::Branch("feature/footer".to_string()))
    );
    assert_eq!(
        calls,
        vec![(
            PathBuf::from("/workspace"),
            "symbolic-ref --quiet --short HEAD".to_string(),
        )]
    );
}

#[test]
fn discovery_falls_back_to_detached_commit_only_after_symbolic_failure() {
    let mut calls = Vec::new();
    let identity = discover_git_identity(Path::new("/workspace"), |_, args| {
        calls.push(args.join(" "));
        if args[0] == "symbolic-ref" {
            Ok(GitCommandResult {
                success: false,
                timed_out: false,
                output_omitted: false,
                stdout: String::new(),
            })
        } else {
            Ok(GitCommandResult {
                success: true,
                timed_out: false,
                output_omitted: false,
                stdout: "5bbb60aa\n".to_string(),
            })
        }
    });

    assert_eq!(
        identity,
        Some(GitIdentity::Detached("5bbb60aa".to_string()))
    );
    assert_eq!(
        calls,
        [
            "symbolic-ref --quiet --short HEAD",
            "rev-parse --short=8 HEAD",
        ]
    );
}

#[test]
fn discovery_rejects_errors_timeouts_omission_and_malformed_output() {
    let cases = [
        Err(io::Error::new(io::ErrorKind::NotFound, "git missing")),
        Ok(GitCommandResult {
            success: true,
            timed_out: true,
            output_omitted: false,
            stdout: "main".to_string(),
        }),
        Ok(GitCommandResult {
            success: true,
            timed_out: false,
            output_omitted: true,
            stdout: "main".to_string(),
        }),
        Ok(GitCommandResult {
            success: true,
            timed_out: false,
            output_omitted: false,
            stdout: "main\ninjected".to_string(),
        }),
    ];

    for result in cases {
        let mut result = Some(result);
        assert_eq!(
            discover_git_identity(Path::new("/workspace"), |_, _| {
                result.take().expect("one symbolic-ref result")
            }),
            None
        );
    }
}

#[test]
fn detached_discovery_requires_exactly_eight_hex_characters() {
    for invalid in ["", "abc", "zzzzzzzz", "123456789", "1234\n5678"] {
        let mut call = 0;
        let identity = discover_git_identity(Path::new("/workspace"), |_, _| {
            call += 1;
            Ok(GitCommandResult {
                success: call == 2,
                timed_out: false,
                output_omitted: false,
                stdout: if call == 2 {
                    invalid.to_string()
                } else {
                    String::new()
                },
            })
        });
        assert_eq!(identity, None, "{invalid:?}");
    }
}
```

- [ ] **Step 6: Run RED for Git discovery**

Run:

```sh
cargo test -p orca-tui workspace_status::tests::discovery_ --lib -- --test-threads=1
cargo test -p orca-tui workspace_status::tests::detached_discovery_ --lib -- --test-threads=1
```

Expected: branch and detached tests FAIL because discovery always returns
`None`.

- [ ] **Step 7: Implement deterministic discovery and bounded production runner**

Add imports:

```rust
use std::process::{Command, Stdio};
use std::time::Duration;
```

Add constants:

```rust
const GIT_TIMEOUT: Duration = Duration::from_millis(500);
const GIT_RETAINED_BYTES_PER_STREAM: usize = 2 * 1024;
```

Implement validation and discovery:

```rust
fn valid_single_line(output: &GitCommandResult) -> Option<String> {
    if !output.success || output.timed_out || output.output_omitted {
        return None;
    }
    let value = output.stdout.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_string())
}

fn discover_git_identity(
    workspace: &Path,
    mut run: impl FnMut(&Path, &[&str]) -> io::Result<GitCommandResult>,
) -> Option<GitIdentity> {
    let symbolic = run(
        workspace,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .ok()?;
    if symbolic.timed_out || symbolic.output_omitted {
        return None;
    }
    if symbolic.success {
        return valid_single_line(&symbolic).map(GitIdentity::Branch);
    }

    let detached = run(workspace, &["rev-parse", "--short=8", "HEAD"]).ok()?;
    let commit = valid_single_line(&detached)?;
    (commit.len() == 8 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(GitIdentity::Detached(commit))
}
```

Implement the production command adapter:

```rust
fn run_git(workspace: &Path, args: &[&str]) -> io::Result<GitCommandResult> {
    let mut command = Command::new("git");
    command
        .current_dir(workspace)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    orca_tools::process::prepare_non_interactive_command(&mut command);
    let child = command.spawn()?;
    let output = orca_tools::process::wait_for_child_output_with_timeout_or_cancel_and_limit(
        child,
        GIT_TIMEOUT,
        || false,
        GIT_RETAINED_BYTES_PER_STREAM,
    )?;
    Ok(GitCommandResult {
        success: output.status.success(),
        timed_out: output.timed_out,
        output_omitted: output.output_was_omitted(),
        stdout: output.stdout_text(),
    })
}

pub(crate) fn snapshot(workspace: &Path) -> WorkspaceStatus {
    WorkspaceStatus {
        cwd: display_cwd(workspace, dirs::home_dir().as_deref()),
        git: discover_git_identity(workspace, run_git),
    }
}
```

The 2 KiB per-stream cap bounds retained stdout plus stderr to at most 4 KiB.

- [ ] **Step 8: Run GREEN for deterministic discovery**

Run:

```sh
cargo test -p orca-tui workspace_status::tests::discovery_ --lib -- --test-threads=1
cargo test -p orca-tui workspace_status::tests::detached_discovery_ --lib -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 9: Add real repository and degradation tests**

Add test helpers:

```rust
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {}", args.join(" "));
}

fn committed_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "orca@example.invalid"]);
    git(repo.path(), &["config", "user.name", "Orca Test"]);
    std::fs::write(repo.path().join("README.md"), "workspace").expect("fixture");
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    git(repo.path(), &["branch", "-M", "footer-test"]);
    repo
}
```

Add:

```rust
#[test]
fn snapshot_reads_real_branch_and_detached_head() {
    if !git_available() {
        return;
    }
    let repo = committed_repo();
    assert_eq!(
        snapshot(repo.path()).git,
        Some(GitIdentity::Branch("footer-test".to_string()))
    );

    git(repo.path(), &["checkout", "-q", "--detach", "HEAD"]);
    let detached = snapshot(repo.path()).git.expect("detached identity");
    assert!(matches!(
        detached,
        GitIdentity::Detached(ref commit)
            if commit.len() == 8 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    ));
}

#[test]
fn snapshot_silently_omits_git_for_non_repo_and_unborn_repo() {
    if !git_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("directory");
    assert_eq!(snapshot(directory.path()).git, None);

    git(directory.path(), &["init", "-q"]);
    assert!(matches!(
        snapshot(directory.path()).git,
        Some(GitIdentity::Branch(_))
    ));
}
```

The unborn assertion proves it never fabricates a detached commit; showing the
symbolic unborn branch is valid.

- [ ] **Step 10: Run the complete module and static gates**

Run:

```sh
cargo test -p orca-tui workspace_status --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
git diff -- Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml
```

Expected:

- all `workspace_status` tests PASS;
- formatting and diff checks exit 0;
- manifest/lock diff is empty.

- [ ] **Step 11: Commit the focused module**

Run:

```sh
git add crates/orca-tui/src/lib.rs crates/orca-tui/src/workspace_status.rs
git commit \
  -m "feat(tui): capture workspace status" \
  -m "Add bounded startup Git identity discovery and grapheme-safe cwd display compaction with silent degradation." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

Verify:

```sh
test "$(git show -s --format=%B HEAD | grep -Fxc 'Co-authored-by: TRAE CLI <noreply@bytedance.com>')" -eq 1
```

---

### Task 2: Store and Install the Startup Snapshot Exactly Once

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write failing fresh-state ownership test**

In `types.rs` tests, extend `fresh_app_state_has_default_syntax_highlight_state`:

```rust
assert!(state.workspace_git.is_none());
```

Run:

```sh
cargo test -p orca-tui types::tests::fresh_app_state_has_default_syntax_highlight_state --lib -- --exact --test-threads=1
```

Expected: FAIL to compile because `workspace_git` does not exist.

- [ ] **Step 2: Add immutable state ownership**

Import:

```rust
use crate::workspace_status::GitIdentity;
```

Add beside `pub cwd: String`:

```rust
pub(crate) workspace_git: Option<GitIdentity>,
```

Initialize it in `AppState::new`:

```rust
workspace_git: None,
```

Run:

```sh
cargo test -p orca-tui types::tests::fresh_app_state_has_default_syntax_highlight_state --lib -- --exact --test-threads=1
```

Expected: PASS.

- [ ] **Step 3: Write failing startup-snapshot source contract**

Add an `app.rs` test:

```rust
#[test]
fn startup_captures_workspace_status_once_before_frame_loop() {
    let production = include_str!("app.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .expect("production source before tests");
    assert_eq!(
        production.matches("workspace_status::snapshot(&workspace_root)").count(),
        1
    );
    let snapshot = production
        .find("workspace_status::snapshot(&workspace_root)")
        .expect("workspace snapshot");
    let state = production
        .find("AppState::new(")
        .expect("app state construction");
    let terminal = production
        .find("Terminal::new")
        .expect("frame loop terminal");
    assert!(snapshot < state);
    assert!(state < terminal);
    assert!(!production[state..].contains("workspace_status::snapshot("));
}
```

Run:

```sh
cargo test -p orca-tui app::tests::startup_captures_workspace_status_once_before_frame_loop --lib -- --exact --test-threads=1
```

Expected: FAIL because startup does not call `workspace_status::snapshot`.

- [ ] **Step 4: Replace the old cwd-only startup path with one snapshot**

Import in `app.rs`:

```rust
use crate::workspace_status;
```

Replace:

```rust
let cwd_display = shorten_home(&workspace_root.display().to_string());

let mut state = AppState::new(
    action_tx.clone(),
    config.app_version.clone(),
    model_name,
    cwd_display,
);
```

with:

```rust
let workspace_status = workspace_status::snapshot(&workspace_root);
let mut state = AppState::new(
    action_tx.clone(),
    config.app_version.clone(),
    model_name,
    workspace_status.cwd,
);
state.workspace_git = workspace_status.git;
```

Delete the now-unused `shorten_home` function from `app.rs`.

- [ ] **Step 5: Run startup and existing workspace regressions**

Run:

```sh
cargo test -p orca-tui app::tests::startup_captures_workspace_status_once_before_frame_loop --lib -- --exact --test-threads=1
cargo test -p orca-tui startup_config --lib -- --test-threads=1
cargo test -p orca-tui syntax_workspace_root --lib -- --test-threads=1
cargo test -p orca-tui workspace_status --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all tests and checks PASS.

- [ ] **Step 6: Commit state and startup wiring**

Run:

```sh
git add crates/orca-tui/src/types.rs crates/orca-tui/src/app.rs
git commit \
  -m "feat(tui): store startup workspace identity" \
  -m "Install the bounded cwd and Git snapshot exactly once before the fullscreen frame loop." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

Verify the trailer count is exactly one.

---

### Task 3: Render the Responsive Workspace Footer

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Write failing workspace footer helper tests**

Import in `ui.rs`:

```rust
use crate::workspace_status::{GitIdentity, compact_cwd};
```

Before changing `status_line`, add test-only expectations using the planned
helper:

```rust
#[test]
fn workspace_status_spans_keep_full_then_compact_cwd_with_git() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let mut state = test_state();
    state.cwd = "~/Documents/GitHub/blade-deepseek".to_string();
    state.workspace_git = Some(GitIdentity::Branch("feature/footer".to_string()));

    assert_eq!(
        workspace_status_spans(&state, &theme, 80)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>(),
        "  ·  ~/Documents/GitHub/blade-deepseek  ·  git:feature/footer"
    );
    assert_eq!(
        workspace_status_spans(&state, &theme, 46)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>(),
        "  ·  ~/…/blade-deepseek  ·  git:feature/footer"
    );
}

#[test]
fn workspace_status_spans_drop_git_before_cwd_and_bound_unicode() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let mut state = test_state();
    state.cwd = "~/项目/👍🏽-workspace".to_string();
    state.workspace_git = Some(GitIdentity::Branch(
        "feature/a-branch-too-wide-for-the-cell".to_string(),
    ));

    let text = workspace_status_spans(&state, &theme, 18)
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect::<String>();
    assert!(text.starts_with("  ·  "));
    assert!(!text.contains("git:"));
    assert!(UnicodeWidthStr::width(text.as_str()) <= 18);
    assert_eq!(text.contains('👍'), text.contains('🏽'));
}

#[test]
fn workspace_status_spans_label_detached_head() {
    let theme = Theme::named(orca_core::config::ThemeName::Dark);
    let mut state = test_state();
    state.cwd = "/repo".to_string();
    state.workspace_git = Some(GitIdentity::Detached("5bbb60aa".to_string()));

    assert!(workspace_status_spans(&state, &theme, 40)
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect::<String>()
        .contains("git:@5bbb60aa"));
}
```

Declare the target signature above `status_line`:

```rust
fn workspace_status_spans(
    _state: &AppState,
    _theme: &Theme,
    _available_width: usize,
) -> Vec<Span<'static>> {
    Vec::new()
}
```

- [ ] **Step 2: Run RED for workspace cells**

Run:

```sh
cargo test -p orca-tui workspace_status_spans --lib -- --test-threads=1
```

Expected: all content assertions FAIL because the helper returns no spans.

- [ ] **Step 3: Implement adaptive workspace-cell fitting**

Implement:

```rust
fn workspace_status_spans(
    state: &AppState,
    theme: &Theme,
    available_width: usize,
) -> Vec<Span<'static>> {
    let separator = "  ·  ";
    let separator_width = UnicodeWidthStr::width(separator);
    if available_width <= separator_width {
        return Vec::new();
    }

    let git_label = state.workspace_git.as_ref().map(GitIdentity::label);
    let git_width = git_label
        .as_deref()
        .map(|label| separator_width + UnicodeWidthStr::width(label))
        .unwrap_or(0);

    let make_spans = |cwd: String, git: Option<String>| {
        let mut spans = vec![Span::styled(
            format!("{separator}{cwd}"),
            Style::default().fg(theme.muted),
        )];
        if let Some(git) = git {
            spans.push(Span::styled(
                format!("{separator}{git}"),
                Style::default().fg(theme.muted),
            ));
        }
        spans
    };

    if let Some(git) = git_label.as_ref()
        && available_width > separator_width + git_width
    {
        let cwd = compact_cwd(
            &state.cwd,
            available_width
                .saturating_sub(separator_width)
                .saturating_sub(git_width),
        );
        if !cwd.is_empty() {
            return make_spans(cwd, Some(git.clone()));
        }
    }

    let cwd = compact_cwd(&state.cwd, available_width - separator_width);
    if cwd.is_empty() {
        Vec::new()
    } else {
        make_spans(cwd, None)
    }
}
```

This naturally keeps the full cwd when it fits, then uses the pure compactor,
and finally retries without Git.

- [ ] **Step 4: Run GREEN for workspace cells**

Run:

```sh
cargo test -p orca-tui workspace_status_spans --lib -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Write failing end-to-end footer priority tests**

Add:

```rust
#[test]
fn status_line_prioritizes_context_workspace_then_usage_and_shortcuts() {
    let mut state = test_state();
    state.context_limit_tokens = 1000;
    state.context_used_tokens = 250;
    state.usage.input_tokens = 8_000;
    state.usage.output_tokens = 664;
    state.usage.estimated_cost_usd = 0.003852;
    state.cwd = "~/Documents/GitHub/blade-deepseek".to_string();
    state.workspace_git = Some(GitIdentity::Branch("feature/footer".to_string()));
    let theme = Theme::named(orca_core::config::ThemeName::Dark);

    let wide = status_line(&state, &theme, 180).to_string();
    assert!(wide.contains("context 75%"));
    assert!(wide.contains("~/Documents/GitHub/blade-deepseek"));
    assert!(wide.contains("git:feature/footer"));
    assert!(wide.contains("8.7k tokens"));
    assert!(wide.contains("F1 shortcuts"));

    let medium = status_line(&state, &theme, 92).to_string();
    assert!(medium.contains("context 75%"));
    assert!(medium.contains("blade-deepseek"));
    assert!(medium.contains("git:feature/footer"));
    assert!(!medium.contains("tokens"));
    assert!(!medium.contains("shortcuts"));

    let narrow = status_line(&state, &theme, 46).to_string();
    assert!(narrow.contains("suggest"));
    assert!(narrow.contains("context 75%"));
    assert!(!narrow.contains("git:"));
    assert!(!narrow.contains("blade-deepseek"));
}

#[test]
fn status_line_is_pure_and_deterministic_for_captured_workspace_state() {
    let mut state = test_state();
    state.cwd = "~/repo".to_string();
    state.workspace_git = Some(GitIdentity::Branch("main".to_string()));
    let theme = Theme::named(orca_core::config::ThemeName::Dark);

    let first = status_line(&state, &theme, 120);
    let second = status_line(&state, &theme, 120);
    assert_eq!(first, second);

    let source = include_str!("ui.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .expect("production UI source");
    for forbidden in [
        "std::process",
        "Command::new",
        "orca_tools::process",
        "workspace_status::snapshot",
        "read_to_string",
        "read_dir",
    ] {
        assert!(
            !source.contains(forbidden),
            "rendering must not perform workspace I/O: {forbidden}"
        );
    }
}
```

- [ ] **Step 6: Run RED for footer integration**

Run:

```sh
cargo test -p orca-tui status_line_prioritizes_context_workspace --lib -- --test-threads=1
cargo test -p orca-tui status_line_is_pure_and_deterministic --lib -- --test-threads=1
```

Expected:

- priority test FAILS because `status_line` does not consume workspace spans;
- purity test PASSES independently as a structural regression gate;
- the combined RED command still fails because the priority assertion fails.

- [ ] **Step 7: Integrate workspace cells before usage and shortcuts**

Refactor `status_line` after required model/mode spans:

1. Append the context cell first when it fits.
2. Compute `remaining = width.saturating_sub(used)`.
3. Append every span returned by `workspace_status_spans`.
4. Append usage and shortcuts with the existing all-or-nothing fit checks.

Use this structure:

```rust
if state.context_limit_tokens > 0 {
    let context = context_cell(state, theme);
    let context_width = UnicodeWidthStr::width(context.content.as_ref());
    if used + context_width <= width {
        used += context_width;
        spans.push(context);
    }
}

for span in workspace_status_spans(state, theme, width.saturating_sub(used)) {
    used += UnicodeWidthStr::width(span.content.as_ref());
    spans.push(span);
}

let mut lower_priority = Vec::new();
if state.usage.total_tokens() > 0 {
    lower_priority.push(Span::styled(
        format!(
            "{separator}{} tokens{separator}{}",
            format_token_count(state.usage.total_tokens()),
            format_cost(state.usage.estimated_cost_usd),
        ),
        Style::default().fg(theme.muted),
    ));
}
lower_priority.push(Span::styled(
    format!("{separator}F1 shortcuts"),
    Style::default().fg(theme.muted),
));

for span in lower_priority {
    let span_width = UnicodeWidthStr::width(span.content.as_ref());
    if used + span_width <= width {
        used += span_width;
        spans.push(span);
    }
}
```

Do not change `render_status`, layout constraints, colors, token formatting, or
context thresholds.

- [ ] **Step 8: Run GREEN and responsive regressions**

Run:

```sh
cargo test -p orca-tui status_line --lib -- --test-threads=1
cargo test -p orca-tui workspace_status_spans --lib -- --test-threads=1
cargo test -p orca-tui responsive_status_line --lib -- --test-threads=1
cargo test -p orca-tui context_cell --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all footer, workspace, context, and responsive tests PASS.

- [ ] **Step 9: Commit footer rendering**

Run:

```sh
git add crates/orca-tui/src/ui.rs
git commit \
  -m "feat(tui): show workspace status in footer" \
  -m "Render compact cwd and startup Git identity ahead of lower-priority usage and shortcut metadata." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

Verify the trailer count is exactly one.

---

### Task 4: Integration Audit, Review, Verification, and Push

**Files:**
- Verify: `crates/orca-tui/src/workspace_status.rs`
- Verify: `crates/orca-tui/src/lib.rs`
- Verify: `crates/orca-tui/src/types.rs`
- Verify: `crates/orca-tui/src/app.rs`
- Verify: `crates/orca-tui/src/ui.rs`
- Verify: `docs/superpowers/specs/2026-07-28-tui-workspace-status-design.md`
- Verify: `docs/superpowers/plans/2026-07-28-tui-workspace-status.md`

- [ ] **Step 1: Run focused acceptance tests**

Run:

```sh
cargo test -p orca-tui workspace_status --lib -- --test-threads=1
cargo test -p orca-tui status_line --lib -- --test-threads=1
cargo test -p orca-tui startup_captures_workspace_status --lib -- --test-threads=1
```

Expected: all focused tests PASS.

- [ ] **Step 2: Run scope and ownership audits**

Run:

```sh
git diff --name-only 0a1d215..HEAD
git diff --exit-code 0a1d215..HEAD -- Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml
rg -n "Command::new|std::process|orca_tools::process|workspace_status::snapshot|read_to_string|read_dir" crates/orca-tui/src/ui.rs
rg -n "dirty|ahead|behind|periodic|refresh|doctor|FPS|keybindings|register|dot.repeat|onboarding" crates/orca-tui/src/workspace_status.rs crates/orca-tui/src/ui.rs crates/orca-tui/src/app.rs
```

Expected:

- changed production files are only the five declared Rust paths;
- manifest/lock diff is empty;
- `ui.rs` source search prints no rendering I/O imports/calls;
- the forbidden-scope search prints no newly implemented feature surface.

- [ ] **Step 3: Request independent spec and quality reviews**

Request two independent reviews against:

```text
docs/superpowers/specs/2026-07-28-tui-workspace-status-design.md
```

The spec review must verify every acceptance criterion, especially:

- one startup snapshot;
- branch/detached/non-Git behavior;
- responsive priority;
- rendering purity;
- no refresh or dirty-state leakage.

The quality review must inspect:

- process timeout/output bounds and cleanup;
- injected discovery error semantics;
- Unicode path compaction;
- status-width arithmetic and overflow safety;
- source-scope tests for false confidence.

Resolve every Important/Critical finding with a new RED/GREEN cycle, amend only
the responsible implementation commit, and rerun focused tests.

- [ ] **Step 4: Run package verification on committed HEAD**

Run:

```sh
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
test -z "$(git status --porcelain=v1 -uall)"
```

Expected:

- all `orca-tui` tests PASS;
- check/format/diff exit 0;
- worktree is clean.

- [ ] **Step 5: Audit every commit trailer**

Run:

```sh
git log --format='%H' 0a1d215..HEAD | while read -r commit; do
  test "$(git show -s --format=%B "$commit" | grep -Fxc 'Co-authored-by: TRAE CLI <noreply@bytedance.com>')" -eq 1
  test "$(git show -s --format=%B "$commit" | tail -n 2 | head -n 1)" = 'Co-authored-by: TRAE CLI <noreply@bytedance.com>'
done
```

Expected: exit 0.

- [ ] **Step 6: Run the full workspace gate**

First run:

```sh
cargo test --workspace --all-targets -- --test-threads=1
```

Expected: PASS.

If either exact unchanged macOS timing test fails:

```text
external::tests::external_tool_timeout_kills_descendant_processes
external::tests::external_tool_timeout_preserves_observed_exit_code
```

prove the file is unchanged:

```sh
test "$(git rev-parse HEAD:crates/orca-tools/src/external.rs)" = \
  "$(git rev-parse origin/feature/tui-syntax-highlighting:crates/orca-tools/src/external.rs)"
```

Then run only:

```sh
cargo test --workspace --all-targets -- --test-threads=1 \
  --skip external::tests::external_tool_timeout_kills_descendant_processes \
  --skip external::tests::external_tool_timeout_preserves_observed_exit_code
```

Expected: all remaining workspace/all-target tests PASS.

- [ ] **Step 7: Push and verify remote SHA**

Run:

```sh
branch=$(git branch --show-current)
local_sha=$(git rev-parse HEAD)
git push origin "$branch"
remote_sha=$(git ls-remote --heads origin "$branch" | awk '{print $1}')
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
test -z "$(git status --porcelain=v1 -uall)"
printf 'verified=%s\n' "$remote_sha"
```

Expected: remote SHA exactly equals local `HEAD`.

Do not create a release, tag, or PR, and do not remove the worktree. Continue
with the next P2 roadmap sub-project after recording the verified SHA.
