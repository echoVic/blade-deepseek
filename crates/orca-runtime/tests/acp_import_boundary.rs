use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "RuntimeHostHandle",
    "RuntimeThreadHandle",
    "RuntimeThreadStartRequest",
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
    "SessionTranscript",
    "load_saved_session",
    "with_preloaded",
    "start_thread_with_request",
    "crate::runtime_host::",
    "crate::history::",
    "crate::thread_store::",
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

#[test]
fn boundary_scan_is_independent_of_checkout_line_endings() {
    let source = normalized_source(
        "production\r\n#[cfg(test)]\r\nmod tests {\r\nHostedTurnRequest\r\n}\r\n",
    );
    assert_eq!(production_source(&source), "production");
}

#[test]
fn prompt_cancel_keeps_only_transport_binding_in_acp() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/acp/agent.rs");
    let source = fs::read_to_string(&path).expect("read ACP agent source");
    let source = normalized_source(&source);
    let production = production_source(&source);
    let binding = between(
        production,
        "enum AcpPromptBinding {",
        "\n\n#[derive(Default)]",
    );
    assert!(
        !binding.contains("operation_id"),
        "ACP prompt transport binding must not own the runtime operation id"
    );
    let cancel = between(
        production,
        "    async fn cancel(&self, args: CancelNotification)",
        "\n    }\n}",
    );
    assert!(
        cancel.contains("cancel_acp_prompt_binding"),
        "ACP cancel must ask runtime to resolve the exact prompt binding"
    );
    assert!(
        !cancel.contains("cancel_operation"),
        "ACP cancel must not select a runtime operation locally"
    );
}

fn production_source(source: &str) -> &str {
    source
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(source, |(production, _)| production)
}

fn normalized_source(source: &str) -> Cow<'_, str> {
    if source.contains('\r') {
        Cow::Owned(source.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(source)
    }
}

fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .split_once(start)
        .unwrap_or_else(|| panic!("missing boundary start `{start}`"))
        .1
        .split_once(end)
        .unwrap_or_else(|| panic!("missing boundary end `{end}`"))
        .0
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
            visit(&path, &normalized_source(&source));
        }
    }
}
