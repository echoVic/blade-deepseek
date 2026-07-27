use ratatui::style::Color;

use orca_core::config::ThemeName;

use crate::syntax_highlight::SyntaxTheme;

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
    pub markdown_h1: Color,
    pub markdown_h2: Color,
    pub markdown_h3: Color,
    pub markdown_inline_code: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    /// Background for the mouse text selection in the transcript.
    pub selection_bg: Color,
    pub(crate) syntax_theme: SyntaxTheme,
    pub(crate) syntax_theme_revision: u64,
}

impl Theme {
    pub fn named(name: ThemeName) -> Self {
        let syntax_theme = match name {
            ThemeName::Dark => SyntaxTheme::OneHalfDark,
            ThemeName::Light => SyntaxTheme::OneHalfLight,
            ThemeName::Solarized => SyntaxTheme::SolarizedDark,
            ThemeName::Catppuccin => SyntaxTheme::CatppuccinMocha,
        };

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
                markdown_h1: Color::Rgb(77, 107, 254),
                markdown_h2: Color::Rgb(169, 139, 245),
                markdown_h3: Color::Rgb(217, 164, 65),
                markdown_inline_code: Color::Rgb(64, 170, 170),
                diff_add: Color::Rgb(47, 177, 112),
                diff_remove: Color::Rgb(214, 81, 81),
                // Muted brand blue: keeps every foreground legible.
                selection_bg: Color::Rgb(46, 62, 132),
                syntax_theme,
                syntax_theme_revision: syntax_theme.revision(),
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
                markdown_h1: Color::Rgb(58, 86, 230),
                markdown_h2: Color::Rgb(138, 92, 230),
                markdown_h3: Color::Rgb(176, 122, 20),
                markdown_inline_code: Color::Rgb(0, 102, 102),
                diff_add: Color::Rgb(31, 142, 86),
                diff_remove: Color::Rgb(196, 52, 52),
                selection_bg: Color::Rgb(198, 210, 250),
                syntax_theme,
                syntax_theme_revision: syntax_theme.revision(),
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
                markdown_h1: Color::Rgb(38, 139, 210),
                markdown_h2: Color::Rgb(42, 161, 152),
                markdown_h3: Color::Rgb(181, 137, 0),
                markdown_inline_code: Color::Rgb(211, 54, 130),
                diff_add: Color::Rgb(133, 153, 0),
                diff_remove: Color::Rgb(220, 50, 47),
                // base02, Solarized's canonical selection background.
                selection_bg: Color::Rgb(7, 54, 66),
                syntax_theme,
                syntax_theme_revision: syntax_theme.revision(),
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
                markdown_h1: Color::Rgb(203, 166, 247),
                markdown_h2: Color::Rgb(116, 199, 236),
                markdown_h3: Color::Rgb(249, 226, 175),
                markdown_inline_code: Color::Rgb(245, 194, 231),
                diff_add: Color::Rgb(166, 227, 161),
                diff_remove: Color::Rgb(243, 139, 168),
                // surface2 from the Mocha palette.
                selection_bg: Color::Rgb(88, 91, 112),
                syntax_theme,
                syntax_theme_revision: syntax_theme.revision(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use ratatui::style::Color;

    use super::Theme;
    use crate::syntax_highlight::SyntaxTheme;

    #[test]
    fn named_themes_map_to_matching_syntax_themes_and_revisions() {
        let cases = [
            (ThemeName::Dark, SyntaxTheme::OneHalfDark),
            (ThemeName::Light, SyntaxTheme::OneHalfLight),
            (ThemeName::Solarized, SyntaxTheme::SolarizedDark),
            (ThemeName::Catppuccin, SyntaxTheme::CatppuccinMocha),
        ];

        for (name, syntax_theme) in cases {
            let theme = Theme::named(name);
            assert_eq!(theme.syntax_theme, syntax_theme);
            assert_eq!(theme.syntax_theme_revision, syntax_theme.revision());
        }
    }

    #[test]
    fn named_themes_define_markdown_semantic_colors() {
        let cases = [
            (
                ThemeName::Dark,
                [
                    Color::Rgb(77, 107, 254),
                    Color::Rgb(169, 139, 245),
                    Color::Rgb(217, 164, 65),
                    Color::Rgb(64, 170, 170),
                ],
            ),
            (
                ThemeName::Light,
                [
                    Color::Rgb(58, 86, 230),
                    Color::Rgb(138, 92, 230),
                    Color::Rgb(176, 122, 20),
                    Color::Rgb(0, 102, 102),
                ],
            ),
            (
                ThemeName::Solarized,
                [
                    Color::Rgb(38, 139, 210),
                    Color::Rgb(42, 161, 152),
                    Color::Rgb(181, 137, 0),
                    Color::Rgb(211, 54, 130),
                ],
            ),
            (
                ThemeName::Catppuccin,
                [
                    Color::Rgb(203, 166, 247),
                    Color::Rgb(116, 199, 236),
                    Color::Rgb(249, 226, 175),
                    Color::Rgb(245, 194, 231),
                ],
            ),
        ];

        for (name, expected) in cases {
            let theme = Theme::named(name);
            assert_eq!(
                [
                    theme.markdown_h1,
                    theme.markdown_h2,
                    theme.markdown_h3,
                    theme.markdown_inline_code,
                ],
                expected,
                "{name:?}"
            );
        }
    }

    #[test]
    fn markdown_semantic_colors_do_not_use_fixed_ansi_accents() {
        let forbidden = [Color::Cyan, Color::Green, Color::Yellow, Color::Magenta];

        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            let theme = Theme::named(name);
            for color in [
                theme.markdown_h1,
                theme.markdown_h2,
                theme.markdown_h3,
                theme.markdown_inline_code,
            ] {
                assert!(!forbidden.contains(&color), "{name:?}: {color:?}");
            }
        }
    }
}
