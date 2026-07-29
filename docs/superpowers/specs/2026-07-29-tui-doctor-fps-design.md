# TUI Doctor and FPS HUD Design

## Goal

Add two related TUI diagnostics surfaces:

- `/doctor` emits one copyable, bounded report that explains Orca's effective
  terminal and rendering posture.
- `/doctor fps`, `/doctor fps on`, and `/doctor fps off` control an optional
  current-session FPS HUD.

The feature targets terminal compatibility issue triage. It reuses the
terminal identity, capability profile, presentation profile, and frame-loop
events Orca already owns. It does not probe the terminal again, execute shell
commands, persist telemetry, or extend onboarding.

## Product Decisions

### Command behavior

The command grammar is:

```text
/doctor
/doctor fps
/doctor fps on
/doctor fps off
```

- `/doctor` appends one `ChatMessage::System` report and scrolls it into view.
- `/doctor fps` toggles the HUD and appends one short system confirmation.
- `/doctor fps on` enables it idempotently.
- `/doctor fps off` disables it idempotently.
- Any other `/doctor ...` form is not parsed as a valid command. It remains
  normal composer text rather than partially executing.

The FPS setting is session-only, defaults to off, and is not added to
`config.toml`, `RunConfig`, environment variables, CLI flags, session history,
or the keybindings file.

### Scope

This sub-project includes:

- the diagnostic snapshot and formatter;
- frame sampling;
- one-line HUD rendering;
- slash command parsing, discovery, and dispatch;
- tests and user documentation.

It excludes:

- provider/model/theme onboarding;
- live terminal reprobes;
- network, filesystem, process, or shell health checks;
- log collection or automatic issue submission;
- persisted telemetry or OpenTelemetry;
- arbitrary profiling;
- per-widget render timings;
- CPU, memory, GPU, or system-load sampling;
- a global command-line `orca doctor`;
- changing the 16ms scheduler target.

Onboarding remains a separate roadmap item with its own spec and plan.

## Architecture

Add a focused module:

```rust
// crates/orca-tui/src/diagnostics.rs

pub(crate) struct DiagnosticSnapshot { /* immutable startup/runtime facts */ }
pub(crate) struct FrameMetrics { /* bounded successful-draw samples */ }
pub(crate) struct FpsHudSnapshot { /* already-formatted render values */ }

pub(crate) fn format_doctor_report(
    snapshot: &DiagnosticSnapshot,
    metrics: &FrameMetrics,
) -> String;
```

Responsibilities are separated:

- `DiagnosticSnapshot` owns stable, safe facts captured at startup and the
  current viewport dimensions.
- `FrameMetrics` receives draw completion events and event-batch counts.
- `format_doctor_report` is pure and deterministic.
- `AppState` owns the snapshot, metrics, and HUD-enabled flag.
- `app.rs` feeds startup identity/profile and successful draw observations.
- `ui.rs` renders only `FpsHudSnapshot`; it never measures time or reads the
  environment.
- slash command code only reports or toggles state.

No background thread, channel, timer, or new terminal owner is added.

## Diagnostic Snapshot

### Stable fields

`DiagnosticSnapshot` stores only bounded, non-secret values:

```rust
pub(crate) struct DiagnosticSnapshot {
    app_version: String,
    os: &'static str,
    arch: &'static str,
    terminal_program: String,
    terminal_version: Option<String>,
    multiplexers: Vec<String>,
    color_level: TerminalColorLevel,
    background: TerminalBackground,
    requested_theme: ThemeName,
    resolved_theme: ThemeName,
    osc9_supported: bool,
    tmux_passthrough: bool,
    focus_events_requested: bool,
    terminal_notifications: bool,
    desktop_notifications: bool,
    vim_mode: bool,
    keybindings_location: KeybindingsLocation,
}
```

Terminal identity comes from the same single
`qwertty::caps::identity_from_env` result already used to construct
`TerminalPresentationProfile`. The app computes the identity once and passes
the same value to both diagnostics and terminal presentation.

The snapshot uses the effective `TerminalProfile` returned by
`InputRuntime::start`; it does not invoke `system_color_level`, OSC 11, or any
qwertty probe again.

The requested theme comes from `RunConfig.theme`. The resolved theme is
computed once with the same
`resolve_base_theme(requested, profile.background)` function used by
`Theme::resolve`; it is not reverse-engineered later from adapted colors.

`multiplexers` preserves qwertty's detected stack order and uses stable display
labels for tmux, screen, and Zellij.

### Dynamic fields

The report reads these current values at invocation:

- viewport cell width and height from the most recent frame;
- current `AppStatus`;
- current panel and Vim mode;
- FPS HUD enabled/disabled;
- keymap generation;
- keybindings file state: built-ins, custom active, or last reload rejected;
- frame metrics snapshot.

Keybindings location is a non-path enum:

```rust
pub(crate) enum KeybindingsLocation {
    DefaultHome,
    OrcaHome,
    Unavailable,
}
```

The report renders `~/.orca/keybindings.json`,
`$ORCA_HOME/keybindings.json`, or `unavailable`; it never expands either
template to an absolute path.

The current Vim mode comes from a small `AppState` projection:

```rust
pub(crate) vim_mode: Option<VimMode>,
```

It is `None` when Vim is disabled and mirrors `VimState.mode` otherwise.
Production synchronizes it centrally in `app.rs`:

- immediately after `VimState` construction;
- immediately before routing every input event;
- immediately after the input event completes;
- after setup/session replacement creates a new textarea/Vim ownership state.

Because slash commands execute inside one routed key event, the pre-routing
projection is current when `/doctor` formats its report. Individual command,
menu, queue, and textarea handlers do not update the projection.

The projection is display-only. `VimState` remains authoritative for input
behavior, commands, registers, pending Insert escape, and undo semantics.
Transition matrix tests cover Insert, Normal, Visual, submit/reset, queued
restore, runtime replacement, setup completion, and Vim-disabled paths.

The report does not include:

- API keys or auth state beyond a generic `configured`/`missing` setup status;
- full environment variables;
- raw OSC replies;
- terminal input bytes;
- absolute keybindings file contents;
- prompt or transcript text;
- arbitrary filesystem paths, including cwd.

### Bounded strings

Terminal program and version display values:

- replace C0/C1 controls with spaces;
- collapse internal whitespace;
- truncate to 160 Unicode scalar values;
- never contain terminal escape sequences.

Unknown values are rendered as `unknown`, not omitted.

## Doctor Report

The report is stable plain text:

```text
Orca diagnostics
version: 0.2.50
platform: macos/aarch64
terminal: Ghostty 1.2.0
multiplexers: tmux
viewport: 120x40 cells
color: truecolor
background: dark
theme: auto -> dark
notifications: terminal=on focus-events=on osc9=yes tmux-passthrough=yes desktop=off
input: qwertty mouse=button paste=bracketed kitty-keyboard=push-succeeded
session: status=idle panel=conversation vim=off
keybindings: custom generation=2 location=default-home reload=ok
fps-hud: off
frames: fps=0.0 render-ms=0.0 p95-ms=0.0 draws=0 input-events=0 runtime-events=0
```

Formatting rules:

- fixed line order;
- lowercase stable labels for enum values;
- one newline between lines;
- no ANSI/OSC styling;
- no trailing spaces;
- at most 4 KiB total;
- no data-dependent unbounded lists.

The `input:` line reports Orca's configured/requested input modes, not a claim
that every terminal supports or granted every protocol. Kitty keyboard is
reported as `push-succeeded`: qwertty wrote and flushed the push sequence, but
the current startup path does not verify the terminal's granted flags. The
report must never label it `active`, `supported`, or `granted`. Focus events are
shown separately because they are optional.

The report is emitted as one system message so transcript search, selection,
copy, history, and rendering use existing bounded paths.

## Frame Metrics

### What counts as a frame

A frame is counted only after:

```rust
terminal.draw(...)?;
scheduler.did_draw(draw_at);
```

returns successfully.

The initial draw also counts after it succeeds. Scheduler wakeups, dirty marks,
animation ticks, attempted draws that return an error, and terminal
presentation writes are not frames.

### Samples

`FrameMetrics` owns fixed-size state:

```rust
const FRAME_SAMPLE_CAPACITY: usize = 120;
const FPS_WINDOW: Duration = Duration::from_secs(2);

pub(crate) struct FrameMetrics {
    draw_times: VecDeque<Instant>,
    render_durations: VecDeque<Duration>,
    total_draws: u64,
    input_events: u64,
    runtime_events: u64,
}
```

The queues never exceed 120 entries. The app captures `started_at` immediately
before `terminal.draw` and `completed_at` immediately after it returns
successfully. On each successful draw:

1. `completed_at` is appended to `draw_times`;
2. entries older than `completed_at - 2s` are removed;
3. `completed_at - started_at` is appended to `render_durations`, capped at
   1 second for display;
4. the oldest duration is removed if capacity is exceeded;
5. `total_draws` increments with saturating arithmetic.

The initial successful draw has a render-duration sample and one FPS timestamp.
An attempted draw that returns an error contributes neither.

Input and runtime counters add the `IterationOutcome` counts after each
completed loop iteration, using saturating arithmetic. They are lifetime
session counts, not rates.

### FPS

FPS is based on actual successful draws inside the rolling two-second window:

```text
fps = (sample_count - 1) / (newest_timestamp - oldest_timestamp)
```

When fewer than two samples exist or elapsed time is zero, FPS is `0.0`.

This definition avoids overclaiming:

- one isolated draw is not 1 FPS;
- idle TUI reports 0 FPS once the window expires;
- a 60 FPS stream approaches 60 rather than counting scheduler wakeups.

### Render time and p95

- `render-ms` is the most recent successful `terminal.draw` duration, clamped
  to 1000ms.
- `p95-ms` is the nearest-rank 95th percentile of the bounded duration samples:
  sort a local stack/vector copy and choose `ceil(0.95 * n) - 1`.
- Empty samples report `0.0`.
- Values are rendered with one decimal place.

These durations include ratatui diffing and backend writes performed inside
`terminal.draw`; they do not include event handling, scheduler wait time, or
terminal-presentation writes.

### Suspend and resume

Before acknowledging terminal suspension:

- clear rolling draw timestamps and render durations;
- preserve lifetime draw and event counters.

After resume, the next successful draw starts a fresh FPS window. Time spent
suspended never affects FPS or render duration.

Resize does not clear samples. A resize-triggered draw is a real frame.

## FPS HUD

### Layout

The HUD is a one-line overlay in the top-right corner:

```text
 FPS 59.8 · 2.3ms · p95 4.1ms 
```

It is rendered after the main surface and every popup, then the hardware cursor
is set once as the final frame operation.
It does not reserve a layout row and does not mutate:

- transcript lines or cache;
- `frame_area`, `transcript_area`, `input_area`, or search geometry;
- mouse hit targets;
- selection/copy text;
- composer wrapping or cursor position.

The full HUD string is formatted first and measured with
`UnicodeWidthStr::width`. The overlay:

- clamps to `frame.area()`;
- hides when its measured display width plus its two border spaces does not fit
  inside `frame.area()`, or when the frame is shorter than 2 rows;
- uses a single-line `Paragraph` with theme border/text colors;
- uses `Clear` only for its exact rectangle;
- never covers the visible hardware cursor cell: if the candidate rectangle
  contains the final cursor position, move the HUD to the top-left; if both
  positions would overlap, hide it.

Modal dialogs, search, setup, picker, and approval do not hide the HUD. This is
intentional: diagnostics must remain visible during compatibility reproduction.

### Hardware cursor centralization

Today `render_textarea_surface` calls `Frame::set_cursor_position` while
rendering each textarea. That is too early for a later HUD overlay: the HUD
could repaint the terminal cell under the already-selected cursor.

As part of this sub-project, cursor ownership becomes explicit:

```rust
fn render(... ) {
    let hardware_cursor = /* active setup/search/composer surface projection */;
    // render base surface, menus, approval, shortcuts
    render_fps_hud(..., hardware_cursor);
    if let Some(position) = hardware_cursor {
        frame.set_cursor_position(position);
    }
}
```

Textarea surface functions return an `Option<Position>` instead of setting the
frame cursor directly. Exactly one top-level cursor application occurs per
frame. Existing visibility rules remain unchanged:

- setup step 1 exposes its masked textarea cursor;
- setup steps 0 and 2 hide it;
- session picker, approval, and shortcut help hide it;
- search owns it while open;
- otherwise the visible composer owns it.

The refactor must preserve every existing hardware-cursor position and
show/hide test before adding HUD-specific collision tests.

### Refresh behavior

When the HUD is off, frame metrics passively record only draws that already
occur; no additional wakeup is scheduled. FPS pruning also happens when a
snapshot is requested, so `/doctor` reports `0.0` after an idle window even if
no new draw arrived to perform pruning.

When the HUD is on:

- it is considered an animation source;
- the existing 80ms animation interval schedules refreshes;
- the scheduler still caps draws at the normal 16ms frame interval;
- no dedicated FPS timer is added.

The HUD displays the metrics snapshot captured before the current draw. The
current draw's duration and completion timestamp become visible on the next
frame. This avoids circular measurement and keeps render code pure.

Toggling the HUD marks the scheduler dirty through the existing slash-command
input event. Turning it off immediately stops HUD-driven animation.

## Slash Command Integration

Add:

```rust
pub enum DoctorSlashCommand {
    Report,
    ToggleFps,
    SetFps(bool),
}

pub enum SlashCommand {
    // existing variants
    Doctor(DoctorSlashCommand),
}
```

Parser behavior:

```text
/doctor         -> Report
/doctor fps     -> ToggleFps
/doctor fps on  -> SetFps(true)
/doctor fps off -> SetFps(false)
```

Extra tokens, alternative booleans, mixed casing, and unknown subcommands are
invalid. Existing slash parsing remains lowercase and exact.

`all_commands()` includes:

```text
/doctor  Show terminal diagnostics and control the FPS HUD
```

The menu contains one row. Subcommands are typed manually; no new submenu is
added.

Slash actions already receive `AppState`. All diagnostic state therefore lives
in `AppState`, and the existing handler signature does not change.

The report action:

1. obtains a pure report string from `state`;
2. pushes one system message;
3. leaves app status, Vim mode, and keymap unchanged.

The standard slash-submit path clears the submitted `/doctor` text and creates
a fresh composer, exactly like every existing recognized slash command. The
command does not restore, replace, or otherwise preserve the submitted command
text.

FPS actions:

1. change only `state.fps_hud_enabled`;
2. push `FPS HUD enabled.` or `FPS HUD disabled.`;
3. preserve pending input, queued messages, panels, and conversation state.

## State and Startup Flow

`AppState` receives:

```rust
pub(crate) diagnostics: DiagnosticSnapshot,
pub(crate) frame_metrics: FrameMetrics,
pub(crate) fps_hud_enabled: bool,
```

To avoid adding terminal details to every test constructor,
`AppState::new` initializes a deterministic unknown/default snapshot. Production
startup replaces it once:

```rust
state.install_diagnostics(snapshot);
```

Production builds the snapshot from:

- `RunConfig`;
- `pending_input_runtime.profile()`;
- the single captured `TerminalIdentity`;
- `TerminalPresentationProfile`;
- resolved theme identity;
- keybindings location source, without resolving the path.

The identity is captured once:

```rust
let terminal_identity =
    qwertty::caps::identity_from_env(None, qwertty::caps::std_env_source);
```

Both diagnostics and `TerminalPresentationProfile::from_identity` consume that
same value.

The viewport is updated from `frame.area()` during render, but the report reads
the stored cell dimensions. No report action touches ratatui.

Keybindings generation and reload state already belong to
`KeymapRuntime`. `AppState` receives a small projection whenever:

- startup uses built-ins;
- a valid custom map is applied;
- deletion restores built-ins;
- a reload is rejected.

Diagnostics does not own or parse keybindings files.

## Error Handling

- Snapshot creation is infallible.
- Formatting is infallible and bounded.
- Missing terminal identity, version, multiplexer, background, viewport, or
  keybindings location source is represented as `unknown` or `none`.
- Counter overflow saturates.
- Clock reversal uses saturating duration and never panics.
- A malformed doctor command is not consumed.
- HUD geometry failure hides the HUD.
- Render errors follow the existing terminal cleanup path; metrics are not
  updated for a failed draw.
- Diagnostic rendering never changes terminal capability or notification
  failure behavior.

## Compatibility

With the HUD disabled and without `/doctor`:

- frame scheduling is unchanged;
- frame contents are unchanged;
- status/footer contents are unchanged;
- no new terminal writes occur;
- no filesystem, environment, or process I/O is added after startup;
- no background thread or channel is added;
- transcript search, selection, mouse, hardware cursor, Vim, keybindings,
  queues, setup, picker, approval, notifications, and title behavior remain
  unchanged.

The new `/doctor` name is reserved before saved workflows and skills, following
the existing built-in-command precedence contract.

## Testing

### Command parsing

- `/doctor`, `/doctor fps`, `/doctor fps on`, `/doctor fps off`;
- reject extra tokens, unknown values, and mixed-case aliases;
- command list and slash menu include exactly one `/doctor` row;
- built-in precedence blocks saved workflow/skill shadowing.

### Snapshot and report

- known Ghostty/tmux profile formats every line in fixed order;
- unknown terminal and profile values remain explicit;
- tmux, screen, and Zellij stack order is preserved;
- control characters and escape sequences are sanitized;
- long values are bounded;
- secrets and forbidden environment names do not appear;
- report size is at most 4 KiB;
- no report code contains filesystem, shell, process, or probe calls;
- kitty keyboard wording is `push-succeeded` and never overclaims a verified
  grant;
- keymap built-in/custom/rejected/restored projections are correct;
- viewport and current status/panel/Vim values are current.

### Frame metrics

- first frame yields zero FPS and one render-duration sample;
- 60 evenly spaced frames approach 60 FPS;
- idle expiry returns zero FPS;
- rolling two-second pruning;
- capacity remains 120;
- recent render duration and nearest-rank p95 are correct;
- one-second duration clamp;
- successful draws only;
- input/runtime counters saturate;
- suspension clears rolling samples but preserves lifetime counters;
- clock reversal is safe.

### HUD

- default is off and produces byte-identical frames;
- centralized cursor projection preserves every existing position and
  show/hide event before HUD rendering is enabled;
- enabled HUD renders exact metrics at top-right;
- compact frames hide it;
- widths smaller than the measured full HUD string hide it without truncation;
- top-right cursor collision falls back to top-left;
- double collision hides it;
- no HUD text enters transcript selection/copy;
- frame/input/search geometry is unchanged;
- all terminal color levels preserve readable styles;
- setup, picker, search, shortcut help, and approval retain hardware-cursor
  semantics.

### Integration

- `/doctor` emits one system message, preserves status, and clears the submitted
  command through the existing slash-submit contract;
- toggle/on/off are idempotent and session-only;
- enabling HUD activates animation; disabling stops HUD-only animation;
- initial draw and later successful draws are recorded once;
- failed draw records nothing;
- iteration event counts are added once;
- suspend clears rolling samples before acknowledgement;
- resume starts a fresh frame interval;
- startup captures terminal identity once and shares it with presentation;
- no new terminal probe, thread, channel, or direct crossterm command appears.

Focused verification:

```text
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui commands -- --nocapture
cargo test -p orca-tui slash_command_actions -- --nocapture
cargo test -p orca-tui frame_scheduler -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
cargo test -p orca-tui app::tests -- --nocapture
```

Full verification:

```text
cargo test -p orca-core
cargo test -p orca-tui
cargo test --workspace --all-targets
cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

Known unrelated process/PTY timing flakes must be isolated by unchanged-source
hash and exact rerun. No diagnostics-related failure may be skipped.

## Documentation

Update `README.md` and `README.zh-CN.md` with:

- `/doctor`;
- `/doctor fps [on|off]`;
- session-only/default-off behavior;
- a short description of safe report fields;
- a note that reports exclude secrets and do not re-probe the terminal.

## Completion Criteria

The sub-project is complete only when:

- `/doctor` produces the bounded safe report above;
- all command forms and invalid forms behave exactly as specified;
- FPS metrics count actual successful draws and bounded event counts;
- HUD is default-off, session-only, layout-neutral, cursor-safe, and
  capability-safe;
- diagnostics reuse one startup identity/profile and do no runtime probe/I/O;
- both independent reviews approve;
- focused, crate, workspace, check, format, and diff gates pass;
- every commit has exactly one final TRAE CLI co-author trailer;
- local and remote branch SHAs match after push.
