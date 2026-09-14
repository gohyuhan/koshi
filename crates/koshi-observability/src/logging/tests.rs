//! Tests for the tracing subscriber bootstrap and the writers it uses.
//!
//! Coverage: the session log path, per-session lazy file creation, the level
//! cutoff, single global install, the disabled no-op, the per-session file
//! writer, and the capture writer.

use super::*;

/// Install a JSON subscriber on this thread with the cutoff `log_level` gives, and
/// capture its output.
fn capture_logs_at_log_level(
    log_level: LogLevel,
) -> (tracing::subscriber::DefaultGuard, CapturedLogs) {
    capture_logs_at_level(resolve_maximum_tracing_level(log_level))
}

// Every correlation ID named in the module policy reaches the line under its
// own field name.
#[test]
fn a_line_carries_every_correlation_id_field_it_was_given() {
    let (_subscriber_guard, captured_logs) = with_test_writer();

    tracing::info!(
        session_id = "sess-1",
        client_id = "client-1",
        tab_id = "tab-1",
        pane_id = "pane-1",
        command_id = "cmd-1",
        plugin_id = "plugin-1",
        subscriber_id = "sub-1",
        "sample event"
    );

    let log_output = captured_logs.contents();
    assert_eq!(log_output.lines().count(), 1, "{log_output}");
    assert!(
        log_output.contains(r#""session_id":"sess-1""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(r#""client_id":"client-1""#),
        "{log_output}"
    );
    assert!(log_output.contains(r#""tab_id":"tab-1""#), "{log_output}");
    assert!(log_output.contains(r#""pane_id":"pane-1""#), "{log_output}");
    assert!(
        log_output.contains(r#""command_id":"cmd-1""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(r#""plugin_id":"plugin-1""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(r#""subscriber_id":"sub-1""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(r#""message":"sample event""#),
        "{log_output}"
    );
}

#[test]
fn session_log_path_is_the_named_file_in_the_logs_folder() {
    let session_id = SessionId::new();
    let log_file_path = session_log_path(session_id);
    let log_file_name = format!("koshi-log-{}.log", session_id.get_uuid());

    // Pins the `logs/<file>` tail on every OS, then the full path when the
    // state directory resolves.
    assert!(
        log_file_path.ends_with(format!("logs/{log_file_name}")),
        "unexpected log path: {}",
        log_file_path.display()
    );
    if let Some(state_directory) = koshi_paths::resolve_state_directory() {
        assert_eq!(
            log_file_path,
            state_directory.join("logs").join(&log_file_name)
        );
    }
}

#[test]
fn two_sessions_get_two_distinct_log_files() {
    let first_log_file_path = session_log_path(SessionId::new());
    let second_log_file_path = session_log_path(SessionId::new());
    assert_ne!(
        first_log_file_path, second_log_file_path,
        "each session must name its own log file"
    );
}

// The file and its `logs/` parent are created on the first write, not at
// install. A second install fails. This is the only test in the binary that
// installs the global subscriber.
#[test]
fn initialize_tracing_at_path_creates_file_lazily_and_installs_once() {
    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-log-test-{}", std::process::id()));
    let log_file_path = test_directory_path.join("logs").join("koshi-log-test.log");
    let _ = std::fs::remove_dir_all(&test_directory_path);

    init_tracing(LoggingParams {
        is_enabled: false,
        log_level: LogLevel::Error,
        log_format: LogFormat::Json,
        session_id: SessionId::new(),
    })
    .expect("disabled logging installs nothing");
    initialize_tracing_at_path(&log_file_path, LogLevel::Warning, LogFormat::Json)
        .expect("first install succeeds");

    // No line has been written yet: the file does not exist.
    assert!(
        !log_file_path.exists(),
        "the file must not exist before the first log line"
    );

    tracing::warn!(session_id = "sess-file", "file sink event");

    // A process has one global subscriber; the second install fails.
    let second_install_result =
        initialize_tracing_at_path(&log_file_path, LogLevel::Warning, LogFormat::Json);
    assert!(matches!(
        second_install_result,
        Err(TracingError::AlreadyInitialized)
    ));

    let log_contents =
        std::fs::read_to_string(&log_file_path).expect("log file was created on first write");
    assert_eq!(log_contents.lines().count(), 1, "{log_contents}");
    assert!(log_contents.contains(r#""level":"WARN""#), "{log_contents}");
    assert!(
        log_contents.contains(r#""session_id":"sess-file""#),
        "missing canonical field: {log_contents}"
    );
    assert!(
        log_contents.contains(r#""message":"file sink event""#),
        "missing log message: {log_contents}"
    );

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// Disabled installs no subscriber: both calls succeed in any test order, and
// no file is created for the session.
#[test]
fn init_tracing_disabled_writes_no_file_and_is_a_noop() {
    let logging_params = LoggingParams {
        is_enabled: false,
        log_level: LogLevel::Warning,
        log_format: LogFormat::Pretty,
        session_id: SessionId::new(),
    };
    let log_file_path = session_log_path(logging_params.session_id);
    init_tracing(logging_params.clone()).expect("disabled logging installs nothing");
    init_tracing(logging_params).expect("a second disabled install also installs nothing");
    assert!(
        !log_file_path.exists(),
        "disabled logging must create no file"
    );
}

#[test]
fn maximum_tracing_level_matches_configured_log_level() {
    assert_eq!(resolve_maximum_tracing_level(LogLevel::Info), Level::INFO);
    assert_eq!(
        resolve_maximum_tracing_level(LogLevel::Warning),
        Level::WARN
    );
    assert_eq!(resolve_maximum_tracing_level(LogLevel::Error), Level::ERROR);
}

// The level cutoff drops a line below it before it reaches the writer: with
// `error`, a warning is not written. Uses a thread-local subscriber; the global
// slot stays untouched.
#[test]
fn a_line_below_the_configured_level_is_dropped() {
    let (_subscriber_guard, captured_logs) = capture_logs_at_log_level(LogLevel::Error);

    tracing::warn!("a warning below the error cutoff");
    tracing::error!("an error at the cutoff");

    let log_output = captured_logs.contents();
    assert_eq!(log_output.lines().count(), 1, "{log_output}");
    assert!(
        !log_output.contains("below the error cutoff"),
        "warning must be dropped at level error"
    );
    assert!(log_output.contains(r#""level":"ERROR""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"an error at the cutoff""#),
        "error must be written at level error"
    );
}

// With `warning`, an info line is dropped and a warning is written.
#[test]
fn an_info_line_is_dropped_when_the_cutoff_is_warning() {
    let (_subscriber_guard, captured_logs) = capture_logs_at_log_level(LogLevel::Warning);

    tracing::info!("an info line below the warning cutoff");
    tracing::warn!("a warning at the warning cutoff");

    let log_output = captured_logs.contents();
    assert_eq!(log_output.lines().count(), 1, "{log_output}");
    assert!(
        !log_output.contains("below the warning cutoff"),
        "info must be dropped at level warning"
    );
    assert!(log_output.contains(r#""level":"WARN""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"a warning at the warning cutoff""#),
        "warning must be written at level warning"
    );
}

// The most verbose configured level, `info`, admits an info line.
#[test]
fn a_line_at_info_level_is_written_when_the_cutoff_is_info() {
    let (_subscriber_guard, captured_logs) = capture_logs_at_log_level(LogLevel::Info);

    tracing::info!("an info line at the info cutoff");

    let log_output = captured_logs.contents();
    assert_eq!(log_output.lines().count(), 1, "{log_output}");
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"an info line at the info cutoff""#),
        "info must be written at level info"
    );
}

// The per-session file writer, exercised directly (without a subscriber): its
// first write creates the `logs/` parent and the file, a second write appends,
// and flush is a no-op that reports success.
#[test]
fn session_log_writer_creates_parent_then_appends_each_write() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-writer.log");

    let mut log_writer = SessionLogWriter {
        log_file_path: log_file_path.clone(),
    };
    let first_write_byte_count = log_writer
        .write(b"line one\n")
        .expect("first write creates the file");
    let second_write_byte_count = log_writer
        .write(b"line two\n")
        .expect("second write appends");
    log_writer.flush().expect("flush is a no-op");

    assert_eq!(
        first_write_byte_count, 9,
        "write reports the byte count it accepted"
    );
    assert_eq!(second_write_byte_count, 9);
    assert_eq!(
        std::fs::read(&log_file_path).unwrap(),
        b"line one\nline two\n"
    );

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A `logs/` directory removed between writes comes back on the next line: the
// failed open triggers one recreate-and-retry.
#[test]
fn session_log_writer_recreates_a_logs_directory_removed_mid_session() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-regrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-writer.log");

    let mut log_writer = SessionLogWriter {
        log_file_path: log_file_path.clone(),
    };
    let first_write_byte_count = log_writer
        .write(b"line one\n")
        .expect("first write creates the file");
    std::fs::remove_dir_all(test_directory_path.join("logs")).expect("remove the logs directory");
    let second_write_byte_count = log_writer
        .write(b"line two\n")
        .expect("the write after the removal recreates the directory");
    assert_eq!(
        first_write_byte_count, 9,
        "write reports the byte count it accepted"
    );
    assert_eq!(second_write_byte_count, 9);

    assert_eq!(std::fs::read(&log_file_path).unwrap(), b"line two\n");

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A log file removed between writes comes back on the next line, holding only
// the lines written after the removal.
#[test]
fn session_log_writer_recreates_a_log_file_removed_mid_session() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-refile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-writer.log");

    let mut log_writer = SessionLogWriter {
        log_file_path: log_file_path.clone(),
    };
    let first_write_byte_count = log_writer
        .write(b"line one\n")
        .expect("first write creates the file");
    std::fs::remove_file(&log_file_path).expect("remove the log file");
    let second_write_byte_count = log_writer
        .write(b"line two\n")
        .expect("the write after the removal recreates the file");
    assert_eq!(
        first_write_byte_count, 9,
        "write reports the byte count it accepted"
    );
    assert_eq!(second_write_byte_count, 9);

    assert_eq!(std::fs::read(&log_file_path).unwrap(), b"line two\n");

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A regular file where the `logs/` directory must go: the open fails, the
// recreate fails with `AlreadyExists`, and the write reports that error and
// creates nothing. With the file gone, the next write succeeds.
#[test]
fn session_log_writer_reports_the_error_when_its_parent_is_a_regular_file() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-blocked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    std::fs::create_dir_all(&test_directory_path).expect("create the test directory");
    let blocking_path = test_directory_path.join("logs");
    std::fs::write(&blocking_path, b"not a directory").expect("create the blocking file");
    let log_file_path = blocking_path.join("koshi-log-writer.log");

    let mut log_writer = SessionLogWriter {
        log_file_path: log_file_path.clone(),
    };
    let write_error = log_writer
        .write(b"line one\n")
        .expect_err("a file where the directory must go fails the write");
    assert_eq!(write_error.kind(), io::ErrorKind::AlreadyExists);
    assert!(
        !log_file_path.exists(),
        "nothing is created under a regular file"
    );

    std::fs::remove_file(&blocking_path).expect("remove the blocking file");
    let written_byte_count = log_writer
        .write(b"line two\n")
        .expect("the write after the removal creates the directory and the file");
    assert_eq!(written_byte_count, 9);
    assert_eq!(std::fs::read(&log_file_path).unwrap(), b"line two\n");

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// The log file and the directory the writer creates for it are this user's
// own, the same as the crash report the cleanup domain writes: file `0600`,
// directory `0700`.
#[cfg(unix)]
#[test]
fn a_created_log_file_is_0600_in_a_0700_directory() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_directory_path = test_directory_path.join("logs");
    let log_file_path = log_directory_path.join("koshi-log-writer.log");

    let mut log_writer = SessionLogWriter {
        log_file_path: log_file_path.clone(),
    };
    log_writer
        .write_all(b"line one\n")
        .expect("the first write");

    let log_file_mode = std::fs::metadata(&log_file_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let log_directory_mode = std::fs::metadata(&log_directory_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        log_file_mode, 0o600,
        "the log file is owner read-write only"
    );
    assert_eq!(
        log_directory_mode, 0o700,
        "the logs directory is owner-only"
    );

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// The capture writer records raw bytes, `contents` returns them verbatim, and
// `lines` splits on newlines; flush reports success without touching the buffer.
#[test]
fn captured_writer_records_bytes_and_lines_split_on_newlines() {
    use std::io::Write as _;

    let captured_logs = CapturedLogs::default();
    let mut captured_writer = captured_logs.make_writer();
    let written_byte_count = captured_writer
        .write(b"first\nsecond\n")
        .expect("capture write always succeeds");
    captured_writer.flush().expect("flush is a no-op");

    assert_eq!(
        written_byte_count, 13,
        "write reports the byte count it captured"
    );
    assert_eq!(captured_logs.contents(), "first\nsecond\n");
    assert_eq!(
        captured_logs.lines(),
        vec!["first".to_string(), "second".to_string()]
    );
}

#[test]
fn a_fresh_capture_holds_no_bytes_and_no_lines() {
    let captured_logs = CapturedLogs::default();

    assert_eq!(captured_logs.contents(), "");
    assert_eq!(captured_logs.lines(), Vec::<String>::new());
}

#[test]
fn captured_lines_keep_a_last_line_that_has_no_newline() {
    use std::io::Write as _;

    let captured_logs = CapturedLogs::default();
    captured_logs
        .make_writer()
        .write_all(b"first\nsecond")
        .expect("capture write always succeeds");

    assert_eq!(
        captured_logs.lines(),
        vec!["first".to_string(), "second".to_string()]
    );
}

#[test]
fn two_capture_writers_append_to_one_buffer_in_write_order() {
    use std::io::Write as _;

    let captured_logs = CapturedLogs::default();
    let mut first_writer = captured_logs.make_writer();
    let mut second_writer = captured_logs.make_writer();
    first_writer
        .write_all(b"one\n")
        .expect("capture write always succeeds");
    second_writer
        .write_all(b"two\n")
        .expect("capture write always succeeds");
    first_writer
        .write_all(b"three\n")
        .expect("capture write always succeeds");

    assert_eq!(captured_logs.contents(), "one\ntwo\nthree\n");
}

// A thread that dies while holding the capture buffer poisons its lock.
// `contents`, `lines` and the next write all recover it.
#[test]
fn a_capture_answers_after_a_writer_thread_died_holding_its_buffer() {
    use std::io::Write as _;

    let captured_logs = CapturedLogs::default();
    captured_logs
        .make_writer()
        .write_all(b"before\n")
        .expect("capture write always succeeds");

    // `resume_unwind` skips the panic hook; the guard dropped while unwinding
    // poisons the lock.
    let captured_log_bytes = Arc::clone(&captured_logs.captured_log_bytes);
    let writer_thread_join_result = std::thread::spawn(move || {
        let _captured_log_bytes_guard = captured_log_bytes
            .lock()
            .expect("the buffer is not poisoned yet");
        std::panic::resume_unwind(Box::new("the thread holding the buffer died"));
    })
    .join();
    assert_eq!(
        writer_thread_join_result
            .unwrap_err()
            .downcast_ref::<&str>(),
        Some(&"the thread holding the buffer died")
    );
    assert!(
        captured_logs.captured_log_bytes.is_poisoned(),
        "the lock must be poisoned"
    );

    assert_eq!(captured_logs.contents(), "before\n");
    captured_logs
        .make_writer()
        .write_all(b"after\n")
        .expect("a write after the poison succeeds");
    assert_eq!(
        captured_logs.lines(),
        vec!["before".to_string(), "after".to_string()]
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
fn emit_probe_warning() {
    tracing::warn!("probe fired");
}

// tracing caches per-call-site interest process-wide. A capture still sees an
// event whose call site was first executed by a thread with no subscriber, and
// the event that thread fired stays out of the capture.
#[test]
fn a_capture_sees_a_call_site_first_fired_from_an_uncaptured_thread() {
    let (_subscriber_guard, captured_logs) = with_test_writer();

    // The call site's first execution is on a thread whose subscriber is
    // `Dispatch::none()`, whatever the global slot holds.
    std::thread::spawn(|| {
        tracing::dispatcher::with_default(&tracing::Dispatch::none(), emit_probe_warning);
    })
    .join()
    .expect("probe thread runs to completion");

    // Same call site, on the thread that holds the capture.
    emit_probe_warning();

    let log_output = captured_logs.contents();
    assert_eq!(log_output.lines().count(), 1, "{log_output}");
    assert!(
        log_output.contains(r#""message":"probe fired""#),
        "capture is empty: {log_output:?}"
    );
}

/// An error both threads in the test below fire through one shared call site.
fn emit_probe_error() {
    tracing::error!("cutoff probe fired");
}

// A capture with a cutoff below `trace` gets the same anchor: it still sees an
// event whose call site was first executed by a thread with no subscriber.
#[test]
fn a_level_capped_capture_sees_a_call_site_first_fired_from_an_uncaptured_thread() {
    let (_subscriber_guard, captured_logs) = capture_logs_at_log_level(LogLevel::Error);

    std::thread::spawn(|| {
        tracing::dispatcher::with_default(&tracing::Dispatch::none(), emit_probe_error);
    })
    .join()
    .expect("probe thread runs to completion");

    emit_probe_error();

    let log_output = captured_logs.contents();
    assert_eq!(log_output.lines().count(), 1, "{log_output}");
    assert!(
        log_output.contains(r#""message":"cutoff probe fired""#),
        "capture is empty: {log_output:?}"
    );
}
