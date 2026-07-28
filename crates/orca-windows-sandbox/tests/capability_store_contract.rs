use std::sync::Arc;

use orca_windows_sandbox::CapabilityStore;

fn only_setup_receipt(root: &std::path::Path) -> std::path::PathBuf {
    let receipts = std::fs::read_dir(root)
        .expect("read capability directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("setup-receipt-") && name.ends_with(".json"))
        })
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1, "expected one workspace receipt");
    receipts.into_iter().next().expect("workspace receipt")
}

#[test]
fn equivalent_windows_roots_share_one_capability_sid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());

    let first = store
        .write_sid(std::path::Path::new(r"C:\Work\Repo"))
        .expect("first SID");
    let second = store
        .write_sid(std::path::Path::new(r"c:/work/repo"))
        .expect("second SID");

    assert_eq!(first, second);
}

#[test]
fn distinct_write_roots_receive_distinct_capabilities() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());

    let first = store
        .write_sid(std::path::Path::new(r"C:\Work\One"))
        .expect("first SID");
    let second = store
        .write_sid(std::path::Path::new(r"C:\Work\Two"))
        .expect("second SID");

    assert_ne!(first, second);
    assert_ne!(first, store.read_only_sid().expect("read-only SID"));
}

#[test]
fn concurrent_writers_converge_on_one_persisted_sid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(CapabilityStore::new(temp.path()));
    let handles = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .write_sid(std::path::Path::new(r"C:\Work\Shared"))
                    .expect("concurrent SID")
            })
        })
        .collect::<Vec<_>>();
    let sids = handles
        .into_iter()
        .map(|handle| handle.join().expect("join writer"))
        .collect::<Vec<_>>();

    assert!(sids.iter().all(|sid| sid == &sids[0]));
}

#[test]
fn corrupted_capability_state_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("capabilities.json"),
        r#"{"version":1,"readOnlySid":"not-a-sid","writeRootSids":{}}"#,
    )
    .expect("write corrupt state");
    let store = CapabilityStore::new(temp.path());

    let error = store.read_only_sid().expect_err("corrupt state must fail");

    assert!(error.to_string().contains("invalid SID"));
}

#[test]
fn setup_receipt_is_required_and_matches_capability_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    let receipt = store
        .provision_setup(
            std::path::Path::new(r"C:\Work\Receipt"),
            orca_windows_sandbox::SETUP_HELPER_VERSION,
        )
        .expect("provision setup receipt");
    assert_eq!(receipt.version, 1);
    assert_eq!(receipt.workspace, r"C:\Work\Receipt");
    assert_eq!(
        store
            .verify_setup_for_workspace(
                std::path::Path::new(r"C:\Work\Receipt"),
                orca_windows_sandbox::SETUP_HELPER_VERSION,
            )
            .expect("verify setup receipt")
            .read_only_sid,
        receipt.read_only_sid
    );
    let error = store
        .verify_setup_for_workspace(
            std::path::Path::new(r"C:\Work\Receipt"),
            "another-helper-version",
        )
        .expect_err("helper version drift must fail closed");
    assert!(error.to_string().contains("receipt"));
}

#[test]
fn setup_verification_compares_workspace_with_windows_path_semantics() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    store
        .provision_setup(
            std::path::Path::new(r"C:\Work\Receipt"),
            orca_windows_sandbox::SETUP_HELPER_VERSION,
        )
        .expect("provision setup receipt");

    store
        .verify_setup_for_workspace(
            std::path::Path::new("c:/work/receipt"),
            orca_windows_sandbox::SETUP_HELPER_VERSION,
        )
        .expect("equivalent Windows workspace");
    assert!(
        store
            .verify_setup_for_workspace(
                std::path::Path::new(r"C:\Work\Other"),
                orca_windows_sandbox::SETUP_HELPER_VERSION,
            )
            .expect_err("different workspace must fail")
            .to_string()
            .contains("receipt is missing")
    );
}

#[test]
fn legacy_single_receipt_is_accepted_and_migrated_by_repair() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    let workspace = std::path::Path::new(r"C:\Work\LegacyReceipt");
    store
        .provision_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("provision setup receipt");
    let workspace_receipt = only_setup_receipt(temp.path());
    let legacy_receipt = temp.path().join("setup-receipt.json");
    std::fs::rename(&workspace_receipt, &legacy_receipt).expect("restore legacy receipt layout");

    store
        .verify_setup_for_workspace(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("legacy receipt remains readable");
    store
        .repair_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("repair migrates legacy receipt");

    assert!(!legacy_receipt.exists());
    assert!(only_setup_receipt(temp.path()).exists());
}

#[test]
fn repair_recreates_a_missing_receipt_without_rotating_capabilities() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    let workspace = std::path::Path::new(r"C:\Work\Repair");
    let provisioned = store
        .provision_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("provision setup receipt");
    std::fs::remove_file(only_setup_receipt(temp.path())).expect("remove setup receipt");

    let repaired = store
        .repair_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("repair setup receipt");

    assert_eq!(repaired.read_only_sid, provisioned.read_only_sid);
    assert_eq!(repaired.write_sid, provisioned.write_sid);
    assert_eq!(
        store
            .verify_setup_for_workspace(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
            .expect("verify repaired receipt")
            .write_sid,
        provisioned.write_sid
    );
}

#[test]
fn remove_revokes_only_the_requested_workspace_and_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    let removed_workspace = std::path::Path::new(r"C:\Work\Remove");
    let retained_workspace = std::path::Path::new(r"C:\Work\Retain");
    let provisioned = store
        .provision_setup(
            removed_workspace,
            orca_windows_sandbox::SETUP_HELPER_VERSION,
        )
        .expect("provision setup receipt");
    let retained_sid = store
        .write_sid(retained_workspace)
        .expect("retained workspace SID");

    assert!(
        store
            .remove_setup(
                removed_workspace,
                orca_windows_sandbox::SETUP_HELPER_VERSION,
            )
            .expect("remove setup")
    );
    assert!(
        !store
            .remove_setup(
                removed_workspace,
                orca_windows_sandbox::SETUP_HELPER_VERSION,
            )
            .expect("repeat setup removal")
    );
    assert_eq!(
        store
            .write_sid(retained_workspace)
            .expect("retained workspace SID after removal"),
        retained_sid
    );
    assert!(
        store
            .verify_setup_for_workspace(
                removed_workspace,
                orca_windows_sandbox::SETUP_HELPER_VERSION,
            )
            .expect_err("removed receipt must not remain trusted")
            .to_string()
            .contains("receipt is missing")
    );
    assert_ne!(
        store
            .write_sid(removed_workspace)
            .expect("new workspace SID after removal"),
        provisioned.write_sid
    );
}

#[test]
fn remove_finishes_cleanup_after_state_was_committed_before_receipt_deletion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    let workspace = std::path::Path::new(r"C:\Work\InterruptedRemove");
    store
        .provision_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("provision setup receipt");
    let receipt_path = only_setup_receipt(temp.path());
    let stale_receipt = std::fs::read(&receipt_path).expect("read setup receipt");
    store
        .remove_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("remove setup state");
    std::fs::write(&receipt_path, stale_receipt).expect("restore stale receipt");

    assert!(
        store
            .remove_setup(workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
            .expect("finish interrupted removal")
    );
    assert!(!receipt_path.exists());
}

#[test]
fn multiple_workspaces_keep_independent_setup_receipts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = CapabilityStore::new(temp.path());
    let first_workspace = std::path::Path::new(r"C:\Work\First");
    let second_workspace = std::path::Path::new(r"C:\Work\Second");

    store
        .provision_setup(first_workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("first setup receipt");
    store
        .provision_setup(second_workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("second setup receipt");

    store
        .verify_setup_for_workspace(first_workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("first workspace remains trusted");
    store
        .verify_setup_for_workspace(second_workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("second workspace remains trusted");
    assert!(
        store
            .remove_setup(first_workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
            .expect("remove first workspace")
    );
    store
        .verify_setup_for_workspace(second_workspace, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .expect("second workspace survives first removal");
}
