use std::collections::VecDeque;
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

    use super::{DiagnosticSnapshot, KeybindingsLocation, SnapshotInput, bounded_diagnostic_text};

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
}
