use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use orca_core::config::ThemeName;

use crate::terminal_capabilities::{TerminalBackground, TerminalColorLevel, TerminalProfile};
use crate::terminal_presentation::TerminalPresentationProfile;

const FRAME_SAMPLE_CAPACITY: usize = 120;
const FPS_WINDOW: Duration = Duration::from_secs(2);
const MAX_RENDER_DURATION: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeybindingsLocation {
    DefaultHome,
    OrcaHome,
    Unavailable,
}

impl KeybindingsLocation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultHome => "default-home",
            Self::OrcaHome => "orca-home",
            Self::Unavailable => "unavailable",
        }
    }
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

impl DiagnosticSnapshot {
    pub(crate) fn new(input: SnapshotInput<'_>) -> Self {
        let terminal_program = input
            .terminal_identity
            .program
            .as_ref()
            .map(ToString::to_string)
            .map(|value| bounded_diagnostic_text(&value))
            .unwrap_or_else(|| "unknown".to_string());
        let terminal_version = input
            .terminal_identity
            .version
            .as_deref()
            .map(bounded_diagnostic_text)
            .filter(|value| value != "unknown");
        let multiplexers = input
            .terminal_identity
            .mux_stack
            .iter()
            .take(4)
            .map(multiplexer_label)
            .map(str::to_string)
            .collect();
        Self {
            app_version: bounded_diagnostic_text(input.app_version),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            terminal_program,
            terminal_version,
            multiplexers,
            color_level: input.terminal_profile.color_level,
            background: input.terminal_profile.background,
            requested_theme: input.requested_theme,
            resolved_theme: input.resolved_theme,
            osc9_supported: input.presentation_profile.osc9_supported(),
            tmux_passthrough: input.presentation_profile.tmux_passthrough(),
            focus_events_requested: input.focus_events_requested,
            terminal_notifications: input.terminal_notifications,
            desktop_notifications: input.desktop_notifications,
            vim_enabled: input.vim_mode,
            keybindings_location: input.keybindings_location,
        }
    }

    pub(crate) fn terminal_program(&self) -> &str {
        &self.terminal_program
    }

    pub(crate) fn terminal_version(&self) -> Option<&str> {
        self.terminal_version.as_deref()
    }

    pub(crate) fn multiplexers(&self) -> &[String] {
        &self.multiplexers
    }

    pub(crate) const fn keybindings_location(&self) -> KeybindingsLocation {
        self.keybindings_location
    }

    pub(crate) const fn color_level(&self) -> TerminalColorLevel {
        self.color_level
    }

    pub(crate) const fn requested_theme(&self) -> ThemeName {
        self.requested_theme
    }

    pub(crate) const fn resolved_theme(&self) -> ThemeName {
        self.resolved_theme
    }
}

impl Default for DiagnosticSnapshot {
    fn default() -> Self {
        Self {
            app_version: "unknown".to_string(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            terminal_program: "unknown".to_string(),
            terminal_version: None,
            multiplexers: Vec::new(),
            color_level: TerminalColorLevel::Monochrome,
            background: TerminalBackground::Unknown,
            requested_theme: ThemeName::Auto,
            resolved_theme: ThemeName::Dark,
            osc9_supported: false,
            tmux_passthrough: false,
            focus_events_requested: false,
            terminal_notifications: false,
            desktop_notifications: false,
            vim_enabled: false,
            keybindings_location: KeybindingsLocation::Unavailable,
        }
    }
}

fn multiplexer_label(multiplexer: &qwertty::Multiplexer) -> &'static str {
    match multiplexer {
        qwertty::Multiplexer::Tmux => "tmux",
        qwertty::Multiplexer::Screen => "screen",
        qwertty::Multiplexer::Zellij => "zellij",
        _ => "unknown",
    }
}

pub(crate) fn bounded_diagnostic_text(source: &str) -> String {
    let sanitized = source
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let collapsed = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = collapsed.chars().take(160).collect::<String>();
    if bounded.is_empty() {
        "unknown".to_string()
    } else {
        bounded
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct FpsHudSnapshot {
    pub(crate) fps: f64,
    pub(crate) render_ms: f64,
    pub(crate) p95_ms: f64,
    pub(crate) total_draws: u64,
    pub(crate) input_events: u64,
    pub(crate) runtime_events: u64,
}

impl FpsHudSnapshot {
    pub(crate) fn hud_text(self) -> String {
        format!(
            " FPS {:.1} · {:.1}ms · p95 {:.1}ms ",
            self.fps, self.render_ms, self.p95_ms
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeybindingsActive {
    BuiltIns,
    Custom,
}

impl KeybindingsActive {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIns => "built-ins",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeybindingsReload {
    Ok,
    Rejected,
    Restored,
}

impl KeybindingsReload {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected => "rejected",
            Self::Restored => "restored",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeybindingsDiagnostic {
    pub(crate) active: KeybindingsActive,
    pub(crate) generation: u64,
    pub(crate) reload: KeybindingsReload,
}

impl Default for KeybindingsDiagnostic {
    fn default() -> Self {
        Self {
            active: KeybindingsActive::BuiltIns,
            generation: 0,
            reload: KeybindingsReload::Ok,
        }
    }
}

impl KeybindingsDiagnostic {
    pub(crate) const fn built_ins(generation: u64) -> Self {
        Self {
            active: KeybindingsActive::BuiltIns,
            generation,
            reload: KeybindingsReload::Ok,
        }
    }

    pub(crate) fn applied_custom(&mut self, generation: u64) {
        self.active = KeybindingsActive::Custom;
        self.generation = generation;
        self.reload = KeybindingsReload::Ok;
    }

    pub(crate) fn restored_built_ins(&mut self, generation: u64) {
        self.active = KeybindingsActive::BuiltIns;
        self.generation = generation;
        self.reload = KeybindingsReload::Restored;
    }

    pub(crate) fn rejected(&mut self, generation: u64) {
        self.generation = generation;
        self.reload = KeybindingsReload::Rejected;
    }

    pub(crate) fn accepted_unchanged(&mut self, generation: u64) {
        self.generation = generation;
        self.reload = KeybindingsReload::Ok;
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DiagnosticRuntimeView {
    pub(crate) viewport: Option<(u16, u16)>,
    pub(crate) status: crate::types::AppStatus,
    pub(crate) panel: crate::types::PanelMode,
    pub(crate) vim_mode: Option<crate::vim::VimMode>,
    pub(crate) fps_hud_enabled: bool,
    pub(crate) keybindings: KeybindingsDiagnostic,
    pub(crate) auth_configured: bool,
}

pub(crate) fn format_doctor_report(
    snapshot: &DiagnosticSnapshot,
    runtime: DiagnosticRuntimeView,
    metrics: FpsHudSnapshot,
) -> String {
    let terminal = snapshot.terminal_version.as_ref().map_or_else(
        || snapshot.terminal_program.clone(),
        |version| format!("{} {version}", snapshot.terminal_program),
    );
    let multiplexers = if snapshot.multiplexers.is_empty() {
        "none".to_string()
    } else {
        snapshot.multiplexers.join(", ")
    };
    let viewport = runtime
        .viewport
        .map(|(width, height)| format!("{width}x{height} cells"))
        .unwrap_or_else(|| "unknown".to_string());
    let mut report = String::with_capacity(1024);
    let _ = writeln!(report, "Orca diagnostics");
    let _ = writeln!(report, "version: {}", snapshot.app_version);
    let _ = writeln!(report, "platform: {}/{}", snapshot.os, snapshot.arch);
    let _ = writeln!(report, "terminal: {terminal}");
    let _ = writeln!(report, "multiplexers: {multiplexers}");
    let _ = writeln!(report, "viewport: {viewport}");
    let _ = writeln!(report, "color: {}", snapshot.color_level.as_str());
    let _ = writeln!(report, "background: {}", snapshot.background.as_str());
    let _ = writeln!(
        report,
        "theme: {} -> {}",
        snapshot.requested_theme.as_str(),
        snapshot.resolved_theme.as_str()
    );
    let _ = writeln!(
        report,
        "notifications: terminal={} focus-events={} osc9={} tmux-passthrough={} desktop={}",
        on_off(snapshot.terminal_notifications),
        on_off(snapshot.focus_events_requested),
        yes_no(snapshot.osc9_supported),
        yes_no(snapshot.tmux_passthrough),
        on_off(snapshot.desktop_notifications),
    );
    let _ = writeln!(
        report,
        "input: qwertty mouse=button paste=bracketed kitty-keyboard=push-succeeded"
    );
    let _ = writeln!(
        report,
        "session: status={} panel={} vim={} auth={}",
        app_status_label(runtime.status),
        panel_label(runtime.panel),
        vim_mode_label(runtime.vim_mode),
        if runtime.auth_configured {
            "configured"
        } else {
            "missing"
        },
    );
    let _ = writeln!(
        report,
        "keybindings: {} generation={} location={} reload={}",
        runtime.keybindings.active.as_str(),
        runtime.keybindings.generation,
        snapshot.keybindings_location.as_str(),
        runtime.keybindings.reload.as_str(),
    );
    let _ = writeln!(report, "fps-hud: {}", on_off(runtime.fps_hud_enabled));
    let _ = write!(
        report,
        "frames: fps={:.1} render-ms={:.1} p95-ms={:.1} draws={} input-events={} runtime-events={}",
        metrics.fps,
        metrics.render_ms,
        metrics.p95_ms,
        metrics.total_draws,
        metrics.input_events,
        metrics.runtime_events,
    );
    truncate_utf8_bytes(report, 4096)
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn app_status_label(status: crate::types::AppStatus) -> &'static str {
    match status {
        crate::types::AppStatus::Setup => "setup",
        crate::types::AppStatus::SessionPicker => "session-picker",
        crate::types::AppStatus::Idle => "idle",
        crate::types::AppStatus::Running => "running",
        crate::types::AppStatus::Compacting => "compacting",
        crate::types::AppStatus::WaitingApproval => "waiting-approval",
        crate::types::AppStatus::WaitingUserInput => "waiting-user-input",
    }
}

fn panel_label(panel: crate::types::PanelMode) -> &'static str {
    match panel {
        crate::types::PanelMode::Conversation => "conversation",
        crate::types::PanelMode::Workflows => "workflows",
        crate::types::PanelMode::Agents => "agents",
    }
}

fn vim_mode_label(mode: Option<crate::vim::VimMode>) -> &'static str {
    match mode {
        None => "off",
        Some(crate::vim::VimMode::Insert) => "insert",
        Some(crate::vim::VimMode::Normal) => "normal",
        Some(crate::vim::VimMode::Visual) => "visual",
    }
}

fn truncate_utf8_bytes(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FrameMetrics {
    draw_times: VecDeque<Instant>,
    render_durations: VecDeque<Duration>,
    total_draws: u64,
    input_events: u64,
    runtime_events: u64,
}

impl FrameMetrics {
    pub(crate) fn record_successful_draw(&mut self, started_at: Instant, completed_at: Instant) {
        self.draw_times.push_back(completed_at);
        prune_draw_times(&mut self.draw_times, completed_at);
        while self.draw_times.len() > FRAME_SAMPLE_CAPACITY {
            self.draw_times.pop_front();
        }

        let duration = completed_at
            .checked_duration_since(started_at)
            .unwrap_or_default()
            .min(MAX_RENDER_DURATION);
        self.render_durations.push_back(duration);
        while self.render_durations.len() > FRAME_SAMPLE_CAPACITY {
            self.render_durations.pop_front();
        }
        self.total_draws = self.total_draws.saturating_add(1);
    }

    pub(crate) fn record_iteration(&mut self, input_events: usize, runtime_events: usize) {
        self.input_events = self
            .input_events
            .saturating_add(saturating_usize_to_u64(input_events));
        self.runtime_events = self
            .runtime_events
            .saturating_add(saturating_usize_to_u64(runtime_events));
    }

    pub(crate) fn snapshot(&self, now: Instant) -> FpsHudSnapshot {
        let draw_times = self
            .draw_times
            .iter()
            .copied()
            .filter(|timestamp| timestamp_in_window(*timestamp, now))
            .collect::<Vec<_>>();
        let fps = match (draw_times.first(), draw_times.last()) {
            (Some(first), Some(last)) if draw_times.len() >= 2 => {
                let elapsed = last.checked_duration_since(*first).unwrap_or_default();
                if elapsed.is_zero() {
                    0.0
                } else {
                    (draw_times.len() - 1) as f64 / elapsed.as_secs_f64()
                }
            }
            _ => 0.0,
        };
        let render_ms = self
            .render_durations
            .back()
            .copied()
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0;
        let p95_ms = nearest_rank_p95(&self.render_durations).as_secs_f64() * 1000.0;
        FpsHudSnapshot {
            fps,
            render_ms,
            p95_ms,
            total_draws: self.total_draws,
            input_events: self.input_events,
            runtime_events: self.runtime_events,
        }
    }

    pub(crate) fn reset_rolling(&mut self) {
        self.draw_times.clear();
        self.render_durations.clear();
    }

    #[cfg(test)]
    fn sample_lengths_for_test(&self) -> (usize, usize) {
        (self.draw_times.len(), self.render_durations.len())
    }

    #[cfg(test)]
    fn with_counters_for_test(total_draws: u64, input_events: u64, runtime_events: u64) -> Self {
        Self {
            total_draws,
            input_events,
            runtime_events,
            ..Self::default()
        }
    }
}

fn prune_draw_times(draw_times: &mut VecDeque<Instant>, now: Instant) {
    while draw_times
        .front()
        .is_some_and(|timestamp| !timestamp_in_window(*timestamp, now))
    {
        draw_times.pop_front();
    }
}

fn timestamp_in_window(timestamp: Instant, now: Instant) -> bool {
    now.checked_duration_since(timestamp)
        .is_none_or(|elapsed| elapsed <= FPS_WINDOW)
}

fn nearest_rank_p95(durations: &VecDeque<Duration>) -> Duration {
    if durations.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = durations.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let rank = (sorted.len() * 95).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use orca_core::config::ThemeName;

    use crate::terminal_capabilities::{TerminalBackground, TerminalColorLevel, TerminalProfile};
    use crate::terminal_presentation::TerminalPresentationProfile;
    use crate::types::{AppStatus, PanelMode};
    use crate::vim::VimMode;

    use super::{
        DiagnosticRuntimeView, DiagnosticSnapshot, FpsHudSnapshot, KeybindingsActive,
        KeybindingsDiagnostic, KeybindingsLocation, KeybindingsReload, SnapshotInput,
        bounded_diagnostic_text, format_doctor_report,
    };

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
        let snapshot = DiagnosticSnapshot::new(SnapshotInput {
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
        });

        assert_eq!(snapshot.terminal_program(), "Ghostty");
        assert_eq!(snapshot.terminal_version(), Some("1.2.0"));
        assert_eq!(snapshot.multiplexers(), ["tmux", "zellij"]);
        assert_eq!(snapshot.keybindings_location().as_str(), "default-home");
        assert_eq!(KeybindingsLocation::OrcaHome.as_str(), "orca-home");
    }

    #[test]
    fn unknown_terminal_identity_is_sanitized() {
        let identity = qwertty::caps::identity_from_env(Some("mystery\u{1b}]0;bad\u{7}"), |_| None);
        let snapshot = DiagnosticSnapshot::new(SnapshotInput {
            app_version: "test",
            terminal_identity: &identity,
            terminal_profile: TerminalProfile {
                background: TerminalBackground::Unknown,
                color_level: TerminalColorLevel::Monochrome,
            },
            presentation_profile: TerminalPresentationProfile::from_identity(&identity),
            requested_theme: ThemeName::Auto,
            resolved_theme: ThemeName::Dark,
            terminal_notifications: false,
            desktop_notifications: false,
            focus_events_requested: false,
            vim_mode: false,
            keybindings_location: KeybindingsLocation::Unavailable,
        });

        assert_eq!(snapshot.terminal_program(), "unknown");
        assert_eq!(snapshot.terminal_version(), Some("mystery ]0;bad"));
        assert!(
            !snapshot
                .terminal_version()
                .expect("unknown XTVERSION is retained as version evidence")
                .chars()
                .any(char::is_control)
        );
    }

    #[test]
    fn first_successful_draw_has_zero_fps_and_one_render_sample() {
        let started = Instant::now();
        let mut metrics = super::FrameMetrics::default();
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
        let mut metrics = super::FrameMetrics::default();
        for frame in 0..60u64 {
            let completed = start + Duration::from_micros(frame * 16_667);
            metrics.record_successful_draw(completed - Duration::from_millis(2), completed);
        }

        let snapshot = metrics.snapshot(start + Duration::from_micros(59 * 16_667));

        assert!((snapshot.fps - 60.0).abs() < 0.2, "{}", snapshot.fps);
    }

    #[test]
    fn idle_snapshot_prunes_fps_without_a_new_draw() {
        let start = Instant::now();
        let mut metrics = super::FrameMetrics::default();
        metrics.record_successful_draw(start, start + Duration::from_millis(1));
        metrics.record_successful_draw(
            start + Duration::from_millis(15),
            start + Duration::from_millis(16),
        );

        assert_eq!(metrics.snapshot(start + Duration::from_secs(3)).fps, 0.0,);
    }

    #[test]
    fn frame_metrics_are_bounded_clamped_and_percentile_stable() {
        let start = Instant::now();
        let mut metrics = super::FrameMetrics::default();
        for index in 0..500u64 {
            let completed = start + Duration::from_millis(index * 10);
            let duration = [1, 2, 3, 100][index as usize % 4];
            metrics.record_successful_draw(
                completed
                    .checked_sub(Duration::from_millis(duration))
                    .unwrap_or(completed),
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
        let mut metrics = super::FrameMetrics::with_counters_for_test(u64::MAX, u64::MAX, u64::MAX);
        metrics.record_successful_draw(now + Duration::from_secs(1), now);
        metrics.record_iteration(usize::MAX, usize::MAX);
        let before = metrics.snapshot(now);
        assert_eq!(before.render_ms, 0.0);
        assert_eq!(before.total_draws, u64::MAX);
        assert_eq!(before.input_events, u64::MAX);
        assert_eq!(before.runtime_events, u64::MAX);

        metrics.reset_rolling();
        let after = metrics.snapshot(now + Duration::from_secs(3));
        assert_eq!((after.fps, after.render_ms, after.p95_ms), (0.0, 0.0, 0.0),);
        assert_eq!(after.total_draws, u64::MAX);
    }

    #[test]
    fn first_draw_after_suspend_restarts_rolling_samples_and_keeps_lifetime_counters() {
        let start = Instant::now();
        let mut metrics = super::FrameMetrics::default();
        metrics.record_successful_draw(start, start + Duration::from_millis(2));
        metrics.record_iteration(3, 4);
        metrics.reset_rolling();
        metrics.record_successful_draw(
            start + Duration::from_secs(1),
            start + Duration::from_secs(1) + Duration::from_millis(5),
        );

        let snapshot = metrics.snapshot(start + Duration::from_secs(1) + Duration::from_millis(5));
        assert_eq!(snapshot.fps, 0.0);
        assert_eq!(snapshot.render_ms, 5.0);
        assert_eq!(snapshot.p95_ms, 5.0);
        assert_eq!(snapshot.total_draws, 2);
        assert_eq!(snapshot.input_events, 3);
        assert_eq!(snapshot.runtime_events, 4);
        assert_eq!(metrics.sample_lengths_for_test(), (1, 1));
    }

    #[test]
    fn doctor_report_has_fixed_safe_line_order_and_bounded_size() {
        let report = format_doctor_report(
            &known_snapshot(),
            DiagnosticRuntimeView {
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
            },
            FpsHudSnapshot {
                fps: 59.8,
                render_ms: 2.3,
                p95_ms: 4.1,
                total_draws: 123,
                input_events: 45,
                runtime_events: 67,
            },
        );

        let expected_platform = format!(
            "platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let lines = report.lines().collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "Orca diagnostics",
                "version: 0.2.50",
                expected_platform.as_str(),
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
        for forbidden in ["DEEPSEEK_API_KEY", "sk-", "/Users/", "C:\\Users\\"] {
            assert!(!report.contains(forbidden));
        }
    }

    #[test]
    fn doctor_report_unknown_and_projection_matrix_is_explicit() {
        for (vim_mode, expected_vim) in [
            (None, "off"),
            (Some(VimMode::Insert), "insert"),
            (Some(VimMode::Normal), "normal"),
            (Some(VimMode::Visual), "visual"),
        ] {
            for (active, reload, expected) in [
                (
                    KeybindingsActive::BuiltIns,
                    KeybindingsReload::Ok,
                    "built-ins",
                ),
                (KeybindingsActive::Custom, KeybindingsReload::Ok, "custom"),
                (
                    KeybindingsActive::Custom,
                    KeybindingsReload::Rejected,
                    "reload=rejected",
                ),
                (
                    KeybindingsActive::BuiltIns,
                    KeybindingsReload::Restored,
                    "reload=restored",
                ),
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
    fn keybindings_diagnostic_projects_reload_outcomes_without_losing_active_state() {
        let mut diagnostic = KeybindingsDiagnostic::built_ins(0);
        assert_eq!(
            diagnostic,
            KeybindingsDiagnostic {
                active: KeybindingsActive::BuiltIns,
                generation: 0,
                reload: KeybindingsReload::Ok,
            },
        );

        diagnostic.applied_custom(1);
        assert_eq!(
            diagnostic,
            KeybindingsDiagnostic {
                active: KeybindingsActive::Custom,
                generation: 1,
                reload: KeybindingsReload::Ok,
            },
        );

        diagnostic.rejected(1);
        assert_eq!(diagnostic.active, KeybindingsActive::Custom);
        assert_eq!(diagnostic.generation, 1);
        assert_eq!(diagnostic.reload, KeybindingsReload::Rejected);

        diagnostic.restored_built_ins(2);
        assert_eq!(
            diagnostic,
            KeybindingsDiagnostic {
                active: KeybindingsActive::BuiltIns,
                generation: 2,
                reload: KeybindingsReload::Restored,
            },
        );
    }

    #[test]
    fn accepted_unchanged_reload_clears_rejected_without_changing_active_state() {
        let mut diagnostic = KeybindingsDiagnostic::built_ins(0);
        diagnostic.applied_custom(1);
        diagnostic.rejected(1);
        diagnostic.accepted_unchanged(1);

        assert_eq!(diagnostic.active, KeybindingsActive::Custom);
        assert_eq!(diagnostic.generation, 1);
        assert_eq!(diagnostic.reload, KeybindingsReload::Ok);
    }

    #[test]
    fn hud_text_is_stable_and_uses_render_duration() {
        let text = FpsHudSnapshot {
            fps: 59.84,
            render_ms: 2.34,
            p95_ms: 4.06,
            ..Default::default()
        }
        .hud_text();

        assert_eq!(text, " FPS 59.8 · 2.3ms · p95 4.1ms ");
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
            "std::env::var",
            "std::env::var_os",
            "probe_capabilities",
            "probe_background",
            "identity_from_env",
        ] {
            assert!(!source.contains(forbidden), "{forbidden}");
        }
    }
}
