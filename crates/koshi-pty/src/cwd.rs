//! The live working directory of a spawned child, asked from the OS.
//!
//! Each platform has its own way to read another process's current
//! directory: Linux exposes it as the `/proc/<pid>/cwd` symlink, and macOS
//! answers `proc_pidinfo` with the `PROC_PIDVNODEPATHINFO` flavor. Windows
//! has no supported way to ask, so the lookup answers `None` there and the
//! caller falls back to the shell's own OSC 7 report or the directory the
//! pane started in.

use std::path::PathBuf;

/// This machine's hostname, or `None` when the OS cannot say. Callers
/// compare it to the host a shell named in an OSC 7 report to decide
/// whether the reported directory is a local one.
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

/// The current working directory of the process `pid`, or `None` when the
/// OS cannot answer (the process exited, permission was denied, or the
/// platform has no lookup).
#[cfg(target_os = "linux")]
pub(crate) fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The current working directory of the process `pid`, or `None` when the
/// OS cannot answer (the process exited, permission was denied, or the
/// platform has no lookup).
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
    // `vip_path` is one NUL-terminated 1024-byte C path, declared by libc as
    // `[[c_char; 32]; 32]` only to sidestep an old-rustc array limit; the
    // bytes are contiguous.
    let bytes: &[u8; 1024] = unsafe { &*info.pvi_cdir.vip_path.as_ptr().cast() };
    let len = bytes.iter().position(|&byte| byte == 0)?;
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&bytes[..len])))
}

/// The current working directory of the process `pid`, or `None` when the
/// OS cannot answer. This platform has no lookup, so the answer is always
/// `None`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests;
