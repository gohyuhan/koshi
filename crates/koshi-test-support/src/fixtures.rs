//! Shared test fixtures.

use tempfile::TempDir;

/// Create an isolated runtime directory and remove it when the returned
/// [`TempDir`] drops.
///
/// Unix uses `/tmp` as the parent directory. Windows uses
/// [`std::env::temp_dir`].
///
/// # Panics
///
/// Panics when the directory cannot be created.
#[must_use]
pub fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

#[cfg(test)]
mod tests;
