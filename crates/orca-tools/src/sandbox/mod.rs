use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "macos")]
pub mod seatbelt;

// Compiled on all platforms so the bwrap argv builder can be unit tested off
// Linux; only the Linux platform block actually launches bwrap.
pub mod bwrap;
// The launch helpers are only invoked from the non-macOS (Linux) platform
// block; keep the module compiled everywhere for testing without warnings.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub mod linux;

/// Platform read roots a Linux shell runtime needs when the sandbox root is a
/// fresh tmpfs. Exposed here so the pure `bwrap` argv builder (compiled on all
/// platforms) can consult the same list the Linux backend uses.
pub(crate) fn linux_platform_default_read_roots() -> Vec<PathBuf> {
    linux::platform_default_read_roots()
}

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

#[cfg(test)]
pub(crate) fn sandbox_test_parent(prefix: &str) -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    {
        let home = PathBuf::from(
            std::env::var_os("HOME").expect("HOME is required for macOS Seatbelt tests"),
        )
        .canonicalize()
        .expect("canonical macOS HOME");
        for root in [
            Some(PathBuf::from("/tmp")),
            std::env::var_os("TMPDIR").map(PathBuf::from),
        ]
        .into_iter()
        .flatten()
        {
            let root = root.canonicalize().unwrap_or(root);
            assert!(
                !home.starts_with(&root),
                "macOS Seatbelt fixtures require HOME outside temporary allow root {}",
                root.display()
            );
        }
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(home)
            .expect("sandbox parent outside temporary allow roots")
    }
    #[cfg(not(target_os = "macos"))]
    {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("sandbox parent")
    }
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

        #[test]
        fn sandbox_blocks_writes_outside_workspace() {
            if !seatbelt_available() {
                return;
            }

            let parent = crate::sandbox::sandbox_test_parent("sandbox-module-deny-");
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

    /// Protected workspace metadata directories: readable but not writable by
    /// default, matching the macOS Seatbelt backend.
    #[cfg(target_os = "linux")]
    const PROTECTED_METADATA_DIRS: [&str; 3] = [".git", ".agents", ".codex"];

    #[cfg(target_os = "linux")]
    fn canonicalize_all(roots: &[PathBuf]) -> Vec<PathBuf> {
        roots
            .iter()
            .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn linux_sensitive_denied_roots() -> Vec<PathBuf> {
        let mut roots = dirs::home_dir()
            .map(|home| vec![home.join(".ssh"), home.join(".orca")])
            .unwrap_or_default();
        if let Some(config_dir) = orca_core::config::folder_trust::config_dir()
            && !roots.contains(&config_dir)
        {
            roots.push(config_dir);
        }
        roots.retain(|path| path.exists());
        roots
    }

    #[cfg(target_os = "linux")]
    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        use crate::sandbox::bwrap::{LinuxReadScope, LinuxSandboxPolicy};
        use crate::sandbox::linux::{LinuxSandboxRequest, sandbox_command};

        let cwd = context
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| context.cwd.to_path_buf());

        // Writable: the workspace cwd plus any explicit additional roots, plus
        // temp dirs unless excluded.
        let additional_roots = canonicalize_all(context.additional_roots);
        let mut writable_roots = vec![cwd.clone()];
        for root in &additional_roots {
            if !writable_roots.contains(root) {
                writable_roots.push(root.clone());
            }
        }
        if !context.exclude_slash_tmp {
            writable_roots.push(PathBuf::from("/tmp"));
        }
        if !context.exclude_tmpdir_env_var
            && let Some(tmpdir) = std::env::var_os("TMPDIR").map(PathBuf::from)
        {
            let tmpdir = tmpdir.canonicalize().unwrap_or(tmpdir);
            if !writable_roots.contains(&tmpdir) {
                writable_roots.push(tmpdir);
            }
        }

        // Protect workspace metadata (readable, not writable) unless the caller
        // explicitly granted it as a writable additional root.
        let mut read_only_roots = Vec::new();
        for name in PROTECTED_METADATA_DIRS {
            let metadata = cwd.join(name);
            if metadata.exists()
                && !additional_roots
                    .iter()
                    .any(|root| metadata.starts_with(root))
            {
                read_only_roots.push(metadata);
            }
        }

        let mut denied_roots = canonicalize_all(context.denied_roots);
        for root in linux_sensitive_denied_roots() {
            if !denied_roots.contains(&root) {
                denied_roots.push(root);
            }
        }

        let request = LinuxSandboxRequest {
            command: context.command.to_string(),
            policy: LinuxSandboxPolicy {
                cwd,
                read_scope: LinuxReadScope::Global,
                readable_roots: canonicalize_all(context.readable_roots),
                allowed_unix_socket_roots: canonicalize_all(context.allowed_unix_socket_roots),
                writable_roots,
                read_only_roots,
                denied_roots,
                network_access: context.network_access,
            },
            strict: false,
        };
        sandbox_command(request)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(context.command).current_dir(context.cwd);
        cmd
    }

    #[cfg(target_os = "linux")]
    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        use crate::sandbox::bwrap::{LinuxReadScope, LinuxSandboxPolicy};
        use crate::sandbox::linux::{LinuxSandboxRequest, sandbox_command};

        let cwd = context
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| context.cwd.to_path_buf());

        // Additional roots are writable even in read-only mode (e.g. an
        // explicitly granted output directory), matching the Seatbelt profile.
        let writable_roots = canonicalize_all(context.additional_roots);

        // A restricted read scope (allow_global_read == false) is fail-closed:
        // only listed roots are visible.
        let read_scope = if context.allow_global_read {
            LinuxReadScope::Global
        } else {
            LinuxReadScope::Restricted
        };

        let mut denied_roots = canonicalize_all(context.denied_roots);
        for root in linux_sensitive_denied_roots() {
            if !denied_roots.contains(&root) {
                denied_roots.push(root);
            }
        }

        let request = LinuxSandboxRequest {
            command: context.command.to_string(),
            policy: LinuxSandboxPolicy {
                cwd,
                read_scope,
                readable_roots: canonicalize_all(context.readable_roots),
                allowed_unix_socket_roots: canonicalize_all(context.allowed_unix_socket_roots),
                writable_roots,
                read_only_roots: Vec::new(),
                denied_roots,
                network_access: context.network_access,
            },
            // Strict read-only (no global read) fails closed if unenforceable.
            strict: !context.allow_global_read,
        };
        sandbox_command(request)
    }

    #[cfg(not(target_os = "linux"))]
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
        #[cfg(target_os = "linux")]
        {
            crate::sandbox::linux::platform_default_read_roots()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        false
    }
}
