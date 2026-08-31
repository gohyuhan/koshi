//! Trust checks on a control-socket address, run before a bind or connect
//! touches it.
//!
//! On Unix the socket is a file.
//! [`validate_socket_addr`](crate::validate::validate_socket_addr) accepts
//! only a path directly inside the koshi runtime directory while that
//! directory is a directory owned by this user with mode `0700`, and is not a
//! symbolic link. On Windows the socket is a named pipe with no
//! filesystem location, and the check is that the name starts with `koshi-`.
//! Who may open the pipe is settled by the listener that creates it and by
//! the check on the connected peer, not by this module.
//!
//! A session other local users may reach sits in this user's own subdirectory
//! of the machine-wide shared directory.
//! [`validate_shared_socket_addr`](crate::validate::validate_shared_socket_addr)
//! accepts only a path directly inside that subdirectory while it is a
//! directory owned by this user with mode `0755`. On Windows that check is
//! the `koshi-` prefix again.
//!
//! A socket file can also be a leftover: the process that bound it died
//! without unlinking it, the file exists, and nothing listens (a "stale"
//! socket). [`reclaim_stale_socket`](crate::validate::reclaim_stale_socket)
//! clears exactly that case for a server about to bind. A caller connecting
//! to a stale socket gets
//! [`IpcError::NoListener`](crate::error::IpcError::NoListener) from
//! [`Connection::connect`](crate::transport::Connection::connect).

use std::path::Path;

use crate::error::IpcError;
use crate::transport::Connection;

/// Check that `addr` is a trustworthy place for a koshi control socket.
///
/// On Unix, `addr` must name a file directly inside `runtime_dir` (no
/// subdirectory, no path that steps out through `..`), and `runtime_dir` must
/// be a directory owned by this user with permission bits exactly `0700`; the
/// set-user-id, set-group-id and sticky bits are not checked. The check reads
/// `runtime_dir` without following a symbolic link, and refuses a link. On
/// Windows, `addr` is a pipe name and must start with `koshi-`; `runtime_dir`
/// is not read.
///
/// Each refusal is [`IpcError::UntrustedSocket`] naming `addr` and the
/// reason.
///
/// Callers resolve `runtime_dir` through `koshi_paths::runtime_dir()`.
pub fn validate_socket_addr(addr: &str, runtime_dir: &Path) -> Result<(), IpcError> {
    let untrusted = |reason: String| IpcError::UntrustedSocket {
        addr: addr.to_string(),
        reason,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if Path::new(addr).parent() != Some(runtime_dir) {
            return Err(untrusted(
                "not directly inside the koshi runtime directory".to_string(),
            ));
        }
        let metadata = std::fs::symlink_metadata(runtime_dir)
            .map_err(|error| untrusted(format!("runtime directory is unreadable: {error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(untrusted(
                "runtime directory is a symbolic link".to_string(),
            ));
        }
        if !metadata.is_dir() {
            return Err(untrusted(
                "runtime directory is not a directory".to_string(),
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(untrusted(format!(
                "runtime directory mode is {mode:03o}, expected 700"
            )));
        }
        let owner = metadata.uid();
        let euid = unsafe { libc::geteuid() };
        if owner != euid {
            return Err(untrusted(format!(
                "runtime directory is owned by uid {owner}, expected {euid}"
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        if !addr.starts_with("koshi-") {
            return Err(untrusted(
                "pipe name is outside the koshi- namespace".to_string(),
            ));
        }
        Ok(())
    }
}

/// Check that `addr` is a trustworthy place for a koshi control socket other
/// local users may reach.
///
/// On Unix, `addr` must name a file directly inside `shared_user_dir` (no
/// subdirectory, no path that steps out through `..`), and `shared_user_dir`
/// must be a directory owned by this user with permission bits exactly
/// `0755`; the set-user-id, set-group-id and sticky bits are not checked. The
/// check reads `shared_user_dir` without following a symbolic link, and
/// refuses a link. On Windows, `addr` is a pipe name and must start with
/// `koshi-`; `shared_user_dir` is not read.
///
/// Each refusal is [`IpcError::UntrustedSocket`] naming `addr` and the
/// reason.
///
/// Callers resolve `shared_user_dir` through
/// `koshi_paths::ensure_shared_user_dir()`.
pub fn validate_shared_socket_addr(addr: &str, shared_user_dir: &Path) -> Result<(), IpcError> {
    let untrusted = |reason: String| IpcError::UntrustedSocket {
        addr: addr.to_string(),
        reason,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if Path::new(addr).parent() != Some(shared_user_dir) {
            return Err(untrusted(
                "not directly inside the koshi shared session directory".to_string(),
            ));
        }
        let metadata = std::fs::symlink_metadata(shared_user_dir).map_err(|error| {
            untrusted(format!("shared session directory is unreadable: {error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(untrusted(
                "shared session directory is a symbolic link".to_string(),
            ));
        }
        if !metadata.is_dir() {
            return Err(untrusted(
                "shared session directory is not a directory".to_string(),
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o755 {
            return Err(untrusted(format!(
                "shared session directory mode is {mode:03o}, expected 755"
            )));
        }
        let owner = metadata.uid();
        let euid = unsafe { libc::geteuid() };
        if owner != euid {
            return Err(untrusted(format!(
                "shared session directory is owned by uid {owner}, expected {euid}"
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = shared_user_dir;
        if !addr.starts_with("koshi-") {
            return Err(untrusted(
                "pipe name is outside the koshi- namespace".to_string(),
            ));
        }
        Ok(())
    }
}

/// Clear a leftover socket at `addr` before a server binds it.
///
/// Probes the address with a connection attempt. A live listener answers,
/// and the address is refused as [`IpcError::SocketBusy`]; the probe
/// connection is dropped without sending anything. No listener means any
/// file at the path — a dead socket or any other leftover — is unlinked on
/// Unix; a file that fails to unlink for any reason other than being absent
/// is [`IpcError::Transport`] carrying the OS error text. On Windows a pipe
/// name vanishes with its last handle, and no listener means the name is
/// already free. Any other probe failure is returned as is.
///
/// The probe and the unlink are two separate steps: a listener that binds
/// `addr` between them has its socket file unlinked.
pub fn reclaim_stale_socket(addr: &str) -> Result<(), IpcError> {
    match Connection::connect(addr) {
        Ok(_) => Err(IpcError::SocketBusy {
            addr: addr.to_string(),
        }),
        Err(IpcError::NoListener { .. }) => {
            #[cfg(unix)]
            match std::fs::remove_file(addr) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(IpcError::Transport {
                        detail: error.to_string(),
                    });
                }
            }
            Ok(())
        }
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests;
