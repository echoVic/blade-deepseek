use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::sandbox::{ReadOnlySandboxCommandContext, WorkspaceWriteSandboxCommandContext};

static SEATBELT_AVAILABLE: OnceLock<bool> = OnceLock::new();

struct WorkspaceWriteProfileContext<'a> {
    cwd: &'a Path,
    readable_roots: &'a [PathBuf],
    additional_roots: &'a [PathBuf],
    denied_roots: &'a [PathBuf],
    network_access: bool,
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
    allowed_unix_socket_roots: &'a [PathBuf],
}

struct ReadOnlyProfileContext<'a> {
    readable_roots: &'a [PathBuf],
    additional_roots: &'a [PathBuf],
    denied_roots: &'a [PathBuf],
    network_access: bool,
    allow_global_read: bool,
    allowed_unix_socket_roots: &'a [PathBuf],
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
    if !available() {
        return plain_bash_command(context.command, context.cwd);
    }

    let canonical_cwd = context
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| context.cwd.to_path_buf());
    let canonical_readable_roots = context
        .readable_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_additional_roots = context
        .additional_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_denied_roots = context
        .denied_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: &canonical_cwd,
            readable_roots: &canonical_readable_roots,
            additional_roots: &canonical_additional_roots,
            denied_roots: &canonical_denied_roots,
            network_access: context.network_access,
            exclude_tmpdir_env_var: context.exclude_tmpdir_env_var,
            exclude_slash_tmp: context.exclude_slash_tmp,
            allowed_unix_socket_roots: context.allowed_unix_socket_roots,
        }))
        .arg("sh")
        .arg("-c")
        .arg(context.command)
        .current_dir(context.cwd);
    cmd
}

pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
    if !available() {
        return plain_bash_command(context.command, context.cwd);
    }

    let canonical_readable_roots = context
        .readable_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_additional_roots = context
        .additional_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_denied_roots = context
        .denied_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(read_only_profile(ReadOnlyProfileContext {
            readable_roots: &canonical_readable_roots,
            additional_roots: &canonical_additional_roots,
            denied_roots: &canonical_denied_roots,
            network_access: context.network_access,
            allow_global_read: context.allow_global_read,
            allowed_unix_socket_roots: context.allowed_unix_socket_roots,
        }))
        .arg("sh")
        .arg("-c")
        .arg(context.command)
        .current_dir(context.cwd);
    cmd
}

pub fn available() -> bool {
    *SEATBELT_AVAILABLE.get_or_init(|| {
        Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

pub fn platform_default_read_roots() -> Vec<PathBuf> {
    ["/bin", "/sbin", "/usr", "/System", "/Library"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn plain_bash_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    cmd
}

fn workspace_write_profile(context: WorkspaceWriteProfileContext<'_>) -> String {
    let WorkspaceWriteProfileContext {
        cwd,
        readable_roots,
        additional_roots,
        denied_roots,
        network_access,
        exclude_tmpdir_env_var,
        exclude_slash_tmp,
        allowed_unix_socket_roots,
    } = context;
    let cwd_escaped = seatbelt_escape(&cwd.display().to_string());
    let additional_read_rules = read_allow_rules(readable_roots);
    let protected_metadata_rules = protected_workspace_metadata_deny_rules(cwd);
    let additional_write_rules = additional_roots
        .iter()
        .map(|root| {
            format!(
                r#"(allow file-write* (subpath "{}"))"#,
                seatbelt_escape(&root.display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let denied_access_rules = access_deny_rules(denied_roots);
    let home = dirs::home_dir();
    let ssh_deny = home
        .as_ref()
        .map(|home| {
            format!(
                r#"(deny file-read* file-write* (subpath "{}"))"#,
                seatbelt_escape(&format!("{}/.ssh", home.display()))
            )
        })
        .unwrap_or_default();
    let orca_deny = home
        .as_ref()
        .map(|home| {
            format!(
                r#"(deny file-read* file-write* (subpath "{}"))"#,
                seatbelt_escape(&format!("{}/.orca", home.display()))
            )
        })
        .unwrap_or_default();
    let tmpdir_write = if exclude_tmpdir_env_var {
        String::new()
    } else {
        std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok().or(Some(path)))
            .map(|path| {
                format!(
                    r#"(allow file-write* (subpath "{}"))"#,
                    seatbelt_escape(&path.display().to_string())
                )
            })
            .unwrap_or_default()
    };
    let slash_tmp_write = if exclude_slash_tmp {
        String::new()
    } else {
        slash_tmp_write_rules()
    };
    let network_rule = if network_access {
        "(allow network-outbound)"
    } else {
        ""
    };
    let unix_socket_rules = unix_socket_allow_rules(allowed_unix_socket_roots);
    // Seatbelt uses last-match-wins: deny rules MUST come after allow to override.
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow signal (target self))
(allow file-read*)
(allow file-read* file-write* (literal "/dev/null"))
(allow file-write* (subpath "{cwd_escaped}"))
{additional_read_rules}
{tmpdir_write}
{slash_tmp_write}
{protected_metadata_rules}
{additional_write_rules}
{denied_access_rules}
{ssh_deny}
{orca_deny}
{network_rule}
{unix_socket_rules}
"#
    )
}

fn read_only_profile(context: ReadOnlyProfileContext<'_>) -> String {
    let ReadOnlyProfileContext {
        readable_roots,
        additional_roots,
        denied_roots,
        network_access,
        allow_global_read,
        allowed_unix_socket_roots,
    } = context;
    let additional_read_rules = read_allow_rules(readable_roots);
    let additional_write_rules = additional_roots
        .iter()
        .map(|root| {
            format!(
                r#"(allow file-write* (subpath "{}"))"#,
                seatbelt_escape(&root.display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let denied_access_rules = access_deny_rules(denied_roots);
    let network_rule = if network_access {
        "(allow network-outbound)"
    } else {
        ""
    };
    let global_read_rule = if allow_global_read {
        "(allow file-read*)"
    } else {
        ""
    };
    let unix_socket_rules = unix_socket_allow_rules(allowed_unix_socket_roots);
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow signal (target self))
{global_read_rule}
(allow file-read* file-write* (literal "/dev/null"))
{additional_read_rules}
{additional_write_rules}
{denied_access_rules}
{network_rule}
{unix_socket_rules}
"#
    )
}

fn unix_socket_allow_rules(allowed_unix_socket_roots: &[PathBuf]) -> String {
    if allowed_unix_socket_roots.is_empty() {
        return String::new();
    }
    let socket_rules = allowed_unix_socket_roots
        .iter()
        .map(|root| {
            let root = seatbelt_escape(&root.display().to_string());
            format!(
                r#"(allow network-bind (local unix-socket (subpath "{root}")))
(allow network-outbound (remote unix-socket (subpath "{root}")))"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("(allow system-socket (socket-domain AF_UNIX))\n{socket_rules}")
}

fn read_allow_rules(readable_roots: &[PathBuf]) -> String {
    readable_roots
        .iter()
        .map(|root| {
            format!(
                r#"(allow file-read* (subpath "{}"))"#,
                seatbelt_escape(&root.display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn access_deny_rules(denied_roots: &[PathBuf]) -> String {
    denied_roots
        .iter()
        .map(|root| {
            let path_matcher = if root.is_file() { "literal" } else { "subpath" };
            format!(
                r#"(deny file-read* file-write* ({path_matcher} "{}"))"#,
                seatbelt_escape(&root.display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Allow rules for `/tmp` writes. On macOS `/tmp` is a symlink to
/// `/private/tmp` and Seatbelt matches the resolved path, so a rule for the
/// literal `/tmp` subpath alone never applies; emit the canonical path too.
fn slash_tmp_write_rules() -> String {
    let mut rules = vec![r#"(allow file-write* (subpath "/tmp"))"#.to_string()];
    if let Ok(canonical) = Path::new("/tmp").canonicalize()
        && canonical != Path::new("/tmp")
    {
        rules.push(format!(
            r#"(allow file-write* (subpath "{}"))"#,
            seatbelt_escape(&canonical.display().to_string())
        ));
    }
    rules.join("\n")
}

fn protected_workspace_metadata_deny_rules(cwd: &Path) -> String {
    [".git", ".agents", ".codex"]
        .into_iter()
        .map(|name| {
            format!(
                r#"(deny file-write* (subpath "{}"))"#,
                seatbelt_escape(&cwd.join(name).display().to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn seatbelt_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Output;
    use tempfile::TempDir;

    #[test]
    fn sandbox_profile_allows_workspace_and_null_device() {
        let workspace = TempDir::new().unwrap();
        let content = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        assert!(content.contains("(version 1)"));
        assert!(content.contains(&workspace.path().display().to_string()));
        assert!(content.contains(r#"(allow file-read* file-write* (literal "/dev/null"))"#));
    }

    #[test]
    fn platform_default_read_roots_include_shell_runtime_paths() {
        let roots = platform_default_read_roots();

        assert!(roots.contains(&PathBuf::from("/bin")));
        assert!(roots.contains(&PathBuf::from("/usr")));
        assert!(roots.contains(&PathBuf::from("/System")));
    }

    #[test]
    fn profile_denies_sensitive_orca_and_ssh_paths() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains("(deny file-read* file-write*"));
        assert!(profile.contains("/.ssh"));
        assert!(profile.contains("/.orca"));
        // deny rules must come AFTER allow rules (last-match-wins in Seatbelt)
        let allow_write_pos = profile.find("(allow file-write*").unwrap();
        let deny_pos = profile.find("(deny file-read* file-write*").unwrap();
        assert!(
            deny_pos > allow_write_pos,
            "deny must come after allow for last-match-wins"
        );
    }

    #[test]
    fn workspace_write_profile_protects_workspace_metadata_by_default() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let allow_workspace = format!(
            r#"(allow file-write* (subpath "{}"))"#,
            workspace.path().display()
        );
        let deny_git = format!(
            r#"(deny file-write* (subpath "{}"))"#,
            workspace.path().join(".git").display()
        );
        let deny_git_reads = format!(
            r#"(deny file-read* file-write* (subpath "{}"))"#,
            workspace.path().join(".git").display()
        );

        assert!(profile.contains(&deny_git), "{profile}");
        assert!(
            !profile.contains(&deny_git_reads),
            "metadata protection must preserve reads for git commands: {profile}"
        );
        assert!(
            profile.find(&deny_git).unwrap() > profile.find(&allow_workspace).unwrap(),
            "metadata deny must override workspace write: {profile}"
        );
    }

    #[test]
    fn workspace_write_profile_allows_explicit_metadata_write_root() {
        let workspace = TempDir::new().unwrap();
        let git_dir = workspace.path().join(".git");
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: std::slice::from_ref(&git_dir),
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let deny_git = format!(r#"(deny file-write* (subpath "{}"))"#, git_dir.display());
        let allow_git = format!(r#"(allow file-write* (subpath "{}"))"#, git_dir.display());

        assert!(profile.contains(&deny_git), "{profile}");
        assert!(profile.contains(&allow_git), "{profile}");
        assert!(
            profile.find(&allow_git).unwrap() > profile.find(&deny_git).unwrap(),
            "explicit metadata grant must override default metadata protection: {profile}"
        );
    }

    #[test]
    fn workspace_write_sandbox_blocks_workspace_git_writes_by_default() {
        if !available() {
            return;
        }

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let target = git_dir.join("config");

        let output = bash_command(
            &format!("printf blocked > {}", target.display()),
            workspace.path(),
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn workspace_write_sandbox_allows_workspace_git_reads_by_default() {
        if !available() {
            return;
        }

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[core]\nrepositoryformatversion = 0\n",
        )
        .unwrap();

        let output = bash_command("cat .git/config", workspace.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "workspace metadata reads must be allowed for git commands\nstatus: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("repositoryformatversion"));
    }

    #[test]
    fn workspace_write_sandbox_allows_explicit_workspace_git_write_root() {
        if !available() {
            return;
        }

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let target = git_dir.join("config");

        let output = bash_command_with_additional_roots(
            &format!("printf allowed > {}", target.display()),
            workspace.path(),
            std::slice::from_ref(&git_dir),
        )
        .output()
        .unwrap();

        assert!(output.status.success());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "allowed");
    }

    #[test]
    fn read_only_profile_does_not_allow_workspace_writes() {
        let workspace = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains(&workspace.path().display().to_string()));
        assert!(!profile.contains("file-write* (subpath"));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn read_only_profile_allows_additional_write_roots() {
        let workspace = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[],
            additional_roots: &[extra.path().to_path_buf()],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains(&workspace.path().display().to_string()));
        assert!(profile.contains(&format!(
            r#"(allow file-write* (subpath "{}"))"#,
            extra.path().display()
        )));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn read_only_profile_allows_additional_read_roots_without_writes() {
        let readable = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[readable.path().to_path_buf()],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains(&format!(
            r#"(allow file-read* (subpath "{}"))"#,
            readable.path().display()
        )));
        assert!(!profile.contains(&format!(
            r#"(allow file-write* (subpath "{}"))"#,
            readable.path().display()
        )));
    }

    #[test]
    fn read_only_profile_denies_additional_root_descendant_access() {
        let extra = TempDir::new().unwrap();
        let blocked = extra.path().join("blocked");
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[],
            additional_roots: &[extra.path().to_path_buf()],
            denied_roots: std::slice::from_ref(&blocked),
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        let allow = format!(
            r#"(allow file-write* (subpath "{}"))"#,
            extra.path().display()
        );
        let deny = format!(
            r#"(deny file-read* file-write* (subpath "{}"))"#,
            blocked.display()
        );

        assert!(profile.contains(&allow));
        assert!(profile.contains(&deny));
        assert!(
            profile.find(&deny).unwrap() > profile.find(&allow).unwrap(),
            "deny access rules must come after allow rules"
        );
    }

    #[test]
    fn read_only_profile_uses_literal_deny_rules_for_files() {
        let extra = TempDir::new().unwrap();
        let denied_file = extra.path().join("secret.env");
        std::fs::write(&denied_file, "secret").unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[],
            additional_roots: &[extra.path().to_path_buf()],
            denied_roots: std::slice::from_ref(&denied_file),
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains(&format!(
            r#"(deny file-read* file-write* (literal "{}"))"#,
            denied_file.display()
        )));
    }

    #[test]
    fn read_only_profile_can_disable_global_reads() {
        let readable = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[readable.path().to_path_buf()],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains("\n(allow file-read*)\n"));
        assert!(profile.contains(&format!(
            r#"(allow file-read* (subpath "{}"))"#,
            readable.path().display()
        )));
    }

    #[test]
    fn strict_read_only_sandbox_blocks_reads_outside_allowed_roots() {
        if !available() {
            return;
        }

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace_path = parent.path().join("workspace");
        let readable_path = parent.path().join("readable");
        std::fs::create_dir(&workspace_path).unwrap();
        std::fs::create_dir(&readable_path).unwrap();
        let allowed = readable_path.join("allowed.txt");
        let blocked = parent.path().join("blocked.txt");
        std::fs::write(&allowed, "allowed").unwrap();
        std::fs::write(&blocked, "blocked").unwrap();

        let command_text = format!(
            "cat {} >/dev/null && cat {} >/dev/null",
            allowed.display(),
            blocked.display()
        );
        let output: Output = read_only_bash_command(ReadOnlySandboxCommandContext {
            command: &command_text,
            cwd: &workspace_path,
            readable_roots: &[readable_path],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(
            !output.status.success(),
            "strict read-only sandbox should reject unlisted reads"
        );
    }

    #[test]
    fn workspace_write_profile_includes_canonical_slash_tmp_rule() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains(r#"(allow file-write* (subpath "/tmp"))"#));
        if let Ok(canonical) = Path::new("/tmp").canonicalize()
            && canonical != Path::new("/tmp")
        {
            assert!(
                profile.contains(&format!(
                    r#"(allow file-write* (subpath "{}"))"#,
                    canonical.display()
                )),
                "profile must allow the resolved /tmp path (seatbelt matches resolved paths): {profile}"
            );
        }
    }

    #[test]
    fn workspace_write_sandbox_allows_writes_under_slash_tmp() {
        if !available() {
            return;
        }

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let tmp_target = TempDir::new_in("/tmp").unwrap();
        let target = tmp_target.path().join("allowed.txt");

        let output = bash_command(
            &format!("printf allowed > {}", target.display()),
            workspace.path(),
        )
        .output()
        .unwrap();

        assert!(
            output.status.success(),
            "writes under /tmp must be allowed by the workspace profile\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "allowed");
    }

    #[test]
    fn workspace_write_profile_can_exclude_tmp_writes_and_network() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains(r#"(subpath "/tmp")"#));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn workspace_write_profile_allows_configured_unix_sockets_without_full_network() {
        let workspace = TempDir::new().unwrap();
        let socket_root = PathBuf::from("/tmp/orca-browser.sock");
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
            allowed_unix_socket_roots: &[socket_root],
        });

        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        assert!(
            profile.contains(
                r#"(allow network-bind (local unix-socket (subpath "/tmp/orca-browser.sock")))"#
            ),
            "profile should allow binding the configured unix socket: {profile}"
        );
        assert!(
            profile.contains(r#"(allow network-outbound (remote unix-socket (subpath "/tmp/orca-browser.sock")))"#),
            "profile should allow outbound traffic to the configured unix socket: {profile}"
        );
        assert!(!profile.contains("\n(allow network-outbound)\n"));
    }

    #[test]
    fn read_only_profile_allows_configured_unix_sockets_without_full_network() {
        let socket_root = PathBuf::from("/tmp/orca-browser.sock");
        let profile = read_only_profile(ReadOnlyProfileContext {
            readable_roots: &[],
            additional_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[socket_root],
        });

        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        assert!(
            profile.contains(r#"(allow network-outbound (remote unix-socket (subpath "/tmp/orca-browser.sock")))"#),
            "profile should allow outbound traffic to the configured unix socket: {profile}"
        );
        assert!(!profile.contains("\n(allow network-outbound)\n"));
    }

    #[test]
    fn sandbox_blocks_writes_outside_workspace() {
        if !available() {
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

    #[test]
    fn sandbox_allows_writes_to_additional_roots() {
        if !available() {
            return;
        }

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace_path = parent.path().join("workspace");
        let extra = parent.path().join("extra");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&workspace_path).unwrap();
        std::fs::create_dir(&extra).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let extra_file = extra.join("allowed.txt");
        let outside_file = outside.join("blocked.txt");

        let output: Output = bash_command_with_additional_roots(
            &format!(
                "printf allowed > {} && printf blocked > {}",
                extra_file.display(),
                outside_file.display()
            ),
            &workspace_path,
            &[extra],
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(extra_file).unwrap(), "allowed");
        assert!(!outside_file.exists());
    }

    #[test]
    fn sandbox_allows_basic_shell_commands_and_null_device() {
        if !available() {
            return;
        }

        let workspace = TempDir::new().unwrap();
        let output: Output = bash_command("printf ok >/dev/null && printf done", workspace.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "status: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "done");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }
}
