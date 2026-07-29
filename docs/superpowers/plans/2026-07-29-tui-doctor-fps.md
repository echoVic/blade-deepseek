# TUI Doctor and FPS HUD Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a bounded `/doctor` terminal diagnostics report and an optional current-session FPS/render-time HUD without changing default TUI output or scheduling.

**Architecture:** Add one pure `diagnostics.rs` module for immutable startup facts, bounded successful-draw metrics, safe report formatting, and HUD snapshots. `AppState` owns projections and command state; `app.rs` installs startup facts and records successful draws/event counts; `ui.rs` centralizes hardware cursor application and renders an already-computed HUD snapshot.

**Tech Stack:** Rust 2024, qwertty terminal identity, ratatui, crossterm event types, existing `FrameScheduler`, existing slash command parser/dispatcher, `unicode-width`.

---

## File Map

- Create `crates/orca-tui/src/diagnostics.rs`
  - Safe startup snapshot, keybindings projection, frame sampler, report formatter, HUD text.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the internal diagnostics module.
- Modify `crates/orca-tui/src/commands/mod.rs`
  - Parse and advertise `/doctor` forms.
- Modify `crates/orca-tui/src/slash_command_actions.rs`
  - Emit reports and toggle session-only HUD state.
- Modify `crates/orca-tui/src/types.rs`
  - Own diagnostic state and Vim display projection in `AppState`.
- Modify `crates/orca-tui/src/ui.rs`
  - Centralize cursor placement and render the cursor-safe HUD overlay.
- Modify `crates/orca-tui/src/app.rs`
  - Capture identity/profile once, synchronize Vim projection, record successful draw metrics and event counts, and reset rolling samples on suspend.
- Modify `README.md`
  - Document `/doctor` and `/doctor fps [on|off]`.
- Modify `README.zh-CN.md`
  - Document the same user contract in Chinese.

No `RunConfig`, config file, CLI, environment, history, database, worker, channel, or terminal-probe schema changes are required.

### Task 1: Model Safe Diagnostics and Bounded Frame Metrics

**Files:**
- Create: `crates/orca-tui/src/diagnostics.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/terminal_capabilities.rs`
- Modify: `crates/orca-tui/src/terminal_presentation.rs`

- [ ] **Step 1: Write failing enum-label, snapshot, and sanitizer tests**

Create `diagnostics.rs`, register it in `lib.rs`, and write:

```rust
#[test]
fn diagnostic_enum_labels_are_stable() {
    assert_eq!(TerminalColorLevel::TrueColor.as_str(), "truecolor");
    assert_eq!(TerminalColorLevel::Ansi256.as_str(), "ansi256");
    assert_eq!(TerminalColorLevel::Ansi16.as_str(), "ansi16");
    assert_eq!(TerminalColorLevel::Monochrome.as_str(), "monochrome");
    assert_eq!(TerminalBackground::Dark.as_str(), "dark");
    assert_eq!(TerminalBackground::Light.as_str(), "light");
    assert_eq!(TerminalBackground::Unknown.as_str(), "unknown");
}

#[test]
fn bounded_diagnostic_text_removes_controls_collapses_space_and_truncates() {
    let source = format!("\u{1b}]0;bad\u{7}\n{}\tend", "x".repeat(200));
    let text = bounded_diagnostic_text(&source);
    assert!(!text.chars().any(char::is_control));
    assert!(!text.contains("  "));
    assert_eq!(text.chars().count(), 160);
}

#[test]
fn snapshot_from_identity_preserves_mux_order_without_absolute_paths() {
    let identity = qwertty::caps::identity_from_env(None, |key| match key {
        "TERM_PROGRAM" => Some("ghostty".to_string()),
        "TERM_PROGRAM_VERSION" => Some("1.2.0".to_string()),
        "TMUX" => Some("tmux-session".to_string()),
        "ZELLIJ" => Some("0".to_string()),
        _ => None,
    });
    let snapshot = DiagnosticSnapshot::new(
        SnapshotInput {
            app_version: "0.2.50",
            terminal_identity: &identity,
            terminal_profile: TerminalProfile {
                background: TerminalBackground::Dark,
                color_level: TerminalColorLevel::TrueColor,
            },
            presentation_profile: TerminalPresentationProfile::from_identity(&identity),
            requested_theme: ThemeName::Auto,
            resolved_theme: ThemeName::Dark,
            terminal_notifications: true,
            desktop_notifications: false,
            focus_events_requested: true,
            vim_mode: true,
            keybindings_location: KeybindingsLocation::DefaultHome,
        },
    );
    assert_eq!(snapshot.terminal_program(), "Ghostty");
    assert_eq!(snapshot.terminal_version(), Some("1.2.0"));
    assert_eq!(snapshot.multiplexers(), ["tmux", "zellij"]);
    assert_eq!(snapshot.keybindings_location().as_str(), "default-home");
}
```

Add this explicit unknown-terminal test:

```rust
#[test]
fn unknown_terminal_identity_is_sanitized() {
    let identity = qwertty::caps::identity_from_env(Some("mystery\u{1b}]0;bad\u{7}"), |_| None);
    let snapshot = snapshot_from_identity(&identity);
    assert_eq!(snapshot.terminal_program(), "unknown (mystery ]0;bad)");
    assert!(!snapshot.terminal_program().chars().any(char::is_control));
}
```

- [ ] **Step 2: Run the focused diagnostics tests and verify RED**

```bash
cargo test -p orca-tui diagnostics::tests::diagnostic_enum_labels -- --nocapture
cargo test -p orca-tui diagnostics::tests::bounded_diagnostic_text -- --nocapture
cargo test -p orca-tui diagnostics::tests::snapshot_from_identity -- --nocapture
```

Expected: compilation fails because the diagnostics types, labels, and sanitizer do not exist.

- [ ] **Step 3: Implement stable labels and immutable snapshot**

In `terminal_capabilities.rs`:

```rust
impl TerminalBackground {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Unknown => "unknown",
        }
    }
}

impl TerminalColorLevel {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TrueColor => "truecolor",
            Self::Ansi256 => "ansi256",
            Self::Ansi16 => "ansi16",
            Self::Monochrome => "monochrome",
        }
    }
}
```

In `terminal_presentation.rs` add read-only label methods:

```rust
impl TerminalPresentationProfile {
    pub(crate) const fn osc9_supported(self) -> bool { self.osc9_supported }
    pub(crate) const fn tmux_passthrough(self) -> bool { self.tmux_passthrough }
}
```

In `diagnostics.rs` define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeybindingsLocation {
    DefaultHome,
    OrcaHome,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
    vim_enabled: bool,
    keybindings_location: KeybindingsLocation,
}

pub(crate) struct SnapshotInput<'a> {
    pub(crate) app_version: &'a str,
    pub(crate) terminal_identity: &'a qwertty::TerminalIdentity,
    pub(crate) terminal_profile: TerminalProfile,
    pub(crate) presentation_profile: TerminalPresentationProfile,
    pub(crate) requested_theme: ThemeName,
    pub(crate) resolved_theme: ThemeName,
    pub(crate) terminal_notifications: bool,
    pub(crate) desktop_notifications: bool,
    pub(crate) focus_events_requested: bool,
    pub(crate) vim_mode: bool,
    pub(crate) keybindings_location: KeybindingsLocation,
}
```

Implement `Default` as the deterministic unknown snapshot used by
`AppState::new`. Map non-exhaustive qwertty terminal/multiplexer values with
wildcard fallbacks to sanitized `unknown`, never panic on future variants.
Limit multiplexer labels to four entries even if a future qwertty release
expands the detected stack.

`bounded_diagnostic_text` must replace every control character with a space, use `split_whitespace().join(" ")`, take at most 160 scalar values, and return `unknown` when empty.

- [ ] **Step 4: Write failing frame-metrics tests**

```rust
#[test]
fn first_successful_draw_has_zero_fps_and_one_render_sample() {
    let started = Instant::now();
    let mut metrics = FrameMetrics::default();
    metrics.record_successful_draw(started, started + Duration::from_millis(3));
    let snapshot = metrics.snapshot(started + Duration::from_millis(3));
    assert_eq!(snapshot.fps, 0.0);
    assert_eq!(snapshot.render_ms, 3.0);
    assert_eq!(snapshot.p95_ms, 3.0);
    assert_eq!(snapshot.total_draws, 1);
}

#[test]
fn sixty_even_draws_approach_sixty_fps() {
    let start = Instant::now();
    let mut metrics = FrameMetrics::default();
    for frame in 0..60u64 {
        let completed = start + Duration::from_micros(frame * 16_667);
        metrics.record_successful_draw(
            completed - Duration::from_millis(2),
            completed,
        );
    }
    let snapshot = metrics.snapshot(start + Duration::from_micros(59 * 16_667));
    assert!((snapshot.fps - 60.0).abs() < 0.2, "{}", snapshot.fps);
}

#[test]
fn idle_snapshot_prunes_fps_without_a_new_draw() {
    let start = Instant::now();
    let mut metrics = FrameMetrics::default();
    metrics.record_successful_draw(start, start + Duration::from_millis(1));
    metrics.record_successful_draw(
        start + Duration::from_millis(15),
        start + Duration::from_millis(16),
    );
    assert_eq!(
        metrics.snapshot(start + Duration::from_secs(3)).fps,
        0.0,
    );
}
```

Add these table-driven tests:

```rust
#[test]
fn frame_metrics_are_bounded_clamped_and_percentile_stable() {
    let start = Instant::now();
    let mut metrics = FrameMetrics::default();
    for index in 0..500u64 {
        let completed = start + Duration::from_millis(index * 10);
        let duration = [1, 2, 3, 100][index as usize % 4];
        metrics.record_successful_draw(
            completed.checked_sub(Duration::from_millis(duration)).unwrap_or(completed),
            completed,
        );
    }
    assert_eq!(metrics.sample_lengths_for_test(), (120, 120));
    let snapshot = metrics.snapshot(start + Duration::from_millis(4_990));
    assert_eq!(snapshot.p95_ms, 100.0);

    metrics.record_successful_draw(start, start + Duration::from_secs(2));
    assert_eq!(
        metrics.snapshot(start + Duration::from_secs(2)).render_ms,
        1000.0,
    );
}

#[test]
fn reversed_clock_saturation_and_suspend_reset_are_safe() {
    let now = Instant::now();
    let mut metrics = FrameMetrics::with_counters_for_test(u64::MAX, u64::MAX, u64::MAX);
    metrics.record_successful_draw(now + Duration::from_secs(1), now);
    metrics.record_iteration(usize::MAX, usize::MAX);
    let before = metrics.snapshot(now);
    assert_eq!(before.render_ms, 0.0);
    assert_eq!(before.total_draws, u64::MAX);
    assert_eq!(before.input_events, u64::MAX);
    assert_eq!(before.runtime_events, u64::MAX);

    metrics.reset_rolling();
    let after = metrics.snapshot(now + Duration::from_secs(3));
    assert_eq!((after.fps, after.render_ms, after.p95_ms), (0.0, 0.0, 0.0));
    assert_eq!(after.total_draws, u64::MAX);
}
```

- [ ] **Step 5: Run frame-metrics tests and verify RED**

```bash
cargo test -p orca-tui diagnostics::tests::first_successful_draw -- --nocapture
cargo test -p orca-tui diagnostics::tests::sixty_even_draws -- --nocapture
cargo test -p orca-tui diagnostics::tests::idle_snapshot_prunes -- --nocapture
```

Expected: compilation fails because `FrameMetrics` and `FpsHudSnapshot` do not exist.

- [ ] **Step 6: Implement bounded frame metrics**

Define:

```rust
const FRAME_SAMPLE_CAPACITY: usize = 120;
const FPS_WINDOW: Duration = Duration::from_secs(2);
const MAX_RENDER_DURATION: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FpsHudSnapshot {
    pub(crate) fps: f64,
    pub(crate) render_ms: f64,
    pub(crate) p95_ms: f64,
    pub(crate) total_draws: u64,
    pub(crate) input_events: u64,
    pub(crate) runtime_events: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FrameMetrics {
    draw_times: VecDeque<Instant>,
    render_durations: VecDeque<Duration>,
    total_draws: u64,
    input_events: u64,
    runtime_events: u64,
}
```

Implement:

```rust
pub(crate) fn record_successful_draw(&mut self, started_at: Instant, completed_at: Instant);
pub(crate) fn record_iteration(&mut self, input_events: usize, runtime_events: usize);
pub(crate) fn snapshot(&self, now: Instant) -> FpsHudSnapshot;
pub(crate) fn reset_rolling(&mut self);
```

`snapshot` filters timestamps against `now - FPS_WINDOW` without mutating `self`, computes FPS from first/last retained timestamps, copies at most 120 durations into a local `Vec`, sorts it, and applies nearest-rank p95.

- [ ] **Step 7: Run diagnostics tests and verify GREEN**

```bash
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui terminal_capabilities -- --nocapture
cargo test -p orca-tui terminal_presentation -- --nocapture
```

Expected: all new and existing diagnostics/profile tests pass without warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-tui/src/diagnostics.rs \
  crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/terminal_capabilities.rs \
  crates/orca-tui/src/terminal_presentation.rs
git commit -m "feat(tui): model terminal diagnostics metrics" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 2: Format Reports and Add Doctor Commands

**Files:**
- Modify: `crates/orca-tui/src/diagnostics.rs`
- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write failing doctor report tests**

In `diagnostics.rs`:

```rust
fn known_snapshot() -> DiagnosticSnapshot {
    let identity = qwertty::caps::identity_from_env(None, |key| match key {
        "TERM_PROGRAM" => Some("ghostty".to_string()),
        "TERM_PROGRAM_VERSION" => Some("1.2.0".to_string()),
        "TMUX" => Some("tmux-session".to_string()),
        "ZELLIJ" => Some("0".to_string()),
        _ => None,
    });
    DiagnosticSnapshot::new(SnapshotInput {
        app_version: "0.2.50",
        terminal_identity: &identity,
        terminal_profile: TerminalProfile {
            background: TerminalBackground::Dark,
            color_level: TerminalColorLevel::TrueColor,
        },
        presentation_profile: TerminalPresentationProfile::from_identity(&identity),
        requested_theme: ThemeName::Auto,
        resolved_theme: ThemeName::Dark,
        terminal_notifications: true,
        desktop_notifications: false,
        focus_events_requested: true,
        vim_mode: true,
        keybindings_location: KeybindingsLocation::DefaultHome,
    })
}

#[test]
fn doctor_report_has_fixed_safe_line_order_and_bounded_size() {
    let snapshot = known_snapshot();
    let runtime = DiagnosticRuntimeView {
        viewport: Some((120, 40)),
        status: AppStatus::Idle,
        panel: PanelMode::Conversation,
        vim_mode: Some(VimMode::Normal),
        fps_hud_enabled: false,
        keybindings: KeybindingsDiagnostic {
            active: KeybindingsActive::Custom,
            generation: 2,
            reload: KeybindingsReload::Ok,
        },
        auth_configured: true,
    };
    let report = format_doctor_report(
        &snapshot,
        runtime,
        FpsHudSnapshot {
            fps: 59.8,
            render_ms: 2.3,
            p95_ms: 4.1,
            total_draws: 123,
            input_events: 45,
            runtime_events: 67,
        },
    );
    assert_eq!(
        report.lines().collect::<Vec<_>>(),
        [
            "Orca diagnostics",
            "version: 0.2.50",
            "platform: macos/aarch64",
            "terminal: Ghostty 1.2.0",
            "multiplexers: tmux, zellij",
            "viewport: 120x40 cells",
            "color: truecolor",
            "background: dark",
            "theme: auto -> dark",
            "notifications: terminal=on focus-events=on osc9=yes tmux-passthrough=yes desktop=off",
            "input: qwertty mouse=button paste=bracketed kitty-keyboard=push-succeeded",
            "session: status=idle panel=conversation vim=normal auth=configured",
            "keybindings: custom generation=2 location=default-home reload=ok",
            "fps-hud: off",
            "frames: fps=59.8 render-ms=2.3 p95-ms=4.1 draws=123 input-events=45 runtime-events=67",
        ],
    );
    assert!(report.len() <= 4096);
    assert!(!report.contains('\u{1b}'));
    for forbidden in ["DEEPSEEK_API_KEY", "sk-", "/Users/", "C:\\\\Users\\\\"] {
        assert!(!report.contains(forbidden));
    }
}
```

Add:

```rust
#[test]
fn doctor_report_unknown_and_projection_matrix_is_explicit() {
    for (vim_mode, expected_vim) in [
        (None, "off"),
        (Some(VimMode::Insert), "insert"),
        (Some(VimMode::Normal), "normal"),
        (Some(VimMode::Visual), "visual"),
    ] {
        for (active, reload, expected) in [
            (KeybindingsActive::BuiltIns, KeybindingsReload::Ok, "built-ins"),
            (KeybindingsActive::Custom, KeybindingsReload::Ok, "custom"),
            (KeybindingsActive::Custom, KeybindingsReload::Rejected, "reload=rejected"),
            (KeybindingsActive::BuiltIns, KeybindingsReload::Restored, "reload=restored"),
        ] {
            let report = format_doctor_report(
                &DiagnosticSnapshot::default(),
                DiagnosticRuntimeView {
                    viewport: None,
                    status: AppStatus::Idle,
                    panel: PanelMode::Conversation,
                    vim_mode,
                    fps_hud_enabled: false,
                    keybindings: KeybindingsDiagnostic {
                        active,
                        generation: 0,
                        reload,
                    },
                    auth_configured: false,
                },
                FpsHudSnapshot::default(),
            );
            assert!(report.contains("terminal: unknown"));
            assert!(report.contains("viewport: unknown"));
            assert!(report.contains(&format!("vim={expected_vim}")));
            assert!(report.contains(expected));
            assert!(report.len() <= 4096);
        }
    }
}

#[test]
fn doctor_formatter_source_has_no_runtime_io_or_probe_calls() {
    let source = include_str!("diagnostics.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .expect("production diagnostics source");
    for forbidden in [
        "std::fs",
        "std::process",
        "Command::new",
        "std::env",
        "probe_capabilities",
        "probe_background",
        "identity_from_env",
    ] {
        assert!(!source.contains(forbidden), "{forbidden}");
    }
}
```

- [ ] **Step 2: Run report tests and verify RED**

```bash
cargo test -p orca-tui diagnostics::tests::doctor_report -- --nocapture
```

Expected: compilation fails because report/runtime projection types do not exist.

- [ ] **Step 3: Implement report projections and formatter**

Define stable label enums:

```rust
pub(crate) enum KeybindingsActive { BuiltIns, Custom }
pub(crate) enum KeybindingsReload { Ok, Rejected, Restored }
pub(crate) struct KeybindingsDiagnostic {
    pub(crate) active: KeybindingsActive,
    pub(crate) generation: u64,
    pub(crate) reload: KeybindingsReload,
}
pub(crate) struct DiagnosticRuntimeView {
    pub(crate) viewport: Option<(u16, u16)>,
    pub(crate) status: AppStatus,
    pub(crate) panel: PanelMode,
    pub(crate) vim_mode: Option<VimMode>,
    pub(crate) fps_hud_enabled: bool,
    pub(crate) keybindings: KeybindingsDiagnostic,
    pub(crate) auth_configured: bool,
}
```

Add `as_str` methods for `AppStatus`, `PanelMode`, and `VimMode` in their owning modules or use exhaustive local pure label functions in `diagnostics.rs`. Prefer local label functions to avoid widening unrelated APIs.

Format into a `String` with `writeln!`, strip the final newline, and apply a final 4096-byte UTF-8-safe cap as defense in depth. The fixed bounded fields must keep normal reports well below the cap.

- [ ] **Step 4: Write failing command parser tests**

In `commands/mod.rs`:

```rust
#[test]
fn parses_doctor_commands_exactly() {
    assert_eq!(
        parse("/doctor"),
        Some(SlashCommand::Doctor(DoctorSlashCommand::Report)),
    );
    assert_eq!(
        parse("/doctor fps"),
        Some(SlashCommand::Doctor(DoctorSlashCommand::ToggleFps)),
    );
    assert_eq!(
        parse("/doctor fps on"),
        Some(SlashCommand::Doctor(DoctorSlashCommand::SetFps(true))),
    );
    assert_eq!(
        parse("/doctor fps off"),
        Some(SlashCommand::Doctor(DoctorSlashCommand::SetFps(false))),
    );
}

#[test]
fn rejects_malformed_doctor_commands() {
    for input in [
        "/doctor on",
        "/doctor fps true",
        "/doctor fps off extra",
        "/doctor extra",
        "/Doctor",
    ] {
        assert_eq!(parse(input), None, "{input}");
    }
}

#[test]
fn doctor_is_one_builtin_menu_row_and_cannot_be_shadowed() {
    assert_eq!(
        all_commands().iter().filter(|(name, _)| *name == "/doctor").count(),
        1,
    );
    assert!(builtin_command_names().contains("doctor"));
}
```

- [ ] **Step 5: Run command tests and verify RED**

```bash
cargo test -p orca-tui commands::tests::parses_doctor -- --nocapture
cargo test -p orca-tui commands::tests::rejects_malformed_doctor -- --nocapture
```

Expected: tests fail because `DoctorSlashCommand` and parser entries do not exist.

- [ ] **Step 6: Implement exact doctor command grammar**

Add:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DoctorSlashCommand {
    Report,
    ToggleFps,
    SetFps(bool),
}
```

Parse only the four exact forms. Add one `/doctor` row to `all_commands()` with:

```text
Show terminal diagnostics and control the FPS HUD
```

Do not add submenu rows.

- [ ] **Step 7: Write failing AppState and slash dispatch tests**

In `types.rs` and `slash_command_actions.rs`:

```rust
fn diagnostic_state() -> (
    AppState,
    mpsc::Receiver<UserAction>,
    RunConfig,
    Arc<Mutex<RunConfig>>,
    mpsc::Sender<UserAction>,
) {
    let (action_tx, action_rx) = mpsc::unbounded();
    let state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    let config = crate::test_support::test_run_config();
    let shared = Arc::new(Mutex::new(config.clone()));
    (state, action_rx, config, shared, action_tx)
}

#[test]
fn app_state_diagnostics_defaults_are_inert() {
    let (state, _, _, _, _) = diagnostic_state();
    assert!(!state.fps_hud_enabled);
    assert_eq!(state.vim_mode, None);
    assert_eq!(state.frame_metrics.snapshot(Instant::now()).total_draws, 0);
}

#[test]
fn doctor_report_pushes_one_message_and_preserves_session_state() {
    let (mut state, _rx, mut config, shared, action_tx) = diagnostic_state();
    state.status = AppStatus::Running;
    state.panel_mode = PanelMode::Agents;
    state.vim_mode = Some(VimMode::Visual);
    let before = (state.status, state.panel_mode, state.vim_mode);
    assert!(handle_slash_command(
        "/doctor",
        &mut config,
        &shared,
        &mut state,
        &action_tx,
    ).is_some());
    assert_eq!((state.status, state.panel_mode, state.vim_mode), before);
    assert!(matches!(
        state.messages.last(),
        Some(ChatMessage::System(report)) if report.starts_with("Orca diagnostics\n")
    ));
}

#[test]
fn doctor_fps_toggle_and_explicit_forms_are_session_only_and_idempotent() {
    let (mut state, _rx, mut config, shared, action_tx) = diagnostic_state();
    let before = orca_core::config::format_config_show(&config);
    for (command, expected, message) in [
        ("/doctor fps", true, "FPS HUD enabled."),
        ("/doctor fps on", true, "FPS HUD enabled."),
        ("/doctor fps off", false, "FPS HUD disabled."),
        ("/doctor fps off", false, "FPS HUD disabled."),
    ] {
        handle_slash_command(
            command,
            &mut config,
            &shared,
            &mut state,
            &action_tx,
        )
        .expect("recognized doctor command");
        assert_eq!(state.fps_hud_enabled, expected);
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::System(text)) if text == message
        ));
        assert_eq!(orca_core::config::format_config_show(&config), before);
        assert_eq!(
            orca_core::config::format_config_show(&shared.lock().unwrap()),
            before,
        );
    }
}
```

Add the exact composer-contract test:

```rust
#[test]
fn submitted_doctor_command_is_cleared_by_existing_submit_contract() {
    let (mut state, _rx, mut config, shared, action_tx) = diagnostic_state();
    let theme = Theme::named(ThemeName::Dark);
    let mut vim = VimState::new(false);
    let mut textarea = make_textarea_with_text("/doctor", &vim, &theme);
    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim,
        &theme,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
    ));
    assert_eq!(textarea_text(&textarea), "");
    assert!(matches!(
        state.messages.last(),
        Some(ChatMessage::System(report)) if report.starts_with("Orca diagnostics\n")
    ));
}
```

- [ ] **Step 8: Run state/dispatch tests and verify RED**

```bash
cargo test -p orca-tui app_state_diagnostics_defaults_are_inert -- --nocapture
cargo test -p orca-tui doctor_report_pushes_one_message -- --nocapture
cargo test -p orca-tui doctor_fps_toggle -- --nocapture
```

Expected: compilation fails because diagnostic state and dispatch arms do not exist.

- [ ] **Step 9: Add AppState diagnostic ownership and slash actions**

Add to `AppState`:

```rust
pub(crate) diagnostics: DiagnosticSnapshot,
pub(crate) frame_metrics: FrameMetrics,
pub(crate) fps_hud_enabled: bool,
pub(crate) vim_mode: Option<VimMode>,
pub(crate) keybindings_diagnostic: KeybindingsDiagnostic,
```

`AppState::new` installs deterministic unknown/inert values.

Add:

```rust
pub(crate) fn doctor_report(&self, now: Instant) -> String;
pub(crate) fn set_fps_hud(&mut self, enabled: bool);
pub(crate) fn toggle_fps_hud(&mut self) -> bool;
```

Dispatch `SlashCommand::Doctor` using only these methods. Do not mutate `RunConfig` or `shared_config`.

- [ ] **Step 10: Project keybinding diagnostics from reload outcomes**

In `app.rs`, initialize:

```rust
state.keybindings_diagnostic = KeybindingsDiagnostic::built_ins(
    keymap_runtime.generation(),
    keybindings_location(),
);
```

Update it on:

- `ReloadOutcome::Applied` -> custom/ok;
- `RestoredDefaults` -> built-ins/restored;
- `Rejected` -> retain active built-in/custom state and set reload=rejected.

Expose a pure `keybindings_location()` helper from `keybindings/reload.rs` that reports `OrcaHome`, `DefaultHome`, or `Unavailable` without returning the resolved path to diagnostics.

- [ ] **Step 11: Run commands/report/dispatch tests and verify GREEN**

```bash
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui commands -- --nocapture
cargo test -p orca-tui slash_command_actions -- --nocapture
cargo test -p orca-tui idle_submit_actions -- --nocapture
```

Expected: all focused tests pass.

- [ ] **Step 12: Commit**

```bash
git add crates/orca-tui/src/diagnostics.rs \
  crates/orca-tui/src/commands/mod.rs \
  crates/orca-tui/src/slash_command_actions.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/app.rs \
  crates/orca-tui/src/keybindings/reload.rs
git commit -m "feat(tui): add doctor diagnostics command" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 3: Centralize Hardware Cursor Projection

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Write a source-level failing cursor ownership test**

```rust
#[test]
fn top_level_render_is_the_only_frame_cursor_owner() {
    let production = include_str!("ui.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .unwrap();
    assert_eq!(production.matches("frame.set_cursor_position(").count(), 1);
    let render_start = production.find("pub fn render(").unwrap();
    let cursor = production.find("frame.set_cursor_position(").unwrap();
    assert!(cursor > render_start);
    assert!(!production[cursor..].contains("frame.render_widget("));
}
```

Expected current failure: more than one surface path sets cursor before later popups.

- [ ] **Step 2: Run existing hardware cursor matrix before refactor**

```bash
cargo test -p orca-tui ui::tests::hardware_cursor -- --nocapture
cargo test -p orca-tui ui::tests::setup_cursor -- --nocapture
cargo test -p orca-tui ui::tests::search_frame -- --nocapture
cargo test -p orca-tui ui::tests::shortcuts_frame -- --nocapture
cargo test -p orca-tui ui::tests::waiting_approval_frame -- --nocapture
cargo test -p orca-tui ui::tests::session_picker_frame -- --nocapture
```

Record the passing counts as baseline evidence.

- [ ] **Step 3: Implement one top-level cursor projection**

Introduce:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HardwareCursorProjection {
    position: Position,
}
```

Refactor:

- `render_textarea_surface` returns `Option<Position>`;
- setup/search/composer paths propagate the candidate;
- modal visibility logic chooses exactly one candidate;
- `render` draws base UI, menus, approval, shortcuts, and later HUD;
- the final operation is:

```rust
if let Some(cursor) = hardware_cursor {
    frame.set_cursor_position(cursor.position);
}
```

Setup and session-picker early returns must call a shared `finish_frame` helper that renders optional overlays and sets the cursor last; do not duplicate `set_cursor_position`.

- [ ] **Step 4: Run source test and full cursor matrix GREEN**

Run the Step 2 commands plus:

```bash
cargo test -p orca-tui top_level_render_is_the_only_frame_cursor_owner -- --nocapture
```

Expected: exact old positions/show-hide behavior remains unchanged, and production has one cursor setter.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/ui.rs
git commit -m "refactor(tui): centralize hardware cursor projection" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 4: Render the Cursor-Safe FPS HUD

**Files:**
- Modify: `crates/orca-tui/src/diagnostics.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Write failing HUD text and geometry tests**

In `diagnostics.rs`:

```rust
#[test]
fn hud_text_is_stable_and_uses_render_duration() {
    let text = FpsHudSnapshot {
        fps: 59.84,
        render_ms: 2.34,
        p95_ms: 4.06,
        ..Default::default()
    }.hud_text();
    assert_eq!(text, " FPS 59.8 · 2.3ms · p95 4.1ms ");
}
```

In `ui.rs`:

```rust
#[test]
fn fps_hud_uses_top_right_then_top_left_and_hides_on_double_collision() {
    let frame = Rect::new(5, 7, 80, 20);
    let text_width = UnicodeWidthStr::width(" FPS 59.8 · 2.3ms · p95 4.1ms ");
    let top_right = fps_hud_area(frame, text_width, None).unwrap();
    assert_eq!(top_right.right(), frame.right());

    let right_cursor = Position::new(top_right.x, top_right.y);
    let top_left = fps_hud_area(frame, text_width, Some(right_cursor)).unwrap();
    assert_eq!(top_left.x, frame.x);

    let overlapping_width = 50;
    let overlap_cursor = Position::new(frame.x + 40, frame.y);
    assert_eq!(
        fps_hud_area(frame, overlapping_width, Some(overlap_cursor)),
        None,
    );
}
```

Add:

```rust
#[test]
fn fps_hud_geometry_obeys_display_width_and_compact_bounds() {
    let frame = Rect::new(5, 7, 20, 2);
    assert_eq!(fps_hud_area(frame, 21, None), None);
    assert_eq!(fps_hud_area(Rect::new(5, 7, 20, 1), 20, None), None);
    assert_eq!(fps_hud_area(frame, 20, None), Some(Rect::new(5, 7, 20, 1)));
    let unicode = " FPS 60 · 你好 ";
    assert_eq!(
        fps_hud_area(frame, UnicodeWidthStr::width(unicode), None)
            .unwrap()
            .width,
        UnicodeWidthStr::width(unicode) as u16,
    );
}

#[test]
fn disabled_hud_is_buffer_and_geometry_identical() {
    let mut baseline = test_state();
    let mut disabled = test_state();
    disabled.fps_hud_enabled = false;
    let baseline_frame = render_test_frame(&mut baseline, 80, 24);
    let disabled_frame = render_test_frame(&mut disabled, 80, 24);
    assert_eq!(baseline_frame.buffer, disabled_frame.buffer);
    assert_eq!(baseline_frame.cursor, disabled_frame.cursor);
    assert_eq!(baseline.frame_area, disabled.frame_area);
    assert_eq!(baseline.transcript_area, disabled.transcript_area);
    assert_eq!(baseline.input_area, disabled.input_area);
    assert_eq!(baseline.search_area, disabled.search_area);
    assert_eq!(baseline.jump_to_bottom_area, disabled.jump_to_bottom_area);
}
```

Add:

```rust
#[test]
fn fps_hud_styles_fit_every_terminal_color_level() {
    for color_level in [
        TerminalColorLevel::TrueColor,
        TerminalColorLevel::Ansi256,
        TerminalColorLevel::Ansi16,
        TerminalColorLevel::Monochrome,
    ] {
        let theme = Theme::resolve(
            ThemeName::Dark,
            TerminalProfile {
                background: TerminalBackground::Dark,
                color_level,
            },
        );
        let style = fps_hud_style(&theme);
        assert_eq!(color_level.adapt_style(style), style, "{color_level:?}");
    }
}
```

- [ ] **Step 2: Run HUD tests and verify RED**

```bash
cargo test -p orca-tui hud_text_is_stable -- --nocapture
cargo test -p orca-tui fps_hud_uses_top_right -- --nocapture
```

Expected: tests fail because HUD text/geometry/rendering do not exist.

- [ ] **Step 3: Implement HUD text and pure geometry**

In `diagnostics.rs`:

```rust
impl FpsHudSnapshot {
    pub(crate) fn hud_text(self) -> String {
        format!(
            " FPS {:.1} · {:.1}ms · p95 {:.1}ms ",
            self.fps, self.render_ms, self.p95_ms
        )
    }
}
```

In `ui.rs`, implement a pure geometry helper that:

1. rejects zero/short frames;
2. converts display width to `u16` safely;
3. rejects widths larger than `frame.width`;
4. tries top-right;
5. rejects rectangles containing the hardware cursor;
6. tries top-left;
7. hides if both collide.

- [ ] **Step 4: Render HUD before final cursor application**

Add:

```rust
fn render_fps_hud(
    frame: &mut Frame,
    state: &AppState,
    theme: &Theme,
    hardware_cursor: Option<Position>,
)
```

It returns immediately when disabled, obtains `state.frame_metrics.snapshot(Instant::now())` before drawing, formats the text, computes geometry, renders `Clear`, then a one-line `Paragraph`. It must not mutate state or layout geometry.

Call it from the centralized `finish_frame` before the single cursor setter.

- [ ] **Step 5: Run HUD and cursor suites GREEN**

```bash
cargo test -p orca-tui fps_hud -- --nocapture
cargo test -p orca-tui ui::tests::hardware_cursor -- --nocapture
cargo test -p orca-tui ui::tests::setup_cursor -- --nocapture
cargo test -p orca-tui ui::tests::search_frame -- --nocapture
cargo test -p orca-tui ui::tests::shortcuts_frame -- --nocapture
cargo test -p orca-tui ui::tests::waiting_approval_frame -- --nocapture
```

Expected: HUD tests and preserved cursor matrix pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-tui/src/diagnostics.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/ui.rs
git commit -m "feat(tui): render optional FPS diagnostics HUD" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 5: Integrate Startup Facts and Successful-Draw Sampling

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/diagnostics.rs`

- [ ] **Step 1: Write failing startup identity/profile tests**

Add source and behavior tests:

```rust
#[test]
fn startup_captures_one_identity_for_presentation_and_diagnostics() {
    let production = production_app_source();
    assert_eq!(
        production.matches("qwertty::caps::identity_from_env(").count(),
        1,
    );
    let identity = production.find("let terminal_identity =").unwrap();
    let presentation = production
        .find("TerminalPresentationProfile::from_identity(&terminal_identity)")
        .unwrap();
    let diagnostics = production
        .find("DiagnosticSnapshot::new(")
        .unwrap();
    assert!(identity < presentation && identity < diagnostics);
}

#[test]
fn production_diagnostics_use_effective_profile_without_reprobe() {
    let production = production_app_source();
    assert_eq!(production.matches("InputRuntime::start").count(), 1);
    assert!(!production.contains("probe_capabilities("));
    assert!(!production.contains("probe_background("));
}
```

Define the helper in the same test module:

```rust
fn production_app_source() -> &'static str {
    include_str!("app.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .expect("production app source")
}
```

Add:

```rust
#[test]
fn startup_snapshot_projects_effective_profile_and_orca_home_location() {
    let identity = qwertty::caps::identity_from_env(None, |key| match key {
        "TERM_PROGRAM" => Some("ghostty".to_string()),
        "TMUX" => Some("session".to_string()),
        _ => None,
    });
    let snapshot = diagnostic_snapshot_for_startup(
        &test_config(HistoryMode::Disabled),
        &identity,
        TerminalProfile {
            background: TerminalBackground::Dark,
            color_level: TerminalColorLevel::Ansi256,
        },
        TerminalPresentationProfile::from_identity(&identity),
        KeybindingsLocation::OrcaHome,
    );
    assert_eq!(snapshot.terminal_program(), "Ghostty");
    assert_eq!(snapshot.multiplexers(), ["tmux"]);
    assert_eq!(snapshot.color_level(), TerminalColorLevel::Ansi256);
    assert_eq!(snapshot.requested_theme(), ThemeName::Dark);
    assert_eq!(snapshot.resolved_theme(), ThemeName::Dark);
    assert_eq!(snapshot.keybindings_location(), KeybindingsLocation::OrcaHome);
}
```

- [ ] **Step 2: Run startup tests and verify RED**

```bash
cargo test -p orca-tui startup_captures_one_identity_for_presentation_and_diagnostics -- --nocapture
cargo test -p orca-tui production_diagnostics_use_effective_profile_without_reprobe -- --nocapture
```

Expected: tests fail because production has no diagnostic snapshot installation.

- [ ] **Step 3: Install one startup snapshot**

In `run_tui_inner`:

```rust
let terminal_identity =
    qwertty::caps::identity_from_env(None, qwertty::caps::std_env_source);
let terminal_profile = pending_input_runtime.profile();
let presentation_profile =
    TerminalPresentationProfile::from_identity(&terminal_identity);
let resolved_theme = resolve_base_theme(config.theme, terminal_profile.background);
let theme = Theme::resolve(config.theme, terminal_profile);
```

After `AppState::new`, install a `DiagnosticSnapshot` built from these exact values. Determine keybindings location from the non-path enum helper.

- [ ] **Step 4: Write failing Vim projection tests**

Add a table-driven app test using the existing key/status routing helpers:

```rust
#[test]
fn doctor_vim_projection_tracks_real_mode_transitions() {
    let theme = Theme::named(ThemeName::Dark);
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx,
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );

    let mut disabled = VimState::new(false);
    state.sync_vim_mode(&disabled);
    assert_eq!(state.vim_mode, None);

    let mut vim = VimState::new(true);
    let mut textarea = make_textarea_with_text("word", &vim, &theme);
    state.sync_vim_mode(&vim);
    assert_eq!(state.vim_mode, Some(VimMode::Normal));

    for (key, expected) in [
        (KeyCode::Char('i'), VimMode::Insert),
        (KeyCode::Esc, VimMode::Normal),
        (KeyCode::Char('v'), VimMode::Visual),
        (KeyCode::Esc, VimMode::Normal),
    ] {
        let event = Event::Key(KeyEvent::new(key, KeyModifiers::NONE));
        vim.handle(Input::from(event), &mut textarea, &theme);
        state.sync_vim_mode(&vim);
        assert_eq!(state.vim_mode, Some(expected));
    }
}
```

Extend real existing tests instead of building partial fixtures:

```rust
// idle_submit_actions.rs: idle_submit_resumes_queued_autosend
state.sync_vim_mode(&vim_state);
assert_eq!(state.vim_mode, vim_state.enabled.then_some(vim_state.mode));

// queued_input_actions.rs: restore_latest_replaces_draft_and_preserves_earlier_fifo_items
state.sync_vim_mode(&vim);
assert_eq!(state.vim_mode, vim.enabled.then_some(vim.mode));

// runtime_event_actions.rs:
// runtime_status_transitions_clear_pending_vim_commands_but_streaming_does_not
state.sync_vim_mode(&vim);
assert_eq!(state.vim_mode, vim.enabled.then_some(vim.mode));
```

Add a typed setup harness in `setup_actions.rs`:

```rust
#[test]
fn setup_completion_projection_matches_authoritative_vim_mode() {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.status = AppStatus::Setup;
    state.setup_step = 2;
    let mut config = crate::test_support::test_run_config();
    let shared = Arc::new(Mutex::new(config.clone()));
    let theme = Theme::named(ThemeName::Dark);
    let vim = VimState::new(true);
    let mut textarea = make_setup_textarea(&theme);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_setup_key(
        &Event::Key(enter),
        &enter,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &mut textarea,
        &vim,
        &theme,
        None,
    )
    .unwrap();
    state.sync_vim_mode(&vim);
    assert_eq!(state.vim_mode, Some(vim.mode));
}

#[test]
fn vim_projection_sync_is_owned_by_app_not_leaf_handlers() {
    let app = production_app_source();
    assert!(app.contains("state.sync_vim_mode(&vim_state)"));
    for source in [
        include_str!("slash_command_actions.rs"),
        include_str!("idle_key_actions.rs"),
        include_str!("queued_input_actions.rs"),
        include_str!("mention_menu_actions.rs"),
        include_str!("slash_menu_actions.rs"),
    ] {
        assert!(!source.contains("sync_vim_mode("));
    }
}
```

- [ ] **Step 5: Run Vim projection tests and verify RED**

```bash
cargo test -p orca-tui doctor_vim_projection -- --nocapture
```

Expected: tests fail because `AppState.vim_mode` is not synchronized.

- [ ] **Step 6: Synchronize Vim display projection centrally**

Add:

```rust
impl AppState {
    pub(crate) fn sync_vim_mode(&mut self, vim_state: &VimState) {
        self.vim_mode = vim_state.enabled.then_some(vim_state.mode);
    }
}
```

Call it:

- after `VimState` construction;
- at the start of each `BatchedInputEvent::Event`;
- after `run_event_loop_iteration` returns and before render;
- after setup/resume replacement paths that occur outside normal key completion.

Do not add sync calls to slash/menu/queue modules. The next event's pre-routing
sync ensures a later `/doctor` in the same input batch sees the previous event's
mode transition.

- [ ] **Step 7: Write failing draw/event sampling tests**

Add a small generic helper:

```rust
fn measure_successful_draw<T, F, Clock>(
    mut now: Clock,
    draw: F,
) -> io::Result<(T, Instant, Instant)>
where
    F: FnOnce() -> io::Result<T>,
    Clock: FnMut() -> Instant;
```

Tests:

```rust
#[test]
fn successful_draw_records_once_and_failed_draw_records_nothing() {
    let start = Instant::now();
    let mut metrics = FrameMetrics::default();
    let mut times = VecDeque::from([
        start,
        start + Duration::from_millis(3),
        start + Duration::from_millis(10),
        start + Duration::from_millis(11),
    ]);
    let (_, started, completed) =
        measure_successful_draw(|| times.pop_front().unwrap(), || Ok(())).unwrap();
    metrics.record_successful_draw(started, completed);
    measure_successful_draw(|| times.pop_front().unwrap(), || {
        Err::<(), _>(io::Error::other("draw failed"))
    })
    .unwrap_err();
    assert_eq!(metrics.snapshot(start + Duration::from_millis(11)).total_draws, 1);
}
```

Add:

```rust
#[test]
fn production_draws_and_iteration_counts_are_recorded_once() {
    let source = production_app_source();
    assert_eq!(source.matches("measure_successful_draw(").count(), 2);
    assert_eq!(
        source
            .matches("frame_metrics.record_successful_draw(")
            .count(),
        2,
    );
    assert_eq!(source.matches("frame_metrics.record_iteration(").count(), 1);
}

#[test]
fn doctor_suspend_resets_rolling_before_acknowledgement() {
    let source = production_app_source();
    let suspend = source.find("InputWake::Suspend").unwrap();
    let reset = source[suspend..].find("frame_metrics.reset_rolling()").unwrap();
    let acknowledge = source[suspend..].find("acknowledge.send").unwrap();
    assert!(reset < acknowledge);
}

#[test]
fn fps_hud_controls_animation_without_changing_frame_interval() {
    let source = production_app_source();
    let animation = source.find("let animation_active =").unwrap();
    let receive = source.find("receive_prioritized_input_or_control").unwrap();
    assert!(source[animation..receive].contains("state.fps_hud_enabled"));
    assert_eq!(source.matches("const FRAME_INTERVAL:").count(), 1);
    assert!(source.contains("Duration::from_millis(16)"));
}
```

Add one behavior test that calls `reset_rolling`, records the next successful
draw, and proves the new snapshot has one draw timestamp and one render sample
while lifetime counters remain unchanged.

- [ ] **Step 8: Run integration tests and verify RED**

```bash
cargo test -p orca-tui successful_draw_records_once -- --nocapture
cargo test -p orca-tui doctor_iteration_counts -- --nocapture
cargo test -p orca-tui doctor_suspend_resets_rolling -- --nocapture
cargo test -p orca-tui fps_hud_controls_animation -- --nocapture
```

Expected: tests fail because the app loop does not feed diagnostics.

- [ ] **Step 9: Integrate draw and iteration sampling**

Measure the initial draw and every loop draw, then record only after the render
closure releases its borrow of `state`:

```rust
let (_, started_at, completed_at) = measure_successful_draw(
    Instant::now,
    || {
        terminal
            .draw(|frame| ui::render(frame, &mut state, &textarea, &theme))
            .map(|_| ())
    },
)?;
state
    .frame_metrics
    .record_successful_draw(started_at, completed_at);
```

After `run_event_loop_iteration` returns:

```rust
state
    .frame_metrics
    .record_iteration(iteration.input_events, iteration.runtime_events);
```

Before suspend acknowledgement:

```rust
state.frame_metrics.reset_rolling();
```

Include `state.fps_hud_enabled` in `animation_active`.

- [ ] **Step 10: Run app/diagnostics/UI tests GREEN**

```bash
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
cargo test -p orca-tui frame_scheduler -- --nocapture
```

Expected: all focused and regression tests pass.

- [ ] **Step 11: Commit**

```bash
git add crates/orca-tui/src/app.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/diagnostics.rs
git commit -m "feat(tui): sample successful frame diagnostics" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 6: Documentation, Independent Review, and Full Verification

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`

- [ ] **Step 1: Write failing README contract test**

In `diagnostics.rs`:

```rust
#[test]
fn readmes_document_doctor_and_session_only_fps_hud() {
    for (name, readme) in [
        ("README.md", include_str!("../../../README.md")),
        ("README.zh-CN.md", include_str!("../../../README.zh-CN.md")),
    ] {
        for required in [
            "/doctor",
            "/doctor fps",
            "/doctor fps on",
            "/doctor fps off",
            "session-only",
            "default",
            "secrets",
            "re-probe",
        ] {
            assert!(readme.contains(required), "{name}: {required}");
        }
    }
}
```

- [ ] **Step 2: Run README test and verify RED**

```bash
cargo test -p orca-tui readmes_document_doctor -- --nocapture
```

Expected: FAIL because the command is not documented.

- [ ] **Step 3: Document the user contract**

English and Chinese documentation must state:

- `/doctor` emits a safe copyable report;
- included categories: version/platform, terminal/multiplexer, effective color/background/theme, notification/input posture, viewport/session/keybindings, frame metrics;
- excluded categories: secrets, raw environment, transcript, cwd/absolute filesystem paths;
- the command does not re-probe the terminal;
- `/doctor fps [on|off]` is default-off and session-only;
- FPS is actual successful output frame rate; `render-ms` is `terminal.draw` duration.

- [ ] **Step 4: Run docs and focused tests GREEN**

```bash
cargo test -p orca-tui readmes_document_doctor -- --nocapture
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui commands -- --nocapture
cargo test -p orca-tui slash_command_actions -- --nocapture
```

Expected: all pass.

- [ ] **Step 5: Commit documentation**

```bash
git add README.md README.zh-CN.md crates/orca-tui/src/diagnostics.rs
git commit -m "docs(tui): document doctor and FPS diagnostics" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 6: Independent spec-compliance review**

Provide the reviewer:

- `docs/superpowers/specs/2026-07-29-tui-doctor-fps-design.md`;
- this plan;
- full diff from the spec commit;
- focused test results.

Require review of:

- exact command grammar and built-in precedence;
- safe report fields, line order, bounds, and privacy;
- no reprobe/runtime I/O;
- single startup terminal identity/profile reuse;
- keybindings and Vim projections;
- successful-draw FPS/render metrics;
- event counts and suspension behavior;
- session-only/default-off HUD;
- layout neutrality, cursor collision, and capability-safe style;
- default behavior parity.

Fix Critical/Important findings through RED/GREEN.

- [ ] **Step 7: Independent code-quality review**

Use a different reviewer for:

- bounded queues/metrics and percentile math;
- clock reversal and saturation;
- non-exhaustive qwertty enum handling;
- report sanitization and UTF-8 cap;
- frame-loop fairness and HUD animation cost;
- cursor centralization and early-return paths;
- draw error handling;
- source tests versus behavior tests;
- documentation accuracy.

Fix Critical/Important findings through RED/GREEN.

- [ ] **Step 8: Focused verification**

```bash
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui commands -- --nocapture
cargo test -p orca-tui slash_command_actions -- --nocapture
cargo test -p orca-tui frame_scheduler -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p orca-tui setup -- --nocapture
cargo test -p orca-tui hardware_cursor -- --nocapture
```

- [ ] **Step 9: Full verification**

```bash
cargo test -p orca-core
cargo test -p orca-tui
cargo test --workspace --all-targets
cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

Known unrelated PTY/process/deadline flakes may be skipped only after:

1. the relevant source blob matches the spec-commit baseline;
2. the exact test passes on a fresh rerun;
3. all non-flaky all-target tests pass with explicit skip filters.

No doctor/FPS/cursor failure may be skipped.

- [ ] **Step 10: Verify history and trailers**

```bash
git status --short
git log --format='%H%n%B%n---' 7102fb4ee7a240b28a5a0dd809b6dd6c08ace961..HEAD
```

Require a clean worktree and exactly one final:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

on every new commit.

- [ ] **Step 11: Push and verify remote SHA**

Fetch first because this branch can be rewritten externally:

```bash
git fetch origin feature/tui-syntax-highlighting
```

If the remote is a content-equivalent history rewrite, compare tree hashes and
rebase local commits onto the remote head. Never force push.

Then:

```bash
git push origin feature/tui-syntax-highlighting
LOCAL_SHA=$(git rev-parse HEAD)
REMOTE_SHA=$(git ls-remote origin refs/heads/feature/tui-syntax-highlighting | awk '{print $1}')
test "$LOCAL_SHA" = "$REMOTE_SHA"
printf 'local=%s\nremote=%s\n' "$LOCAL_SHA" "$REMOTE_SHA"
```

## Completion Criteria

The sub-project is complete only when:

- all four doctor command forms and invalid forms match the grammar;
- the safe bounded report contains every specified field and no forbidden data;
- startup identity/profile/theme and keybindings facts are projected once without reprobe;
- Vim projection stays current without becoming authoritative;
- metrics count only successful draws, actual render duration, and bounded event totals;
- FPS window, p95, saturation, idle pruning, and suspension reset are correct;
- HUD is default-off, session-only, layout-neutral, cursor-safe, and capability-safe;
- cursor centralization preserves the complete existing position/show-hide matrix;
- both independent reviews approve;
- focused, crate, workspace, check, format, and diff gates pass;
- every commit has the exact co-author trailer once;
- local and remote branch SHAs match after push.
