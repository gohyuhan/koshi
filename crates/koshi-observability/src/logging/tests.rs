//! Tests for the tracing subscriber bootstrap and redaction machinery.
//!
//! Coverage: the session log path, per-session lazy file creation, the level
//! cutoff, single global install, the disabled no-op, and environment-map
//! redaction.

use super::*;

use koshi_core::ids::SessionId;

// A sample event must carry the canonical join fields, and any env-derived value
// must arrive already scrubbed — the token must never appear in raw output.
#[test]
fn sample_event_has_canonical_fields_and_redacts_token() {
    let (_guard, logs) = with_test_writer();

    let mut env = BTreeMap::new();
    env.insert(
        "KOSHI_CONTEXT_TOKEN".to_string(),
        "super-secret".to_string(),
    );
    env.insert("HOME".to_string(), "/home/dev".to_string());

    tracing::info!(
        session_id = "sess-1",
        client_id = "client-1",
        tab_id = "tab-1",
        pane_id = "pane-1",
        command_id = "cmd-1",
        plugin_id = "plugin-1",
        subscriber_id = "sub-1",
        env = %redacted_env_field(&env),
        "sample event"
    );

    let out = logs.contents();
    for field in CANONICAL_FIELDS {
        assert!(out.contains(field), "missing canonical field `{field}`");
    }
    assert!(
        !out.contains("super-secret"),
        "token leaked into log output"
    );
    assert!(out.contains("***"), "redaction marker absent");
    assert!(
        out.contains("/home/dev"),
        "non-sensitive value should pass through"
    );
}

#[test]
fn redacted_env_field_hides_sensitive_keys_only() {
    let mut env = BTreeMap::new();
    env.insert("API_TOKEN".to_string(), "abc123".to_string());
    env.insert("PATH".to_string(), "/usr/bin".to_string());

    let field = redacted_env_field(&env);

    assert_eq!(field, "API_TOKEN=*** PATH=/usr/bin");
    assert!(!field.contains("abc123"));
}

#[test]
fn redacted_env_field_of_empty_map_is_empty_string() {
    let env: BTreeMap<String, String> = BTreeMap::new();
    assert_eq!(redacted_env_field(&env), "");
}

#[test]
fn session_log_path_is_the_named_file_in_the_logs_folder() {
    let session = SessionId::new();
    let path = session_log_path(session);
    let file = format!("koshi-log-{}.log", session.as_uuid());

    // The state dir's own tail differs per OS, so pin the `logs/<file>` tail
    // plus the state-dir prefix instead of a full literal path.
    assert!(
        path.ends_with(format!("logs/{file}")),
        "unexpected log path: {}",
        path.display()
    );
    if let Some(state) = koshi_paths::state_dir() {
        assert_eq!(path, state.join("logs").join(&file));
    }
}

#[test]
fn two_sessions_get_two_distinct_log_files() {
    let a = session_log_path(SessionId::new());
    let b = session_log_path(SessionId::new());
    assert_ne!(a, b, "each session must name its own log file");
}

#[test]
fn tail_logs_reads_local_files_in_name_order_and_applies_since() {
    let dir = std::env::temp_dir().join(format!("koshi-tail-log-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the log directory");
    std::fs::write(
        dir.join("koshi-log-b.log"),
        "1970-01-01T00:00:01.000000Z old\n  1970-01-01T00:01:00.000000Z boundary\n1970-01-01T00:01:10.000000Z new\n",
    )
    .expect("write the older and newer records");
    std::fs::write(
        dir.join("koshi-log-a.log"),
        "{\"timestamp\":\"1970-01-01T00:01:20.000000Z\",\"message\":\"json\"}\n",
    )
    .expect("write the JSON record");
    std::fs::write(dir.join("not-a-koshi-log.txt"), "ignored\n").expect("write the ignored file");

    let rendered = tail_logs(&dir, Some(UNIX_EPOCH + Duration::from_secs(60)))
        .expect("read the local log files");

    assert_eq!(
        rendered,
        "== koshi-log-a.log ==\n{\"timestamp\":\"1970-01-01T00:01:20.000000Z\",\"message\":\"json\"}\n== koshi-log-b.log ==\n  1970-01-01T00:01:00.000000Z boundary\n1970-01-01T00:01:10.000000Z new\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tail_logs_returns_an_empty_answer_before_logging_creates_the_directory() {
    let dir = std::env::temp_dir().join(format!("koshi-tail-log-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        tail_logs(&dir, None).expect("a missing log directory is empty"),
        ""
    );
}

#[test]
fn tail_logs_keeps_only_the_bounded_tail_of_each_file() {
    let dir = std::env::temp_dir().join(format!("koshi-tail-log-cap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the log directory");
    let contents = (0..=1000)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(dir.join("koshi-log-one.log"), contents).expect("write the log lines");

    let rendered = tail_logs(&dir, None).expect("read the local log file");

    assert_eq!(
        rendered.lines().count(),
        1001,
        "the header plus exactly the newest lines are rendered"
    );
    assert!(
        !rendered.contains("line 0\n"),
        "the oldest line was not tailed"
    );
    assert!(
        rendered.contains("line 1\n"),
        "the first retained line was tailed"
    );
    assert!(
        rendered.contains("line 1000\n"),
        "the newest line was tailed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tail_logs_reports_invalid_matching_file_bytes_without_partial_output() {
    let dir = std::env::temp_dir().join(format!("koshi-tail-log-invalid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the log directory");
    std::fs::write(
        dir.join("koshi-log-a.log"),
        "1970-01-01T00:01:20.000000Z readable\n",
    )
    .expect("write the readable log");
    std::fs::write(dir.join("koshi-log-b.log"), [0xff, 0xfe]).expect("write the invalid log bytes");

    let error = tail_logs(&dir, None).expect_err("invalid log bytes must fail the snapshot");
    let LogTailError::File { path, source } = error else {
        panic!("the error must name the unreadable matching file");
    };

    assert_eq!(path, dir.join("koshi-log-b.log"));
    assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);

    let _ = std::fs::remove_dir_all(&dir);
}

// Enabled + a line at the configured level: the file (and its `logs/` parent)
// is created lazily on that first write, and a second install fails since a
// process has one global subscriber. This is the only test that claims the
// global slot, so it is deterministic regardless of test order.
#[test]
fn init_to_path_creates_the_file_lazily_and_installs_once() {
    let dir = std::env::temp_dir().join(format!("koshi-log-test-{}", std::process::id()));
    let path = dir.join("logs").join("koshi-log-test.log");
    let _ = std::fs::remove_dir_all(&dir);

    init_to_path(&path, LogLevel::Warning, LogFormat::Json).expect("first install succeeds");

    // Nothing written yet: no subscriber event has fired, so the file must not
    // exist — creation is on the first line, not at install.
    assert!(
        !path.exists(),
        "the file must not exist before the first log line"
    );

    tracing::warn!(session_id = "sess-file", "file sink event");

    // A second install fails: a process has a single global subscriber.
    let second = init_to_path(&path, LogLevel::Warning, LogFormat::Json);
    assert!(matches!(second, Err(TracingError::AlreadyInitialized)));

    let contents = std::fs::read_to_string(&path).expect("log file was created on first write");
    assert!(contents.contains("session_id"), "missing canonical field");
    assert!(contents.contains("file sink event"), "missing log message");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_tracing_disabled_writes_no_file_and_is_a_noop() {
    // Disabled installs no global subscriber, so it never conflicts with the
    // one global install and is safely idempotent — both calls succeed
    // regardless of test order, and neither touches disk.
    let params = || LoggingParams {
        enabled: false,
        level: LogLevel::Warning,
        format: LogFormat::Pretty,
        session_id: SessionId::new(),
    };
    let path = session_log_path(params().session_id);
    assert!(init_tracing(params()).is_ok());
    assert!(init_tracing(params()).is_ok());
    assert!(!path.exists(), "disabled logging must create no file");
}

// The level cutoff drops a line below it before it reaches the writer: with
// `error`, a warning must not be written. Uses the thread-local test writer so
// it never races the one global install above.
#[test]
fn a_line_below_the_configured_level_is_dropped() {
    let logs = CapturedLogs::default();
    let subscriber = fmt()
        .with_max_level(max_level(LogLevel::Error))
        .json()
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    tracing::warn!("a warning below the error cutoff");
    tracing::error!("an error at the cutoff");

    let out = logs.contents();
    assert!(
        !out.contains("below the error cutoff"),
        "warning must be dropped at level error"
    );
    assert!(
        out.contains("at the cutoff"),
        "error must be written at level error"
    );
}

// The most verbose configured level, `info`, admits an info line — the arm the
// other cutoff tests never reach (they configure `warning`/`error`). Uses the
// thread-local test writer so it never races the one global install above.
#[test]
fn a_line_at_info_level_is_written_when_the_cutoff_is_info() {
    let logs = CapturedLogs::default();
    let subscriber = fmt()
        .with_max_level(max_level(LogLevel::Info))
        .json()
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    tracing::info!("an info line at the info cutoff");

    assert!(
        logs.contents().contains("an info line at the info cutoff"),
        "info must be written at level info"
    );
}

// The per-session file writer, exercised directly (without a subscriber): its
// first write creates the `logs/` parent and the file, a second write appends,
// and flush is a no-op that reports success.
#[test]
fn session_log_writer_creates_parent_then_appends_each_write() {
    use std::io::Write as _;

    let dir = std::env::temp_dir().join(format!("koshi-writer-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("logs").join("koshi-log-writer.log");

    let mut writer = SessionLogWriter { path: path.clone() };
    let first = writer
        .write(b"line one\n")
        .expect("first write creates the file");
    let second = writer.write(b"line two\n").expect("second write appends");
    writer.flush().expect("flush is a no-op");

    assert_eq!(first, 9, "write reports the byte count it accepted");
    assert_eq!(second, 9);
    assert_eq!(std::fs::read(&path).unwrap(), b"line one\nline two\n");

    let _ = std::fs::remove_dir_all(&dir);
}

// A `logs/` directory removed between writes comes back on the next line: the
// failed open triggers one recreate-and-retry.
#[test]
fn session_log_writer_recreates_a_logs_directory_removed_mid_session() {
    use std::io::Write as _;

    let dir = std::env::temp_dir().join(format!("koshi-writer-regrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("logs").join("koshi-log-writer.log");

    let mut writer = SessionLogWriter { path: path.clone() };
    let first = writer
        .write(b"line one\n")
        .expect("first write creates the file");
    std::fs::remove_dir_all(dir.join("logs")).expect("remove the logs directory");
    let second = writer
        .write(b"line two\n")
        .expect("the write after the removal recreates the directory");
    assert_eq!(first, 9, "write reports the byte count it accepted");
    assert_eq!(second, 9);

    assert_eq!(std::fs::read(&path).unwrap(), b"line two\n");

    let _ = std::fs::remove_dir_all(&dir);
}

// The capture writer records raw bytes, `contents` returns them verbatim, and
// `lines` splits on newlines; flush reports success without touching the buffer.
#[test]
fn captured_writer_records_bytes_and_lines_split_on_newlines() {
    use std::io::Write as _;

    let logs = CapturedLogs::default();
    let mut writer = logs.make_writer();
    let written = writer
        .write(b"first\nsecond\n")
        .expect("capture write always succeeds");
    writer.flush().expect("flush is a no-op");

    assert_eq!(written, 13, "write reports the byte count it captured");
    assert_eq!(logs.contents(), "first\nsecond\n");
    assert_eq!(
        logs.lines(),
        vec!["first".to_string(), "second".to_string()]
    );
}

#[test]
fn tracing_error_display_names_the_already_initialized_cause() {
    assert_eq!(
        TracingError::AlreadyInitialized.to_string(),
        "tracing is already initialized for this process"
    );
}

/// A warning both threads in the test below fire through one shared call site.
fn probe_warning() {
    tracing::warn!("probe fired");
}

// tracing caches per-call-site interest process-wide. A capture must still see
// an event whose call site was first executed by a thread with no subscriber.
#[test]
fn a_capture_sees_a_call_site_first_fired_from_an_uncaptured_thread() {
    let (_guard, logs) = with_test_writer();

    // First execution of the call site happens on a thread with no subscriber.
    std::thread::spawn(probe_warning)
        .join()
        .expect("probe thread runs to completion");

    // Same call site, now on the thread that holds the capture.
    probe_warning();

    let out = logs.contents();
    assert!(out.contains("probe fired"), "capture is empty: {out:?}");
}
