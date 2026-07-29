use std::collections::BTreeMap;
use std::path::PathBuf;

use orca_platform::host::{Architecture, HostPlatform, OperatingSystem};
use orca_platform::shell::{PowerShellEdition, ShellKind, ShellResolver};

fn windows() -> HostPlatform {
    HostPlatform::new(OperatingSystem::Windows, Architecture::X86_64)
}

fn linux() -> HostPlatform {
    HostPlatform::new(OperatingSystem::Linux, Architecture::X86_64)
}

#[test]
fn windows_prefers_pwsh_over_native_fallbacks() {
    let available = BTreeMap::from([
        (
            "pwsh.exe".to_string(),
            PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
        ),
        (
            "powershell.exe".to_string(),
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        ),
        (
            "cmd.exe".to_string(),
            PathBuf::from(r"C:\Windows\System32\cmd.exe"),
        ),
    ]);
    let shell = ShellResolver::new(windows(), |name| available.get(name).cloned())
        .resolve(None)
        .expect("resolve shell");
    assert_eq!(shell.kind(), ShellKind::PowerShell(PowerShellEdition::Core));
    assert_eq!(shell.executable(), available["pwsh.exe"]);
    assert_eq!(shell.tool_name(), "powershell");
}

#[test]
fn windows_falls_back_to_cmd_before_windows_powershell() {
    let windows_powershell =
        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    let cmd = PathBuf::from(r"C:\Windows\System32\cmd.exe");
    let available = BTreeMap::from([
        ("powershell.exe".to_string(), windows_powershell.clone()),
        ("cmd.exe".to_string(), cmd.clone()),
    ]);
    let shell = ShellResolver::new(windows(), |name| available.get(name).cloned())
        .resolve(None)
        .expect("resolve cmd fallback");
    assert_eq!(shell.kind(), ShellKind::Cmd);
    assert_eq!(shell.executable(), cmd);

    let shell = ShellResolver::new(windows(), |name| {
        (name == "powershell.exe").then(|| windows_powershell.clone())
    })
    .resolve(None)
    .expect("resolve final Windows PowerShell fallback");
    assert_eq!(
        shell.kind(),
        ShellKind::PowerShell(PowerShellEdition::Windows)
    );
    assert_eq!(shell.executable(), windows_powershell);
}

#[test]
fn explicit_override_has_precedence_and_missing_override_never_falls_back() {
    let override_path = r"C:\Portable\PowerShell\pwsh.exe";
    let fallback_path = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
    let shell = ShellResolver::new(windows(), |name| match name {
        value if value == override_path => Some(PathBuf::from(override_path)),
        "pwsh.exe" => Some(fallback_path.clone()),
        _ => None,
    })
    .resolve(Some(override_path))
    .expect("resolve explicit override");
    assert_eq!(shell.executable(), PathBuf::from(override_path));

    let error = ShellResolver::new(windows(), |name| {
        (name == "pwsh.exe").then(|| fallback_path.clone())
    })
    .resolve(Some(r"C:\missing\pwsh.exe"))
    .expect_err("invalid explicit override must fail");
    assert!(error.to_string().contains("ORCA_SHELL"));
}

#[test]
fn explicit_git_bash_is_supported_but_never_implicitly_preferred() {
    let git_bash = r"C:\Program Files\Git\bin\bash.exe";
    let shell = ShellResolver::new(windows(), |name| {
        (name == git_bash).then(|| PathBuf::from(git_bash))
    })
    .resolve(Some(git_bash))
    .expect("resolve explicit Git Bash");
    assert_eq!(shell.kind(), ShellKind::GitBash);
    assert_eq!(shell.tool_name(), "bash");

    let error = ShellResolver::new(windows(), |_| None)
        .resolve(None)
        .expect_err("Windows requires a native fallback");
    assert!(!error.to_string().contains("Git Bash"));
}

#[test]
fn windows_shell_commands_preserve_the_active_dialect() {
    let pwsh = ShellResolver::new(windows(), |name| {
        (name == "pwsh.exe").then(|| PathBuf::from(r"C:\pwsh.exe"))
    })
    .resolve(None)
    .expect("pwsh");
    let command = pwsh.command("Write-Output 'orca-test'");
    assert_eq!(command.program, PathBuf::from(r"C:\pwsh.exe"));
    assert_eq!(command.args[0], "-NoLogo");
    assert_eq!(command.args[1], "-NoProfile");
    assert_eq!(command.args[2], "-NonInteractive");
    assert_eq!(command.args[3], "-Command");
    let script = command.args[4].to_string_lossy();
    assert!(script.contains("OutputEncoding"));
    assert!(script.contains("LanguageMode -eq 'FullLanguage'"));
    assert!(script.ends_with("Write-Output 'orca-test'"));
    assert!(pwsh.prompt_dialect().contains("PowerShell 7"));

    let cmd = ShellResolver::new(windows(), |name| {
        (name == "cmd.exe").then(|| PathBuf::from(r"C:\cmd.exe"))
    })
    .resolve(None)
    .expect("cmd");
    let command = cmd.command("echo orca-test");
    assert_eq!(command.args, ["/D", "/S", "/C", "echo orca-test"]);
    assert!(cmd.prompt_dialect().contains("cmd.exe"));
}

#[test]
fn unix_shell_behavior_remains_posix() {
    let shell = ShellResolver::new(linux(), |name| {
        (name == "sh").then(|| PathBuf::from("/bin/sh"))
    })
    .resolve(None)
    .expect("resolve Unix shell");
    assert_eq!(shell.kind(), ShellKind::Posix);
    assert_eq!(shell.tool_name(), "bash");
    let command = shell.command("printf orca-test");
    assert_eq!(command.program, PathBuf::from("/bin/sh"));
    assert_eq!(command.args, ["-c", "printf orca-test"]);
}

#[test]
fn unix_default_does_not_implicitly_switch_away_from_sh() {
    let shell = ShellResolver::new(linux(), |name| match name {
        "sh" => Some(PathBuf::from("/bin/sh")),
        "/bin/zsh" => Some(PathBuf::from("/bin/zsh")),
        _ => None,
    })
    .resolve(None)
    .expect("resolve Unix shell");
    assert_eq!(shell.executable(), PathBuf::from("/bin/sh"));
}

#[cfg(windows)]
#[test]
fn installed_native_windows_shells_emit_the_expected_encoding() {
    use std::path::Path;
    use std::process::Command;

    let resolver = ShellResolver::for_current_host();
    let standard_pwsh = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .map(|root| root.join("PowerShell").join("7").join("pwsh.exe"));
    if standard_pwsh.as_deref().is_some_and(Path::is_file) {
        let shell = resolver
            .resolve(None)
            .expect("resolve the installed PowerShell 7 default");
        assert_eq!(
            shell.kind(),
            ShellKind::PowerShell(PowerShellEdition::Core),
            "an installed standard-path pwsh.exe must win even when PATH omits it"
        );
        assert_eq!(
            std::fs::canonicalize(shell.executable()).expect("canonical resolved pwsh.exe"),
            std::fs::canonicalize(standard_pwsh.unwrap()).expect("canonical standard pwsh.exe")
        );
    }
    let mut tested_powershell_editions = 0;
    for candidate in ["pwsh.exe", "powershell.exe"] {
        let Ok(shell) = resolver.resolve(Some(candidate)) else {
            continue;
        };
        let command = shell.command("Write-Output 'orca-测试-🙂'");
        let output = Command::new(command.program)
            .args(command.args)
            .output()
            .unwrap_or_else(|error| panic!("execute {candidate}: {error}"));
        assert!(output.status.success(), "{candidate} failed: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("PowerShell UTF-8 stdout");
        assert!(
            stdout.contains("orca-测试-🙂"),
            "{candidate} stdout was {stdout:?}"
        );
        tested_powershell_editions += 1;
    }
    assert!(
        tested_powershell_editions > 0,
        "Windows 10/11 support requires at least Windows PowerShell 5.1"
    );

    let cmd = resolver
        .resolve(Some("cmd.exe"))
        .expect("resolve cmd.exe fallback");
    let command = cmd.command("echo orca-native-cmd");
    let output = Command::new(command.program)
        .args(command.args)
        .output()
        .expect("execute cmd.exe");
    assert!(output.status.success(), "cmd.exe failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("cmd ASCII stdout");
    assert!(stdout.contains("orca-native-cmd"), "stdout was {stdout:?}");
}
