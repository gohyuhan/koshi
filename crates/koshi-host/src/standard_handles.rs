//! The standard input, output, and error handles of this process on Windows.

use windows_sys::Win32::Foundation::{
    SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Clears `HANDLE_FLAG_INHERIT` on the standard input, output, and error
/// handles of this process. A child process then holds only the standard
/// handles that its own spawn sets for it: `std::process::Command` gives a
/// child an inheritable duplicate for `Stdio::inherit()`, and a new handle for
/// `Stdio::null()` and `Stdio::piped()`. A standard handle that is not set or
/// is invalid is skipped, and a handle whose flag cannot be cleared keeps it.
///
/// Example: this process writes its standard output into a pipe, and starts a
/// child with `Stdio::null()` for all three streams. The child holds no handle
/// to that pipe.
pub fn clear_standard_handle_inheritance() {
    for standard_handle_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes a standard handle id and reads no
        // memory of this process.
        let standard_handle = unsafe { GetStdHandle(standard_handle_id) };
        if standard_handle.is_null() || standard_handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `standard_handle` is a handle this process holds, and
        // `SetHandleInformation` changes only its flags.
        unsafe { SetHandleInformation(standard_handle, HANDLE_FLAG_INHERIT, 0) };
    }
}

#[cfg(test)]
mod tests;
