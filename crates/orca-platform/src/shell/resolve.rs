use std::env;
use std::path::{Path, PathBuf};

use crate::PlatformError;
use crate::host::{HostPlatform, OperatingSystem};

use super::{PowerShellEdition, ShellKind, ShellSpec};

pub struct ShellResolver<P> {
    host: HostPlatform,
    probe: P,
}

impl<P> ShellResolver<P>
where
    P: Fn(&str) -> Option<PathBuf>,
{
    pub fn new(host: HostPlatform, probe: P) -> Self {
        Self { host, probe }
    }

    pub fn resolve(&self, explicit_override: Option<&str>) -> Result<ShellSpec, PlatformError> {
        if let Some(value) = explicit_override {
            return self.resolve_override(value);
        }
        match self.host.os {
            OperatingSystem::Windows => self.resolve_windows_default(),
            OperatingSystem::MacOs | OperatingSystem::Linux => self.resolve_unix_default(),
            _ => Err(PlatformError::UnsupportedHost {
                platform: self.host.clone(),
            }),
        }
    }

    pub fn resolve_from_environment(&self) -> Result<ShellSpec, PlatformError> {
        let override_value = env::var_os("ORCA_SHELL")
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| PlatformError::InvalidShellOverride {
                        value: "<non-utf8>".to_string(),
                        reason: "ORCA_SHELL must be valid UTF-8".to_string(),
                    })
            })
            .transpose()?;
        self.resolve(override_value.as_deref())
    }

    fn resolve_override(&self, value: &str) -> Result<ShellSpec, PlatformError> {
        if value.trim().is_empty() {
            return Err(PlatformError::InvalidShellOverride {
                value: value.to_string(),
                reason: "the value is empty".to_string(),
            });
        }
        let executable =
            (self.probe)(value).ok_or_else(|| PlatformError::InvalidShellOverride {
                value: value.to_string(),
                reason: "the executable does not exist or is not available".to_string(),
            })?;
        let kind = match self.host.os {
            OperatingSystem::Windows => windows_override_kind(value)?,
            OperatingSystem::MacOs | OperatingSystem::Linux => ShellKind::Posix,
            _ => {
                return Err(PlatformError::UnsupportedHost {
                    platform: self.host.clone(),
                });
            }
        };
        Ok(ShellSpec::new(executable, kind))
    }

    fn resolve_windows_default(&self) -> Result<ShellSpec, PlatformError> {
        for (candidate, kind) in [
            ("pwsh.exe", ShellKind::PowerShell(PowerShellEdition::Core)),
            (
                "powershell.exe",
                ShellKind::PowerShell(PowerShellEdition::Windows),
            ),
            ("cmd.exe", ShellKind::Cmd),
        ] {
            if let Some(executable) = (self.probe)(candidate) {
                return Ok(ShellSpec::new(executable, kind));
            }
        }
        Err(PlatformError::ExecutableNotFound {
            executable: "pwsh.exe, powershell.exe, or cmd.exe".to_string(),
        })
    }

    fn resolve_unix_default(&self) -> Result<ShellSpec, PlatformError> {
        (self.probe)("sh")
            .map(|executable| ShellSpec::new(executable, ShellKind::Posix))
            .ok_or_else(|| PlatformError::ExecutableNotFound {
                executable: "sh".to_string(),
            })
    }
}

impl ShellResolver<fn(&str) -> Option<PathBuf>> {
    pub fn for_current_host() -> Self {
        Self::new(HostPlatform::current(), find_executable)
    }
}

fn windows_override_kind(value: &str) -> Result<ShellKind, PlatformError> {
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    match name.to_ascii_lowercase().as_str() {
        "pwsh" | "pwsh.exe" => Ok(ShellKind::PowerShell(PowerShellEdition::Core)),
        "powershell" | "powershell.exe" => Ok(ShellKind::PowerShell(PowerShellEdition::Windows)),
        "cmd" | "cmd.exe" => Ok(ShellKind::Cmd),
        "bash" | "bash.exe" => Ok(ShellKind::GitBash),
        _ => Err(PlatformError::InvalidShellOverride {
            value: value.to_string(),
            reason: "expected pwsh.exe, powershell.exe, cmd.exe, or explicitly selected bash.exe"
                .to_string(),
        }),
    }
}

fn find_executable(candidate: &str) -> Option<PathBuf> {
    let candidate_path = Path::new(candidate);
    if candidate_path.components().count() > 1 {
        return absolute_existing_path(candidate_path);
    }
    let current_directory = env::current_dir().ok();
    env::var_os("PATH")
        .into_iter()
        .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .find_map(|directory| {
            let candidate_path = directory.join(candidate);
            if current_directory
                .as_deref()
                .is_some_and(|cwd| is_current_directory_executable(&candidate_path, cwd))
            {
                return None;
            }
            absolute_existing_path(&candidate_path)
        })
}

/// Resolve a bare child-process name on Windows, including `.cmd`/`.bat`
/// launchers exposed through `PATHEXT` (for example npm's `npx.cmd`).
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    if !cfg!(windows) || program.contains('/') || program.contains('\\') {
        return None;
    }
    find_executable(program).or_else(|| {
        env::var_os("PATHEXT")
            .into_iter()
            .flat_map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .map(|extension| format!("{program}{extension}"))
            .find_map(|candidate| find_executable(&candidate))
    })
}

#[cfg(test)]
fn plan_program(
    program: &str,
    is_windows: bool,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if !is_windows || program.contains('/') || program.contains('\\') {
        return None;
    }
    resolve(program)
}

#[cfg(windows)]
fn is_current_directory_executable(path: &Path, cwd: &Path) -> bool {
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let current = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let candidate = candidate
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let current = current
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    candidate == current || candidate.starts_with(&format!("{current}\\"))
}

#[cfg(not(windows))]
fn is_current_directory_executable(_path: &Path, _cwd: &Path) -> bool {
    false
}

fn absolute_existing_path(path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    std::fs::canonicalize(path).ok().or_else(|| {
        if path.is_absolute() {
            Some(path.to_path_buf())
        } else {
            env::current_dir().ok().map(|cwd| cwd.join(path))
        }
    })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{is_current_directory_executable, plan_program};
    use std::path::Path;

    #[test]
    fn shell_lookup_rejects_current_directory_and_descendants() {
        let cwd = Path::new(r"C:\Work\repo");
        assert!(is_current_directory_executable(
            Path::new(r"C:\Work\repo\pwsh.exe"),
            cwd,
        ));
        assert!(is_current_directory_executable(
            Path::new(r"C:\Work\repo\tools\cmd.exe"),
            cwd,
        ));
        assert!(!is_current_directory_executable(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            cwd,
        ));
    }

    #[test]
    fn bare_windows_launcher_can_be_resolved_without_touching_absolute_paths() {
        let resolved = plan_program("npx", true, |program| {
            assert_eq!(program, "npx");
            Some(Path::new(r"C:\Node\npx.cmd").to_path_buf())
        });
        assert_eq!(resolved, Some(Path::new(r"C:\Node\npx.cmd").to_path_buf()));
        assert_eq!(
            plan_program(r"C:\Node\npx.cmd", true, |_| {
                panic!("absolute launcher paths must not be resolved")
            }),
            None,
        );
    }
}
