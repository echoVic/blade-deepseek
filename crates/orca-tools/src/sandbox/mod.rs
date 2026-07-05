use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "macos")]
pub mod seatbelt;

pub struct WorkspaceWriteSandboxCommandContext<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub readable_roots: &'a [PathBuf],
    pub additional_roots: &'a [PathBuf],
    pub denied_roots: &'a [PathBuf],
    pub network_access: bool,
    pub exclude_tmpdir_env_var: bool,
    pub exclude_slash_tmp: bool,
    pub allowed_unix_socket_roots: &'a [PathBuf],
}

pub struct ReadOnlySandboxCommandContext<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub readable_roots: &'a [PathBuf],
    pub additional_roots: &'a [PathBuf],
    pub denied_roots: &'a [PathBuf],
    pub network_access: bool,
    pub allow_global_read: bool,
    pub allowed_unix_socket_roots: &'a [PathBuf],
}

pub fn bash_command(command: &str, cwd: &Path) -> Command {
    workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
        command,
        cwd,
        readable_roots: &[],
        additional_roots: &[],
        denied_roots: &[],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
        allowed_unix_socket_roots: &[],
    })
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
    workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
        command,
        cwd,
        readable_roots: &[],
        additional_roots,
        denied_roots: &[],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
        allowed_unix_socket_roots: &[],
    })
}

pub fn workspace_write_bash_command(context: WorkspaceWriteSandboxCommandContext<'_>) -> Command {
    let mut command = platform::workspace_write_bash_command(context);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
    let mut command = platform::read_only_bash_command(context);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn platform_default_read_roots() -> Vec<PathBuf> {
    platform::platform_default_read_roots()
}

#[cfg(test)]
pub fn seatbelt_available() -> bool {
    platform::seatbelt_available()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        crate::sandbox::seatbelt::workspace_write_bash_command(context)
    }

    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        crate::sandbox::seatbelt::read_only_bash_command(context)
    }

    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    pub fn platform_default_read_roots() -> Vec<PathBuf> {
        crate::sandbox::seatbelt::platform_default_read_roots()
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
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(context.command).current_dir(context.cwd);
        cmd
    }

    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(context.command).current_dir(context.cwd);
        cmd
    }

    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    pub fn platform_default_read_roots() -> Vec<PathBuf> {
        Vec::new()
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        false
    }
}
