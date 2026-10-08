//! Tests for the endpoint file: the per-session path shape, the write/read
//! roundtrip through the atomic writer, the format number it carries, the two
//! format 1 shapes it converts, redaction in `Debug`, the private mode of a
//! fresh file, and the missing / unreadable / unwritable failure cases. Also
//! the address helpers, the empty advertisement marker, and the program file.

use std::time::SystemTime;

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

#[test]
fn a_fresh_resume_file_reads_as_a_swap_in_flight_and_a_stale_one_does_not() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_id = SessionId::new();
    assert!(
        !is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a session with no resume file is not replacing its image"
    );

    let resume_file_path = resolve_resume_file_path(runtime_directory_fixture.path(), session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");
    assert!(
        is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file written just now is a swap in flight"
    );

    let stale_resume_file = std::fs::File::options()
        .write(true)
        .open(&resume_file_path)
        .expect("the resume file opens for writing");
    stale_resume_file
        .set_modified(SystemTime::now() - RESTART_WINDOW_DURATION - Duration::from_secs(1))
        .expect("the resume file is aged");
    assert!(
        !is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file older than the window is a swap that died"
    );
}

#[test]
fn a_resume_file_stamped_ahead_of_this_machines_clock_reads_as_a_swap_in_flight() {
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory_fixture.path(), session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");
    let ahead_resume_file = std::fs::File::options()
        .write(true)
        .open(&resume_file_path)
        .expect("the resume file opens for writing");
    ahead_resume_file
        .set_modified(SystemTime::now() + RESTART_WINDOW_DURATION * 10)
        .expect("the resume file is stamped ahead");

    assert!(
        is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a stamp this machine's clock has not reached yet reads as fresh"
    );
}

#[test]
fn a_resume_file_exactly_as_old_as_the_window_reads_as_a_swap_that_died() {
    // 2 seconds inside the window gives `true`; the window itself gives `false`.
    let runtime_directory_fixture = TempDir::new().expect("create runtime directory fixture");
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory_fixture.path(), session_id);
    std::fs::write(&resume_file_path, b"{}").expect("the resume file is written");
    let aged_resume_file = std::fs::File::options()
        .write(true)
        .open(&resume_file_path)
        .expect("the resume file opens for writing");

    aged_resume_file
        .set_modified(SystemTime::now() - RESTART_WINDOW_DURATION + Duration::from_secs(2))
        .expect("the resume file is aged to just inside the window");
    assert!(
        is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file younger than the window is a swap in flight"
    );

    aged_resume_file
        .set_modified(SystemTime::now() - RESTART_WINDOW_DURATION)
        .expect("the resume file is aged to the window itself");
    assert!(
        !is_replacing_its_image(runtime_directory_fixture.path(), session_id),
        "a resume file as old as the window is a swap that died"
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

/// A written marker exists and holds zero bytes.
#[test]
fn a_written_advertisement_marker_is_an_empty_file() {
    let test_directory = TempDir::new().expect("create test directory");
    let advertisement_marker_path =
        resolve_advertisement_marker_path(test_directory.path(), SessionId::new());

    write_advertisement_marker(&advertisement_marker_path).expect("write advertisement marker");

    assert_eq!(
        std::fs::metadata(&advertisement_marker_path)
            .expect("stat advertisement marker")
            .len(),
        0
    );
}

#[test]
fn deleting_the_advertisement_marker_takes_it_off_the_disk() {
    let test_directory = TempDir::new().expect("create test directory");
    let advertisement_marker_path =
        resolve_advertisement_marker_path(test_directory.path(), SessionId::new());
    write_advertisement_marker(&advertisement_marker_path).expect("write advertisement marker");

    delete_advertisement_marker(&advertisement_marker_path);

    assert!(!advertisement_marker_path.exists());
    // A second deletion of a path with nothing at it does nothing and reports nothing.
    delete_advertisement_marker(&advertisement_marker_path);
    assert!(!advertisement_marker_path.exists());
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

/// The file on disk holds the format number and the real secret.
#[test]
fn the_file_on_disk_carries_its_format_and_the_real_secret() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-secret.json");

    build_test_endpoint_file()
        .write_to_path(&endpoint_file_path)
        .expect("write endpoint file");

    let endpoint_json = std::fs::read_to_string(&endpoint_file_path).expect("read file bytes");
    assert_eq!(
        endpoint_json,
        r#"{"file_format":2,"socket_address":"/run/koshi/session-abc.sock","connection_token":"k7QxSecret","process_id":4242}"#
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
fn a_file_with_a_field_this_build_does_not_know_reads_the_fields_it_knows() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-unknown.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"file_format":2,"socket_address":"/run/koshi/session-abc.sock","connection_token":"k7QxSecret","process_id":4242,"extra":1}"#,
    )
    .expect("write file");

    assert_eq!(
        EndpointFile::load_from_path(&endpoint_file_path).expect("read endpoint file"),
        build_test_endpoint_file()
    );
}

#[test]
fn a_format_one_file_with_the_long_field_names_reads_as_the_same_endpoint() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-long-names.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket_address":"/run/koshi/session-abc.sock","connection_token":"k7QxSecret","process_id":4242}"#,
    )
    .expect("write file");

    assert_eq!(
        EndpointFile::load_from_path(&endpoint_file_path).expect("read endpoint file"),
        build_test_endpoint_file()
    );
}

#[test]
fn a_format_one_file_with_the_short_field_names_reads_as_the_same_endpoint() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-short-names.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","pid":4242}"#,
    )
    .expect("write file");

    assert_eq!(
        EndpointFile::load_from_path(&endpoint_file_path).expect("read endpoint file"),
        build_test_endpoint_file()
    );
}

#[test]
fn a_format_one_file_with_a_short_name_and_a_field_it_does_not_know_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-short-extra.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret","pid":4242,"extra":1}"#,
    )
    .expect("write file");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable { error_detail, .. }) => {
            assert_eq!(
                error_detail,
                "missing field `socket_address` at line 1 column 82"
            );
        }
        unexpected_result => {
            panic!("expected EndpointFileUnreadable, got {unexpected_result:?}")
        }
    }
}

#[test]
fn the_file_a_koshi_0_1_0_window_writes_is_that_windows_endpoint_file() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-window.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket":"/run/koshi/session-abc.sock","token":"k7QxSecret"}"#,
    )
    .expect("write file");

    let load_error = EndpointFile::load_from_path(&endpoint_file_path)
        .expect_err("a koshi 0.1.0 window's file is not this build's endpoint file");

    let IpcError::Koshi010WindowEndpointFile {
        endpoint_file_path: reported_endpoint_file_path,
    } = &load_error
    else {
        panic!("expected Koshi010WindowEndpointFile, got {load_error:?}");
    };
    assert_eq!(
        reported_endpoint_file_path,
        &endpoint_file_path.display().to_string()
    );
    assert_eq!(
        load_error.to_string(),
        format!(
            "endpoint file {} is unreadable: a koshi 0.1.0 window wrote it, and this koshi \
             cannot talk to that window; the window ends when its terminal closes",
            endpoint_file_path.display()
        )
    );
}

#[test]
fn a_file_in_a_format_above_this_builds_is_unreadable_naming_both_formats() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-newer.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"file_format":3,"socket_address":"/run/koshi/session-abc.sock","connection_token":"k7QxSecret","process_id":4242}"#,
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
            assert_eq!(error_detail, "format 3 is not the 2 this build reads");
        }
        unexpected_result => {
            panic!("expected EndpointFileUnreadable, got {unexpected_result:?}")
        }
    }
}

#[test]
fn a_file_naming_format_one_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-numbered-one.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"file_format":1,"socket_address":"/run/koshi/session-abc.sock","connection_token":"k7QxSecret","process_id":4242}"#,
    )
    .expect("write file");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable { error_detail, .. }) => {
            assert_eq!(error_detail, "format 1 is not the 2 this build reads");
        }
        unexpected_result => {
            panic!("expected EndpointFileUnreadable, got {unexpected_result:?}")
        }
    }
}

#[test]
fn a_format_two_file_missing_a_field_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-numbered-partial.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"file_format":2,"socket_address":"/run/koshi/session-abc.sock","process_id":4242}"#,
    )
    .expect("write file");

    match EndpointFile::load_from_path(&endpoint_file_path) {
        Err(IpcError::EndpointFileUnreadable { error_detail, .. }) => {
            assert_eq!(
                error_detail,
                "missing field `connection_token` at line 1 column 82"
            );
        }
        unexpected_result => {
            panic!("expected EndpointFileUnreadable, got {unexpected_result:?}")
        }
    }
}

#[test]
fn a_format_one_file_missing_a_field_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let endpoint_file_path = test_directory.path().join("session-partial.json");
    std::fs::write(
        &endpoint_file_path,
        r#"{"socket_address":"/run/koshi/session-abc.sock","process_id":4242}"#,
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
                "missing field `connection_token` at line 1 column 66"
            );
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
fn writing_the_advertisement_marker_into_a_missing_directory_names_the_marker() {
    let test_directory = TempDir::new().expect("create test directory");
    let advertisement_marker_path = resolve_advertisement_marker_path(
        &test_directory.path().join("no-such-subdir"),
        SessionId::new(),
    );

    let advertisement_marker_error = write_advertisement_marker(&advertisement_marker_path)
        .expect_err("a missing directory refuses the write");

    let expected_error_detail = std::fs::write(&advertisement_marker_path, b"")
        .expect_err("the same write fails the same way")
        .to_string();
    assert_eq!(
        advertisement_marker_error.to_string(),
        format!(
            "advertisement marker {} could not be written: {expected_error_detail}",
            advertisement_marker_path.display()
        )
    );
    let IpcError::AdvertisementMarkerWrite {
        advertisement_marker_path: reported_marker_path,
        error_detail: reported_error_detail,
    } = advertisement_marker_error
    else {
        panic!("expected AdvertisementMarkerWrite, got {advertisement_marker_error:?}");
    };
    assert_eq!(
        (reported_marker_path, reported_error_detail),
        (
            advertisement_marker_path.display().to_string(),
            expected_error_detail
        )
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
fn deleting_the_socket_file_takes_the_path_off_the_disk() {
    let test_directory = TempDir::new().expect("create test directory");
    let session_id = SessionId::new();
    let socket_address = compute_socket_address(test_directory.path(), session_id);
    std::fs::write(&socket_address, b"").expect("create the leftover socket file");

    delete_socket_file(&socket_address);

    assert!(!Path::new(&socket_address).exists());
    // A second deletion of a path with nothing at it does nothing and reports nothing.
    delete_socket_file(&socket_address);
    assert!(!Path::new(&socket_address).exists());
}

#[cfg(windows)]
#[test]
fn deleting_a_pipe_name_leaves_the_filesystem_untouched() {
    // A Windows address is a pipe name. A file in the working directory that
    // carries the same name stays on the disk.
    let test_directory = TempDir::new().expect("create test directory");
    let session_id = SessionId::new();
    let socket_address = compute_socket_address(test_directory.path(), session_id);
    let pipe_name_file_path = test_directory.path().join(&socket_address);
    std::fs::write(&pipe_name_file_path, b"").expect("create the pipe-name file");

    delete_socket_file(&socket_address);

    assert!(pipe_name_file_path.exists());
}

/// An endpoint file written just now inside `runtime_directory`, naming
/// `process_id`. Hands back its path.
fn write_endpoint_file_naming_process(runtime_directory: &TempDir, process_id: u32) -> PathBuf {
    let endpoint_file_path = runtime_directory.path().join("session-endpoint.json");
    EndpointFile {
        process_id,
        ..build_test_endpoint_file()
    }
    .write_to_path(&endpoint_file_path)
    .expect("the endpoint file is written");
    endpoint_file_path
}

#[test]
fn a_missing_endpoint_file_never_reads_as_a_live_session() {
    let runtime_directory = TempDir::new().expect("the test directory");

    assert!(!is_refusal_from_live_session(
        &runtime_directory.path().join("session-absent.json"),
        std::process::id(),
    ));
}

#[test]
fn a_process_id_no_session_can_hold_finds_no_endpoint_process() {
    let runtime_directory = TempDir::new().expect("the test directory");

    for unheld_process_id in [0, 2_147_483_647, u32::MAX] {
        let endpoint_file_path =
            write_endpoint_file_naming_process(&runtime_directory, unheld_process_id);
        assert_eq!(
            find_endpoint_process_record(&endpoint_file_path, unheld_process_id),
            None,
            "process id {unheld_process_id}"
        );
        assert!(
            !is_refusal_from_live_session(&endpoint_file_path, unheld_process_id),
            "process id {unheld_process_id}"
        );
    }
}

#[test]
fn an_endpoint_file_written_after_its_process_started_finds_that_process() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());

    assert_eq!(
        find_endpoint_process_record(&endpoint_file_path, std::process::id()),
        Some(find_process_record(std::process::id()).expect("this process reads"))
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_running_process_that_started_before_its_endpoint_file_reads_as_live() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());

    assert!(is_refusal_from_live_session(
        &endpoint_file_path,
        std::process::id()
    ));
}

#[test]
fn a_process_that_started_after_its_endpoint_file_was_modified_is_not_the_endpoint_process() {
    // The endpoint file was last written at `UNIX_EPOCH + 1s`, long before
    // this test process started.
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());
    std::fs::File::options()
        .write(true)
        .open(&endpoint_file_path)
        .expect("the endpoint file opens")
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
        .expect("the endpoint file is aged");

    assert_eq!(
        find_endpoint_process_record(&endpoint_file_path, std::process::id()),
        None
    );
    assert!(!is_refusal_from_live_session(
        &endpoint_file_path,
        std::process::id()
    ));
}

#[cfg(unix)]
#[test]
fn an_exited_child_nothing_has_waited_on_is_not_the_endpoint_process() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let mut exited_child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("the shell runs");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, exited_child.id());
    let wait_started_at = std::time::Instant::now();
    while find_endpoint_process_record(&endpoint_file_path, exited_child.id()).is_some() {
        assert!(
            wait_started_at.elapsed() < Duration::from_secs(5),
            "the child exits within 5 seconds"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!is_refusal_from_live_session(
        &endpoint_file_path,
        exited_child.id()
    ));
    assert_eq!(
        exited_child
            .wait()
            .expect("the child is reaped after the check")
            .code(),
        Some(0),
        "the check left the exited child for its parent to reap"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_running_process_never_reads_as_live_where_a_refused_connect_means_nothing_listens() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());

    assert!(!is_refusal_from_live_session(
        &endpoint_file_path,
        std::process::id()
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn writing_an_endpoint_file_sets_its_modified_time_to_the_named_process_start_time() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());

    assert_eq!(
        std::fs::metadata(&endpoint_file_path)
            .and_then(|endpoint_metadata| endpoint_metadata.modified())
            .expect("the modified time reads"),
        find_process_record(std::process::id())
            .expect("this process reads")
            .started_at
    );
}

/// Set the modified time of the endpoint file at `endpoint_file_path` to
/// `start_second_offset` seconds after this process's start second, plus
/// `microsecond_count` microseconds.
fn set_endpoint_time_from_this_process_start_second(
    endpoint_file_path: &Path,
    start_second_offset: i64,
    microsecond_count: u64,
) {
    let this_process_started_at = find_process_record(std::process::id())
        .expect("this process reads")
        .started_at;
    let start_second = this_process_started_at
        .duration_since(std::time::UNIX_EPOCH)
        .expect("this process started after the epoch")
        .as_secs()
        .checked_add_signed(start_second_offset)
        .expect("the second is after the epoch");
    std::fs::File::options()
        .write(true)
        .open(endpoint_file_path)
        .expect("the endpoint file opens")
        .set_modified(
            std::time::UNIX_EPOCH
                + Duration::from_secs(start_second)
                + Duration::from_micros(microsecond_count),
        )
        .expect("the endpoint file time is set");
}

#[test]
fn a_modified_time_cut_to_the_start_second_still_finds_the_endpoint_process() {
    // A filesystem that keeps whole seconds stores the start time without its
    // microseconds.
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());
    set_endpoint_time_from_this_process_start_second(&endpoint_file_path, 0, 0);

    assert_eq!(
        find_endpoint_process_record(&endpoint_file_path, std::process::id()),
        Some(find_process_record(std::process::id()).expect("this process reads"))
    );
}

#[test]
fn a_process_that_started_in_the_second_after_the_modified_time_is_not_the_endpoint_process() {
    // The file's time is the last microsecond of the second before this
    // process started.
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());
    set_endpoint_time_from_this_process_start_second(&endpoint_file_path, -1, 999_999);

    assert_eq!(
        find_endpoint_process_record(&endpoint_file_path, std::process::id()),
        None
    );
}

#[test]
fn the_update_lock_is_update_lock_directly_inside_the_runtime_directory() {
    let update_lock_path = resolve_update_lock_path(Path::new("/home/user/run"));

    assert_eq!(update_lock_path.parent(), Some(Path::new("/home/user/run")));
    assert_eq!(
        update_lock_path.file_name(),
        Some(std::ffi::OsStr::new("update.lock"))
    );
}

/// Open the update lock file in `runtime_directory`, creating it.
fn open_update_lock_file(runtime_directory: &TempDir) -> std::fs::File {
    std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(resolve_update_lock_path(runtime_directory.path()))
        .expect("the update lock file opens")
}

#[test]
fn no_update_lock_file_means_no_update_is_restarting_servers() {
    let runtime_directory = TempDir::new().expect("the test directory");

    assert!(!is_update_restarting_servers(runtime_directory.path()));
}

#[test]
fn an_update_lock_held_exclusively_means_an_update_is_restarting_servers_until_released() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let update_lock_file = open_update_lock_file(&runtime_directory);
    update_lock_file.lock().expect("the update lock is taken");

    assert!(is_update_restarting_servers(runtime_directory.path()));
    update_lock_file
        .unlock()
        .expect("the update lock is released");
    assert!(!is_update_restarting_servers(runtime_directory.path()));
}

#[test]
fn a_shared_hold_of_the_update_lock_is_not_an_update() {
    // Another client asking at the same moment holds the lock shared.
    let runtime_directory = TempDir::new().expect("the test directory");
    let probing_client_file = open_update_lock_file(&runtime_directory);
    probing_client_file
        .lock_shared()
        .expect("the update lock is shared");

    assert!(!is_update_restarting_servers(runtime_directory.path()));
}

#[test]
fn an_endpoint_file_dated_before_the_epoch_finds_no_endpoint_process() {
    let runtime_directory = TempDir::new().expect("the test directory");
    let endpoint_file_path =
        write_endpoint_file_naming_process(&runtime_directory, std::process::id());
    std::fs::File::options()
        .write(true)
        .open(&endpoint_file_path)
        .expect("the endpoint file opens")
        .set_modified(std::time::UNIX_EPOCH - Duration::from_secs(1))
        .expect("the endpoint file is dated before the epoch");

    assert_eq!(
        find_endpoint_process_record(&endpoint_file_path, std::process::id()),
        None
    );
}

/// A program file holding a fixed process id, version and program path.
fn build_test_program_file() -> ServerProgramFile {
    ServerProgramFile {
        process_id: 5000,
        build_version: "0.6.0".to_string(),
        program_path: "/usr/local/bin/koshi".to_string(),
    }
}

#[test]
fn the_program_file_path_is_session_uuid_program_beside_the_endpoint_file() {
    let uuid = Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid");
    let session_id = SessionId::from_uuid(uuid);
    assert_eq!(
        ServerProgramFile::resolve_session_program_file_path(Path::new("/run/koshi"), session_id),
        Path::new("/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.program")
    );
}

#[test]
fn a_written_program_file_reads_back_unchanged() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("session-x.program");

    build_test_program_file()
        .write_to_path(&program_file_path)
        .expect("write the program file");

    assert_eq!(
        std::fs::read_to_string(&program_file_path).expect("read the program file"),
        r#"{"file_format":1,"process_id":5000,"build_version":"0.6.0","program_path":"/usr/local/bin/koshi"}"#
    );
    assert_eq!(
        ServerProgramFile::load_from_path(&program_file_path).expect("read the program file"),
        Some(build_test_program_file())
    );
}

#[cfg(unix)]
#[test]
fn a_fresh_program_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("session-private.program");

    build_test_program_file()
        .write_to_path(&program_file_path)
        .expect("write the program file");

    let file_mode = std::fs::metadata(&program_file_path)
        .expect("stat the program file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
}

#[test]
fn reading_a_missing_program_file_gives_none() {
    let test_directory = TempDir::new().expect("create test directory");

    assert_eq!(
        ServerProgramFile::load_from_path(&test_directory.path().join("router.program"))
            .expect("a missing file is no error"),
        None
    );
}

#[test]
fn reading_a_program_file_skips_a_field_this_build_does_not_know() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("router.program");
    std::fs::write(
        &program_file_path,
        br#"{"file_format":1,"process_id":5000,"build_version":"0.6.0","program_path":"/usr/local/bin/koshi","install_source":"homebrew"}"#,
    )
    .expect("write the program file");

    assert_eq!(
        ServerProgramFile::load_from_path(&program_file_path).expect("read the program file"),
        Some(build_test_program_file())
    );
}

#[test]
fn reading_junk_bytes_is_program_file_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("router.program");
    std::fs::write(&program_file_path, b"not json").expect("write junk");

    match ServerProgramFile::load_from_path(&program_file_path) {
        Err(IpcError::ProgramFileUnreadable {
            program_file_path: reported_program_file_path,
            error_detail,
        }) => {
            assert_eq!(
                reported_program_file_path,
                program_file_path.display().to_string()
            );
            assert_eq!(error_detail, "expected ident at line 1 column 2");
        }
        unexpected_result => panic!("expected ProgramFileUnreadable, got {unexpected_result:?}"),
    }
}

#[test]
fn a_program_file_with_no_format_number_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("router.program");
    std::fs::write(
        &program_file_path,
        br#"{"process_id":5000,"build_version":"0.6.0","program_path":"/usr/local/bin/koshi"}"#,
    )
    .expect("write the program file");

    match ServerProgramFile::load_from_path(&program_file_path) {
        Err(IpcError::ProgramFileUnreadable { error_detail, .. }) => {
            assert_eq!(error_detail, "it has no file_format field");
        }
        unexpected_result => panic!("expected ProgramFileUnreadable, got {unexpected_result:?}"),
    }
}

#[test]
fn a_program_file_in_a_format_above_this_builds_is_unreadable_naming_both_formats() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("router.program");
    std::fs::write(
        &program_file_path,
        br#"{"file_format":2,"process_id":5000,"build_version":"0.7.0","program_path":"/usr/local/bin/koshi"}"#,
    )
    .expect("write the program file");

    match ServerProgramFile::load_from_path(&program_file_path) {
        Err(IpcError::ProgramFileUnreadable {
            program_file_path: reported_program_file_path,
            error_detail,
        }) => {
            assert_eq!(
                reported_program_file_path,
                program_file_path.display().to_string()
            );
            assert_eq!(error_detail, "format 2 is not the 1 this build reads");
        }
        unexpected_result => panic!("expected ProgramFileUnreadable, got {unexpected_result:?}"),
    }
}

#[test]
fn a_program_file_missing_a_field_is_unreadable() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory.path().join("router.program");
    std::fs::write(
        &program_file_path,
        br#"{"file_format":1,"process_id":5000,"program_path":"/usr/local/bin/koshi"}"#,
    )
    .expect("write the program file");

    match ServerProgramFile::load_from_path(&program_file_path) {
        Err(IpcError::ProgramFileUnreadable { error_detail, .. }) => {
            assert_eq!(
                error_detail,
                "missing field `build_version` at line 1 column 73"
            );
        }
        unexpected_result => panic!("expected ProgramFileUnreadable, got {unexpected_result:?}"),
    }
}

#[test]
fn writing_a_program_file_into_a_missing_directory_is_program_file_write() {
    let test_directory = TempDir::new().expect("create test directory");
    let program_file_path = test_directory
        .path()
        .join("no-such-subdir")
        .join("router.program");

    match build_test_program_file().write_to_path(&program_file_path) {
        Err(IpcError::ProgramFileWrite {
            program_file_path: reported_program_file_path,
            ..
        }) => {
            assert_eq!(
                reported_program_file_path,
                program_file_path.display().to_string()
            );
        }
        unexpected_result => panic!("expected ProgramFileWrite, got {unexpected_result:?}"),
    }
}
