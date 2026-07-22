//! Tests for the endpoint file: the per-session path shape, the write/read
//! roundtrip through the atomic writer, redaction in `Debug`, the private
//! mode of a fresh file, and the missing / unreadable / unwritable failure
//! cases.

use tempfile::TempDir;
use uuid::Uuid;

use super::*;

/// An endpoint file holding a fixed address and secret.
fn endpoint() -> EndpointFile {
    EndpointFile {
        socket: "/run/koshi/session-abc.sock".to_string(),
        token: ConnectionToken::new("k7QxSecret"),
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
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret"}"#
    );
}

#[test]
fn debug_prints_the_token_redacted() {
    let rendered = format!("{:?}", endpoint());
    assert!(!rendered.contains("k7QxSecret"), "{rendered}");
    assert!(rendered.contains("ConnectionToken(***)"), "{rendered}");
}

#[test]
fn rewriting_replaces_the_previous_content() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-rewrite.json");
    endpoint().write(&path).expect("write first endpoint file");
    let second = EndpointFile {
        socket: "/run/koshi/session-def.sock".to_string(),
        token: ConnectionToken::new("secondSecret"),
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
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","extra":1}"#,
    )
    .expect("write file");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable { .. }) => {}
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
}

#[test]
fn a_file_missing_a_field_is_unreadable() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("session-partial.json");
    std::fs::write(&path, r#"{"socket":"/run/koshi/session-abc.sock"}"#).expect("write file");

    match EndpointFile::read(&path) {
        Err(IpcError::EndpointFileUnreadable { .. }) => {}
        other => panic!("expected EndpointFileUnreadable, got {other:?}"),
    }
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
