//! Trust checks on a control-socket address, run before a bind or connect
//! touches it.
//!
//! On Unix the socket is a file.
//! [`validate_socket_address`](crate::validate::validate_socket_address) accepts
//! only a path directly inside the koshi runtime directory while that
//! directory is a directory owned by this user with mode `0700`, and is not a
//! symbolic link. On Windows the socket is a named pipe with no
//! filesystem location, and the check is that the name starts with `koshi-`.
//! Who may open the pipe is settled by the listener that creates it and by
//! the check on the connected peer, not by this module.
//!
//! A session other local users may reach sits in this user's own subdirectory
//! of the machine-wide shared directory.
//! [`validate_shared_socket_address`](crate::validate::validate_shared_socket_address)
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

/// Check that `socket_address` is a trustworthy place for a koshi control socket.
///
/// On Unix, `socket_address` must name a file directly inside `runtime_directory` (no
/// subdirectory, no path that steps out through `..`), and `runtime_directory` must
/// be a directory owned by this user with permission bits exactly `0700`; the
/// set-user-id, set-group-id and sticky bits are not checked. The check reads
/// `runtime_directory` without following a symbolic link, and refuses a link. On
/// Windows, `socket_address` is a pipe name and must start with `koshi-`; `runtime_directory`
/// is not read.
///
/// Each refusal is [`IpcError::UntrustedSocket`] naming `socket_address` and the
/// reason.
///
/// Callers resolve `runtime_directory` through `koshi_paths::resolve_runtime_directory()`.
pub fn validate_socket_address(
    socket_address: &str,
    runtime_directory: &Path,
) -> Result<(), IpcError> {
    let build_untrusted_socket_error = |trust_failure_reason: String| IpcError::UntrustedSocket {
        socket_address: socket_address.to_string(),
        trust_failure_reason,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if Path::new(socket_address).parent() != Some(runtime_directory) {
            return Err(build_untrusted_socket_error(
                "not directly inside the koshi runtime directory".to_string(),
            ));
        }
        let metadata = std::fs::symlink_metadata(runtime_directory).map_err(|io_error| {
            build_untrusted_socket_error(format!("runtime directory is unreadable: {io_error}"))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(build_untrusted_socket_error(
                "runtime directory is a symbolic link".to_string(),
            ));
        }
        if !metadata.is_dir() {
            return Err(build_untrusted_socket_error(
                "runtime directory is not a directory".to_string(),
            ));
        }
        let permission_mode = metadata.permissions().mode() & 0o777;
        if permission_mode != 0o700 {
            return Err(build_untrusted_socket_error(format!(
                "runtime directory mode is {permission_mode:03o}, expected 700"
            )));
        }
        let owner_user_id = metadata.uid();
        let effective_user_id = unsafe { libc::geteuid() };
        if owner_user_id != effective_user_id {
            return Err(build_untrusted_socket_error(format!(
                "runtime directory is owned by uid {owner_user_id}, expected {effective_user_id}"
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = runtime_directory;
        if !socket_address.starts_with("koshi-") {
            return Err(build_untrusted_socket_error(
                "pipe name is outside the koshi- namespace".to_string(),
            ));
        }
        Ok(())
    }
}

/// Check that `socket_address` is a trustworthy place for a koshi control socket other
/// local users may reach.
///
/// On Unix, `socket_address` must name a file directly inside `shared_user_directory` (no
/// subdirectory, no path that steps out through `..`), and `shared_user_directory`
/// must be a directory owned by this user with permission bits exactly
/// `0755`; the set-user-id, set-group-id and sticky bits are not checked. The
/// check reads `shared_user_directory` without following a symbolic link, and
/// refuses a link. On Windows, `socket_address` is a pipe name and must start with
/// `koshi-`; `shared_user_directory` is not read.
///
/// Each refusal is [`IpcError::UntrustedSocket`] naming `socket_address` and the
/// reason.
///
/// Callers resolve `shared_user_directory` through
/// `koshi_paths::ensure_shared_user_directory()`.
pub fn validate_shared_socket_address(
    socket_address: &str,
    shared_user_directory: &Path,
) -> Result<(), IpcError> {
    let build_untrusted_socket_error = |trust_failure_reason: String| IpcError::UntrustedSocket {
        socket_address: socket_address.to_string(),
        trust_failure_reason,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if Path::new(socket_address).parent() != Some(shared_user_directory) {
            return Err(build_untrusted_socket_error(
                "not directly inside the koshi shared session directory".to_string(),
            ));
        }
        let metadata = std::fs::symlink_metadata(shared_user_directory).map_err(|io_error| {
            build_untrusted_socket_error(format!(
                "shared session directory is unreadable: {io_error}"
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(build_untrusted_socket_error(
                "shared session directory is a symbolic link".to_string(),
            ));
        }
        if !metadata.is_dir() {
            return Err(build_untrusted_socket_error(
                "shared session directory is not a directory".to_string(),
            ));
        }
        let permission_mode = metadata.permissions().mode() & 0o777;
        if permission_mode != 0o755 {
            return Err(build_untrusted_socket_error(format!(
                "shared session directory mode is {permission_mode:03o}, expected 755"
            )));
        }
        let owner_user_id = metadata.uid();
        let effective_user_id = unsafe { libc::geteuid() };
        if owner_user_id != effective_user_id {
            return Err(build_untrusted_socket_error(format!(
                "shared session directory is owned by uid {owner_user_id}, expected {effective_user_id}"
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = shared_user_directory;
        if !socket_address.starts_with("koshi-") {
            return Err(build_untrusted_socket_error(
                "pipe name is outside the koshi- namespace".to_string(),
            ));
        }
        Ok(())
    }
}

/// Clear a leftover socket at `socket_address` before a server binds it.
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
/// `socket_address` between them has its socket file unlinked.
pub fn reclaim_stale_socket(socket_address: &str) -> Result<(), IpcError> {
    match Connection::connect(socket_address) {
        Ok(_) => Err(IpcError::SocketBusy {
            socket_address: socket_address.to_string(),
        }),
        Err(IpcError::NoListener { .. }) => {
            #[cfg(unix)]
            match std::fs::remove_file(socket_address) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(IpcError::Transport {
                        error_detail: error.to_string(),
                    });
                }
            }
            Ok(())
        }
        Err(probe_error) => Err(probe_error),
    }
}

#[cfg(test)]
mod tests;
