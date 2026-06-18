use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static PROFILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static SEATBELT_AVAILABLE: OnceLock<bool> = OnceLock::new();

pub fn bash_command(command: &str, cwd: &Path) -> Command {
    if !available() {
        return plain_bash_command(command, cwd);
    }

    match write_profile(cwd) {
        Ok(profile_path) => {
            let wrapped = format!(
                r#"__rc=0; sh -c {} || __rc=$?; rm -f {}; exit $__rc"#,
                shell_escape(command),
                shell_escape(&profile_path.display().to_string()),
            );
            let mut cmd = Command::new("sandbox-exec");
            cmd.arg("-f")
                .arg(&profile_path)
                .arg("sh")
                .arg("-c")
                .arg(wrapped)
                .current_dir(cwd);
            cmd
        }
        Err(_) => plain_bash_command(command, cwd),
    }
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

fn plain_bash_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    cmd
}

fn write_profile(cwd: &Path) -> std::io::Result<std::path::PathBuf> {
    let id = PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let profile_path =
        std::env::temp_dir().join(format!("orca-seatbelt-{}-{id}.sb", std::process::id()));
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    std::fs::write(&profile_path, profile(&canonical_cwd))?;
    Ok(profile_path)
}

fn profile(cwd: &Path) -> String {
    let cwd_escaped = seatbelt_escape(&cwd.display().to_string());
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
    // Seatbelt uses last-match-wins: deny rules MUST come after allow to override.
    format!(
        r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow signal (target self))
(allow file-read*)
(allow file-write* (subpath "{cwd_escaped}"))
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/private/tmp"))
{ssh_deny}
{orca_deny}
(allow network-outbound)
"#
    )
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
    fn sandbox_command_uses_generated_profile_file() {
        let workspace = TempDir::new().unwrap();
        let profile_path = write_profile(workspace.path()).unwrap();

        assert!(profile_path.exists());
        let content = std::fs::read_to_string(&profile_path).unwrap();
        assert!(content.contains("(version 1)"));
        assert!(content.contains(&workspace.path().display().to_string()));

        std::fs::remove_file(&profile_path).unwrap();
    }

    #[test]
    fn profile_denies_sensitive_orca_and_ssh_paths() {
        let workspace = TempDir::new().unwrap();
        let profile = profile(workspace.path());

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
    fn sandbox_blocks_writes_outside_workspace() {
        if !available() {
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
