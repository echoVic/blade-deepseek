use std::fs;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orca_file_search::{
    MatchKind, SearchMode, SearchPhase, SearchSession, SearchSessionOptions, SearchSnapshot,
    SessionGeneration,
};
use tempfile::tempdir;

fn wait_for_snapshot(
    session: &SearchSession,
    notifications: &mpsc::Receiver<SessionGeneration>,
    predicate: impl Fn(&SearchSnapshot) -> bool,
) -> SearchSnapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        notifications
            .recv_timeout(remaining)
            .expect("search snapshot notification");
        if let Some(snapshot) = session.take_latest_snapshot()
            && predicate(&snapshot)
        {
            return snapshot;
        }
    }
}

#[test]
fn fuzzy_session_streams_complete_workspace_snapshot() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".gitignore"), "dist/\n").unwrap();
    fs::create_dir_all(root.path().join("src/runtime/config")).unwrap();
    fs::write(root.path().join("src/runtime/config/mod.rs"), "mod config;").unwrap();
    fs::write(root.path().join(".hidden.rs"), "hidden").unwrap();
    fs::create_dir(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/generated.rs"), "generated").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(1), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("rcm"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert!(snapshot.matches.iter().any(|candidate| {
        candidate.path == "src/runtime/config/mod.rs" && candidate.kind == MatchKind::File
    }));
    assert!(snapshot.progress.walk_complete);
    assert!(snapshot.progress.scanned_paths >= 5);
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| !candidate.path.starts_with("dist/"))
    );
    session.join();
}

#[test]
fn browse_mode_lists_direct_children_while_catalog_builds() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src/nested")).unwrap();
    fs::write(root.path().join("src/lib.rs"), "lib").unwrap();
    fs::write(root.path().join("src/nested/deep.rs"), "deep").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(2), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::browse("src/"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::browse("src/") && !snapshot.matches.is_empty()
    });

    assert_eq!(snapshot.matches[0].path, "src/nested/");
    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "src/lib.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "src/nested/deep.rs")
    );
    session.join();
}

#[test]
fn cancellation_joins_synthetic_workers_within_deadline() {
    let root = tempdir().unwrap();
    let paths = (0..100_000)
        .map(|index| format!("src/package-{index:06}/file-{index:06}.rs"))
        .collect();
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_with_paths(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(3), move |_| {
            let _ = notify_tx.send(Instant::now());
        }),
        paths,
    )
    .unwrap();
    session.update(SearchMode::fuzzy("file"));
    notify_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("initial search notification");

    let started = Instant::now();
    session.join();

    assert!(
        started.elapsed() <= Duration::from_millis(500),
        "workers should join within 500 ms, took {:?}",
        started.elapsed()
    );
    assert!(
        notify_rx
            .try_iter()
            .all(|published_at| published_at <= started + Duration::from_millis(50)),
        "cancelled workers must stop publication within 50 ms"
    );
}

#[test]
fn advancing_generation_retags_reused_catalog_snapshots() {
    let root = tempdir().unwrap();
    let paths = vec!["src/main.rs".to_string(), "README.md".to_string()];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_with_paths(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(7), move |generation| {
            let _ = notify_tx.send(generation);
        }),
        paths,
    )
    .unwrap();
    session.update(SearchMode::fuzzy("main"));
    let _ = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.generation == SessionGeneration(7)
            && matches!(snapshot.phase, SearchPhase::Complete)
    });

    session.set_generation(SessionGeneration(8));
    session.update(SearchMode::fuzzy("read"));
    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.generation == SessionGeneration(8)
            && snapshot.mode == SearchMode::fuzzy("read")
            && matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert_eq!(snapshot.generation, SessionGeneration(8));
    assert_eq!(session.generation(), SessionGeneration(8));
    session.join();
}

#[test]
fn fuzzy_ranking_is_deterministic_and_exposes_unicode_indices() {
    let root = tempdir().unwrap();
    let paths = vec![
        "b/main.rs".to_string(),
        "a/main.rs".to_string(),
        "src/main".to_string(),
        "src/main/".to_string(),
        "src/你好.rs".to_string(),
        "src/Main.rs".to_string(),
    ];
    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start_with_paths(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(5), move |generation| {
            let _ = notify_tx.send(generation);
        }),
        paths,
    )
    .unwrap();

    session.update(SearchMode::fuzzy("main"));
    let first = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });
    let a = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "a/main.rs")
        .unwrap();
    let b = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "b/main.rs")
        .unwrap();
    let file = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "src/main")
        .unwrap();
    let directory = first
        .matches
        .iter()
        .position(|candidate| candidate.path == "src/main/")
        .unwrap();
    assert_eq!(first.matches[a].score, first.matches[b].score);
    assert!(a < b);
    assert_eq!(first.matches[file].score, first.matches[directory].score);
    assert!(file < directory);

    session.update(SearchMode::fuzzy("你"));
    let unicode = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::fuzzy("你") && matches!(snapshot.phase, SearchPhase::Complete)
    });
    let chinese = unicode
        .matches
        .iter()
        .find(|candidate| candidate.path == "src/你好.rs")
        .unwrap();
    assert_eq!(chinese.indices, vec![4]);

    session.update(SearchMode::fuzzy("Main"));
    let smart_case = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::fuzzy("Main")
            && matches!(snapshot.phase, SearchPhase::Complete)
    });
    assert!(
        smart_case
            .matches
            .iter()
            .any(|candidate| candidate.path == "src/Main.rs")
    );
    assert!(
        smart_case
            .matches
            .iter()
            .all(|candidate| candidate.path != "src/main")
    );
    session.join();
}

#[cfg(unix)]
#[test]
fn discovery_excludes_external_symlink_candidate() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("outside.rs"), "outside").unwrap();
    symlink(
        outside.path().join("outside.rs"),
        root.path().join("outside-link.rs"),
    )
    .unwrap();
    fs::write(root.path().join("inside.rs"), "inside").unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(4), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::fuzzy("rs"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        matches!(snapshot.phase, SearchPhase::Complete)
    });

    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "inside.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "outside-link.rs")
    );
    session.join();
}

#[cfg(unix)]
#[test]
fn browsing_an_internal_directory_symlink_preserves_its_logical_prefix() {
    use std::os::unix::fs::symlink;

    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("real")).unwrap();
    fs::write(root.path().join("real/inside.rs"), "inside").unwrap();
    symlink(root.path().join("real"), root.path().join("alias")).unwrap();

    let (notify_tx, notify_rx) = mpsc::channel();
    let mut session = SearchSession::start(
        root.path(),
        SearchSessionOptions::new(SessionGeneration(6), move |generation| {
            let _ = notify_tx.send(generation);
        }),
    )
    .unwrap();
    session.update(SearchMode::browse("alias/"));

    let snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::browse("alias/") && !snapshot.matches.is_empty()
    });

    assert!(
        snapshot
            .matches
            .iter()
            .any(|candidate| candidate.path == "alias/inside.rs")
    );
    assert!(
        snapshot
            .matches
            .iter()
            .all(|candidate| candidate.path != "real/inside.rs")
    );
    session.join();
}
