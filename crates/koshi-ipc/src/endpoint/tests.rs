//! Tests for the endpoint file: the per-session path shape, the write/read
//! roundtrip through the atomic writer, redaction in `Debug`, the private
//! mode of a fresh file, and the missing / unreadable / unwritable failure
//! cases. Also the address helpers and the empty advert marker.

use tempfile::TempDir;
use uuid::Uuid;

use super::*;

/// An endpoint file holding a fixed address, secret and process id.
fn endpoint() -> EndpointFile {
    EndpointFile {
        socket: "/run/koshi/session-abc.sock".to_string(),
        token: ConnectionToken::new("k7QxSecret"),
        pid: 4242,
    }
}

#[test]
fn the_path_is_session_uuid_json_directly_inside_the_runtime_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        EndpointFile::path(Path::new("/run/koshi"), session),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.json")
    );
}

#[test]
fn the_resume_path_sits_beside_the_endpoint_file_under_the_same_name() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        resume_path(Path::new("/run/koshi"), session),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.resume")
    );
    assert_eq!(
        resume_path(Path::new("/run/koshi"), session).parent(),
        EndpointFile::path(Path::new("/run/koshi"), session).parent()
    );
}

#[cfg(unix)]
#[test]
fn the_shared_socket_addr_is_session_uuid_sock_inside_the_shared_user_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        shared_socket_addr(Path::new("/tmp/koshi/501"), session),
        "/tmp/koshi/501/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.sock"
    );
}

#[cfg(windows)]
#[test]
fn the_shared_socket_addr_is_the_same_koshi_namespaced_pipe_name() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        shared_socket_addr(Path::new(r"C:\unused"), session),
        "koshi-session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    );
    assert_eq!(
        shared_socket_addr(Path::new(r"C:\unused"), session),
        socket_addr(Path::new(r"C:\other"), session)
    );
}

#[test]
fn the_advert_path_is_session_uuid_directly_inside_the_shared_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        advert_path(Path::new("/run/koshi"), session),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
    );
}

/// The marker only has to exist; carrying no bytes is what keeps a secret
/// out of a file every local user can read.
#[test]
fn a_written_advert_marker_is_an_empty_file() {
    let dir = TempDir::new().expect("create temp dir");
    let path = advert_path(dir.path(), SessionId::new());

    write_advert(&path).expect("write advert marker");

    assert_eq!(
        std::fs::metadata(&path).expect("stat advert marker").len(),
        0
    );
}

#[test]
fn removing_the_advert_marker_takes_it_off_the_disk() {
    let dir = TempDir::new().expect("create temp dir");
    let path = advert_path(dir.path(), SessionId::new());
    write_advert(&path).expect("write advert marker");

    remove_advert(&path);

    assert!(!path.exists());
    // A path with nothing at it is left alone rather than reported.
    remove_advert(&path);
    assert!(!path.exists());
}

#[test]
fn a_written_endpoint_file_reads_back_identical() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-roundtrip.json");
    let original = endpoint();

    original.write(&path).expect("write endpoint file");

    assert_eq!(
        EndpointFile::read(&path).expect("read endpoint file"),
        original
    );
}

/// The file is how the CLI learns the secret, so it carries the real value.
#[test]
fn the_file_on_disk_carries_the_real_secret() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-secret.json");

    endpoint().write(&path).expect("write endpoint file");

    let data = std::fs::read_to_string(&path).expect("read file bytes");
    assert_eq!(
        data,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","pid":4242}"#
    );
}

#[test]
fn debug_prints_the_token_redacted() {
    assert_eq!(
        format!("{:?}", endpoint()),
        r#"EndpointFile { socket: "/run/koshi/session-abc.sock", token: ConnectionToken(***), pid: 4242 }"#
    );
}

#[test]
fn rewriting_replaces_the_previous_content() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-rewrite.json");
    endpoint().write(&path).expect("write first endpoint file");
    let second = EndpointFile {
        socket: "/run/koshi/session-def.sock".to_string(),
        token: ConnectionToken::new("secondSecret"),
        pid: 4343,
    };

    second.write(&path).expect("write second endpoint file");

    assert_eq!(
        EndpointFile::read(&path).expect("read endpoint file"),
        second
    );
}

#[cfg(unix)]
#[test]
fn a_fresh_endpoint_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-private.json");

    endpoint().write(&path).expect("write endpoint file");

    let mode = std::fs::metadata(&path)
        .expect("stat endpoint file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn reading_a_missing_file_is_endpoint_file_missing() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-none.json");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileMissing { path: reported }) => {
            assert_eq!(reported, path.display().to_string());
        }
        other => panic!("expected EndpointFileMissing, got {other:?}"),
    }
}

#[test]
fn reading_junk_bytes_is_endpoint_file_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-junk.json");
    std::fs::write(&path, b"not json").expect("write junk");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(detail, "expected ident at line 1 column 2");
        }
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
}

#[test]
fn reading_a_directory_is_endpoint_file_unreadable_not_missing() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-dir.json");
    std::fs::create_dir(&path).expect("create a directory at the path");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable { path: reported, .. }) => {
            assert_eq!(reported, path.display().to_string());
        }
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
}

#[test]
fn a_file_with_an_unknown_field_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-unknown.json");
    std::fs::write(
        &path,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","pid":4242,"extra":1}"#,
    )
    .expect("write file");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(
                detail,
                "unknown field `extra`, expected one of `socket`, `token`, `pid` at line 1 column 79"
            );
        }
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
}

#[test]
fn a_file_missing_a_field_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-partial.json");
    std::fs::write(
        &path,
        r#"{"socket":"/run/koshi/session-abc.sock","pid":4242}"#,
    )
    .expect("write file");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(detail, "missing field `token` at line 1 column 51");
        }
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
}

#[test]
fn an_empty_file_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-empty.json");
    std::fs::write(&path, b"").expect("write empty file");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable {
            path: reported,
            detail,
        }) => {
            assert_eq!(reported, path.display().to_string());
            assert_eq!(detail, "EOF while parsing a value at line 1 column 0");
        }
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
}

#[test]
fn writing_the_advert_marker_into_a_missing_directory_is_endpoint_file_write() {
    let dir = TempDir::new().expect("create temp dir");
    let path = advert_path(&dir.path().join("no-such-subdir"), SessionId::new());

    match write_advert(&path) {
        Err(IpcError::EndpointFileWrite { path: reported, .. }) => {
            assert_eq!(reported, path.display().to_string());
        }
        other => panic!("expected EndpointFileWrite, got {other:?}"),
    }
}

#[test]
fn the_shared_socket_addr_is_the_socket_addr_inside_the_shared_user_dir() {
    let dir = TempDir::new().expect("create temp dir");
    let session = SessionId::new();

    assert_eq!(
        shared_socket_addr(dir.path(), session),
        socket_addr(dir.path(), session)
    );
}

#[test]
fn writing_into_a_missing_directory_is_endpoint_file_write() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("no-such-subdir").join("session-x.json");

    match endpoint().write(&path) {
        Err(IpcError::EndpointFileWrite { path: reported, .. }) => {
            assert_eq!(reported, path.display().to_string());
        }
        other => panic!("expected EndpointFileWrite, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn the_socket_addr_is_session_uuid_sock_inside_the_runtime_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        socket_addr(Path::new("/run/koshi"), session),
        "/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.sock"
    );
}

#[cfg(windows)]
#[test]
fn the_socket_addr_is_a_koshi_namespaced_pipe_name() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session = SessionId::from_uuid(uuid);
    assert_eq!(
        socket_addr(Path::new(r"C:\unused"), session),
        "koshi-session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    );
}

#[test]
fn the_socket_addr_passes_the_socket_location_check() {
    let dir = TempDir::new().expect("create temp dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restrict runtime dir");
    }
    let session = SessionId::new();
    let addr = socket_addr(dir.path(), session);
    crate::validate::validate_socket_addr(&addr, dir.path()).expect("validate socket addr");
}

#[cfg(unix)]
#[test]
fn removing_the_socket_file_takes_the_path_off_the_disk() {
    let dir = TempDir::new().expect("create temp dir");
    let session = SessionId::new();
    let addr = socket_addr(dir.path(), session);
    std::fs::write(&addr, b"").expect("create the leftover socket file");

    remove_socket_file(&addr);

    assert!(!Path::new(&addr).exists());
    // A path with nothing at it is left alone rather than reported.
    remove_socket_file(&addr);
    assert!(!Path::new(&addr).exists());
}

#[cfg(windows)]
#[test]
fn removing_a_pipe_name_leaves_the_filesystem_untouched() {
    // A Windows address is a pipe name, not a path, so a file that happens to
    // carry that name in the working directory must survive.
    let dir = TempDir::new().expect("create temp dir");
    let session = SessionId::new();
    let addr = socket_addr(dir.path(), session);
    let namesake = dir.path().join(&addr);
    std::fs::write(&namesake, b"").expect("create the namesake file");

    remove_socket_file(&addr);

    assert!(namesake.exists());
}
