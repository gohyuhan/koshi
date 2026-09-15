//! Tests for the self-update helpers: version comparison, check scheduling,
//! archive URL construction, bounded downloads, checksum verification, state
//! serialization, the restart confirmation wait, and the walk that restarts every
//! running session.

use super::*;

use std::io::Write;
use std::thread::JoinHandle;

use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterHandshake, RouterRequest,
    RouterResponse, RouterResult, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_test_support::fixtures::build_test_runtime_directory;

/// Serve one Hello-only connection as a router would: bind the router's
/// address, write the endpoint file advertising it, accept one caller, and
/// answer its Hello with `reported_build_version`.
fn spawn_fake_router_reporting(
    runtime_directory: &Path,
    reported_build_version: &str,
) -> JoinHandle<()> {
    let connection_token = ConnectionToken::generate();
    let socket_address = compute_router_socket_address(runtime_directory);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in router");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");

    let reported_build_version = reported_build_version.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let mut router_handshake = RouterHandshake::from_connection_token(connection_token);
        let hello_request: RouterRequest = connection.recv().expect("read the hello");
        let response_result =
            match router_handshake.validate_request_kind(&hello_request.request_kind) {
                Ok(()) => RouterResult::Hello {
                    protocol_version: ROUTER_PROTOCOL_VERSION,
                    build_version: reported_build_version,
                },
                Err(error_response) => RouterResult::Error(error_response),
            };
        connection
            .send(&RouterResponse {
                request_id: Some(hello_request.request_id),
                answer_result: response_result,
            })
            .expect("send the hello reply");
    })
}

#[test]
fn a_router_reporting_the_installed_version_confirms_the_restart() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_reporting(runtime_directory.path(), "3.3.3");

    let confirmed = wait_for_version("3.3.3", Duration::from_secs(5), || {
        probe_router_version(runtime_directory.path())
    });

    assert_eq!(confirmed, VersionProbeOutcome::Installed);
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn a_router_still_on_another_version_is_reported_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_reporting(runtime_directory.path(), "1.0.0");

    let answered = wait_for_version("2.0.0", Duration::from_millis(250), || {
        probe_router_version(runtime_directory.path())
    });

    assert_eq!(
        answered,
        VersionProbeOutcome::OtherVersion("1.0.0".to_string())
    );
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn no_router_answering_reports_no_version_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    assert_eq!(
        wait_for_version("2.0.0", Duration::from_millis(50), || probe_router_version(
            runtime_directory.path()
        )),
        VersionProbeOutcome::Silent
    );
}

// --- restarting every running session ---

/// What a stand-in session answers with.
struct SessionRestartScript {
    /// The answer to the Restart request.
    restart_result: IpcResult,
    /// The build version every Hello answer of this session carries.
    reported_build_version: String,
}

/// Serve `connection_count` callers as a session would: bind the session's address,
/// write the endpoint file advertising it, then answer that many callers.
///
/// The first caller writes a Hello and a Restart back to back and is answered
/// per `session_script`. Every caller after the first writes a Hello alone and is
/// answered with the script's build version. A caller arriving once `connection_count`
/// are served finds nothing listening, which is what a session that is
/// replacing its own image looks like.
fn spawn_fake_session(
    runtime_directory: &Path,
    session_id: SessionId,
    session_script: SessionRestartScript,
    connection_count: usize,
) -> JoinHandle<()> {
    let connection_token = ConnectionToken::generate();
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in session");
    EndpointFile {
        socket_address,
        connection_token: connection_token.clone(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the session endpoint file");

    std::thread::spawn(move || {
        let SessionRestartScript {
            restart_result,
            reported_build_version,
        } = session_script;
        for connection_index in 0..connection_count {
            let mut connection = listener.accept().expect("accept the caller");
            let hello_request: IpcRequest = connection.recv().expect("read the hello");
            let IpcRequestKind::Hello {
                connection_token: presented_connection_token,
                ..
            } = &hello_request.request_kind
            else {
                panic!("expected a Hello first");
            };
            assert_eq!(
                presented_connection_token, &connection_token,
                "the caller presents the endpoint file's token"
            );

            if connection_index == 0 {
                let restart_request: IpcRequest = connection.recv().expect("read the restart");
                assert_eq!(
                    restart_request.request_kind,
                    IpcRequestKind::Restart,
                    "expected a Restart after the Hello"
                );
                send_ipc_response(
                    &mut connection,
                    hello_request.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: reported_build_version.clone(),
                    },
                );
                send_ipc_response(
                    &mut connection,
                    restart_request.request_id,
                    restart_result.clone(),
                );
            } else {
                send_ipc_response(
                    &mut connection,
                    hello_request.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: reported_build_version.clone(),
                    },
                );
            }
        }
    })
}

/// Answer `request_id` with `result` on `connection`.
fn send_ipc_response(connection: &mut Connection, request_id: u64, response_result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            answer_result: response_result,
        })
        .expect("send the scripted reply");
}

/// The refusal a koshi whose build has no Restart request answers with.
fn build_unsupported_restart_response() -> IpcResult {
    IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::UnsupportedKind,
        message: "this koshi has no Restart request".to_string(),
    })
}

#[test]
fn a_session_reporting_the_installed_version_confirms_its_restart() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Restarting,
            reported_build_version: "3.3.3".to_string(),
        },
        2,
    );

    let session_outcomes =
        restart_advertised_sessions(runtime_directory.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(
        session_outcomes,
        vec![(session_id, SessionOutcome::Confirmed)]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_still_on_the_old_version_is_reported_after_the_wait() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: IpcResult::Restarting,
            reported_build_version: "1.0.0".to_string(),
        },
        2,
    );

    let session_outcomes = restart_advertised_sessions(
        runtime_directory.path(),
        "2.0.0",
        Duration::from_millis(250),
    );

    assert_eq!(
        session_outcomes,
        vec![(
            session_id,
            SessionOutcome::StillOnVersion("1.0.0".to_string())
        )]
    );
    session_thread
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_with_no_restart_request_is_reported_as_too_old() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_thread = spawn_fake_session(
        runtime_directory.path(),
        session_id,
        SessionRestartScript {
            restart_result: build_unsupported_restart_response(),
            reported_build_version: "1.0.0".to_string(),
        },
        1,
    );

    let session_outcomes =
        restart_advertised_sessions(runtime_directory.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(session_outcomes, vec![(session_id, SessionOutcome::TooOld)]);
    session_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn one_session_refusing_still_leaves_every_other_session_asked() {
    let runtime_directory = build_test_runtime_directory();
    let confirmed_session_id_one = SessionId::new();
    let refusing_session_id = SessionId::new();
    let confirmed_session_id_two = SessionId::new();
    let fake_session_threads = vec![
        spawn_fake_session(
            runtime_directory.path(),
            confirmed_session_id_one,
            SessionRestartScript {
                restart_result: IpcResult::Restarting,
                reported_build_version: "3.3.3".to_string(),
            },
            2,
        ),
        spawn_fake_session(
            runtime_directory.path(),
            refusing_session_id,
            SessionRestartScript {
                restart_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::Unknown,
                    message: "a pane is mid-write".to_string(),
                }),
                reported_build_version: "3.3.3".to_string(),
            },
            1,
        ),
        spawn_fake_session(
            runtime_directory.path(),
            confirmed_session_id_two,
            SessionRestartScript {
                restart_result: IpcResult::Restarting,
                reported_build_version: "3.3.3".to_string(),
            },
            2,
        ),
    ];

    let mut session_outcomes =
        restart_advertised_sessions(runtime_directory.path(), "3.3.3", Duration::from_secs(5));
    session_outcomes.sort_by_key(|(session_id, _)| session_id.to_string());
    let mut expected_session_outcomes = vec![
        (confirmed_session_id_one, SessionOutcome::Confirmed),
        (
            refusing_session_id,
            SessionOutcome::Failed("IPC unavailable: a pane is mid-write".to_string()),
        ),
        (confirmed_session_id_two, SessionOutcome::Confirmed),
    ];
    expected_session_outcomes.sort_by_key(|(session_id, _)| session_id.to_string());

    assert_eq!(session_outcomes, expected_session_outcomes);
    for session_thread in fake_session_threads {
        session_thread
            .join()
            .expect("the stand-in served its connections");
    }
}

#[test]
fn no_running_session_leaves_the_router_confirmation_unchanged() {
    let runtime_directory = build_test_runtime_directory();
    let router_thread = spawn_fake_router_reporting(runtime_directory.path(), "3.3.3");

    let session_outcomes =
        restart_advertised_sessions(runtime_directory.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(session_outcomes, Vec::new());
    assert_eq!(
        wait_for_version("3.3.3", Duration::from_secs(5), || probe_router_version(
            runtime_directory.path()
        )),
        VersionProbeOutcome::Installed
    );
    router_thread
        .join()
        .expect("the stand-in served its connection");
}

#[test]
fn strip_version_prefix_drops_a_leading_v_only() {
    assert_eq!(strip_version_prefix("v1.2.3"), "1.2.3");
    assert_eq!(strip_version_prefix("1.2.3"), "1.2.3");
    assert_eq!(strip_version_prefix("version"), "ersion");
}

#[test]
fn a_far_higher_release_tag_is_newer() {
    assert!(is_release_newer("v9999.0.0"));
    assert!(is_release_newer("9999.0.0"));
}

#[test]
fn a_zero_release_tag_is_not_newer() {
    assert!(!is_release_newer("v0.0.0"));
}

#[test]
fn the_current_build_is_not_newer_than_itself() {
    assert!(!is_release_newer(APP_VERSION));
}

#[test]
fn a_malformed_release_tag_is_not_newer() {
    assert!(!is_release_newer("not-a-version"));
    assert!(!is_release_newer("v"));
}

#[test]
fn a_first_ever_check_is_due() {
    let update_state = UpdateState::default();
    assert!(is_update_due(&update_state, 14));
}

#[test]
fn a_check_within_the_interval_is_not_due() {
    let update_state = UpdateState {
        last_check_unix_seconds: Some(get_current_unix_seconds()),
    };
    assert!(!is_update_due(&update_state, 14));
}

#[test]
fn a_check_older_than_the_interval_is_due() {
    let fifteen_days_ago_unix_seconds =
        get_current_unix_seconds().saturating_sub(15 * SECONDS_PER_DAY);
    let update_state = UpdateState {
        last_check_unix_seconds: Some(fifteen_days_ago_unix_seconds),
    };
    assert!(is_update_due(&update_state, 14));
}

#[test]
fn a_zero_interval_is_always_due() {
    let update_state = UpdateState {
        last_check_unix_seconds: Some(get_current_unix_seconds()),
    };
    assert!(is_update_due(&update_state, 0));
}

#[test]
fn compute_binary_url_matches_release_naming_on_supported_platforms() {
    // The exact archive name is platform-specific; assert the invariant parts
    // for whichever platform the test runs on.
    let archive_url = compute_binary_url("v0.2.0").expect("dev + CI platforms are all supported");
    assert!(
        archive_url.starts_with(
            "https://github.com/gohyuhan/koshi/releases/download/v0.2.0/koshi-v0.2.0-"
        ),
        "unexpected archive URL: {archive_url}"
    );
    let archive_extension = if cfg!(windows) { ".zip" } else { ".tar.gz" };
    assert!(
        archive_url.ends_with(archive_extension),
        "unexpected archive extension in {archive_url}"
    );
}

#[test]
fn a_standard_shasum_row_returns_the_archive_checksum() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!(
        "0000000000000000000000000000000000000000000000000000000000000000  other-file\nBA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  {archive_file_name}\n"
    );

    assert_eq!(
        find_release_checksum(&checksums_text, archive_file_name)
            .expect("the archive row has a valid checksum"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn a_missing_checksum_row_names_the_release_archive() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let error = find_release_checksum(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  other-file\n",
        archive_file_name,
    )
    .expect_err("an absent archive row must fail");

    assert_eq!(
        error,
        "checksums.txt has no row for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn duplicate_checksum_rows_name_the_release_archive() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {archive_file_name}\nba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {archive_file_name}\n"
    );
    let error = find_release_checksum(&checksums_text, archive_file_name)
        .expect_err("duplicate archive rows must fail");

    assert_eq!(
        error,
        "checksums.txt has multiple rows for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn malformed_checksum_row_for_the_release_archive_is_rejected() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!(
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {archive_file_name} extra\n"
    );
    let error = find_release_checksum(&checksums_text, archive_file_name)
        .expect_err("an archive row with extra fields must fail");

    assert_eq!(
        error,
        "checksums.txt has a malformed row for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn invalid_checksum_for_the_release_archive_is_rejected() {
    let archive_file_name = "koshi-v0.5.0-linux-amd64.tar.gz";
    let checksums_text = format!("{}  {archive_file_name}\n", "z".repeat(64));
    let error = find_release_checksum(&checksums_text, archive_file_name)
        .expect_err("a non-hex checksum must fail");

    assert_eq!(
        error,
        "checksums.txt has an invalid SHA-256 checksum for release archive koshi-v0.5.0-linux-amd64.tar.gz"
    );
}

#[test]
fn a_stream_at_the_byte_limit_is_copied() {
    let mut release_file_reader = b"abc".as_slice();
    let mut copied_bytes = Vec::new();

    copy_stream_with_byte_limit(&mut release_file_reader, &mut copied_bytes, 3)
        .expect("a stream at the limit is accepted");

    assert_eq!(copied_bytes, b"abc");
}

#[test]
fn a_stream_over_the_byte_limit_is_rejected_without_copying_the_extra_byte() {
    let mut release_file_reader = b"abcd".as_slice();
    let mut copied_bytes = Vec::new();

    let error = copy_stream_with_byte_limit(&mut release_file_reader, &mut copied_bytes, 3)
        .expect_err("a stream over the limit must fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "download response exceeds 3 bytes");
    assert_eq!(copied_bytes, b"abc");
}

fn write_release_file(release_file_bytes: &[u8]) -> TempPath {
    let mut release_file = Builder::new()
        .prefix("koshi-test-")
        .tempfile()
        .expect("release tempfile");
    release_file
        .as_file_mut()
        .write_all(release_file_bytes)
        .expect("write release file");
    release_file.into_temp_path()
}

#[test]
fn matching_release_archive_checksum_is_accepted() {
    let archive_path = write_release_file(b"abc");

    verify_release_archive(
        archive_path.as_ref(),
        "koshi-v0.5.0-linux-amd64.tar.gz",
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    )
    .expect("the matching checksum is accepted");
}

#[test]
fn mismatched_release_archive_checksum_names_both_digests() {
    let archive_path = write_release_file(b"abc");
    let expected_checksum = "0000000000000000000000000000000000000000000000000000000000000000";
    let error = extract_verified_release_binary(
        archive_path.as_ref(),
        "koshi.tar.gz",
        "koshi-v0.5.0-linux-amd64.tar.gz",
        expected_checksum,
    )
    .expect_err("the changed checksum must fail before unpacking");

    assert_eq!(
        error,
        "checksum mismatch for release archive koshi-v0.5.0-linux-amd64.tar.gz: expected 0000000000000000000000000000000000000000000000000000000000000000, computed ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn get_binary_file_name_is_platform_specific() {
    if cfg!(windows) {
        assert_eq!(get_binary_file_name(), "koshi.exe");
    } else {
        assert_eq!(get_binary_file_name(), "koshi");
    }
}

#[cfg(windows)]
#[test]
fn a_windows_swap_replaces_the_executable_and_cleans_the_backup() {
    let test_directory = Builder::new()
        .prefix("koshi-test-")
        .tempdir()
        .expect("swap directory");
    let executable_path = test_directory.path().join("koshi.exe");
    let new_binary_path = test_directory.path().join("new-binary.exe");
    let staged_binary_path = test_directory
        .path()
        .join(format!("koshi-update-{}.exe", std::process::id()));
    let backup_executable_path = executable_path.with_extension("old");
    fs::write(&executable_path, b"old-binary").expect("write the old executable");
    fs::write(&new_binary_path, b"new-binary").expect("write the replacement executable");

    swap_executable(&new_binary_path, &executable_path).expect("replace the executable");

    assert_eq!(
        fs::read(&executable_path).expect("read the replacement executable"),
        b"new-binary"
    );
    assert!(!backup_executable_path.exists());
    assert!(!staged_binary_path.exists());
}

#[test]
fn update_state_defaults_when_deserialized_from_empty_object() {
    let update_state: UpdateState =
        serde_json::from_str("{}").expect("empty object is valid update state");
    assert_eq!(update_state.last_check_unix_seconds, None);
}

#[test]
fn update_state_survives_a_serialize_deserialize_round_trip() {
    let original_update_state = UpdateState {
        last_check_unix_seconds: Some(1_700_000_000),
    };
    let serialized_update_state =
        serde_json::to_string(&original_update_state).expect("serializable");
    let restored_update_state: UpdateState =
        serde_json::from_str(&serialized_update_state).expect("deserializable");
    assert_eq!(
        restored_update_state.last_check_unix_seconds,
        original_update_state.last_check_unix_seconds
    );
}

// --- release JSON parsing (no network: fixture strings only) ---

#[test]
fn a_release_object_deserializes_its_tag_name() {
    let release: Release = serde_json::from_str(r#"{"tag_name":"v0.2.0","name":"ignored"}"#)
        .expect("a release object with extra fields still parses");
    assert_eq!(release.release_tag, "v0.2.0");
}

#[test]
fn a_release_list_deserializes_every_tag_in_order() {
    let releases: Vec<Release> =
        serde_json::from_str(r#"[{"tag_name":"v0.2.0"},{"tag_name":"v0.1.0"}]"#)
            .expect("a release array parses");
    let release_tags: Vec<String> = releases
        .into_iter()
        .map(|release| release.release_tag)
        .collect();
    assert_eq!(
        release_tags,
        vec!["v0.2.0".to_string(), "v0.1.0".to_string()]
    );
}

// --- update error + current Unix time ---

#[test]
fn update_error_wraps_detail_in_cli_update_error() {
    match build_update_error("boom") {
        CliError::Update { detail } => assert_eq!(detail, "boom"),
        unexpected_error => panic!("expected CliError::Update, got {unexpected_error:?}"),
    }
}

#[test]
fn get_current_unix_seconds_is_after_the_year_2023() {
    // A whole-second Unix timestamp taken now is always past 2023-11-14.
    assert!(get_current_unix_seconds() > 1_700_000_000);
}

// --- archive extraction (local files, no network) ---

/// Writes a gzip-compressed tar to a temp file, one regular-file entry per
/// `(name, bytes)`.
fn write_tar_gz(archive_entries: &[(&str, &[u8])]) -> TempPath {
    let archive_file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".tar.gz")
        .tempfile()
        .expect("archive tempfile");
    {
        let gzip_encoder =
            flate2::write::GzEncoder::new(archive_file.as_file(), flate2::Compression::default());
        let mut tar_archive = tar::Builder::new(gzip_encoder);
        for (archive_entry_name, archive_entry_bytes) in archive_entries {
            let mut archive_header = tar::Header::new_gnu();
            archive_header
                .set_path(archive_entry_name)
                .expect("archive entry path");
            archive_header.set_size(archive_entry_bytes.len() as u64);
            archive_header.set_mode(0o755);
            archive_header.set_cksum();
            tar_archive
                .append(&archive_header, *archive_entry_bytes)
                .expect("append archive entry");
        }
        tar_archive
            .into_inner()
            .expect("finish tar archive")
            .finish()
            .expect("finish gzip archive");
    }
    archive_file.into_temp_path()
}

/// Writes a zip archive to a temp file, one entry per `(name, bytes)`.
fn write_zip(archive_entries: &[(&str, &[u8])]) -> TempPath {
    let archive_file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".zip")
        .tempfile()
        .expect("archive tempfile");
    {
        let mut zip_archive = zip::ZipWriter::new(archive_file.as_file());
        let zip_entry_options = zip::write::SimpleFileOptions::default();
        for (archive_entry_name, archive_entry_bytes) in archive_entries {
            zip_archive
                .start_file(*archive_entry_name, zip_entry_options)
                .expect("start archive entry");
            zip_archive
                .write_all(archive_entry_bytes)
                .expect("write archive entry");
        }
        zip_archive.finish().expect("finish zip archive");
    }
    archive_file.into_temp_path()
}

#[test]
fn extracting_a_tar_gz_returns_the_named_binary_bytes() {
    let archive = write_tar_gz(&[
        ("readme.txt", b"docs"),
        (get_binary_file_name(), b"binary-bytes"),
    ]);
    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");
    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn extracting_a_tar_gz_without_the_binary_is_an_error() {
    let archive = write_tar_gz(&[("readme.txt", b"docs")]);
    assert_eq!(
        extract_release_binary(archive.as_ref(), "koshi.tar.gz").expect_err("no binary present"),
        "binary not found in archive"
    );
}

#[test]
fn extracting_a_zip_returns_the_named_binary_bytes() {
    let archive = write_zip(&[
        ("readme.txt", b"docs"),
        (get_binary_file_name(), b"binary-bytes"),
    ]);
    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.zip").expect("extract the binary");
    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn extracting_a_zip_without_the_binary_is_an_error() {
    let archive = write_zip(&[("readme.txt", b"docs")]);
    assert_eq!(
        extract_release_binary(archive.as_ref(), "koshi.zip").expect_err("no binary present"),
        "binary not found in archive"
    );
}

/// Writes a gzip-compressed tar to a temp file, one entry per
/// `(name, entry type, bytes)`.
fn write_tar_gz_of_kinds(archive_entries: &[(&str, tar::EntryType, &[u8])]) -> TempPath {
    let archive_file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".tar.gz")
        .tempfile()
        .expect("archive tempfile");
    {
        let gzip_encoder =
            flate2::write::GzEncoder::new(archive_file.as_file(), flate2::Compression::default());
        let mut tar_archive = tar::Builder::new(gzip_encoder);
        for (archive_entry_name, archive_entry_type, archive_entry_bytes) in archive_entries {
            let mut archive_header = tar::Header::new_gnu();
            archive_header
                .set_path(archive_entry_name)
                .expect("archive entry path");
            archive_header.set_entry_type(*archive_entry_type);
            archive_header.set_size(archive_entry_bytes.len() as u64);
            archive_header.set_mode(0o755);
            archive_header.set_cksum();
            tar_archive
                .append(&archive_header, *archive_entry_bytes)
                .expect("append archive entry");
        }
        tar_archive
            .into_inner()
            .expect("finish tar archive")
            .finish()
            .expect("finish gzip archive");
    }
    archive_file.into_temp_path()
}

#[test]
fn a_directory_carrying_the_binary_name_is_passed_over_for_the_real_file() {
    let archive = write_tar_gz_of_kinds(&[
        (get_binary_file_name(), tar::EntryType::Directory, b""),
        (
            get_binary_file_name(),
            tar::EntryType::Regular,
            b"binary-bytes",
        ),
    ]);

    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn a_symbolic_link_carrying_the_binary_name_is_passed_over_for_the_real_file() {
    let archive = write_tar_gz_of_kinds(&[
        (get_binary_file_name(), tar::EntryType::Symlink, b""),
        (
            get_binary_file_name(),
            tar::EntryType::Regular,
            b"binary-bytes",
        ),
    ]);

    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn a_tar_gz_binary_under_a_top_level_directory_is_found_by_its_file_name() {
    let nested_archive_path = format!("koshi-v9.9.9-linux-amd64/{}", get_binary_file_name());
    let archive = write_tar_gz(&[(nested_archive_path.as_str(), b"nested-bytes")]);

    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"nested-bytes"
    );
}

#[test]
fn a_zip_binary_under_a_top_level_directory_is_found_by_its_file_name() {
    let nested_archive_path = format!("koshi-v9.9.9-windows-amd64/{}", get_binary_file_name());
    let archive = write_zip(&[(nested_archive_path.as_str(), b"nested-bytes")]);

    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.zip").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"nested-bytes"
    );
}

#[cfg(unix)]
#[test]
fn an_extracted_binary_is_left_runnable() {
    use std::os::unix::fs::PermissionsExt;

    let archive = write_tar_gz_of_kinds(&[(
        get_binary_file_name(),
        tar::EntryType::Regular,
        b"binary-bytes" as &[u8],
    )]);

    let extracted =
        extract_release_binary(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    let permission_mode = fs::metadata(AsRef::<Path>::as_ref(&extracted))
        .expect("read the extracted binary's metadata")
        .permissions()
        .mode();
    assert_eq!(permission_mode & 0o777, 0o755);
}

/// The pre-release picker takes the highest version by semver order, never
/// the newest by publish date: a re-published older-versioned tag loses to a
/// higher one wherever it sits in the list.
#[test]
fn highest_release_version_picks_semver_order_not_list_order() {
    let releases = |release_tags: &[&str]| -> Vec<Release> {
        release_tags
            .iter()
            .map(|release_tag| Release {
                release_tag: (*release_tag).to_string(),
            })
            .collect()
    };

    assert_eq!(
        find_highest_release_version(releases(&["v0.3.0-rc.2", "v0.3.0-rc.10", "v0.2.0",]))
            .unwrap(),
        "v0.3.0-rc.10"
    );
    // List order plays no part: the highest wins from the front too.
    assert_eq!(
        find_highest_release_version(releases(&["v0.4.0", "v0.3.0"])).unwrap(),
        "v0.4.0"
    );
    // A tag that is not a version is skipped, not an error.
    assert_eq!(
        find_highest_release_version(releases(&["nightly", "v0.1.0"])).unwrap(),
        "v0.1.0"
    );
    assert_eq!(
        find_highest_release_version(Vec::new()).unwrap_err(),
        "no releases found"
    );
    // A list where no tag is a version reads the same as an empty one.
    assert_eq!(
        find_highest_release_version(releases(&["nightly", "edge"])).unwrap_err(),
        "no releases found"
    );
}
