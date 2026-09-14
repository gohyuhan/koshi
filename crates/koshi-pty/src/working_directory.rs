//! The live working directory of a spawned child, asked from the OS.
//!
//! Each platform has its own way to read another process's current
//! directory: Linux exposes it as the `/proc/<process_id>/cwd` symlink, and macOS
//! answers `proc_pidinfo` with the `PROC_PIDVNODEPATHINFO` flavor. On every
//! other platform, Windows included, the lookup answers `None`.

use std::path::PathBuf;

/// This machine's hostname, or `None` when the OS cannot say.
///
/// On Unix this is `gethostname(2)`, with invalid UTF-8 replaced by U+FFFD.
/// On Windows it is the `COMPUTERNAME` environment variable. Every other
/// platform answers `None`.
#[must_use]
pub fn get_local_hostname() -> Option<String> {
    #[cfg(unix)]
    {
        Some(
            nix::unistd::gethostname()
                .ok()?
                .to_string_lossy()
                .into_owned(),
        )
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The current working directory of the process `process_id`, read from the
/// `/proc/<process_id>/cwd` symlink, or `None` when the OS cannot answer (the
/// process exited or permission was denied).
#[cfg(target_os = "linux")]
pub(crate) fn get_process_working_directory(process_id: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{process_id}/cwd")).ok()
}

/// The current working directory of the process `process_id`, asked from
/// `proc_pidinfo`, or `None` when the OS cannot answer (the process exited,
/// permission was denied, or the answer is an empty path).
#[cfg(target_os = "macos")]
pub(crate) fn get_process_working_directory(process_id: u32) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut process_vnode_path_info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
    let process_vnode_path_info_byte_count =
        std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: the buffer pointer and size describe one properly aligned
    // `proc_vnodepathinfo`, which the kernel fills; no other invariants.
    let written_byte_count = unsafe {
        libc::proc_pidinfo(
            process_id as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            process_vnode_path_info.as_mut_ptr().cast(),
            process_vnode_path_info_byte_count,
        )
    };
    if written_byte_count != process_vnode_path_info_byte_count {
        return None;
    }
    // SAFETY: the kernel reported it filled the whole struct.
    let process_vnode_path_info = unsafe { process_vnode_path_info.assume_init() };
    // SAFETY: `vip_path` is one NUL-terminated 1024-byte C path; libc declares
    // it as `[[c_char; 32]; 32]`, and the bytes are contiguous.
    let working_directory_path_bytes: &[u8; 1024] =
        unsafe { &*process_vnode_path_info.pvi_cdir.vip_path.as_ptr().cast() };
    let working_directory_path_byte_count = working_directory_path_bytes
        .iter()
        .position(|&byte| byte == 0)?;
    if working_directory_path_byte_count == 0 {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(
        &working_directory_path_bytes[..working_directory_path_byte_count],
    )))
}

/// The current working directory of the process `process_id`. This platform has no
/// lookup; the answer is always `None`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn get_process_working_directory(_process_id: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests;
