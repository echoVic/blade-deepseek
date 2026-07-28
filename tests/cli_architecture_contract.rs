use std::fs;

#[test]
fn root_cli_is_only_argument_parsing_conversion_and_forwarding() {
    let cli = fs::read_to_string("src/cli.rs").expect("read root CLI");
    assert!(
        cli.lines().count() < 1_000,
        "root CLI must stay below 1,000 lines"
    );
    assert!(cli.contains("Cli::parse()"));
    for forbidden in [
        "WorkflowRunner",
        "WorkflowStateStore",
        "ProcessCommand",
        "terminal::enable_raw_mode",
        "fs::write",
        "RunConfig {",
        "check_latest_for_prompt",
        "#[cfg(test)]",
    ] {
        assert!(!cli.contains(forbidden), "root CLI still owns {forbidden}");
    }
    for facade in [
        "orca_runtime::command::exec",
        "orca_runtime::command::history",
        "orca_runtime::command::trust",
        "orca_runtime::workflow::command",
        "orca_runtime::command::launch",
        "orca_tui::cli",
    ] {
        assert!(
            cli.contains(facade),
            "root CLI does not forward through {facade}"
        );
    }
}

#[test]
fn binary_entrypoint_has_no_library_reexport_shims() {
    let main = fs::read_to_string("src/main.rs").expect("read main");
    assert_eq!(main.matches("mod ").count(), 1);
    assert!(main.contains("mod cli;"));
    assert!(!main.contains("mod runtime;"));
    assert!(!main.contains("mod config;"));

    for shim in [
        "src/acp.rs",
        "src/approval/mod.rs",
        "src/config/mod.rs",
        "src/event/mod.rs",
        "src/mcp/mod.rs",
        "src/mentions.rs",
        "src/model.rs",
        "src/provider/mod.rs",
        "src/runtime/mod.rs",
        "src/sandbox/mod.rs",
        "src/server.rs",
        "src/tools/mod.rs",
        "src/tui/mod.rs",
        "src/verification/mod.rs",
    ] {
        assert!(
            !std::path::Path::new(shim).exists(),
            "obsolete shim remains: {shim}"
        );
    }
}

#[test]
fn root_runtime_dependencies_match_the_thin_binary() {
    let manifest = fs::read_to_string("Cargo.toml").expect("read root manifest");
    let value: toml::Value = toml::from_str(&manifest).expect("parse root manifest");
    let dependencies = value["dependencies"]
        .as_table()
        .expect("root dependencies")
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        dependencies,
        ["clap", "orca-core", "orca-runtime", "orca-tui"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
}

#[test]
fn workflow_and_update_behavior_live_in_library_crates() {
    let workflow = fs::read_to_string("crates/orca-runtime/src/workflow/command.rs")
        .expect("workflow command library module");
    assert!(workflow.contains("pub enum WorkflowCommandRequest"));
    assert!(workflow.contains("pub fn run("));
    assert!(workflow.contains("spawn_workflow_worker"));

    let update = fs::read_to_string("crates/orca-runtime/src/update_check.rs")
        .expect("update library module");
    assert!(update.contains("pub enum UpdateAction"));
    assert!(update.contains("pub fn run_update"));
}
