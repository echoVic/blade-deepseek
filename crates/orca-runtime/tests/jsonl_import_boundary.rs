use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

fn normalized_source(source: &str) -> Cow<'_, str> {
    if source.contains('\r') {
        Cow::Owned(source.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(source)
    }
}

#[test]
fn production_source_scan_is_independent_of_checkout_line_endings() {
    assert_eq!(
        normalized_source("production\r\n#[cfg(test)]\r\nmod tests {}\r\n")
            .split_once("\n#[cfg(test)]\nmod tests")
            .map(|(production, _)| production),
        Some("production")
    );
}

#[test]
fn jsonl_runtime_and_thread_bound_waiter_owner_modules_are_deleted() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/server_runtime.rs",
        "src/server/active_turn_registry.rs",
        "src/server/permission_manager.rs",
        "src/server/user_input_manager.rs",
        "src/server/mcp_elicitation_manager.rs",
    ] {
        let path = root.join(relative);
        assert!(
            !path.exists(),
            "JSONL ownership module must be deleted: {}",
            path.display()
        );
    }
}

#[test]
fn jsonl_production_imports_only_typed_surface_and_transport_boundaries() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut production = read_production(&root.join("src/server.rs"));
    for path in rust_files(&root.join("src/server")) {
        production.push_str(&read_production(&path));
    }
    for forbidden in [
        "ServerThreadRuntime",
        "PendingPermissionManager",
        "PendingUserInputManager",
        "PendingMcpElicitationManager",
        "ServerActiveTurnRegistry",
        "SessionStore",
        "ThreadStore",
    ] {
        assert!(
            !production.contains(forbidden),
            "JSONL production still owns forbidden boundary `{forbidden}`"
        );
    }
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(root)
        .expect("read server sources")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn read_production(path: &Path) -> String {
    let source =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let source = normalized_source(&source);
    match source.split_once("\n#[cfg(test)]\nmod tests") {
        Some((production, _)) => production.to_string(),
        None => source.into_owned(),
    }
}
