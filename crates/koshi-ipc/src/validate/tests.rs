//! Tests for the socket-address trust checks: the location and privacy
//! checks per platform for the private address and for the shared one, and
//! stale-socket reclaim over real sockets.

use super::*;
use crate::transport::Listener;

/// A socket address unique to this test: a temp-dir file path on Unix, a
/// pipe name on Windows.
fn test_addr(tag: &str) -> String {
    let unique = format!("koshi-validate-{}-{tag}", std::process::id());
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(unique)
            .with_extension("sock")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        unique
    }
}

// --- validate_socket_addr, Unix: location + privacy ---

/// A fresh directory with mode `0700`, standing in for the runtime dir.
#[cfg(unix)]
fn private_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("koshi-validate-dir-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    set_mode(&dir, 0o700);
    dir
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

#[cfg(unix)]
#[test]
fn an_address_directly_inside_a_private_runtime_dir_passes() {
    let dir = private_dir("passes");
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    validate_socket_addr(&addr, &dir).expect("validate");
}

/// The location check compares path components: a trailing slash on
/// `runtime_dir` makes no difference.
#[cfg(unix)]
#[test]
fn a_runtime_dir_spelled_with_a_trailing_slash_still_matches() {
    let dir = private_dir("trailingslash");
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let with_slash = std::path::PathBuf::from(format!("{}/", dir.display()));
    validate_socket_addr(&addr, &with_slash).expect("validate");
}

#[cfg(unix)]
#[test]
fn an_address_outside_the_runtime_dir_is_untrusted() {
    let dir = private_dir("outside");
    let addr = std::env::temp_dir()
        .join("elsewhere.sock")
        .to_string_lossy()
        .into_owned();
    let err = validate_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn an_address_nested_below_the_runtime_dir_is_untrusted() {
    let dir = private_dir("nested");
    let addr = dir.join("sub").join("session.sock");
    let addr = addr.to_string_lossy();
    let err = validate_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn a_dot_dot_step_cannot_escape_the_runtime_dir() {
    let dir = private_dir("dotdot");
    let addr = format!("{}/../evil.sock", dir.display());
    let err = validate_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn an_address_that_is_the_runtime_dir_itself_is_untrusted() {
    let dir = private_dir("self");
    let addr = dir.to_string_lossy().into_owned();
    let err = validate_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: not directly inside the koshi runtime directory")
    );
}

#[cfg(unix)]
#[test]
fn a_runtime_dir_open_to_the_group_is_untrusted() {
    let dir = private_dir("groupopen");
    set_mode(&dir, 0o750);
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let err = validate_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: runtime directory mode is 750, expected 700")
    );
}

#[cfg(unix)]
#[test]
fn a_missing_runtime_dir_is_untrusted() {
    let dir = std::env::temp_dir().join(format!("koshi-validate-missing-{}", std::process::id()));
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let err = validate_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "untrusted socket address {addr}: runtime directory is unreadable: \
             No such file or directory (os error 2)"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_regular_file_standing_in_for_the_runtime_dir_is_untrusted() {
    let file = std::env::temp_dir().join(format!("koshi-validate-dir-{}-file", std::process::id()));
    std::fs::write(&file, b"not a directory").expect("write file");
    set_mode(&file, 0o700);
    let addr = file.join("session.sock").to_string_lossy().into_owned();

    let err = validate_socket_addr(&addr, &file).unwrap_err();

    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: runtime directory is not a directory")
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_standing_in_for_the_runtime_dir_is_untrusted() {
    // The link points at a directory that passes every other check; the link
    // itself is refused, so another user who plants it at the runtime path
    // before koshi first runs cannot place this session's socket inside a
    // directory the user never chose.
    let target = private_dir("linktarget");
    let link = std::env::temp_dir().join(format!("koshi-validate-dir-{}-link", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let addr = link.join("session.sock").to_string_lossy().into_owned();

    let err = validate_socket_addr(&addr, &link).unwrap_err();

    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: runtime directory is a symbolic link")
    );
}

#[cfg(unix)]
#[test]
fn a_runtime_dir_this_user_owns_passes_the_owner_check() {
    let dir = private_dir("owner");
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let owner = {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(&dir).expect("stat").uid()
    };

    assert_eq!(owner, unsafe { libc::geteuid() });
    validate_socket_addr(&addr, &dir).expect("validate");
}

// --- validate_shared_socket_addr, Unix: location + shape ---

/// A fresh directory with mode `0755`, standing in for this user's own
/// subdirectory of the machine-wide shared directory.
#[cfg(unix)]
fn shared_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "koshi-validate-shared-{}-{tag}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    set_mode(&dir, 0o755);
    dir
}

#[cfg(unix)]
#[test]
fn an_address_directly_inside_a_shared_dir_this_user_owns_passes() {
    let dir = shared_dir("passes");
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    validate_shared_socket_addr(&addr, &dir).expect("validate");
}

#[cfg(unix)]
#[test]
fn an_address_outside_the_shared_dir_is_untrusted() {
    let dir = shared_dir("outside");
    let addr = std::env::temp_dir()
        .join("elsewhere.sock")
        .to_string_lossy()
        .into_owned();
    let err = validate_shared_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "untrusted socket address {addr}: \
             not directly inside the koshi shared session directory"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_dot_dot_step_cannot_escape_the_shared_dir() {
    let dir = shared_dir("dotdot");
    let addr = format!("{}/../evil.sock", dir.display());
    let err = validate_shared_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "untrusted socket address {addr}: \
             not directly inside the koshi shared session directory"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_missing_shared_dir_is_untrusted() {
    let dir = std::env::temp_dir().join(format!(
        "koshi-validate-shared-{}-missing",
        std::process::id()
    ));
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let err = validate_shared_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "untrusted socket address {addr}: shared session directory is unreadable: \
             No such file or directory (os error 2)"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_regular_file_standing_in_for_the_shared_dir_is_untrusted() {
    let file =
        std::env::temp_dir().join(format!("koshi-validate-shared-{}-file", std::process::id()));
    std::fs::write(&file, b"not a directory").expect("write file");
    let addr = file.join("session.sock").to_string_lossy().into_owned();
    let err = validate_shared_socket_addr(&addr, &file).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: shared session directory is not a directory")
    );
}

#[cfg(unix)]
#[test]
fn a_shared_dir_other_users_may_write_is_untrusted() {
    let dir = shared_dir("groupwrite");
    set_mode(&dir, 0o775);
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let err = validate_shared_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "untrusted socket address {addr}: shared session directory mode is 775, expected 755"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_shared_dir_closed_to_other_users_is_untrusted() {
    let dir = shared_dir("private");
    set_mode(&dir, 0o700);
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    let err = validate_shared_socket_addr(&addr, &dir).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "untrusted socket address {addr}: shared session directory mode is 700, expected 755"
        )
    );
}

/// Only the permission bits are checked: the sticky bit on a `0755` directory
/// is ignored.
#[cfg(unix)]
#[test]
fn a_shared_dir_with_the_sticky_bit_set_passes() {
    let dir = shared_dir("sticky");
    set_mode(&dir, 0o1755);
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    validate_shared_socket_addr(&addr, &dir).expect("validate");
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_standing_in_for_the_shared_dir_is_untrusted() {
    // The link points at a directory that passes every other check; the link
    // itself is refused.
    let target = shared_dir("linktarget");
    let link =
        std::env::temp_dir().join(format!("koshi-validate-shared-{}-link", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let addr = link.join("session.sock").to_string_lossy().into_owned();

    let err = validate_shared_socket_addr(&addr, &link).unwrap_err();

    assert_eq!(
        err.to_string(),
        format!("untrusted socket address {addr}: shared session directory is a symbolic link")
    );
}

// --- validate_socket_addr, Windows: pipe namespace ---

#[cfg(windows)]
#[test]
fn a_koshi_prefixed_pipe_name_passes() {
    validate_socket_addr("koshi-session-abc", Path::new("unused")).expect("validate");
}

#[cfg(windows)]
#[test]
fn a_pipe_name_outside_the_koshi_namespace_is_untrusted() {
    let err = validate_socket_addr("other-session-abc", Path::new("unused")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "untrusted socket address other-session-abc: pipe name is outside the koshi- namespace"
    );
}

#[cfg(windows)]
#[test]
fn a_koshi_prefixed_shared_pipe_name_passes() {
    validate_shared_socket_addr("koshi-session-abc", Path::new("unused")).expect("validate");
}

#[cfg(windows)]
#[test]
fn a_shared_pipe_name_outside_the_koshi_namespace_is_untrusted() {
    let err = validate_shared_socket_addr("other-session-abc", Path::new("unused")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "untrusted socket address other-session-abc: pipe name is outside the koshi- namespace"
    );
}

// --- reclaim_stale_socket ---

#[test]
fn reclaiming_a_free_address_succeeds() {
    reclaim_stale_socket(&test_addr("free")).expect("reclaim");
}

/// Nothing at the address and no directory to hold it: the probe finds no
/// listener, and the unlink finds nothing to remove.
#[cfg(unix)]
#[test]
fn reclaiming_an_address_in_a_missing_directory_succeeds() {
    let dir = std::env::temp_dir().join(format!(
        "koshi-validate-{}-missing-parent",
        std::process::id()
    ));
    let addr = dir.join("session.sock").to_string_lossy().into_owned();
    assert!(!dir.exists());

    reclaim_stale_socket(&addr).expect("reclaim");
}

#[cfg(unix)]
#[test]
fn reclaiming_a_stale_socket_unlinks_its_file() {
    let addr = test_addr("stale");
    // `std`'s listener does not unlink its socket file on drop: the file
    // stays behind with nothing listening, as after a crash.
    let dead = std::os::unix::net::UnixListener::bind(&addr).expect("bind stale");
    drop(dead);
    assert!(Path::new(&addr).exists());

    reclaim_stale_socket(&addr).expect("reclaim");
    assert!(!Path::new(&addr).exists());
}

#[cfg(unix)]
#[test]
fn reclaiming_an_address_holding_a_regular_file_deletes_it() {
    let addr = test_addr("regularfile");
    // A non-socket file at the address refuses a socket connection the same
    // way a stale socket does; reclaim clears it as a leftover.
    std::fs::write(&addr, b"not a socket").expect("write file");

    reclaim_stale_socket(&addr).expect("reclaim");
    assert!(!Path::new(&addr).exists());
}

#[cfg(unix)]
#[test]
fn reclaiming_an_address_holding_a_directory_reports_the_unlink_failure() {
    let addr = test_addr("directory");
    std::fs::create_dir_all(&addr).expect("create dir");
    // The same unlink on a second directory gives the OS text the error carries.
    let control = test_addr("directory-control");
    std::fs::create_dir_all(&control).expect("create control dir");
    let expected_detail = std::fs::remove_file(&control).unwrap_err().to_string();

    let err = reclaim_stale_socket(&addr).unwrap_err();

    let IpcError::Transport { detail } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(detail, expected_detail);
    assert!(Path::new(&addr).is_dir());
    std::fs::remove_dir(&addr).expect("cleanup");
    std::fs::remove_dir(&control).expect("cleanup control");
}

#[cfg(unix)]
#[test]
fn a_reclaimed_address_can_be_bound_again() {
    let addr = test_addr("rebind");
    let dead = std::os::unix::net::UnixListener::bind(&addr).expect("bind stale");
    drop(dead);

    reclaim_stale_socket(&addr).expect("reclaim");
    Listener::bind(&addr).expect("bind after reclaim");
}

#[test]
fn reclaiming_an_address_with_a_live_listener_is_refused() {
    let addr = test_addr("busy");
    let _listener = Listener::bind(&addr).expect("bind");

    let err = reclaim_stale_socket(&addr).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("another process is already listening at {addr}")
    );
    // The refused reclaim leaves the live listener's socket file in place.
    #[cfg(unix)]
    assert!(Path::new(&addr).exists());
}
