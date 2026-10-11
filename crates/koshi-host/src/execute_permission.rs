//! Whether this user may run a file, asked of the kernel.

use std::path::Path;

#[cfg(test)]
mod tests;

/// Whether this user may run `program_path`, asked of the kernel with
/// `access(X_OK)`.
///
/// The answer covers the owner, group and other bits, any access control
/// list, and a filesystem mounted without execute permission. It is made
/// against this process's real user and group. A directory this user may
/// search gives `true`. A path that holds a NUL byte gives `false`.
#[must_use]
pub fn can_user_execute(program_path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(program_path_c_string) = CString::new(program_path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `program_path_c_string` holds a NUL-terminated C string that
    // outlives the call, and `access` only reads it.
    unsafe { libc::access(program_path_c_string.as_ptr(), libc::X_OK) == 0 }
}
