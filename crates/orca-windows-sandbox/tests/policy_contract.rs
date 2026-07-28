use std::path::PathBuf;

use orca_windows_sandbox::{SandboxFilesystemMode, WindowsSandboxPlan, WindowsSandboxPolicyInput};

#[test]
fn workspace_write_deduplicates_case_and_protects_metadata() {
    let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
        mode: SandboxFilesystemMode::WorkspaceWrite,
        cwd: PathBuf::from(r"C:\Work\Repo"),
        readable_roots: vec![PathBuf::from(r"c:\work\repo")],
        writable_roots: vec![PathBuf::from(r"C:\Output")],
        denied_roots: vec![],
        network_access: true,
    })
    .expect("build workspace-write policy");

    assert_eq!(plan.readable_roots.len(), 2);
    assert_eq!(plan.writable_roots.len(), 2);
    assert!(
        plan.denied_roots
            .contains(&PathBuf::from(r"C:\Work\Repo\.git"))
    );
    assert!(
        plan.denied_roots
            .contains(&PathBuf::from(r"C:\Work\Repo\.agents"))
    );
    assert!(
        plan.denied_roots
            .contains(&PathBuf::from(r"C:\Work\Repo\.codex"))
    );
}

#[test]
fn read_only_keeps_explicit_output_roots_narrow() {
    let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
        mode: SandboxFilesystemMode::ReadOnly {
            allow_global_read: false,
        },
        cwd: PathBuf::from(r"C:\Work\Repo"),
        readable_roots: vec![PathBuf::from(r"D:\Inputs")],
        writable_roots: vec![PathBuf::from(r"D:\Output")],
        denied_roots: vec![],
        network_access: false,
    })
    .expect("build read-only policy");

    assert_eq!(plan.writable_roots, [PathBuf::from(r"D:\Output")]);
    assert_eq!(plan.readable_roots.len(), 3);
    assert!(!plan.network_access);
}

#[test]
fn writable_root_inside_denied_root_fails_closed() {
    let error = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
        mode: SandboxFilesystemMode::ReadOnly {
            allow_global_read: true,
        },
        cwd: PathBuf::from(r"C:\Work\Repo"),
        readable_roots: vec![],
        writable_roots: vec![PathBuf::from(r"C:\Secrets\Output")],
        denied_roots: vec![PathBuf::from(r"C:\Secrets")],
        network_access: true,
    })
    .expect_err("contradictory policy must fail");

    assert!(error.to_string().contains("contained by a denied root"));
}

#[test]
fn unc_roots_are_rejected_until_the_acl_backend_supports_them() {
    let error = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
        mode: SandboxFilesystemMode::WorkspaceWrite,
        cwd: PathBuf::from(r"\\server\share\repo"),
        readable_roots: vec![],
        writable_roots: vec![],
        denied_roots: vec![],
        network_access: true,
    })
    .expect_err("UNC policy must fail closed");

    assert!(error.to_string().contains("UNC paths are disabled"));
}
