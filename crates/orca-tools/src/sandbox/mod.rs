use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "macos")]
pub mod seatbelt;

pub fn bash_command(command: &str, cwd: &Path) -> Command {
    workspace_write_bash_command(command, cwd, &[], &[], true, false, false)
}

pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
    let mut command = platform::plain_bash_command(command, cwd);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn bash_command_with_additional_roots(
    command: &str,
    cwd: &Path,
    additional_roots: &[PathBuf],
) -> Command {
    workspace_write_bash_command(command, cwd, additional_roots, &[], true, false, false)
}

pub fn workspace_write_bash_command(
    command: &str,
    cwd: &Path,
    additional_roots: &[PathBuf],
    denied_roots: &[PathBuf],
    network_access: bool,
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
) -> Command {
    let mut command = platform::workspace_write_bash_command(
        command,
        cwd,
        additional_roots,
        denied_roots,
        network_access,
        exclude_tmpdir_env_var,
        exclude_slash_tmp,
    );
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn read_only_bash_command(
    command: &str,
    cwd: &Path,
    additional_roots: &[PathBuf],
    denied_roots: &[PathBuf],
    network_access: bool,
) -> Command {
    let mut command = platform::read_only_bash_command(
        command,
        cwd,
        additional_roots,
        denied_roots,
        network_access,
    );
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

#[cfg(test)]
pub fn seatbelt_available() -> bool {
    platform::seatbelt_available()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn workspace_write_bash_command(
        command: &str,
        cwd: &Path,
        additional_roots: &[PathBuf],
        denied_roots: &[PathBuf],
        network_access: bool,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Command {
        crate::sandbox::seatbelt::workspace_write_bash_command(
            command,
            cwd,
            additional_roots,
            denied_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        )
    }

    pub fn read_only_bash_command(
        command: &str,
        cwd: &Path,
        additional_roots: &[PathBuf],
        denied_roots: &[PathBuf],
        network_access: bool,
    ) -> Command {
        crate::sandbox::seatbelt::read_only_bash_command(
            command,
            cwd,
            additional_roots,
            denied_roots,
            network_access,
        )
    }

    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
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

            let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
            let workspace_path = parent.path().join("workspace");
            std::fs::create_dir(&workspace_path).unwrap();
            let outside = parent.path().join("blocked.txt");

            let output: Output = bash_command(
                &format!("printf blocked > {}", outside.display()),
                &workspace_path,
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

    pub fn workspace_write_bash_command(
        command: &str,
        cwd: &Path,
        _additional_roots: &[PathBuf],
        _denied_roots: &[PathBuf],
        _network_access: bool,
        _exclude_tmpdir_env_var: bool,
        _exclude_slash_tmp: bool,
    ) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    pub fn read_only_bash_command(
        command: &str,
        cwd: &Path,
        _additional_roots: &[PathBuf],
        _denied_roots: &[PathBuf],
        _network_access: bool,
    ) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        false
    }
}
