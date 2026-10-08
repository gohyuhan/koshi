//! Tests for clearing the inherit flag on the standard handles of this
//! process.

use super::*;

use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};

/// The flags `GetHandleInformation` reads for `handle`.
fn read_handle_flags(handle: HANDLE) -> u32 {
    let mut handle_flags: u32 = 0;
    // SAFETY: `handle` is a handle this process holds, and `handle_flags`
    // outlives the call.
    let has_read_flags = unsafe { GetHandleInformation(handle, &raw mut handle_flags) } != 0;
    assert!(has_read_flags, "GetHandleInformation reads the flags");
    handle_flags
}

/// The standard handles of this process that are set and valid.
fn list_set_standard_handles() -> Vec<HANDLE> {
    [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        // SAFETY: `GetStdHandle` takes a standard handle id and reads no
        // memory of this process.
        .map(|standard_handle_id| unsafe { GetStdHandle(standard_handle_id) })
        .filter(|standard_handle| {
            !standard_handle.is_null() && *standard_handle != INVALID_HANDLE_VALUE
        })
        .collect()
}

#[test]
fn clearing_standard_handle_inheritance_leaves_no_set_standard_handle_inheritable() {
    let standard_handles = list_set_standard_handles();
    assert_ne!(standard_handles.len(), 0);
    for standard_handle in &standard_handles {
        // SAFETY: `standard_handle` is a handle this process holds, and
        // `SetHandleInformation` changes only its flags.
        unsafe { SetHandleInformation(*standard_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        assert_eq!(
            read_handle_flags(*standard_handle) & HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT
        );
    }

    clear_standard_handle_inheritance();

    for standard_handle in &standard_handles {
        assert_eq!(read_handle_flags(*standard_handle) & HANDLE_FLAG_INHERIT, 0);
    }
}
