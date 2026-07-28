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
        TrustAction::Show => write_success(
            stdout,
            format_args!(
                "{}: {}",
                cwd.display(),
                trust_level_label(folder_trust::trust_level(&cwd))
            ),
        ),
        TrustAction::Add => match folder_trust::set_trust(&cwd, TrustLevel::Trusted) {
            Ok(()) => write_success(stdout, format_args!("trusted {}", cwd.display())),
            Err(error) => {
                let _ = writeln!(stderr, "orca: failed to trust folder: {error}");
                1
            }
        },
        TrustAction::Remove => match folder_trust::set_trust(&cwd, TrustLevel::Untrusted) {
            Ok(()) => write_success(stdout, format_args!("marked {} untrusted", cwd.display())),
            Err(error) => {
                let _ = writeln!(stderr, "orca: failed to update folder trust: {error}");
                1
            }
        },
    }
}

fn write_success(writer: &mut impl Write, args: std::fmt::Arguments<'_>) -> i32 {
    if writeln!(writer, "{args}").is_ok() {
        0
    } else {
        1
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
    use std::io;

    use super::*;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unknown_trust_level_has_explicit_safe_label() {
        assert_eq!(trust_level_label(None), "unknown (treated as untrusted)");
    }

    #[test]
    fn show_returns_failure_when_stdout_is_closed() {
        let temp = tempfile::tempdir().unwrap();
        let mut stdout = FailingWriter;
        let mut stderr = Vec::new();

        let code = run_with_writers(
            TrustCommandRequest {
                cwd: Some(temp.path().to_path_buf()),
                action: TrustAction::Show,
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 1);
    }

    #[test]
    fn shared_success_writer_reports_closed_output() {
        assert_eq!(
            write_success(&mut FailingWriter, format_args!("success")),
            1
        );
    }
}
