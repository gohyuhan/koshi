//! Tests for the endpoint file: the per-session path shape, the write/read
//! roundtrip through the atomic writer, redaction in `Debug`, the private
//! mode of a fresh file, and the missing / unreadable / unwritable failure
//! cases. Also the address helpers and the empty advert marker.

use tempfile::TempDir;
use uuid::Uuid;

use super::*;

/// An endpoint file holding a fixed address, secret and process id.
fn build_test_endpoint_file() -> EndpointFile {
    EndpointFile {
        socket_address: "/run/koshi/session-abc.sock".to_string(),
        connection_token: ConnectionToken::from_secret("k7QxSecret"),
        process_id: 4242,
    }
}

#[test]
fn the_path_is_session_uuid_json_directly_inside_the_runtime_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        EndpointFile::resolve_endpoint_file_path(Path::new("/run/koshi"), session_id),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.json")
    );
}

#[test]
fn the_resolve_resume_file_path_sits_beside_the_endpoint_file_under_the_same_name() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        resolve_resume_file_path(Path::new("/run/koshi"), session_id),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.resume")
    );
    assert_eq!(
        resolve_resume_file_path(Path::new("/run/koshi"), session_id).parent(),
        EndpointFile::resolve_endpoint_file_path(Path::new("/run/koshi"), session_id).parent()
    );
}

#[cfg(unix)]
#[test]
fn the_compute_shared_socket_address_is_session_uuid_sock_inside_the_shared_user_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        compute_shared_socket_address(Path::new("/tmp/koshi/501"), session_id),
        "/tmp/koshi/501/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.sock"
    );
}

#[cfg(windows)]
#[test]
fn the_compute_shared_socket_address_is_the_same_koshi_namespaced_pipe_name() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        compute_shared_socket_address(Path::new(r"C:\unused"), session_id),
        "koshi-session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    );
    assert_eq!(
        compute_shared_socket_address(Path::new(r"C:\unused"), session_id),
        compute_socket_address(Path::new(r"C:\other"), session_id)
    );
}

#[test]
fn the_resolve_advertisement_marker_path_is_session_uuid_directly_inside_the_shared_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        resolve_advertisement_marker_path(Path::new("/run/koshi"), session_id),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
    );
}

/// The marker only has to exist; carrying no bytes is what keeps a secret
/// out of a file every local user can read.
#[test]
fn a_written_advert_marker_is_an_empty_file() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path =
        resolve_advertisement_marker_path(test_directory.path(), SessionId::new());

    write_advertisement_marker(&endpoint_file_path).expect("write advert marker");

    assert_eq!(
        std::fs::metadata(&endpoint_file_path)
            .expect("stat advert marker")
            .len(),
        0
    );
}

#[test]
fn removing_the_advert_marker_takes_it_off_the_disk() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path =
        resolve_advertisement_marker_path(test_directory.path(), SessionId::new());
    write_advertisement_marker(&endpoint_file_path).expect("write advert marker");

    remove_advertisement_marker(&endpoint_file_path);

    assert!(!endpoint_file_path.exists());
    // A path with nothing at it is left alone rather than reported.
    remove_advertisement_marker(&endpoint_file_path);
    assert!(!endpoint_file_path.exists());
}

#[test]
fn a_written_endpoint_file_reads_back_identical() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-roundtrip.json");
    let original_endpoint_file = build_test_endpoint_file();

    original_endpoint_file
        .write_to_path(&endpoint_file_path)
        .expect("write endpoint file");

    assert_eq!(
        EndpointFile::load_from_path(&endpoint_file_path).expect("read endpoint file"),
        original_endpoint_file
    );
}

/// The file is how the CLI learns the secret, so it carries the real value.
#[test]
fn the_file_on_disk_carries_the_real_secret() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-secret.json");

    build_test_endpoint_file()
        .write_to_path(&endpoint_file_path)
        .expect("write endpoint file");

    let endpoint_json = std::fs::read_to_string(&endpoint_file_path).expect("read file bytes");
    assert_eq!(
        endpoint_json,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","pid":4242}"#
    );
}

#[test]
fn debug_prints_the_token_redacted() {
    assert_eq!(
        format!("{:?}", build_test_endpoint_file()),
        r#"EndpointFile { socket_address: "/run/koshi/session-abc.sock", connection_token: ConnectionToken(***), process_id: 4242 }"#
    );
}

#[test]
fn rewriting_replaces_the_previous_content() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-rewrite.json");
    build_test_endpoint_file()
        .write_to_path(&endpoint_file_path)
        .expect("write first endpoint file");
    let replacement_endpoint_file = EndpointFile {
        socket_address: "/run/koshi/session-def.sock".to_string(),
        connection_token: ConnectionToken::from_secret("secondSecret"),
        process_id: 4343,
    };

    replacement_endpoint_file
        .write_to_path(&endpoint_file_path)
        .expect("write second endpoint file");

    assert_eq!(
        EndpointFile::load_from_path(&endpoint_file_path).expect("read endpoint file"),
        replacement_endpoint_file
    );
}

#[cfg(unix)]
#[test]
fn a_fresh_endpoint_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-private.json");

    build_test_endpoint_file()
        .write_to_path(&endpoint_file_path)
        .expect("write endpoint file");

    let file_mode = std::fs::metadata(&endpoint_file_path)
        .expect("stat endpoint file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
}

#[test]
fn reading_a_missing_file_is_endpoint_file_missing() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-none.json");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileMissing {
            endpoint_file_path: reported_endpoint_file_path,
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
        }
        unexpected_error => panic!("expected EndpointFileMissing, got {unexpected_error:?}"),
    }
}

#[test]
fn reading_junk_bytes_is_endpoint_file_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-junk.json");
    std::fs::write(&endpoint_file_path, b"not json").expect("write junk");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable {
            endpoint_file_path: reported_endpoint_file_path,
            error_detail,
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
            assert_eq!(error_detail, "expected ident at line 1 column 2");
        }
        unexpected_error => {
            panic!("expected EndpointFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn reading_a_directory_is_endpoint_file_unreadable_not_missing() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-test.json");
    std::fs::create_dir(&endpoint_file_path).expect("create a directory at the path");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable {
            endpoint_file_path: reported_endpoint_file_path,
            ..
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
        }
        unexpected_error => {
            panic!("expected EndpointFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn a_file_with_an_unknown_field_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-unknown.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","pid":4242,"extra":1}"#,
    )
    .expect("write file");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable {
            endpoint_file_path: reported_endpoint_file_path,
            error_detail,
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
            assert_eq!(
                error_detail,
                "unknown field `extra`, expected one of `socket`, `token`, `pid` at line 1 column 79"
            );
        }
        unexpected_error => {
            panic!("expected EndpointFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn a_file_missing_a_field_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-partial.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket":"/run/koshi/session-abc.sock","pid":4242}"#,
    )
    .expect("write file");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable {
            endpoint_file_path: reported_endpoint_file_path,
            error_detail,
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
            assert_eq!(error_detail, "missing field `token` at line 1 column 51");
        }
        unexpected_error => {
            panic!("expected EndpointFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn an_empty_file_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-empty.json");
    std::fs::write(&endpoint_file_path, b"").expect("write empty file");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable {
            endpoint_file_path: reported_endpoint_file_path,
            error_detail,
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
            assert_eq!(error_detail, "EOF while parsing a value at line 1 column 0");
        }
        unexpected_error => {
            panic!("expected EndpointFileUnreadable, got {unexpected_error:?}")
        }
    }
}

#[test]
fn writing_the_advert_marker_into_a_missing_directory_names_the_marker() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = resolve_advertisement_marker_path(
        &test_directory.path().join("no-such-subdir"),
        SessionId::new(),
    );

    let advertisement_marker_error = write_advertisement_marker(&endpoint_file_path)
        .expect_err("a missing directory refuses the write");

    match &advertisement_marker_error {
        IpcError::AdvertWrite {
            advert_marker_path: reported_marker_path,
            ..
        } => {
            assert_eq!(
                *reported_marker_path,
                endpoint_file_path.display().to_string()
            );
        }
        unexpected_error => panic!("expected AdvertWrite, got {unexpected_error:?}"),
    }
    assert!(
        advertisement_marker_error
            .to_string()
            .starts_with("advert marker "),
        "the message names the marker, not an endpoint file: {advertisement_marker_error}"
    );
}

#[test]
fn the_compute_shared_socket_address_is_the_compute_socket_address_inside_the_shared_user_dir() {
    let test_directory = TempDir::new().expect("create test directory");
    let session_id = SessionId::new();

    assert_eq!(
        compute_shared_socket_address(test_directory.path(), session_id),
        compute_socket_address(test_directory.path(), session_id)
    );
}

#[test]
fn writing_into_a_missing_directory_is_endpoint_file_write() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory
        .path()
        .join("no-such-subdir")
        .join("session-x.json");

    match build_test_endpoint_file().write_to_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileWrite {
            endpoint_file_path: reported_endpoint_file_path,
            ..
        }) => {
            assert_eq!(
                reported_endpoint_file_path,
                endpoint_file_path.display().to_string()
            );
        }
        unexpected_error => panic!("expected EndpointFileWrite, got {unexpected_error:?}"),
    }
}

#[cfg(unix)]
#[test]
fn the_compute_socket_address_is_session_uuid_sock_inside_the_runtime_dir() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        compute_socket_address(Path::new("/run/koshi"), session_id),
        "/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.sock"
    );
}

#[cfg(windows)]
#[test]
fn the_compute_socket_address_is_a_koshi_namespaced_pipe_name() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        compute_socket_address(Path::new(r"C:\unused"), session_id),
        "koshi-session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    );
}

#[test]
fn the_compute_socket_address_passes_the_socket_location_check() {
    let test_directory = TempDir::new().expect("create test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            test_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("restrict runtime directory");
    }
    let session_id = SessionId::new();
    let socket_address = compute_socket_address(test_directory.path(), session_id);
    crate::validate::validate_socket_address(&socket_address, test_directory.path())
        .expect("validate socket address");
}

#[cfg(unix)]
#[test]
fn removing_the_socket_file_takes_the_path_off_the_disk() {
    let test_directory = TempDir::new().expect("create test directory");
    let session_id = SessionId::new();
    let socket_address = compute_socket_address(test_directory.path(), session_id);
    std::fs::write(&socket_address, b"").expect("create the leftover socket file");

    remove_socket_file(&socket_address);

    assert!(!Path::new(&socket_address).exists());
    // A path with nothing at it is left alone rather than reported.
    remove_socket_file(&socket_address);
    assert!(!Path::new(&socket_address).exists());
}

#[cfg(windows)]
#[test]
fn removing_a_pipe_name_leaves_the_filesystem_untouched() {
    // A Windows address is a pipe name, not a endpoint_file_path, so a file that happens to
    // carry that name in the working directory must survive.
    let test_directory = TempDir::new().expect("create test directory");
    let session_id = SessionId::new();
    let socket_address = compute_socket_address(test_directory.path(), session_id);
    let pipe_name_file_path = test_directory.path().join(&socket_address);
    std::fs::write(&pipe_name_file_path, b"").expect("create the pipe-name file");

    remove_socket_file(&socket_address);

    assert!(pipe_name_file_path.exists());
}
