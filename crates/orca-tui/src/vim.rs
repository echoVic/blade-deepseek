use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, Input, Key, TextArea};

use crate::theme::Theme;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VimMode {
    Insert,
    Normal,
    Visual,
}

#[derive(Clone, Debug)]
pub struct VimState {
    pub enabled: bool,
    pub mode: VimMode,
}

impl VimState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            mode: if enabled {
                VimMode::Normal
            } else {
                VimMode::Insert
            },
        }
    }

    pub fn title(&self) -> &'static str {
        if !self.enabled {
            " Input "
        } else {
            match self.mode {
                VimMode::Insert => " Input [vi insert] ",
                VimMode::Normal => " Input [vi normal] ",
                VimMode::Visual => " Input [vi visual] ",
            }
        }
    }

    pub fn configure_block(&self, textarea: &mut TextArea<'_>, theme: &Theme) {
        textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(self.title())
                .border_style(Style::default().fg(theme.border)),
        );
        let cursor_color = match self.mode {
            VimMode::Insert => theme.border,
            VimMode::Normal => theme.warning,
            VimMode::Visual => theme.approval,
        };
        textarea.set_cursor_style(
            Style::default()
                .fg(cursor_color)
                .add_modifier(Modifier::REVERSED),
        );
    }

    pub fn reset_insert(&mut self, textarea: &mut TextArea<'_>, theme: &Theme) {
        self.mode = if self.enabled {
            VimMode::Normal
        } else {
            VimMode::Insert
        };
        textarea.cancel_selection();
        self.configure_block(textarea, theme);
    }

    pub fn handle(&mut self, input: Input, textarea: &mut TextArea<'_>, theme: &Theme) -> bool {
        if !self.enabled {
            return textarea.input(input);
        }

        let changed = match self.mode {
            VimMode::Insert => self.handle_insert(input, textarea),
            VimMode::Normal | VimMode::Visual => self.handle_command(input, textarea),
        };
        self.configure_block(textarea, theme);
        changed
    }

    fn handle_insert(&mut self, input: Input, textarea: &mut TextArea<'_>) -> bool {
        if input.key == Key::Esc {
            self.mode = VimMode::Normal;
            textarea.cancel_selection();
            return true;
        }
        textarea.input(input)
    }

    fn handle_command(&mut self, input: Input, textarea: &mut TextArea<'_>) -> bool {
        if input.key == Key::Esc {
            self.mode = VimMode::Normal;
            textarea.cancel_selection();
            return true;
        }

        let visual = self.mode == VimMode::Visual;
        match input {
            Input {
                key: Key::Char('h'),
                ..
            } => move_cursor(textarea, CursorMove::Back, visual),
            Input {
                key: Key::Char('j'),
                ..
            } => move_cursor(textarea, CursorMove::Down, visual),
            Input {
                key: Key::Char('k'),
                ..
            } => move_cursor(textarea, CursorMove::Up, visual),
            Input {
                key: Key::Char('l'),
                ..
            } => move_cursor(textarea, CursorMove::Forward, visual),
            Input {
                key: Key::Char('w'),
                ..
            } => move_cursor(textarea, CursorMove::WordForward, visual),
            Input {
                key: Key::Char('e'),
                ..
            } => move_cursor(textarea, CursorMove::WordEnd, visual),
            Input {
                key: Key::Char('b'),
                ctrl: false,
                ..
            } => move_cursor(textarea, CursorMove::WordBack, visual),
            Input {
                key: Key::Char('^'),
                ..
            } => move_cursor(textarea, CursorMove::Head, visual),
            Input {
                key: Key::Char('$'),
                ..
            } => move_cursor(textarea, CursorMove::End, visual),
            Input {
                key: Key::Char('i'),
                ..
            } => {
                textarea.cancel_selection();
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('a'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::Forward);
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('A'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::End);
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('o'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::End);
                textarea.insert_newline();
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('O'),
                ..
            } => {
                textarea.cancel_selection();
                textarea.move_cursor(CursorMove::Head);
                textarea.insert_newline();
                textarea.move_cursor(CursorMove::Up);
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('v'),
                ..
            } => {
                if self.mode == VimMode::Visual {
                    textarea.cancel_selection();
                    self.mode = VimMode::Normal;
                } else {
                    textarea.start_selection();
                    self.mode = VimMode::Visual;
                }
                true
            }
            Input {
                key: Key::Char('y'),
                ..
            } if self.mode == VimMode::Visual => {
                textarea.copy();
                textarea.cancel_selection();
                self.mode = VimMode::Normal;
                true
            }
            Input {
                key: Key::Char('d'),
                ..
            } if self.mode == VimMode::Visual => {
                textarea.cut();
                self.mode = VimMode::Normal;
                true
            }
            Input {
                key: Key::Char('c'),
                ..
            } if self.mode == VimMode::Visual => {
                textarea.cut();
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('x'),
                ..
            } => textarea.delete_next_char(),
            Input {
                key: Key::Char('D'),
                ..
            } => textarea.delete_line_by_end(),
            Input {
                key: Key::Char('C'),
                ..
            } => {
                textarea.delete_line_by_end();
                self.mode = VimMode::Insert;
                true
            }
            Input {
                key: Key::Char('p'),
                ..
            } => textarea.paste(),
            Input {
                key: Key::Char('u'),
                ctrl: false,
                ..
            } => textarea.undo(),
            Input {
                key: Key::Char('r'),
                ctrl: true,
                ..
            } => textarea.redo(),
            _ => false,
        }
    }
}

fn move_cursor(textarea: &mut TextArea<'_>, movement: CursorMove, visual: bool) -> bool {
    if visual && textarea.selection_range().is_none() {
        textarea.start_selection();
    }
    if visual {
        textarea.input_without_shortcuts(Input {
            key: match movement {
                CursorMove::Back => Key::Left,
                CursorMove::Down => Key::Down,
                CursorMove::Up => Key::Up,
                CursorMove::Forward => Key::Right,
                CursorMove::Head => Key::Home,
                CursorMove::End => Key::End,
                _ => {
                    textarea.move_cursor(movement);
                    return true;
                }
            },
            ctrl: false,
            alt: false,
            shift: true,
        });
    } else {
        textarea.move_cursor(movement);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::ThemeName;

    fn input(ch: char) -> Input {
        Input {
            key: Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    #[test]
    fn vi_insert_esc_returns_to_normal() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::default();
        state.handle(input('i'), &mut textarea, &theme);
        assert_eq!(state.mode, VimMode::Insert);
        state.handle(
            Input {
                key: Key::Esc,
                ctrl: false,
                alt: false,
                shift: false,
            },
            &mut textarea,
            &theme,
        );
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn vi_normal_x_deletes_character() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(vec!["abc".to_string()]);
        state.handle(input('x'), &mut textarea, &theme);
        assert_eq!(textarea.lines(), &["bc".to_string()]);
    }
}
