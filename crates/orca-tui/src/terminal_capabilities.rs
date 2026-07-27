#![cfg_attr(not(test), allow(dead_code))]

use orca_core::config::ThemeName;
use ratatui::style::{Color, Style};

use crate::syntax_highlight::SyntaxTheme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalProfile {
    pub(crate) background: TerminalBackground,
    pub(crate) color_level: TerminalColorLevel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalBackground {
    Dark,
    Light,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
    Monochrome,
}

#[derive(Clone, Copy, Debug, Default)]
struct ColorSupportFacts {
    has_basic: bool,
    has_256: bool,
    has_16m: bool,
}

pub(crate) fn terminal_background_from_rgb(
    requested: ThemeName,
    background: Option<qwertty::Rgb>,
) -> TerminalBackground {
    if requested != ThemeName::Auto {
        return TerminalBackground::Unknown;
    }
    let Some(background) = background else {
        return TerminalBackground::Unknown;
    };
    if perceived_lightness(background) > 0.5 {
        TerminalBackground::Light
    } else {
        TerminalBackground::Dark
    }
}

fn perceived_lightness(color: qwertty::Rgb) -> f32 {
    let luminance = 0.2126 * linear_channel(color.red())
        + 0.7152 * linear_channel(color.green())
        + 0.0722 * linear_channel(color.blue());
    let lightness = if luminance <= 216.0 / 24389.0 {
        luminance * (24389.0 / 27.0)
    } else {
        luminance.cbrt() * 116.0 - 16.0
    };
    lightness / 100.0
}

fn linear_channel(channel: u8) -> f32 {
    let channel = f32::from(channel) / f32::from(u8::MAX);
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn system_color_level() -> TerminalColorLevel {
    color_level_from_facts(
        supports_color::on(supports_color::Stream::Stdout).map(|level| ColorSupportFacts {
            has_basic: level.has_basic,
            has_256: level.has_256,
            has_16m: level.has_16m,
        }),
    )
}

pub(crate) const fn resolve_base_theme(
    requested: ThemeName,
    background: TerminalBackground,
) -> ThemeName {
    match (requested, background) {
        (ThemeName::Auto, TerminalBackground::Light) => ThemeName::Light,
        (ThemeName::Auto, TerminalBackground::Dark | TerminalBackground::Unknown) => {
            ThemeName::Dark
        }
        (explicit, _) => explicit,
    }
}

const fn color_level_from_facts(facts: Option<ColorSupportFacts>) -> TerminalColorLevel {
    match facts {
        Some(ColorSupportFacts { has_16m: true, .. }) => TerminalColorLevel::TrueColor,
        Some(ColorSupportFacts { has_256: true, .. }) => TerminalColorLevel::Ansi256,
        Some(ColorSupportFacts {
            has_basic: true, ..
        }) => TerminalColorLevel::Ansi16,
        Some(_) | None => TerminalColorLevel::Monochrome,
    }
}

impl TerminalColorLevel {
    pub(crate) const fn revision(self) -> u64 {
        match self {
            Self::TrueColor => 0,
            Self::Ansi256 => 0x100,
            Self::Ansi16 => 0x200,
            Self::Monochrome => 0x300,
        }
    }

    pub(crate) fn adapt_color(self, color: Color) -> Color {
        match self {
            Self::TrueColor => color,
            Self::Ansi256 => match color {
                Color::Rgb(red, green, blue) => Color::Indexed(nearest_xterm_256(red, green, blue)),
                _ => color,
            },
            Self::Ansi16 => match color {
                Color::Rgb(red, green, blue) => nearest_ansi_16(red, green, blue),
                Color::Indexed(index) => {
                    let (red, green, blue) = xterm_index_rgb(index);
                    nearest_ansi_16(red, green, blue)
                }
                _ => color,
            },
            Self::Monochrome => match color {
                Color::Reset => Color::Reset,
                _ => Color::Reset,
            },
        }
    }

    pub(crate) fn adapt_style(self, style: Style) -> Style {
        Style {
            fg: style.fg.map(|color| self.adapt_color(color)),
            bg: style.bg.map(|color| self.adapt_color(color)),
            underline_color: style.underline_color.map(|color| self.adapt_color(color)),
            ..style
        }
    }
}

pub(crate) const fn syntax_style_revision(
    syntax_theme: SyntaxTheme,
    color_level: TerminalColorLevel,
) -> u64 {
    syntax_theme.revision() + color_level.revision()
}

const ANSI_16: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (205, 0, 0)),
    (Color::Green, (0, 205, 0)),
    (Color::Yellow, (205, 205, 0)),
    (Color::Blue, (0, 0, 238)),
    (Color::Magenta, (205, 0, 205)),
    (Color::Cyan, (0, 205, 205)),
    (Color::Gray, (229, 229, 229)),
    (Color::DarkGray, (127, 127, 127)),
    (Color::LightRed, (255, 0, 0)),
    (Color::LightGreen, (0, 255, 0)),
    (Color::LightYellow, (255, 255, 0)),
    (Color::LightBlue, (92, 92, 255)),
    (Color::LightMagenta, (255, 0, 255)),
    (Color::LightCyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

const XTERM_CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn nearest_xterm_256(red: u8, green: u8, blue: u8) -> u8 {
    let mut nearest = 16;
    let mut nearest_distance = i32::MAX;
    for index in 16..=255 {
        let (candidate_red, candidate_green, candidate_blue) = xterm_index_rgb(index);
        let distance = color_distance(
            (red, green, blue),
            (candidate_red, candidate_green, candidate_blue),
        );
        if distance < nearest_distance {
            nearest = index;
            nearest_distance = distance;
        }
    }
    nearest
}

fn nearest_ansi_16(red: u8, green: u8, blue: u8) -> Color {
    let mut nearest = Color::Black;
    let mut nearest_distance = i32::MAX;
    for (candidate, rgb) in ANSI_16 {
        let distance = color_distance((red, green, blue), rgb);
        if distance < nearest_distance {
            nearest = candidate;
            nearest_distance = distance;
        }
    }
    nearest
}

fn xterm_index_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI_16[index as usize].1,
        16..=231 => {
            let cube_index = index - 16;
            (
                XTERM_CUBE_LEVELS[(cube_index / 36) as usize],
                XTERM_CUBE_LEVELS[((cube_index % 36) / 6) as usize],
                XTERM_CUBE_LEVELS[(cube_index % 6) as usize],
            )
        }
        232..=255 => {
            let level = 8 + 10 * (index - 232);
            (level, level, level)
        }
    }
}

fn color_distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> i32 {
    let red = i32::from(left.0) - i32::from(right.0);
    let green = i32::from(left.1) - i32::from(right.1);
    let blue = i32::from(left.2) - i32::from(right.2);
    red * red + green * green + blue * blue
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use ratatui::style::{Color, Modifier, Style};

    use super::{
        ColorSupportFacts, TerminalBackground, TerminalColorLevel, color_level_from_facts,
        resolve_base_theme, terminal_background_from_rgb,
    };

    #[test]
    fn qwertty_background_rgb_maps_by_perceived_lightness() {
        assert_eq!(
            terminal_background_from_rgb(ThemeName::Auto, Some(qwertty::Rgb::new(0, 0, 0))),
            TerminalBackground::Dark
        );
        assert_eq!(
            terminal_background_from_rgb(ThemeName::Auto, Some(qwertty::Rgb::new(255, 255, 255))),
            TerminalBackground::Light
        );
        assert_eq!(
            terminal_background_from_rgb(ThemeName::Auto, Some(qwertty::Rgb::new(118, 118, 118))),
            TerminalBackground::Dark
        );
        assert_eq!(
            terminal_background_from_rgb(ThemeName::Auto, None),
            TerminalBackground::Unknown
        );
    }

    #[test]
    fn explicit_theme_ignores_qwertty_background_rgb() {
        for requested in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            assert_eq!(
                terminal_background_from_rgb(requested, Some(qwertty::Rgb::new(255, 255, 255))),
                TerminalBackground::Unknown
            );
        }
    }

    #[test]
    fn auto_uses_detected_background_and_explicit_themes_ignore_it() {
        assert_eq!(
            resolve_base_theme(ThemeName::Auto, TerminalBackground::Light),
            ThemeName::Light
        );
        assert_eq!(
            resolve_base_theme(ThemeName::Auto, TerminalBackground::Dark),
            ThemeName::Dark
        );
        assert_eq!(
            resolve_base_theme(ThemeName::Auto, TerminalBackground::Unknown),
            ThemeName::Dark
        );

        for explicit in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            for background in [
                TerminalBackground::Dark,
                TerminalBackground::Light,
                TerminalBackground::Unknown,
            ] {
                assert_eq!(resolve_base_theme(explicit, background), explicit);
            }
        }
    }

    #[test]
    fn color_support_facts_map_to_exact_levels() {
        assert_eq!(color_level_from_facts(None), TerminalColorLevel::Monochrome);
        assert_eq!(
            color_level_from_facts(Some(ColorSupportFacts {
                has_basic: true,
                ..Default::default()
            })),
            TerminalColorLevel::Ansi16
        );
        assert_eq!(
            color_level_from_facts(Some(ColorSupportFacts {
                has_basic: true,
                has_256: true,
                ..Default::default()
            })),
            TerminalColorLevel::Ansi256
        );
        assert_eq!(
            color_level_from_facts(Some(ColorSupportFacts {
                has_basic: true,
                has_256: true,
                has_16m: true,
            })),
            TerminalColorLevel::TrueColor
        );
    }

    #[test]
    fn rgb_quantization_uses_stable_xterm_palettes() {
        assert_eq!(
            TerminalColorLevel::Ansi256.adapt_color(Color::Rgb(255, 0, 0)),
            Color::Indexed(196)
        );
        assert_eq!(
            TerminalColorLevel::Ansi256.adapt_color(Color::Rgb(128, 128, 128)),
            Color::Indexed(244)
        );
        assert_eq!(
            TerminalColorLevel::Ansi16.adapt_color(Color::Rgb(255, 0, 0)),
            Color::LightRed
        );
        assert_eq!(
            TerminalColorLevel::Ansi16.adapt_color(Color::Indexed(196)),
            Color::LightRed
        );
        assert_eq!(
            TerminalColorLevel::Ansi16.adapt_color(Color::Rgb(205, 0, 0)),
            Color::Red
        );
    }

    #[test]
    fn monochrome_style_preserves_modifiers_and_resets_colors() {
        let style = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Indexed(42))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::REVERSED);
        let adapted = TerminalColorLevel::Monochrome.adapt_style(style);

        assert_eq!(adapted.fg, Some(Color::Reset));
        assert_eq!(adapted.bg, Some(Color::Reset));
        assert_eq!(adapted.add_modifier, style.add_modifier);
    }

    #[test]
    fn style_underline_color_obeys_terminal_level_and_preserves_modifiers() {
        let style = Style::default()
            .underline_color(Color::Rgb(255, 0, 0))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED);

        for (level, expected) in [
            (TerminalColorLevel::TrueColor, Color::Rgb(255, 0, 0)),
            (TerminalColorLevel::Ansi256, Color::Indexed(196)),
            (TerminalColorLevel::Ansi16, Color::LightRed),
            (TerminalColorLevel::Monochrome, Color::Reset),
        ] {
            let adapted = level.adapt_style(style);
            assert_eq!(adapted.underline_color, Some(expected), "{level:?}");
            assert_eq!(adapted.add_modifier, style.add_modifier, "{level:?}");
        }
    }
}
