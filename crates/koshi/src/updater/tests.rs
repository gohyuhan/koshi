//! Tests for the self-update helpers: version comparison, check scheduling,
//! archive URL construction, state serialization, the restart confirmation
//! wait, and the walk that restarts every running session.

use super::*;

use std::io::Write;
use std::thread::JoinHandle;

use koshi_ipc::endpoint::{socket_addr, EndpointFile};
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::router::{
    router_endpoint_path, router_socket_addr, RouterHandshake, RouterRequest, RouterResponse,
    RouterResult, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_test_support::fixtures::test_runtime_dir;

/// Serve one Hello-only connection as a router would: bind the router's
/// address, write the endpoint file advertising it, accept one caller, and
/// answer its Hello with `version`.
fn fake_router_reporting(runtime_dir: &Path, version: &str) -> JoinHandle<()> {
    let held = ConnectionToken::generate();
    let addr = router_socket_addr(runtime_dir);
    let listener = Listener::bind(&addr).expect("bind the stand-in router");
    EndpointFile {
        socket: addr,
        token: held.clone(),
        pid: std::process::id(),
    }
    .write(&router_endpoint_path(runtime_dir))
    .expect("write the router endpoint file");

    let version = version.to_string();
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the caller");
        let mut gate = RouterHandshake::new(held);
        let hello: RouterRequest = connection.recv().expect("read the hello");
        let result = match gate.check(&hello.kind) {
            Ok(()) => RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                version,
            },
            Err(refusal) => RouterResult::Error(refusal),
        };
        connection
            .send(&RouterResponse {
                request_id: Some(hello.request_id),
                result,
            })
            .expect("send the hello reply");
    })
}

#[test]
fn a_router_reporting_the_installed_version_confirms_the_restart() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_reporting(runtime_dir.path(), "3.3.3");

    let confirmed = wait_for_version("3.3.3", Duration::from_secs(5), || {
        probe_router_version(runtime_dir.path())
    });

    assert_eq!(confirmed, VersionAnswer::Installed);
    router.join().expect("the stand-in served its connection");
}

#[test]
fn a_router_still_on_another_version_is_reported_after_the_wait() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_reporting(runtime_dir.path(), "1.0.0");

    let answered = wait_for_version("2.0.0", Duration::from_millis(250), || {
        probe_router_version(runtime_dir.path())
    });

    assert_eq!(answered, VersionAnswer::Other("1.0.0".to_string()));
    router.join().expect("the stand-in served its connection");
}

#[test]
fn no_router_answering_reports_no_version_after_the_wait() {
    let runtime_dir = test_runtime_dir();
    assert_eq!(
        wait_for_version("2.0.0", Duration::from_millis(50), || probe_router_version(
            runtime_dir.path()
        )),
        VersionAnswer::Silent
    );
}

// --- restarting every running session ---

/// What a stand-in session answers with.
struct SessionScript {
    /// The answer to the Restart request.
    restart: IpcResult,
    /// The build version every Hello answer of this session carries.
    version: String,
}

/// Serve `connections` callers as a session would: bind the session's address,
/// write the endpoint file advertising it, then answer that many callers.
///
/// The first caller writes a Hello and a Restart back to back and is answered
/// per `script`. Every caller after the first writes a Hello alone and is
/// answered with the script's version. A caller arriving once `connections`
/// are served finds nothing listening, which is what a session that is
/// replacing its own image looks like.
fn fake_session(
    runtime_dir: &Path,
    session: SessionId,
    script: SessionScript,
    connections: usize,
) -> JoinHandle<()> {
    let held = ConnectionToken::generate();
    let addr = socket_addr(runtime_dir, session);
    let listener = Listener::bind(&addr).expect("bind the stand-in session");
    EndpointFile {
        socket: addr,
        token: held.clone(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session))
    .expect("write the session endpoint file");

    std::thread::spawn(move || {
        let SessionScript { restart, version } = script;
        for served in 0..connections {
            let mut connection = listener.accept().expect("accept the caller");
            let hello: IpcRequest = connection.recv().expect("read the hello");
            let IpcRequestKind::Hello {
                token: presented, ..
            } = &hello.kind
            else {
                panic!("expected a Hello first");
            };
            assert_eq!(
                presented, &held,
                "the caller presents the endpoint file's token"
            );

            if served == 0 {
                let asked: IpcRequest = connection.recv().expect("read the restart");
                assert_eq!(
                    asked.kind,
                    IpcRequestKind::Restart,
                    "expected a Restart after the Hello"
                );
                answer(
                    &mut connection,
                    hello.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        version: version.clone(),
                    },
                );
                answer(&mut connection, asked.request_id, restart.clone());
            } else {
                answer(
                    &mut connection,
                    hello.request_id,
                    IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        version: version.clone(),
                    },
                );
            }
        }
    })
}

/// Answer `request_id` with `result` on `connection`.
fn answer(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send the scripted reply");
}

/// The refusal a koshi whose build has no Restart request answers with.
fn no_such_request() -> IpcResult {
    IpcResult::Error(IpcErrorPayload {
        code: IpcErrorCode::UnsupportedKind,
        message: "this koshi has no Restart request".to_string(),
    })
}

#[test]
fn a_session_reporting_the_installed_version_confirms_its_restart() {
    let runtime_dir = test_runtime_dir();
    let session = SessionId::new();
    let stand_in = fake_session(
        runtime_dir.path(),
        session,
        SessionScript {
            restart: IpcResult::Restarting,
            version: "3.3.3".to_string(),
        },
        2,
    );

    let outcomes = restart_advertised_sessions(runtime_dir.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(outcomes, vec![(session, SessionOutcome::Confirmed)]);
    stand_in
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_still_on_the_old_version_is_reported_after_the_wait() {
    let runtime_dir = test_runtime_dir();
    let session = SessionId::new();
    let stand_in = fake_session(
        runtime_dir.path(),
        session,
        SessionScript {
            restart: IpcResult::Restarting,
            version: "1.0.0".to_string(),
        },
        2,
    );

    let outcomes =
        restart_advertised_sessions(runtime_dir.path(), "2.0.0", Duration::from_millis(250));

    assert_eq!(
        outcomes,
        vec![(session, SessionOutcome::StillOn("1.0.0".to_string()))]
    );
    stand_in
        .join()
        .expect("the stand-in served its connections");
}

#[test]
fn a_session_with_no_restart_request_is_reported_as_too_old() {
    let runtime_dir = test_runtime_dir();
    let session = SessionId::new();
    let stand_in = fake_session(
        runtime_dir.path(),
        session,
        SessionScript {
            restart: no_such_request(),
            version: "1.0.0".to_string(),
        },
        1,
    );

    let outcomes = restart_advertised_sessions(runtime_dir.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(outcomes, vec![(session, SessionOutcome::TooOld)]);
    stand_in.join().expect("the stand-in served its connection");
}

#[test]
fn one_session_refusing_still_leaves_every_other_session_asked() {
    let runtime_dir = test_runtime_dir();
    let confirms_one = SessionId::new();
    let refuses = SessionId::new();
    let confirms_two = SessionId::new();
    let stand_ins = vec![
        fake_session(
            runtime_dir.path(),
            confirms_one,
            SessionScript {
                restart: IpcResult::Restarting,
                version: "3.3.3".to_string(),
            },
            2,
        ),
        fake_session(
            runtime_dir.path(),
            refuses,
            SessionScript {
                restart: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::MalformedRequest,
                    message: "a pane is mid-write".to_string(),
                }),
                version: "3.3.3".to_string(),
            },
            1,
        ),
        fake_session(
            runtime_dir.path(),
            confirms_two,
            SessionScript {
                restart: IpcResult::Restarting,
                version: "3.3.3".to_string(),
            },
            2,
        ),
    ];

    let mut outcomes =
        restart_advertised_sessions(runtime_dir.path(), "3.3.3", Duration::from_secs(5));
    outcomes.sort_by_key(|(id, _)| id.to_string());
    let mut expected = vec![
        (confirms_one, SessionOutcome::Confirmed),
        (
            refuses,
            SessionOutcome::Failed("IPC unavailable: a pane is mid-write".to_string()),
        ),
        (confirms_two, SessionOutcome::Confirmed),
    ];
    expected.sort_by_key(|(id, _)| id.to_string());

    assert_eq!(outcomes, expected);
    for stand_in in stand_ins {
        stand_in
            .join()
            .expect("the stand-in served its connections");
    }
}

#[test]
fn no_running_session_leaves_the_router_confirmation_unchanged() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_reporting(runtime_dir.path(), "3.3.3");

    let outcomes = restart_advertised_sessions(runtime_dir.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(outcomes, Vec::new());
    assert_eq!(
        wait_for_version("3.3.3", Duration::from_secs(5), || probe_router_version(
            runtime_dir.path()
        )),
        VersionAnswer::Installed
    );
    router.join().expect("the stand-in served its connection");
}

#[test]
fn strip_v_drops_a_leading_v_only() {
    assert_eq!(strip_v("v1.2.3"), "1.2.3");
    assert_eq!(strip_v("1.2.3"), "1.2.3");
    assert_eq!(strip_v("version"), "ersion");
}

#[test]
fn a_far_higher_tag_is_newer() {
    assert!(is_newer("v9999.0.0"));
    assert!(is_newer("9999.0.0"));
}

#[test]
fn a_zero_tag_is_not_newer() {
    assert!(!is_newer("v0.0.0"));
}

#[test]
fn the_current_build_is_not_newer_than_itself() {
    assert!(!is_newer(APP_VERSION));
}

#[test]
fn a_malformed_tag_is_not_newer() {
    assert!(!is_newer("not-a-version"));
    assert!(!is_newer("v"));
}

#[test]
fn a_first_ever_check_is_due() {
    let state = UpdateState::default();
    assert!(is_due(&state, 14));
}

#[test]
fn a_check_within_the_interval_is_not_due() {
    let state = UpdateState {
        last_check: Some(now_secs()),
    };
    assert!(!is_due(&state, 14));
}

#[test]
fn a_check_older_than_the_interval_is_due() {
    let fifteen_days_ago = now_secs().saturating_sub(15 * SECONDS_PER_DAY);
    let state = UpdateState {
        last_check: Some(fifteen_days_ago),
    };
    assert!(is_due(&state, 14));
}

#[test]
fn a_zero_interval_is_always_due() {
    let state = UpdateState {
        last_check: Some(now_secs()),
    };
    assert!(is_due(&state, 0));
}

#[test]
fn binary_url_matches_the_release_naming_on_supported_platforms() {
    // The exact archive name is platform-specific; assert the invariant parts
    // for whichever platform the test runs on.
    let url = binary_url("v0.2.0").expect("dev + CI platforms are all supported");
    assert!(
        url.starts_with("https://github.com/gohyuhan/koshi/releases/download/v0.2.0/koshi-v0.2.0-"),
        "unexpected url: {url}"
    );
    let ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
    assert!(url.ends_with(ext), "unexpected extension in {url}");
}

#[test]
fn binary_name_is_platform_specific() {
    if cfg!(windows) {
        assert_eq!(binary_name(), "koshi.exe");
    } else {
        assert_eq!(binary_name(), "koshi");
    }
}

#[test]
fn state_defaults_when_deserialized_from_empty_object() {
    let state: UpdateState = serde_json::from_str("{}").expect("empty object is valid state");
    assert_eq!(state.last_check, None);
}

#[test]
fn state_survives_a_serialize_deserialize_round_trip() {
    let original = UpdateState {
        last_check: Some(1_700_000_000),
    };
    let text = serde_json::to_string(&original).expect("serializable");
    let restored: UpdateState = serde_json::from_str(&text).expect("deserializable");
    assert_eq!(restored.last_check, original.last_check);
}

// --- release JSON parsing (no network: fixture strings only) ---

#[test]
fn a_release_object_deserializes_its_tag_name() {
    let release: Release = serde_json::from_str(r#"{"tag_name":"v0.2.0","name":"ignored"}"#)
        .expect("a release object with extra fields still parses");
    assert_eq!(release.tag_name, "v0.2.0");
}

#[test]
fn a_release_list_deserializes_every_tag_in_order() {
    let releases: Vec<Release> =
        serde_json::from_str(r#"[{"tag_name":"v0.2.0"},{"tag_name":"v0.1.0"}]"#)
            .expect("a release array parses");
    let tags: Vec<String> = releases.into_iter().map(|r| r.tag_name).collect();
    assert_eq!(tags, vec!["v0.2.0".to_string(), "v0.1.0".to_string()]);
}

// --- update_err + now_secs ---

#[test]
fn update_err_wraps_the_detail_in_a_cli_update_error() {
    match update_err("boom") {
        CliError::Update { detail } => assert_eq!(detail, "boom"),
        other => panic!("expected CliError::Update, got {other:?}"),
    }
}

#[test]
fn now_secs_is_after_the_year_2023() {
    // A whole-second Unix timestamp taken now is always past 2023-11-14.
    assert!(now_secs() > 1_700_000_000);
}

// --- archive extraction (local files, no network) ---

/// Writes a gzip-compressed tar to a temp file, one regular-file entry per
/// `(name, bytes)`.
fn write_tar_gz(entries: &[(&str, &[u8])]) -> TempPath {
    let file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".tar.gz")
        .tempfile()
        .expect("temp file");
    {
        let encoder = flate2::write::GzEncoder::new(file.as_file(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).expect("path");
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, *data).expect("append entry");
        }
        tar.into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
    }
    file.into_temp_path()
}

/// Writes a zip archive to a temp file, one entry per `(name, bytes)`.
fn write_zip(entries: &[(&str, &[u8])]) -> TempPath {
    let file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".zip")
        .tempfile()
        .expect("temp file");
    {
        let mut zip = zip::ZipWriter::new(file.as_file());
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            zip.start_file(*name, options).expect("start entry");
            zip.write_all(data).expect("write entry");
        }
        zip.finish().expect("finish zip");
    }
    file.into_temp_path()
}

#[test]
fn extracting_a_tar_gz_returns_the_named_binary_bytes() {
    let archive = write_tar_gz(&[("readme.txt", b"docs"), (binary_name(), b"binary-bytes")]);
    let extracted = extract(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");
    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn extracting_a_tar_gz_without_the_binary_is_an_error() {
    let archive = write_tar_gz(&[("readme.txt", b"docs")]);
    assert_eq!(
        extract(archive.as_ref(), "koshi.tar.gz").expect_err("no binary present"),
        "binary not found in archive"
    );
}

#[test]
fn extracting_a_zip_returns_the_named_binary_bytes() {
    let archive = write_zip(&[("readme.txt", b"docs"), (binary_name(), b"binary-bytes")]);
    let extracted = extract(archive.as_ref(), "koshi.zip").expect("extract the binary");
    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn extracting_a_zip_without_the_binary_is_an_error() {
    let archive = write_zip(&[("readme.txt", b"docs")]);
    assert_eq!(
        extract(archive.as_ref(), "koshi.zip").expect_err("no binary present"),
        "binary not found in archive"
    );
}

/// Writes a gzip-compressed tar to a temp file, one entry per
/// `(name, entry type, bytes)`.
fn write_tar_gz_of_kinds(entries: &[(&str, tar::EntryType, &[u8])]) -> TempPath {
    let file = Builder::new()
        .prefix("koshi-test-")
        .suffix(".tar.gz")
        .tempfile()
        .expect("temp file");
    {
        let encoder = flate2::write::GzEncoder::new(file.as_file(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (name, entry_type, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).expect("path");
            header.set_entry_type(*entry_type);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, *data).expect("append entry");
        }
        tar.into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
    }
    file.into_temp_path()
}

#[test]
fn a_directory_carrying_the_binary_name_is_passed_over_for_the_real_file() {
    let archive = write_tar_gz_of_kinds(&[
        (binary_name(), tar::EntryType::Directory, b""),
        (binary_name(), tar::EntryType::Regular, b"binary-bytes"),
    ]);

    let extracted = extract(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn a_symbolic_link_carrying_the_binary_name_is_passed_over_for_the_real_file() {
    let archive = write_tar_gz_of_kinds(&[
        (binary_name(), tar::EntryType::Symlink, b""),
        (binary_name(), tar::EntryType::Regular, b"binary-bytes"),
    ]);

    let extracted = extract(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"binary-bytes"
    );
}

#[test]
fn a_tar_gz_binary_under_a_top_level_directory_is_found_by_its_file_name() {
    let nested = format!("koshi-v9.9.9-linux-amd64/{}", binary_name());
    let archive = write_tar_gz(&[(nested.as_str(), b"nested-bytes")]);

    let extracted = extract(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    assert_eq!(
        fs::read(AsRef::<Path>::as_ref(&extracted)).expect("read extracted binary"),
        b"nested-bytes"
    );
}

#[test]
fn a_zip_binary_under_a_top_level_directory_is_found_by_its_file_name() {
    let nested = format!("koshi-v9.9.9-windows-amd64/{}", binary_name());
    let archive = write_zip(&[(nested.as_str(), b"nested-bytes")]);

    let extracted = extract(archive.as_ref(), "koshi.zip").expect("extract the binary");

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
        binary_name(),
        tar::EntryType::Regular,
        b"binary-bytes" as &[u8],
    )]);

    let extracted = extract(archive.as_ref(), "koshi.tar.gz").expect("extract the binary");

    let mode = fs::metadata(AsRef::<Path>::as_ref(&extracted))
        .expect("read the extracted binary's metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

/// The pre-release picker takes the highest version by semver order, never
/// the newest by publish date: a re-published older-versioned tag loses to a
/// higher one wherever it sits in the list.
#[test]
fn highest_version_picks_semver_order_not_list_order() {
    let releases = |tags: &[&str]| -> Vec<Release> {
        tags.iter()
            .map(|tag| Release {
                tag_name: (*tag).to_string(),
            })
            .collect()
    };

    assert_eq!(
        highest_version(releases(&["v0.3.0-rc.2", "v0.3.0-rc.10", "v0.2.0"])).unwrap(),
        "v0.3.0-rc.10"
    );
    // List order plays no part: the highest wins from the front too.
    assert_eq!(
        highest_version(releases(&["v0.4.0", "v0.3.0"])).unwrap(),
        "v0.4.0"
    );
    // A tag that is not a version is skipped, not an error.
    assert_eq!(
        highest_version(releases(&["nightly", "v0.1.0"])).unwrap(),
        "v0.1.0"
    );
    assert_eq!(
        highest_version(Vec::new()).unwrap_err(),
        "no releases found"
    );
    // A list where no tag is a version reads the same as an empty one.
    assert_eq!(
        highest_version(releases(&["nightly", "edge"])).unwrap_err(),
        "no releases found"
    );
}
