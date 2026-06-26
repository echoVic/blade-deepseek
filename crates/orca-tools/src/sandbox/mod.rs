use std::path::Path;
use std::process::Command;

#[cfg(target_os = "macos")]
pub mod seatbelt;

pub fn bash_command(command: &str, cwd: &Path) -> Command {
    let mut command = platform::bash_command(command, cwd);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn bash_command(command: &str, cwd: &Path) -> Command {
        crate::sandbox::seatbelt::bash_command(command, cwd)
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        crate::sandbox::seatbelt::available()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Output;
        use tempfile::TempDir;

        #[test]
        fn sandbox_blocks_writes_outside_workspace() {
            if !seatbelt_available() {
                return;
            }

            let workspace = TempDir::new().unwrap();
            let outside = std::env::temp_dir().join(format!(
                "orca-sandbox-test-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));

            let output: Output = bash_command(
                &format!("printf blocked > {}", outside.display()),
                workspace.path(),
            )
            .output()
            .unwrap();

            assert!(!output.status.success());
            assert!(!outside.exists());
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        false
    }
}
