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
    validate_socket_directory(
        socket_address,
        runtime_directory,
        0o700,
        "runtime directory",
    )
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
    validate_socket_directory(
        socket_address,
        shared_user_directory,
        0o755,
        "shared session directory",
    )
}

/// Check that `socket_address` names a file directly inside `socket_directory`,
/// and that `socket_directory`, read without following a symbolic link, is a
/// directory owned by this user with permission bits exactly
/// `expected_permission_mode`. On Windows, check that `socket_address` starts
/// with `koshi-`; `socket_directory` is not read.
///
/// Each refusal is [`IpcError::UntrustedSocket`] naming `socket_address` and a
/// reason that names the directory as `directory_description`, e.g.
/// `runtime directory mode is 755, expected 700`.
fn validate_socket_directory(
    socket_address: &str,
    socket_directory: &Path,
    expected_permission_mode: u32,
    directory_description: &str,
) -> Result<(), IpcError> {
    let build_untrusted_socket_error = |trust_failure_reason: String| IpcError::UntrustedSocket {
        socket_address: socket_address.to_string(),
        trust_failure_reason,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if Path::new(socket_address).parent() != Some(socket_directory) {
            return Err(build_untrusted_socket_error(format!(
                "not directly inside the koshi {directory_description}"
            )));
        }
        let directory_metadata =
            std::fs::symlink_metadata(socket_directory).map_err(|io_error| {
                build_untrusted_socket_error(format!(
                    "{directory_description} is unreadable: {io_error}"
                ))
            })?;
        if directory_metadata.file_type().is_symlink() {
            return Err(build_untrusted_socket_error(format!(
                "{directory_description} is a symbolic link"
            )));
        }
        if !directory_metadata.is_dir() {
            return Err(build_untrusted_socket_error(format!(
                "{directory_description} is not a directory"
            )));
        }
        let permission_mode = directory_metadata.permissions().mode() & 0o777;
        if permission_mode != expected_permission_mode {
            return Err(build_untrusted_socket_error(format!(
                "{directory_description} mode is {permission_mode:03o}, expected {expected_permission_mode:03o}"
            )));
        }
        let owner_user_id = directory_metadata.uid();
        let effective_user_id = unsafe { libc::geteuid() };
        if owner_user_id != effective_user_id {
            return Err(build_untrusted_socket_error(format!(
                "{directory_description} is owned by uid {owner_user_id}, expected {effective_user_id}"
            )));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = (
            socket_directory,
            expected_permission_mode,
            directory_description,
        );
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
                Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {}
                Err(io_error) => {
                    return Err(IpcError::Transport {
                        error_detail: io_error.to_string(),
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
