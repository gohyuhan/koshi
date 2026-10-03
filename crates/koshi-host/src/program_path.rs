//! The path of the koshi program this process runs, with the symbolic links it
//! was started through kept.
//!
//! Example: a package manager links `/usr/local/bin/koshi` to
//! `/opt/koshi/0.5.0/koshi`, and the user runs `koshi`.
//! [`resolve_program_path`] gives `/usr/local/bin/koshi`. Once an upgrade
//! points the link at `/opt/koshi/0.6.0/koshi`, that path runs 0.6.0.

use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
#[cfg(unix)]
use std::{ffi::OsStr, path::Path};

#[cfg(test)]
mod tests;

/// The path of the koshi program this process runs.
///
/// On Linux and macOS, the first command-line argument names the path:
///
/// - An argument that holds a `/`, such as `/usr/local/bin/koshi` or
///   `./bin/koshi`, is the path as written, made absolute against the working
///   directory.
/// - A bare name, such as `koshi`, is the first file of that name in the
///   folders of `PATH`, in order, that has an execute permission bit set.
///
/// That path counts only when it names the same file as
/// [`std::env::current_exe`]. In every other case, and on Windows, the result
/// is `current_exe`. On Linux, `current_exe` gives the file a symbolic link
/// points at. On macOS and Windows, it gives the path the program was started
/// from.
///
/// The first call that succeeds reads the path, and every call after it in
/// this process gives that same path. A first call made after an upgrade points
/// the link at another file gives `current_exe`: the link then names a file
/// this process does not run.
///
/// # Errors
/// The [`io::Error`] of `current_exe`.
pub fn resolve_program_path() -> io::Result<PathBuf> {
    static PROGRAM_PATH: OnceLock<PathBuf> = OnceLock::new();
    if let Some(program_path) = PROGRAM_PATH.get() {
        return Ok(program_path.clone());
    }
    let executable_path = std::env::current_exe()?;
    #[cfg(unix)]
    let executable_path = std::env::args_os()
        .next()
        .and_then(|launch_argument| {
            find_launch_path(&launch_argument, std::env::var_os("PATH").as_deref())
        })
        .filter(|launch_path| is_same_file(launch_path, &executable_path))
        .unwrap_or(executable_path);
    Ok(PROGRAM_PATH.get_or_init(|| executable_path).clone())
}

/// The absolute path that `launch_argument`, the first command-line argument,
/// names: as written when it holds a `/`, and otherwise the first file of that
/// name, with an execute permission bit set, in the folders of `search_path`,
/// read as `PATH` is. An empty folder in `search_path` is the working
/// directory. `None` when no such file is found, or the working directory
/// cannot be read.
///
/// Example: `koshi` with `search_path` `/opt/none:/usr/local/bin`, where only
/// `/usr/local/bin/koshi` exists, gives `/usr/local/bin/koshi`.
#[cfg(unix)]
fn find_launch_path(launch_argument: &OsStr, search_path: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    if launch_argument.as_bytes().contains(&b'/') {
        return std::path::absolute(launch_argument).ok();
    }
    let launch_path = std::env::split_paths(search_path?)
        .map(|search_directory| search_directory.join(launch_argument))
        .find(|candidate_path| is_executable_file(candidate_path))?;
    std::path::absolute(launch_path).ok()
}

/// Whether `candidate_path` names a file, following symbolic links, with at
/// least one execute permission bit set.
#[cfg(unix)]
fn is_executable_file(candidate_path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(candidate_path).is_ok_and(|candidate_metadata| {
        candidate_metadata.is_file() && candidate_metadata.permissions().mode() & 0o111 != 0
    })
}

/// Whether `first_path` and `second_path` name the same file once every
/// symbolic link in them is followed. `false` when either cannot be followed
/// to a file that exists.
#[cfg(unix)]
fn is_same_file(first_path: &Path, second_path: &Path) -> bool {
    match (
        std::fs::canonicalize(first_path),
        std::fs::canonicalize(second_path),
    ) {
        (Ok(first_file_path), Ok(second_file_path)) => first_file_path == second_file_path,
        _ => false,
    }
}
