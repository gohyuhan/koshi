//! Tests for the self-update helpers: version comparison, check scheduling,
//! archive URL construction, state serialization, and the restart
//! confirmation wait.

use super::*;

use std::thread::JoinHandle;

use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::router::{
    router_endpoint_path, router_socket_addr, RouterHandshake, RouterRequest, RouterResponse,
    RouterResult, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Listener;
use tempfile::TempDir;

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

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

    let confirmed = wait_for_router_version(runtime_dir.path(), "3.3.3", Duration::from_secs(5));

    assert_eq!(confirmed, Some("3.3.3".to_string()));
    router.join().expect("the stand-in served its connection");
}

#[test]
fn a_router_still_on_another_version_is_reported_after_the_wait() {
    let runtime_dir = test_runtime_dir();
    let router = fake_router_reporting(runtime_dir.path(), "1.0.0");

    let answered = wait_for_router_version(runtime_dir.path(), "2.0.0", Duration::from_millis(250));

    assert_eq!(answered, Some("1.0.0".to_string()));
    router.join().expect("the stand-in served its connection");
}

#[test]
fn no_router_answering_reports_no_version_after_the_wait() {
    let runtime_dir = test_runtime_dir();
    assert_eq!(
        wait_for_router_version(runtime_dir.path(), "2.0.0", Duration::from_millis(50)),
        None
    );
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
