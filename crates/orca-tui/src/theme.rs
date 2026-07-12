use ratatui::style::Color;

use orca_core::config::ThemeName;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub user: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub approval: Color,
    pub plan_mode: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
}

impl Theme {
    pub fn named(name: ThemeName) -> Self {
        match name {
            // DeepSeek-blue truecolor palette. Brand accent #4D6BFE drives
            // borders, selection, and the user prompt.
            ThemeName::Dark => Self {
                border: Color::Rgb(77, 107, 254),
                text: Color::Rgb(232, 236, 246),
                muted: Color::Rgb(139, 147, 167),
                user: Color::Rgb(77, 107, 254),
                success: Color::Rgb(47, 177, 112),
                warning: Color::Rgb(217, 164, 65),
                error: Color::Rgb(214, 81, 81),
                approval: Color::Rgb(169, 139, 245),
                plan_mode: Color::Rgb(64, 170, 170),
                diff_add: Color::Rgb(47, 177, 112),
                diff_remove: Color::Rgb(214, 81, 81),
            },
            ThemeName::Light => Self {
                border: Color::Rgb(58, 86, 230),
                text: Color::Rgb(28, 32, 44),
                muted: Color::Rgb(110, 118, 138),
                user: Color::Rgb(58, 86, 230),
                success: Color::Rgb(31, 142, 86),
                warning: Color::Rgb(176, 122, 20),
                error: Color::Rgb(196, 52, 52),
                approval: Color::Rgb(138, 92, 230),
                plan_mode: Color::Rgb(0, 102, 102),
                diff_add: Color::Rgb(31, 142, 86),
                diff_remove: Color::Rgb(196, 52, 52),
            },
            ThemeName::Solarized => Self {
                border: Color::Rgb(38, 139, 210),
                text: Color::Rgb(147, 161, 161),
                muted: Color::Rgb(88, 110, 117),
                user: Color::Rgb(38, 139, 210),
                success: Color::Rgb(133, 153, 0),
                warning: Color::Rgb(181, 137, 0),
                error: Color::Rgb(220, 50, 47),
                approval: Color::Rgb(108, 113, 196),
                plan_mode: Color::Rgb(42, 161, 152),
                diff_add: Color::Rgb(133, 153, 0),
                diff_remove: Color::Rgb(220, 50, 47),
            },
            ThemeName::Catppuccin => Self {
                border: Color::Rgb(203, 166, 247),
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(147, 153, 178),
                user: Color::Rgb(137, 220, 235),
                success: Color::Rgb(166, 227, 161),
                warning: Color::Rgb(249, 226, 175),
                error: Color::Rgb(243, 139, 168),
                approval: Color::Rgb(203, 166, 247),
                plan_mode: Color::Rgb(148, 226, 213),
                diff_add: Color::Rgb(166, 227, 161),
                diff_remove: Color::Rgb(243, 139, 168),
            },
        }
    }
}
