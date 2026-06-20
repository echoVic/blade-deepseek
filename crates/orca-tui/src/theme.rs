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
    pub diff_add: Color,
    pub diff_remove: Color,
}

impl Theme {
    pub fn named(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark => Self {
                border: Color::Cyan,
                text: Color::White,
                muted: Color::DarkGray,
                user: Color::Blue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                approval: Color::Magenta,
                diff_add: Color::Green,
                diff_remove: Color::Red,
            },
            ThemeName::Light => Self {
                border: Color::Blue,
                text: Color::Black,
                muted: Color::Gray,
                user: Color::Blue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                approval: Color::Magenta,
                diff_add: Color::Green,
                diff_remove: Color::Red,
            },
            ThemeName::Solarized => Self {
                border: Color::Cyan,
                text: Color::Gray,
                muted: Color::DarkGray,
                user: Color::Blue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                approval: Color::Magenta,
                diff_add: Color::Green,
                diff_remove: Color::Red,
            },
            ThemeName::Catppuccin => Self {
                border: Color::Magenta,
                text: Color::White,
                muted: Color::Gray,
                user: Color::Cyan,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                approval: Color::Magenta,
                diff_add: Color::Green,
                diff_remove: Color::Red,
            },
        }
    }
}
