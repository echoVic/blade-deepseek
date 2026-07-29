use std::io::{self, Write};
use std::path::Path;

use orca_platform::fs::{AtomicWritePolicy, atomic_write, atomic_write_with, open_nofollow};

#[test]
fn atomic_replace_never_leaves_a_partial_file_or_temp_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.json");

    atomic_write(&path, br#"{"revision":1}"#, AtomicWritePolicy::NoFollow).expect("first write");
    atomic_write(&path, br#"{"revision":2}"#, AtomicWritePolicy::NoFollow).expect("replace");

    assert_eq!(std::fs::read(&path).expect("read"), br#"{"revision":2}"#);
    assert_no_temp_artifacts(temp.path());
}

#[test]
fn atomic_write_with_streams_and_replaces_the_destination() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("transcript.jsonl");
    std::fs::write(&path, b"old").expect("old content");

    atomic_write_with(&path, AtomicWritePolicy::NoFollow, |file| {
        file.write_all(b"first\n")?;
        file.write_all(b"second\n")
    })
    .expect("streamed replace");

    assert_eq!(std::fs::read(&path).expect("read"), b"first\nsecond\n");
    assert_no_temp_artifacts(temp.path());
}

#[test]
fn failed_atomic_write_with_keeps_the_old_destination_and_cleans_the_temp_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("transcript.jsonl");
    std::fs::write(&path, b"old").expect("old content");

    let error = atomic_write_with(&path, AtomicWritePolicy::NoFollow, |file| {
        file.write_all(b"partial")?;
        Err(io::Error::other("injected writer failure"))
    })
    .expect_err("writer failure");

    assert!(matches!(
        error,
        orca_platform::PlatformError::Io {
            kind: io::ErrorKind::Other,
            ..
        }
    ));
    assert_eq!(std::fs::read(&path).expect("old destination"), b"old");
    assert_no_temp_artifacts(temp.path());
}

#[test]
fn failed_replace_keeps_the_old_destination_and_cleans_the_temp_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let destination = temp.path().join("state.json");
    std::fs::create_dir(&destination).expect("directory collision");
    std::fs::write(destination.join("keep"), b"old").expect("old content");

    assert!(atomic_write(&destination, b"new", AtomicWritePolicy::NoFollow).is_err());
    assert_eq!(
        std::fs::read(destination.join("keep")).expect("old destination survives"),
        b"old"
    );
    assert_no_temp_artifacts(temp.path());
}

#[cfg(unix)]
#[test]
fn no_follow_rejects_symlink_destinations_and_opening_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target.json");
    let link = temp.path().join("link.json");
    std::fs::write(&target, b"old").expect("target");
    symlink(&target, &link).expect("symlink");

    assert!(atomic_write(&link, b"new", AtomicWritePolicy::NoFollow).is_err());
    assert!(open_nofollow(&link).is_err());
    assert_eq!(std::fs::read(&target).expect("target remains"), b"old");
    assert_no_temp_artifacts(temp.path());
}

#[cfg(unix)]
#[test]
fn replace_destination_replaces_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target.json");
    let link = temp.path().join("link.json");
    std::fs::write(&target, b"target").expect("target");
    symlink(&target, &link).expect("symlink");

    atomic_write(&link, b"replacement", AtomicWritePolicy::ReplaceDestination)
        .expect("replace symlink directory entry");

    assert_eq!(std::fs::read(&link).expect("replacement"), b"replacement");
    assert_eq!(std::fs::read(&target).expect("target remains"), b"target");
    assert!(
        !std::fs::symlink_metadata(&link)
            .expect("replacement metadata")
            .file_type()
            .is_symlink()
    );
    assert_no_temp_artifacts(temp.path());
}

#[cfg(unix)]
#[test]
fn replacement_preserves_existing_unix_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("private.json");
    std::fs::write(&path, b"old").expect("old file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
        .expect("set permissions");

    atomic_write(&path, b"new", AtomicWritePolicy::NoFollow).expect("replace");

    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
}

#[test]
fn no_follow_opens_a_regular_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("regular.txt");
    std::fs::write(&path, b"content").expect("regular file");

    let file = open_nofollow(&path).expect("open regular file");
    assert_eq!(file.metadata().expect("metadata").len(), 7);
}

#[cfg(windows)]
#[test]
fn no_follow_atomic_write_rejects_a_directory_junction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let junction = temp.path().join("junction");
    std::fs::create_dir(&target).expect("junction target");

    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .expect("invoke mklink /J");
    assert!(
        output.status.success(),
        "mklink /J failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for policy in [
        AtomicWritePolicy::NoFollow,
        AtomicWritePolicy::ReplaceDestination,
    ] {
        let error = atomic_write(&junction, b"new", policy).expect_err("junction must be rejected");
        assert!(matches!(
            error,
            orca_platform::PlatformError::ReparsePointRejected { .. }
        ));
    }
    assert_no_temp_artifacts(temp.path());
}

fn assert_no_temp_artifacts(directory: &Path) {
    let artifacts = directory
        .read_dir()
        .expect("entries")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".orca-") && name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(artifacts.is_empty(), "temp artifacts: {artifacts:?}");
}
