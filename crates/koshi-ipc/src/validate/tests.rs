//! Tests for the socket-address trust checks: the location and privacy
//! checks per platform for the private address and for the shared one, and
//! stale-socket reclaim over real sockets.

use super::*;
use crate::transport::Listener;

/// A socket address unique to this test: a temp-dir file path on Unix, a
/// pipe name on Windows.
fn build_test_socket_address(socket_label: &str) -> String {
    let unique_socket_label = format!("koshi-validate-{}-{socket_label}", std::process::id());
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(unique_socket_label)
            .with_extension("sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        unique_socket_label
    }
}

// --- validate_socket_address, Unix: location + privacy ---

/// A fresh directory with mode `0700`, standing in for the runtime dir.
#[cfg(unix)]
fn build_private_runtime_directory(directory_label: &str) -> std::path::PathBuf {
    let runtime_directory = std::env::temp_dir().join(format!(
        "koshi-validate-dir-{}-{directory_label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&runtime_directory).expect("create directory");
    set_file_mode(&runtime_directory, 0o700);
    runtime_directory
}

#[cfg(unix)]
fn set_file_mode(file_path: &Path, file_mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(file_path, std::fs::Permissions::from_mode(file_mode)).expect("chmod");
}

#[cfg(unix)]
#[test]
fn an_address_directly_inside_a_private_runtime_directory_passes() {
    let runtime_directory = build_private_runtime_directory("passes");
    let socket_address = runtime_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    validate_socket_address(&socket_address, &runtime_directory).expect("validate");
}

/// The location check compares path components: a trailing slash on
/// `runtime_directory` makes no difference.
#[cfg(unix)]
#[test]
fn a_runtime_directory_spelled_with_a_trailing_slash_still_matches() {
    let runtime_directory = build_private_runtime_directory("trailingslash");
    let socket_address = runtime_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let runtime_directory_with_slash =
        std::path::PathBuf::from(format!("{}/", runtime_directory.display()));
    validate_socket_address(&socket_address, &runtime_directory_with_slash).expect("validate");
}

#[cfg(unix)]
#[test]
fn an_address_outside_the_runtime_directory_is_untrusted() {
    let runtime_directory = build_private_runtime_directory("outside");
    let socket_address = std::env::temp_dir()
        .join("elsewhere.sock")
        .to_string_lossy()
        .into_owned();
    let error = validate_socket_address(&socket_address, &runtime_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn an_address_nested_below_the_runtime_directory_is_untrusted() {
    let runtime_directory = build_private_runtime_directory("nested");
    let socket_address = runtime_directory.join("sub").join("session.sock");
    let socket_address = socket_address.to_string_lossy();
    let error = validate_socket_address(&socket_address, &runtime_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn a_dot_dot_step_cannot_escape_the_runtime_directory() {
    let runtime_directory = build_private_runtime_directory("dotdot");
    let socket_address = format!("{}/../evil.sock", runtime_directory.display());
    let error = validate_socket_address(&socket_address, &runtime_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn an_address_that_is_the_runtime_directory_itself_is_untrusted() {
    let runtime_directory = build_private_runtime_directory("self");
    let socket_address = runtime_directory.to_string_lossy().into_owned();
    let error = validate_socket_address(&socket_address, &runtime_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_open_to_the_group_is_untrusted() {
    let runtime_directory = build_private_runtime_directory("groupopen");
    set_file_mode(&runtime_directory, 0o750);
    let socket_address = runtime_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let error = validate_socket_address(&socket_address, &runtime_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: runtime directory mode is 750, expected 700")
    );
}

#[cfg(unix)]
#[test]
fn a_missing_runtime_directory_is_untrusted() {
    let missing_runtime_directory =
        std::env::temp_dir().join(format!("koshi-validate-missing-{}", std::process::id()));
    let socket_address = missing_runtime_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let error = validate_socket_address(&socket_address, &missing_runtime_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "untrusted socket address {socket_address}: runtime directory is unreadable: \
             No such file or directory (os error 2)"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_regular_file_standing_in_for_the_runtime_directory_is_untrusted() {
    let blocking_directory_path =
        std::env::temp_dir().join(format!("koshi-validate-dir-{}-file", std::process::id()));
    std::fs::write(&blocking_directory_path, b"not a directory").expect("write file");
    set_file_mode(&blocking_directory_path, 0o700);
    let socket_address = blocking_directory_path
        .join("session.sock")
        .to_string_lossy()
        .into_owned();

    let error = validate_socket_address(&socket_address, &blocking_directory_path).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: runtime directory is not a directory")
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_standing_in_for_the_runtime_directory_is_untrusted() {
    // The link points at a directory that passes every other check; the link
    // itself is refused, so another user who plants it at the runtime path
    // before koshi first runs cannot place this session's socket inside a
    // directory the user never chose.
    let linked_directory = build_private_runtime_directory("linktarget");
    let link = std::env::temp_dir().join(format!("koshi-validate-dir-{}-link", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&linked_directory, &link).expect("symlink");
    let socket_address = link.join("session.sock").to_string_lossy().into_owned();

    let error = validate_socket_address(&socket_address, &link).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: runtime directory is a symbolic link")
    );
}

#[cfg(unix)]
#[test]
fn a_runtime_directory_this_user_owns_passes_the_owner_check() {
    let runtime_directory = build_private_runtime_directory("owner");
    let socket_address = runtime_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let owner = {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(&runtime_directory)
            .expect("read runtime directory metadata")
            .uid()
    };

    assert_eq!(owner, unsafe { libc::geteuid() });
    validate_socket_address(&socket_address, &runtime_directory).expect("validate");
}

// --- validate_shared_socket_address, Unix: location + shape ---

/// A fresh directory with mode `0755`, standing in for this user's own
/// subdirectory of the machine-wide shared directory.
#[cfg(unix)]
fn build_shared_session_directory(directory_label: &str) -> std::path::PathBuf {
    let shared_directory = std::env::temp_dir().join(format!(
        "koshi-validate-shared-{}-{directory_label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&shared_directory).expect("create directory");
    set_file_mode(&shared_directory, 0o755);
    shared_directory
}

#[cfg(unix)]
#[test]
fn an_address_directly_inside_a_shared_dir_this_user_owns_passes() {
    let shared_directory = build_shared_session_directory("passes");
    let socket_address = shared_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    validate_shared_socket_address(&socket_address, &shared_directory).expect("validate");
}

#[cfg(unix)]
#[test]
fn an_address_outside_the_shared_dir_is_untrusted() {
    let shared_directory = build_shared_session_directory("outside");
    let socket_address = std::env::temp_dir()
        .join("elsewhere.sock")
        .to_string_lossy()
        .into_owned();
    let error = validate_shared_socket_address(&socket_address, &shared_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "untrusted socket address {socket_address}: \
             not directly inside the koshi shared session directory"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_dot_dot_step_cannot_escape_the_shared_dir() {
    let shared_directory = build_shared_session_directory("dotdot");
    let socket_address = format!("{}/../evil.sock", shared_directory.display());
    let error = validate_shared_socket_address(&socket_address, &shared_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "untrusted socket address {socket_address}: \
             not directly inside the koshi shared session directory"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_missing_shared_dir_is_untrusted() {
    let missing_shared_directory = std::env::temp_dir().join(format!(
        "koshi-validate-shared-{}-missing",
        std::process::id()
    ));
    let socket_address = missing_shared_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let error =
        validate_shared_socket_address(&socket_address, &missing_shared_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "untrusted socket address {socket_address}: shared session directory is unreadable: \
             No such file or directory (os error 2)"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_regular_file_standing_in_for_the_shared_dir_is_untrusted() {
    let blocking_directory_path =
        std::env::temp_dir().join(format!("koshi-validate-shared-{}-file", std::process::id()));
    std::fs::write(&blocking_directory_path, b"not a directory").expect("write file");
    let socket_address = blocking_directory_path
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let error =
        validate_shared_socket_address(&socket_address, &blocking_directory_path).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: shared session directory is not a directory")
    );
}

#[cfg(unix)]
#[test]
fn a_shared_dir_other_users_may_write_is_untrusted() {
    let shared_directory = build_shared_session_directory("groupwrite");
    set_file_mode(&shared_directory, 0o775);
    let socket_address = shared_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let error = validate_shared_socket_address(&socket_address, &shared_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "untrusted socket address {socket_address}: shared session directory mode is 775, expected 755"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_shared_dir_closed_to_other_users_is_untrusted() {
    let shared_directory = build_shared_session_directory("private");
    set_file_mode(&shared_directory, 0o700);
    let socket_address = shared_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    let error = validate_shared_socket_address(&socket_address, &shared_directory).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "untrusted socket address {socket_address}: shared session directory mode is 700, expected 755"
        )
    );
}

/// Only the permission bits are checked: the sticky bit on a `0755` directory
/// is ignored.
#[cfg(unix)]
#[test]
fn a_shared_dir_with_the_sticky_bit_set_passes() {
    let shared_directory = build_shared_session_directory("sticky");
    set_file_mode(&shared_directory, 0o1755);
    let socket_address = shared_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    validate_shared_socket_address(&socket_address, &shared_directory).expect("validate");
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_standing_in_for_the_shared_dir_is_untrusted() {
    // The link points at a directory that passes every other check; the link
    // itself is refused.
    let linked_directory = build_shared_session_directory("linktarget");
    let link =
        std::env::temp_dir().join(format!("koshi-validate-shared-{}-link", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&linked_directory, &link).expect("symlink");
    let socket_address = link.join("session.sock").to_string_lossy().into_owned();

    let error = validate_shared_socket_address(&socket_address, &link).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("untrusted socket address {socket_address}: shared session directory is a symbolic link")
    );
}

// --- validate_socket_address, Windows: pipe namespace ---

#[cfg(windows)]
#[test]
fn a_koshi_prefixed_pipe_name_passes() {
    validate_socket_address("koshi-session-abc", Path::new("unused")).expect("validate");
}

#[cfg(windows)]
#[test]
fn a_pipe_name_outside_the_koshi_namespace_is_untrusted() {
    let error = validate_socket_address("other-session-abc", Path::new("unused")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "untrusted socket address other-session-abc: pipe name is outside the koshi- namespace"
    );
}

#[cfg(windows)]
#[test]
fn a_koshi_prefixed_shared_pipe_name_passes() {
    validate_shared_socket_address("koshi-session-abc", Path::new("unused")).expect("validate");
}

#[cfg(windows)]
#[test]
fn a_shared_pipe_name_outside_the_koshi_namespace_is_untrusted() {
    let error =
        validate_shared_socket_address("other-session-abc", Path::new("unused")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "untrusted socket address other-session-abc: pipe name is outside the koshi- namespace"
    );
}

// --- reclaim_stale_socket ---

#[test]
fn reclaiming_a_free_address_succeeds() {
    reclaim_stale_socket(&build_test_socket_address("free")).expect("reclaim");
}

/// Nothing at the address and no directory to hold it: the probe finds no
/// listener, and the unlink finds nothing to remove.
#[cfg(unix)]
#[test]
fn reclaiming_an_address_in_a_missing_directory_succeeds() {
    let missing_parent_directory = std::env::temp_dir().join(format!(
        "koshi-validate-{}-missing-parent",
        std::process::id()
    ));
    let socket_address = missing_parent_directory
        .join("session.sock")
        .to_string_lossy()
        .into_owned();
    assert!(!missing_parent_directory.exists());

    reclaim_stale_socket(&socket_address).expect("reclaim");
}

#[cfg(unix)]
#[test]
fn reclaiming_a_stale_socket_unlinks_its_file() {
    let socket_address = build_test_socket_address("stale");
    // `std`'s listener does not unlink its socket file on drop: the file
    // stays behind with nothing listening, as after a crash.
    let dead = std::os::unix::net::UnixListener::bind(&socket_address).expect("bind stale");
    drop(dead);
    assert!(Path::new(&socket_address).exists());

    reclaim_stale_socket(&socket_address).expect("reclaim");
    assert!(!Path::new(&socket_address).exists());
}

#[cfg(unix)]
#[test]
fn reclaiming_an_address_holding_a_regular_file_deletes_it() {
    let socket_address = build_test_socket_address("regularfile");
    // A non-socket file at the address refuses a socket connection the same
    // way a stale socket does; reclaim clears it as a leftover.
    std::fs::write(&socket_address, b"not a socket").expect("write file");

    reclaim_stale_socket(&socket_address).expect("reclaim");
    assert!(!Path::new(&socket_address).exists());
}

#[cfg(unix)]
#[test]
fn reclaiming_an_address_holding_a_directory_reports_the_unlink_failure() {
    let socket_address = build_test_socket_address("directory");
    std::fs::create_dir_all(&socket_address).expect("create dir");
    // The same unlink on a second directory gives the OS text the error carries.
    let control = build_test_socket_address("directory-control");
    std::fs::create_dir_all(&control).expect("create control dir");
    let expected_detail = std::fs::remove_file(&control).unwrap_err().to_string();

    let error = reclaim_stale_socket(&socket_address).unwrap_err();

    let IpcError::Transport { error_detail } = error else {
        panic!("wrong error: {error}");
    };
    assert_eq!(error_detail, expected_detail);
    assert!(Path::new(&socket_address).is_dir());
    std::fs::remove_dir(&socket_address).expect("cleanup");
    std::fs::remove_dir(&control).expect("cleanup control");
}

#[cfg(unix)]
#[test]
fn a_reclaimed_address_can_be_bound_again() {
    let socket_address = build_test_socket_address("rebind");
    let dead = std::os::unix::net::UnixListener::bind(&socket_address).expect("bind stale");
    drop(dead);

    reclaim_stale_socket(&socket_address).expect("reclaim");
    Listener::bind(&socket_address).expect("bind after reclaim");
}

#[test]
fn reclaiming_an_address_with_a_live_listener_is_refused() {
    let socket_address = build_test_socket_address("busy");
    let _listener = Listener::bind(&socket_address).expect("bind");

    let error = reclaim_stale_socket(&socket_address).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("another process is already listening at {socket_address}")
    );
    // The refused reclaim leaves the live listener's socket file in place.
    #[cfg(unix)]
    assert!(Path::new(&socket_address).exists());
}
