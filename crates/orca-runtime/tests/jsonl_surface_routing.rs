use std::fs;
use std::path::PathBuf;

#[test]
fn jsonl_runtime_routes_host_thread_reads_and_turns_through_the_surface_adapter() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let adapter = root.join("src/server/surface_adapter.rs");
    let adapter_source = fs::read_to_string(&adapter)
        .unwrap_or_else(|error| panic!("read {}: {error}", adapter.display()));
    for required in [
        "RuntimeSurfaceHostHandle",
        "RuntimeSurfaceThreadHandle",
        "RuntimeSurfaceClientHandle",
        "OperationIngressCorrelation::JsonlThreadTurn",
        "reserve_operation",
        "admit_reserved_with_output",
    ] {
        assert!(
            adapter_source.contains(required),
            "JSONL surface adapter must route through `{required}`"
        );
    }

    assert!(
        !root.join("src/server_runtime.rs").exists(),
        "JSONL must not retain a second runtime ownership wrapper"
    );
    assert!(
        adapter_source.contains("transport_turns: Vec<JsonlTransportTurn>"),
        "the typed surface adapter must retain its projection workers"
    );
    assert!(
        adapter_source.contains("project_surface_batch"),
        "durably committed surface batches must drive JSONL visibility"
    );
    assert!(
        !adapter_source.contains("Some(SurfaceSubscriptionItem::Batch { .. }) => {}"),
        "JSONL must not discard committed surface batches"
    );
}

#[test]
fn jsonl_processors_do_not_select_or_control_runtime_operations_locally() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/server");
    let active_registry = root.join("active_turn_registry.rs");
    assert!(
        !active_registry.exists(),
        "the JSONL adapter must not retain an OperationHandle registry"
    );

    let turn = fs::read_to_string(root.join("processors/turn.rs")).expect("read turn processor");
    let production = production_source(&turn);
    for forbidden in [
        ".operation().interrupt()",
        ".operation().resume()",
        ".operation().steer(",
        "InterruptOperationResult",
        "ResumeOperationResult",
        "SteerOperationResult",
    ] {
        assert!(
            !production.contains(forbidden),
            "JSONL turn control must use SurfaceHostCommand::ControlJsonlTurn, found `{forbidden}`"
        );
    }
    assert!(
        production.contains("control_turn"),
        "JSONL turn processor must delegate control to the surface adapter"
    );
}

#[test]
fn jsonl_production_server_does_not_read_or_write_the_session_store_directly() {
    let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/server.rs");
    let source = fs::read_to_string(&server).expect("read server.rs");
    let production = production_source(&source);
    let mut violations = Vec::new();
    for function in [
        "run_thread_list",
        "run_thread_search",
        "run_thread_turns_list",
        "run_thread_items_list",
        "run_thread_read",
        "run_thread_metadata_update",
    ] {
        let body = function_source(production, function);
        for forbidden in ["SessionStore", "ThreadStore"] {
            if body.contains(forbidden) {
                violations.push(format!("{function} contains `{forbidden}`"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "JSONL production persistence boundary violations:\n{}",
        violations.join("\n")
    );
}

fn function_source<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(&format!("fn {name}<"))
        .unwrap_or_else(|| panic!("missing function {name}"));
    let rest = &source[start..];
    rest.find("\nfn ").map_or(rest, |end| &rest[..end])
}

fn production_source(source: &str) -> &str {
    source
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(source, |(production, _)| production)
}
