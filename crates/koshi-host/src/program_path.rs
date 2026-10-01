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

#[cfg(test)]
mod tests;

/// The path of the koshi program this process runs: the path the operating
/// system started it from, with every symbolic link in that path kept,
/// whatever the names in it. The first command-line argument plays no part.
///
/// - On Linux, the path is the one `execve` received, from the `AT_EXECFN`
///   entry of the auxiliary vector (getauxval(3)), made absolute against the
///   working directory. Example: `/usr/local/bin/kk`, a link to
///   `/opt/koshi/koshi-0.5.0`, gives `/usr/local/bin/kk`, and so does
///   `exec -a /usr/bin/true /usr/local/bin/kk`. A path under `/proc` or
///   `/dev/fd` names an open file of a process, such as `/dev/fd/3` after
///   `fexecve`. That path, a process with no `AT_EXECFN` entry, and a working
///   directory that cannot be read give [`std::env::current_exe`], which on
///   Linux is the file a symbolic link points at.
/// - On macOS and Windows, the result is [`std::env::current_exe`], which
///   gives the path the program was started from.
///
/// The first call that succeeds reads the path, and every call after it in
/// this process gives that same path.
///
/// # Errors
/// The [`io::Error`] of `current_exe`.
pub fn resolve_program_path() -> io::Result<PathBuf> {
    static PROGRAM_PATH: OnceLock<PathBuf> = OnceLock::new();
    if let Some(program_path) = PROGRAM_PATH.get() {
        return Ok(program_path.clone());
    }
    #[cfg(target_os = "linux")]
    let program_path = match find_executed_path() {
        Some(executed_path) => executed_path,
        None => std::env::current_exe()?,
    };
    #[cfg(not(target_os = "linux"))]
    let program_path = std::env::current_exe()?;
    Ok(PROGRAM_PATH.get_or_init(|| program_path).clone())
}

/// The path that `execve` received to start this process: the `AT_EXECFN`
/// entry of the auxiliary vector (getauxval(3)), made absolute against the
/// working directory. `None` when the process has no `AT_EXECFN` entry, when
/// the working directory cannot be read, or when the path is under `/proc` or
/// `/dev/fd`.
///
/// Example: a shell runs `kk`, and `PATH` finds `/usr/local/bin/kk`: the
/// result is `/usr/local/bin/kk`. `fexecve` on file descriptor 3 gives the
/// path `/dev/fd/3`, and the result is `None`.
#[cfg(target_os = "linux")]
fn find_executed_path() -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    // SAFETY: `getauxval` takes one integer and returns one integer.
    let executed_path_address = unsafe { libc::getauxval(libc::AT_EXECFN) };
    if executed_path_address == 0 {
        return None;
    }
    // SAFETY: a nonzero `AT_EXECFN` value is the address of the 0-ended path
    // that the kernel copies onto the process stack at `execve`. That memory
    // stays mapped for the life of the process, and koshi never writes it.
    let executed_path_text =
        unsafe { CStr::from_ptr(executed_path_address as *const libc::c_char) };
    let executed_path =
        std::path::absolute(Path::new(OsStr::from_bytes(executed_path_text.to_bytes()))).ok()?;
    if executed_path.starts_with("/proc") || executed_path.starts_with("/dev/fd") {
        return None;
    }
    Some(executed_path)
}
