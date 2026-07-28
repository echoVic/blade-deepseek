use std::io::Write;
use std::path::PathBuf;

use orca_core::config::folder_trust::{self, TrustLevel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrustAction {
    Show,
    Add,
    Remove,
}

#[derive(Clone, Debug)]
pub struct TrustCommandRequest {
    pub cwd: Option<PathBuf>,
    pub action: TrustAction,
}

pub fn run(request: TrustCommandRequest) -> i32 {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with_writers(request, &mut stdout, &mut stderr)
}

pub fn run_with_writers(
    request: TrustCommandRequest,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let cwd = request
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    match request.action {
        TrustAction::Show => {
            let _ = writeln!(
                stdout,
                "{}: {}",
                cwd.display(),
                trust_level_label(folder_trust::trust_level(&cwd))
            );
            0
        }
        TrustAction::Add => match folder_trust::set_trust(&cwd, TrustLevel::Trusted) {
            Ok(()) => {
                let _ = writeln!(stdout, "trusted {}", cwd.display());
                0
            }
            Err(error) => {
                let _ = writeln!(stderr, "orca: failed to trust folder: {error}");
                1
            }
        },
        TrustAction::Remove => match folder_trust::set_trust(&cwd, TrustLevel::Untrusted) {
            Ok(()) => {
                let _ = writeln!(stdout, "marked {} untrusted", cwd.display());
                0
            }
            Err(error) => {
                let _ = writeln!(stderr, "orca: failed to update folder trust: {error}");
                1
            }
        },
    }
}

fn trust_level_label(level: Option<TrustLevel>) -> &'static str {
    match level {
        Some(TrustLevel::Trusted) => "trusted",
        Some(TrustLevel::Untrusted) => "untrusted",
        None => "unknown (treated as untrusted)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_trust_level_has_explicit_safe_label() {
        assert_eq!(trust_level_label(None), "unknown (treated as untrusted)");
    }
}
