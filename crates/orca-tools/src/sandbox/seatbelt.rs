use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static SEATBELT_AVAILABLE: OnceLock<bool> = OnceLock::new();

pub fn bash_command(command: &str, cwd: &Path) -> Command {
    workspace_write_bash_command(command, cwd, &[], &[], &[], true, false, false)
}

pub fn bash_command_with_additional_roots(
    command: &str,
    cwd: &Path,
    additional_roots: &[PathBuf],
) -> Command {
    workspace_write_bash_command(command, cwd, &[], additional_roots, &[], true, false, false)
}

pub fn workspace_write_bash_command(
    command: &str,
    cwd: &Path,
    readable_roots: &[PathBuf],
    additional_roots: &[PathBuf],
    denied_roots: &[PathBuf],
    network_access: bool,
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
) -> Command {
    if !available() {
        return plain_bash_command(command, cwd);
    }

    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let canonical_readable_roots = readable_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_additional_roots = additional_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_denied_roots = denied_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(workspace_write_profile(
            &canonical_cwd,
            &canonical_readable_roots,
            &canonical_additional_roots,
            &canonical_denied_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        ))
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd);
    cmd
}

pub fn read_only_bash_command(
    command: &str,
    cwd: &Path,
    readable_roots: &[PathBuf],
    additional_roots: &[PathBuf],
    denied_roots: &[PathBuf],
    network_access: bool,
) -> Command {
    if !available() {
        return plain_bash_command(command, cwd);
    }

    let canonical_readable_roots = readable_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_additional_roots = additional_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let canonical_denied_roots = denied_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(read_only_profile(
            &canonical_readable_roots,
            &canonical_additional_roots,
            &canonical_denied_roots,
            network_access,
        ))
        .arg("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd);
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

fn workspace_write_profile(
    cwd: &Path,
    readable_roots: &[PathBuf],
    additional_roots: &[PathBuf],
    denied_roots: &[PathBuf],
    network_access: bool,
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
) -> String {
    let cwd_escaped = seatbelt_escape(&cwd.display().to_string());
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
        r#"(allow file-write* (subpath "/tmp"))"#.to_string()
    };
    let network_rule = if network_access {
        "(allow network-outbound)"
    } else {
        ""
    };
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
{additional_write_rules}
{denied_access_rules}
{ssh_deny}
{orca_deny}
{network_rule}
"#
    )
}

fn read_only_profile(
    readable_roots: &[PathBuf],
    additional_roots: &[PathBuf],
    denied_roots: &[PathBuf],
    network_access: bool,
) -> String {
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
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow signal (target self))
(allow file-read*)
(allow file-read* file-write* (literal "/dev/null"))
{additional_read_rules}
{additional_write_rules}
{denied_access_rules}
{network_rule}
"#
    )
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
            format!(
                r#"(deny file-read* file-write* (subpath "{}"))"#,
                seatbelt_escape(&root.display().to_string())
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
        let content = workspace_write_profile(workspace.path(), &[], &[], &[], true, false, false);
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
        let profile = workspace_write_profile(workspace.path(), &[], &[], &[], true, false, false);

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
    fn read_only_profile_does_not_allow_workspace_writes() {
        let workspace = TempDir::new().unwrap();
        let profile = read_only_profile(&[], &[], &[], false);

        assert!(!profile.contains(&workspace.path().display().to_string()));
        assert!(!profile.contains("file-write* (subpath"));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn read_only_profile_allows_additional_write_roots() {
        let workspace = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let profile = read_only_profile(&[], &[extra.path().to_path_buf()], &[], false);

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
        let profile = read_only_profile(&[readable.path().to_path_buf()], &[], &[], false);

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
        let profile = read_only_profile(
            &[],
            &[extra.path().to_path_buf()],
            &[blocked.clone()],
            false,
        );

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
    fn workspace_write_profile_can_exclude_tmp_writes_and_network() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(workspace.path(), &[], &[], &[], false, true, true);

        assert!(!profile.contains(r#"(subpath "/tmp")"#));
        assert!(!profile.contains("network-outbound"));
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
