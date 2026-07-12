#[cfg(target_os = "macos")]
use std::path::PathBuf;

use tempfile::TempDir;

pub fn sandbox_test_parent(prefix: &str) -> TempDir {
    #[cfg(target_os = "macos")]
    {
        let home = PathBuf::from(
            std::env::var_os("HOME").expect("HOME is required for macOS Seatbelt tests"),
        )
        .canonicalize()
        .expect("canonical macOS HOME");
        for root in [
            Some(PathBuf::from("/tmp")),
            std::env::var_os("TMPDIR").map(PathBuf::from),
        ]
        .into_iter()
        .flatten()
        {
            let root = root.canonicalize().unwrap_or(root);
            assert!(
                !home.starts_with(&root),
                "macOS Seatbelt fixtures require HOME outside temporary allow root {}",
                root.display()
            );
        }
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(home)
            .expect("sandbox parent outside temporary allow roots")
    }
    #[cfg(not(target_os = "macos"))]
    {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("sandbox parent")
    }
}
