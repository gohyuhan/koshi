//! The live working directory of a spawned child, asked from the OS.
//!
//! Each platform has its own way to read another process's current
//! directory: Linux exposes it as the `/proc/<pid>/cwd` symlink, and macOS
//! answers `proc_pidinfo` with the `PROC_PIDVNODEPATHINFO` flavor. On every
//! other platform, Windows included, the lookup answers `None`.

use std::path::PathBuf;

/// This machine's hostname, or `None` when the OS cannot say.
///
/// On Unix this is `gethostname(2)`, with invalid UTF-8 replaced by U+FFFD.
/// On Windows it is the `COMPUTERNAME` environment variable. Every other
/// platform answers `None`.
#[must_use]
pub fn local_hostname() -> Option<String> {
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

/// The current working directory of the process `pid`, read from the
/// `/proc/<pid>/cwd` symlink, or `None` when the OS cannot answer (the
/// process exited or permission was denied).
#[cfg(target_os = "linux")]
pub(crate) fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The current working directory of the process `pid`, asked from
/// `proc_pidinfo`, or `None` when the OS cannot answer (the process exited,
/// permission was denied, or the answer is an empty path).
#[cfg(target_os = "macos")]
pub(crate) fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: the buffer pointer and size describe one properly aligned
    // `proc_vnodepathinfo`, which the kernel fills; no other invariants.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the kernel reported it filled the whole struct.
    let info = unsafe { info.assume_init() };
    // SAFETY: `vip_path` is one NUL-terminated 1024-byte C path; libc declares
    // it as `[[c_char; 32]; 32]`, and the bytes are contiguous.
    let bytes: &[u8; 1024] = unsafe { &*info.pvi_cdir.vip_path.as_ptr().cast() };
    let len = bytes.iter().position(|&byte| byte == 0)?;
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&bytes[..len])))
}

/// The current working directory of the process `pid`. This platform has no
/// lookup; the answer is always `None`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests;
