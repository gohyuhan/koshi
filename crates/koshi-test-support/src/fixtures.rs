//! Test fixtures shared across the suite.

use tempfile::TempDir;

/// A fresh temporary directory standing in for the koshi runtime directory,
/// removed when the returned handle drops.
///
/// On Unix the directory is made under `/tmp`, whose short paths keep a
/// socket path inside the platform's socket-address length cap; on Windows
/// it is made under the user's temporary directory.
///
/// # Panics
/// Panics when the directory cannot be created.
#[must_use]
pub fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}
