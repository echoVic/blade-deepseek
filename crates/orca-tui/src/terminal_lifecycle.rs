use std::io;

use crossterm::ExecutableCommand;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags};
use crossterm::terminal;

pub(crate) struct TerminalCleanup {
    raw_mode: bool,
    keyboard_enhanced: bool,
    bracketed_paste: bool,
    mouse_captured: bool,
    cleaned: bool,
}

impl TerminalCleanup {
    pub(crate) fn raw_mode_enabled() -> Self {
        Self {
            raw_mode: true,
            keyboard_enhanced: false,
            bracketed_paste: false,
            mouse_captured: false,
            cleaned: false,
        }
    }

    pub(crate) fn set_keyboard_enhanced(&mut self, enabled: bool) {
        self.keyboard_enhanced = enabled;
    }

    pub(crate) fn set_bracketed_paste(&mut self, enabled: bool) {
        self.bracketed_paste = enabled;
    }

    pub(crate) fn set_mouse_captured(&mut self, enabled: bool) {
        self.mouse_captured = enabled;
    }

    pub(crate) fn finish(mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;

        if self.keyboard_enhanced {
            let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
        }
        if self.bracketed_paste {
            let _ = io::stdout().execute(DisableBracketedPaste);
        }
        if self.mouse_captured {
            let _ = io::stdout().execute(DisableMouseCapture);
        }
        let _ = io::stdout().execute(crossterm::cursor::Show);
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
        }
        println!();
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        self.cleanup();
    }
}
