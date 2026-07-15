//! Write text to the system clipboard from inside the TUI.
//!
//! Primary channel is OSC 52 written straight to stdout: the terminal
//! emulator (VS Code, iTerm2, kitty, WezTerm, ...) performs the clipboard
//! write, so it also works over SSH. Inside tmux the sequence is wrapped in a
//! DCS passthrough envelope. Very large selections skip OSC 52 (terminals cap
//! the sequence length, commonly around 100 KB) and rely on the local
//! fallback: `pbcopy` on macOS, `wl-copy`/`xclip` elsewhere on Unix.

use std::io::{self, Write as _};

use crate::selection::{osc52_copy_sequence, tmux_passthrough};

/// Above this size the OSC 52 write is skipped: common terminals silently
/// truncate or drop oversized sequences, which would make the "copied" notice
/// a lie. The local fallback still receives the full text.
pub(crate) const OSC52_MAX_TEXT_BYTES: usize = 100_000;

pub(crate) fn copy_to_clipboard(text: &str) {
    // The OSC 52 write is an in-memory buffer flush on the UI thread; the
    // terminal does the actual clipboard work.
    if text.len() <= OSC52_MAX_TEXT_BYTES {
        let sequence = osc52_copy_sequence(text);
        let sequence = if std::env::var_os("TMUX").is_some() {
            tmux_passthrough(&sequence)
        } else {
            sequence
        };
        let mut stdout = io::stdout();
        let _ = stdout.write_all(sequence.as_bytes());
        let _ = stdout.flush();
    }

    // Local fallbacks spawn and wait on a child process; a slow or wedged
    // helper must not freeze the UI, so they run on their own thread.
    let owned = text.to_owned();
    std::thread::spawn(move || local_clipboard_best_effort(&owned));
}

#[cfg(target_os = "macos")]
fn local_clipboard_best_effort(text: &str) {
    pipe_through(&["pbcopy"], text);
}

#[cfg(all(unix, not(target_os = "macos")))]
fn local_clipboard_best_effort(text: &str) {
    if !pipe_through(&["wl-copy"], text) {
        pipe_through(&["xclip", "-selection", "clipboard"], text);
    }
}

#[cfg(not(unix))]
fn local_clipboard_best_effort(_text: &str) {}

#[cfg(unix)]
fn pipe_through(command: &[&str], text: &str) -> bool {
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new(command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.wait();
            return false;
        }
    }
    matches!(child.wait(), Ok(status) if status.success())
}
