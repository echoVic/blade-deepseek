use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn jsonl_connection_owns_one_bounded_opaque_request_admission_domain() {
    let server = server_root();
    let permission = read(&server.join("opaque_permission_router.rs"));
    let direct = read(&server.join("direct_interaction_adapter.rs"));

    for required in [
        "JSONL_LIVE_REQUEST_LIMIT",
        "JSONL_REPAIR_AUTHORITY_LIMIT",
        "JsonlConnectionAdmission",
        "JsonlRetirementSequence",
        "JsonlRetiredRequestOwner",
    ] {
        assert!(
            permission.contains(required) || direct.contains(required),
            "JSONL opaque request ownership is missing `{required}`"
        );
    }
    assert!(
        permission.contains("JsonlOpaquePermissionRouter"),
        "permission/respond must select exactly one connection-owned route"
    );
    assert!(
        direct.contains("JsonlDirectInteractionAdapter"),
        "direct user-input and MCP responses must use their own closed ledger"
    );
}

#[test]
fn jsonl_response_processors_never_remove_a_route_before_owner_settlement() {
    let server = server_root();
    for processor in ["permission.rs", "user_input.rs", "mcp_elicitation.rs"] {
        let path = server.join("processors").join(processor);
        let source = read(&path);
        assert!(
            !source.contains(".remove(request_id)?")
                && !source.contains(".remove_surface(request_id)?"),
            "{} still removes a live response route before typed settlement",
            path.display()
        );
    }
}

#[test]
fn jsonl_connection_supervisor_is_the_only_shutdown_rail() {
    let server = server_root();
    let supervisor = read(&server.join("connection_supervisor.rs"));
    for required in [
        "JsonlConnectionSupervisor",
        "JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS",
        "JSONL_SUPERVISOR_JOIN_DEADLINE_MS",
        "JsonlServiceSettlements",
        "JsonlCommittedRepairSettlements",
        "settle_committed_repairs_until",
        "DeadlineRetained",
        "FailedRetained",
        "cleanup_errors",
        "JsonlSupervisorCloseResult",
    ] {
        assert!(
            supervisor.contains(required),
            "JSONL supervisor is missing `{required}`"
        );
    }

    let server_source = read(&server.with_extension("rs"));
    assert!(
        server_source.contains("JsonlConnectionSupervisor"),
        "server connection must delegate close to the typed supervisor"
    );
    for manager in ["fuzzy_file_search_manager.rs", "mention_search_manager.rs"] {
        assert!(
            read(&server.join(manager)).contains("settle_until"),
            "{manager} must settle against the supervisor's shared absolute deadline"
        );
    }
}

fn server_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/server")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
