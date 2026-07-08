use tui_textarea::{CursorMove, TextArea};

use crate::theme::Theme;
use crate::vim::VimState;

pub(crate) fn make_textarea<'a>(vim_state: &VimState, theme: &Theme) -> TextArea<'a> {
    let mut textarea = TextArea::default();
    configure_textarea(&mut textarea, vim_state, theme);
    textarea
}

pub(crate) fn make_textarea_with_text<'a>(
    text: &str,
    vim_state: &VimState,
    theme: &Theme,
) -> TextArea<'a> {
    let lines: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines().map(str::to_string).collect()
    };
    let mut textarea = TextArea::from(lines);
    configure_textarea(&mut textarea, vim_state, theme);
    textarea.move_cursor(CursorMove::Bottom);
    textarea.move_cursor(CursorMove::End);
    textarea
}

fn configure_textarea(textarea: &mut TextArea, vim_state: &VimState, theme: &Theme) {
    textarea.set_placeholder_text("Type a message... (Enter send, Alt+Enter newline)");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    vim_state.configure_block(textarea, theme);
}

pub(crate) fn textarea_text(textarea: &TextArea) -> String {
    textarea.lines().join("\n")
}

pub(crate) fn insert_pasted_text(textarea: &mut TextArea, pasted: &str) -> bool {
    if pasted.is_empty() {
        return false;
    }
    textarea.insert_str(pasted)
}

pub(crate) fn make_setup_textarea<'a>(theme: &Theme) -> TextArea<'a> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("sk-...");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_mask_char('*');
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(" API Key ")
            .border_style(ratatui::style::Style::default().fg(theme.border)),
    );
    textarea
}
