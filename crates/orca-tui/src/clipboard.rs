//! Write text to the system clipboard from inside the TUI.
//!
//! Primary channel is OSC 52 written straight to stdout: the terminal
//! emulator (VS Code, iTerm2, kitty, WezTerm, ...) performs the clipboard
//! write, so it also works over SSH. On macOS we additionally pipe through
//! `pbcopy` as a best-effort fallback for terminals with OSC 52 disabled.

use std::io::{self, Write as _};

use crate::selection::osc52_copy_sequence;

pub(crate) fn copy_to_clipboard(text: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(osc52_copy_sequence(text).as_bytes());
    let _ = stdout.flush();

    #[cfg(target_os = "macos")]
    pbcopy_best_effort(text);
}

#[cfg(target_os = "macos")]
fn pbcopy_best_effort(text: &str) {
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}
