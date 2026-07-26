use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "RuntimeHostHandle",
    "RuntimeThreadHandle",
    "OperationHandle",
    "HostedTurnRequest",
    "OperationOutcome",
    "EventObserver",
    "EventEnvelope",
    "current_op",
    "cancel_requested",
    "unbounded_channel",
    "UnboundedSender",
    "UnboundedReceiver",
    "AgentSideConnection",
    "tokio_util::compat",
    "event_map",
];

#[test]
fn production_acp_uses_only_the_typed_runtime_surface_and_bounded_transport() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/acp");
    let mut violations = Vec::new();
    scan_rs_files(&root, &mut |path, source| {
        let production = production_source(source);
        for forbidden in FORBIDDEN {
            if production.contains(forbidden) {
                violations.push(format!("{} contains `{forbidden}`", path.display()));
            }
        }
    });

    assert!(
        violations.is_empty(),
        "ACP production ownership boundary violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn boundary_scan_ignores_only_the_trailing_test_module() {
    let source = "production\n#[cfg(test)]\nuse test_only;\nRuntimeHostHandle\n#[cfg(test)]\nmod tests {\nHostedTurnRequest\n}\n";
    let production = production_source(source);
    assert!(production.contains("RuntimeHostHandle"));
    assert!(!production.contains("HostedTurnRequest"));
}

fn production_source(source: &str) -> &str {
    source
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(source, |(production, _)| production)
}

fn scan_rs_files(root: &Path, visit: &mut impl FnMut(&Path, &str)) {
    let mut entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("read {}: {error}", root.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("walk {}: {error}", root.display()));
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            scan_rs_files(&path, visit);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            visit(&path, &source);
        }
    }
}
