//! Tests for the socket-address trust checks: the location and privacy
//! checks per platform, and stale-socket reclaim over real sockets.

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

// --- reclaim_stale_socket ---

#[test]
fn reclaiming_a_free_address_succeeds() {
    reclaim_stale_socket(&test_addr("free")).expect("reclaim");
}

#[cfg(unix)]
#[test]
fn reclaiming_a_stale_socket_unlinks_its_file() {
    let addr = test_addr("stale");
    // `std`'s listener does not unlink its socket file on drop, which is
    // exactly the leftover a crashed process leaves behind.
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
    // way a stale socket does, so reclaim clears it as a leftover.
    std::fs::write(&addr, b"not a socket").expect("write file");

    reclaim_stale_socket(&addr).expect("reclaim");
    assert!(!Path::new(&addr).exists());
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
}
