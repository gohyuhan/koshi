//! Tests for the tracing subscriber bootstrap and the writers it uses.
//!
//! Coverage: the session and router log paths, lazy file creation, the level
//! cutoff, single global install, the disabled no-op, the log file writer, and
//! the capture writer.

use super::*;

/// The size in bytes `max-log-file-size-in-mb 4` gives, the default.
const DEFAULT_MAX_LOG_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024;

/// The size in bytes `max-log-file-size-in-mb 1` gives, the least `koshi.kdl`
/// takes.
const LOWEST_MAX_LOG_FILE_SIZE_BYTES: u64 = 1024 * 1024;

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
    let log_file_path = resolve_log_file_path(LogFileOwner::Session { session_id });
    let log_file_name = format!("koshi-log-{}.log", session_id.get_uuid());

    // Pins the `logs/<file>` tail on every OS, then the full path when the
    // state directory resolves.
    assert!(
        log_file_path.ends_with(format!("logs/{log_file_name}")),
        "unexpected log path: {}",
        log_file_path.display()
    );
    if let Some(state_directory_path) = koshi_paths::resolve_state_directory() {
        assert_eq!(
            log_file_path,
            state_directory_path.join("logs").join(&log_file_name)
        );
    }
}

#[test]
fn router_log_path_is_koshi_log_router_in_the_logs_folder() {
    let log_file_path = resolve_log_file_path(LogFileOwner::Router);

    assert!(
        log_file_path.ends_with("logs/koshi-log-router.log"),
        "unexpected log path: {}",
        log_file_path.display()
    );
    if let Some(state_directory_path) = koshi_paths::resolve_state_directory() {
        assert_eq!(
            log_file_path,
            state_directory_path
                .join("logs")
                .join("koshi-log-router.log")
        );
    }
}

#[test]
fn two_sessions_get_two_distinct_log_files() {
    let first_log_file_path = resolve_log_file_path(LogFileOwner::Session {
        session_id: SessionId::new(),
    });
    let second_log_file_path = resolve_log_file_path(LogFileOwner::Session {
        session_id: SessionId::new(),
    });
    assert_ne!(
        first_log_file_path, second_log_file_path,
        "each session must name its own log file"
    );
}

// Two disabled installs install nothing, so the install after them is the
// first. The file and its `logs/` parent are created on the first write, not at
// install. A second install fails. This is the only test in the binary that
// installs the global subscriber.
#[test]
fn initialize_tracing_at_path_creates_file_lazily_and_installs_once() {
    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-log-test-{}", std::process::id()));
    let log_file_path = test_directory_path.join("logs").join("koshi-log-test.log");
    let _ = std::fs::remove_dir_all(&test_directory_path);

    let disabled_logging_parameters = LoggingParameters {
        is_enabled: false,
        log_level: LogLevel::Error,
        log_format: LogFormat::Json,
        log_file_owner: LogFileOwner::Router,
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };
    initialize_tracing(disabled_logging_parameters.clone())
        .expect("disabled logging installs nothing");
    initialize_tracing(disabled_logging_parameters)
        .expect("a second disabled install also installs nothing");
    initialize_tracing_at_path(
        &log_file_path,
        LogLevel::Warning,
        LogFormat::Json,
        DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    )
    .expect("first install succeeds");

    // No line has been written yet: the file does not exist.
    assert!(
        !log_file_path.exists(),
        "the file must not exist before the first log line"
    );

    tracing::warn!(session_id = "sess-file", "file sink event");

    // A process has one global subscriber; the second install fails.
    let second_install_result = initialize_tracing_at_path(
        &log_file_path,
        LogLevel::Warning,
        LogFormat::Json,
        DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    );
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

// The log file writer, exercised directly (without a subscriber): its
// first write creates the `logs/` parent and the file, a second write appends,
// and flush is a no-op that reports success.
#[test]
fn log_file_writer_creates_parent_then_appends_each_write() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-writer.log");

    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
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
fn log_file_writer_recreates_a_logs_directory_removed_while_koshi_runs() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-regrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-writer.log");

    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
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
fn log_file_writer_recreates_a_log_file_removed_while_koshi_runs() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-refile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-writer.log");

    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
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

// Two writers on one path, as two processes writing one file: each write
// appends after the other's, and no write truncates or overwrites a line.
#[test]
fn two_log_file_writers_on_one_path_append_whole_lines_in_write_order() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-router.log");

    let mut first_log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };
    let mut second_log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };
    first_log_writer
        .write_all(b"first router line\n")
        .expect("the first write creates the file");
    second_log_writer
        .write_all(b"second router line\n")
        .expect("the second write appends");
    first_log_writer
        .write_all(b"third router line\n")
        .expect("the third write appends");

    assert_eq!(
        std::fs::read(&log_file_path).unwrap(),
        b"first router line\nsecond router line\nthird router line\n"
    );

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

/// `<log_file_path>.1`, where a moved log file goes.
fn build_previous_log_file_path(log_file_path: &Path) -> PathBuf {
    let mut previous_log_file_path = log_file_path.as_os_str().to_owned();
    previous_log_file_path.push(".1");
    PathBuf::from(previous_log_file_path)
}

/// An empty directory named `koshi-<test_name>-<process id>` in
/// [`std::env::temp_dir`], with no `logs` folder in it.
fn create_empty_test_directory(test_name: &str) -> PathBuf {
    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-{test_name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    std::fs::create_dir_all(&test_directory_path).expect("create the test directory");
    test_directory_path
}

/// Assert that the file at `log_file_path` holds exactly `expected_log_bytes`,
/// naming both lengths when it does not.
fn assert_log_file_bytes(log_file_path: &Path, expected_log_bytes: &[u8]) {
    let log_file_bytes = std::fs::read(log_file_path)
        .unwrap_or_else(|read_error| panic!("read {}: {read_error}", log_file_path.display()));
    assert!(
        log_file_bytes == expected_log_bytes,
        "{} holds {} bytes, expected {} bytes",
        log_file_path.display(),
        log_file_bytes.len(),
        expected_log_bytes.len()
    );
}

/// `byte_count` copies of `filler_byte`.
fn build_filler_bytes(filler_byte: u8, byte_count: u64) -> Vec<u8> {
    vec![filler_byte; usize::try_from(byte_count).expect("the size fits in memory")]
}

// For a writer at 1 MiB and one at 4 MiB: a file at exactly the writer's
// size stays. The line that takes it over moves it to `.1`, with that line in
// it, and the next line starts a new file.
#[test]
fn a_line_that_takes_the_log_file_over_the_writer_size_moves_it_to_dot_one() {
    use std::io::Write as _;

    for max_log_file_size_bytes in [
        LOWEST_MAX_LOG_FILE_SIZE_BYTES,
        DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    ] {
        let test_directory_path =
            create_empty_test_directory(&format!("rotate-over-size-{max_log_file_size_bytes}"));
        let log_file_path = test_directory_path
            .join("logs")
            .join("koshi-log-router.log");
        let previous_log_file_path = build_previous_log_file_path(&log_file_path);
        let mut log_writer = LogFileWriter {
            log_file_path: log_file_path.clone(),
            max_log_file_size_bytes,
        };

        let full_log_bytes = build_filler_bytes(b'a', max_log_file_size_bytes);
        log_writer
            .write_all(&full_log_bytes)
            .expect("the file fills to the size");
        assert!(
            !previous_log_file_path.exists(),
            "a file at {max_log_file_size_bytes} bytes is not moved"
        );
        assert_log_file_bytes(&log_file_path, &full_log_bytes);

        log_writer
            .write_all(b"over\n")
            .expect("the line over the size is written");
        let mut moved_log_bytes = full_log_bytes;
        moved_log_bytes.extend_from_slice(b"over\n");
        assert!(
            !log_file_path.exists(),
            "the file over {max_log_file_size_bytes} bytes is moved away"
        );
        assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

        log_writer
            .write_all(b"next\n")
            .expect("the next line starts a new file");
        assert_log_file_bytes(&log_file_path, b"next\n");
        assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

        let _ = std::fs::remove_dir_all(&test_directory_path);
    }
}

// A file that goes over the size again replaces the `.1` before it.
#[test]
fn a_second_move_replaces_the_dot_one_before_it() {
    use std::io::Write as _;

    let test_directory_path = create_empty_test_directory("rotate-replaces");
    let log_directory_path = test_directory_path.join("logs");
    std::fs::create_dir_all(&log_directory_path).expect("create the logs directory");
    let log_file_path = log_directory_path.join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    std::fs::write(&previous_log_file_path, b"older moved file\n").expect("write the older .1");
    std::fs::write(&log_file_path, b"next\n").expect("write the current file");
    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };

    let full_log_bytes = build_filler_bytes(b'b', DEFAULT_MAX_LOG_FILE_SIZE_BYTES);
    log_writer
        .write_all(&full_log_bytes)
        .expect("the line over the size is written");

    let mut moved_log_bytes = b"next\n".to_vec();
    moved_log_bytes.extend_from_slice(&full_log_bytes);
    assert!(!log_file_path.exists(), "the full file is moved away");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// While another writer holds `rotate.lock`, a file over the size stays where
// it is. Once the lock is free, the next line moves it.
#[test]
fn a_file_over_the_size_stays_while_another_writer_holds_the_rotate_lock() {
    use std::io::Write as _;

    let test_directory_path = create_empty_test_directory("rotate-locked");
    let log_directory_path = test_directory_path.join("logs");
    std::fs::create_dir_all(&log_directory_path).expect("create the logs directory");
    let log_file_path = log_directory_path.join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    let held_rotate_lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(log_directory_path.join("rotate.lock"))
        .expect("open the rotate lock");
    held_rotate_lock_file
        .try_lock()
        .expect("the test takes the rotate lock");
    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };

    let over_size_log_bytes = build_filler_bytes(b'c', DEFAULT_MAX_LOG_FILE_SIZE_BYTES + 1);
    log_writer
        .write_all(&over_size_log_bytes)
        .expect("the line over the size is written");
    assert!(
        !previous_log_file_path.exists(),
        "a locked rotation moves nothing"
    );
    assert_log_file_bytes(&log_file_path, &over_size_log_bytes);

    held_rotate_lock_file
        .unlock()
        .expect("the test lets the rotate lock go");
    drop(held_rotate_lock_file);
    log_writer
        .write_all(b"x\n")
        .expect("the next line is written");
    let mut moved_log_bytes = over_size_log_bytes;
    moved_log_bytes.extend_from_slice(b"x\n");
    assert!(!log_file_path.exists(), "the full file is moved away");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// Two writers on one file, at 4 MiB and at 1 MiB: a line the 4 MiB writer
// writes past 1 MiB leaves the file, and the next line the 1 MiB writer
// writes moves it.
#[test]
fn a_file_two_writers_share_moves_at_the_smaller_size() {
    use std::io::Write as _;

    let test_directory_path = create_empty_test_directory("rotate-two-sizes");
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    let mut larger_log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };
    let mut smaller_log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: LOWEST_MAX_LOG_FILE_SIZE_BYTES,
    };

    let over_smaller_size_log_bytes = build_filler_bytes(b'g', LOWEST_MAX_LOG_FILE_SIZE_BYTES + 1);
    larger_log_writer
        .write_all(&over_smaller_size_log_bytes)
        .expect("the line past 1 MiB is written");
    assert!(
        !previous_log_file_path.exists(),
        "the 4 MiB writer moves no file under 4 MiB"
    );
    assert_log_file_bytes(&log_file_path, &over_smaller_size_log_bytes);

    smaller_log_writer
        .write_all(b"z\n")
        .expect("the 1 MiB writer's line is written");
    let mut moved_log_bytes = over_smaller_size_log_bytes;
    moved_log_bytes.extend_from_slice(b"z\n");
    assert!(!log_file_path.exists(), "the 1 MiB writer moves the file");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A move that fails, here because a directory holds the `.1` name, leaves the
// file where it is and fails no line. Once the `.1` name is free, the next
// line moves the file.
#[test]
fn a_move_that_fails_leaves_the_file_and_the_next_line_moves_it() {
    use std::io::Write as _;

    let test_directory_path = create_empty_test_directory("rotate-fails");
    let log_directory_path = test_directory_path.join("logs");
    std::fs::create_dir_all(&log_directory_path).expect("create the logs directory");
    let log_file_path = log_directory_path.join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    std::fs::create_dir(&previous_log_file_path).expect("create a directory at the .1 name");
    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: LOWEST_MAX_LOG_FILE_SIZE_BYTES,
    };

    let over_size_log_bytes = build_filler_bytes(b'f', LOWEST_MAX_LOG_FILE_SIZE_BYTES + 1);
    log_writer
        .write_all(&over_size_log_bytes)
        .expect("a failed move does not fail the line");
    assert_log_file_bytes(&log_file_path, &over_size_log_bytes);
    assert!(
        previous_log_file_path.is_dir(),
        "the directory at the .1 name stays"
    );

    std::fs::remove_dir(&previous_log_file_path).expect("free the .1 name");
    log_writer
        .write_all(b"y\n")
        .expect("the next line is written");
    let mut moved_log_bytes = over_size_log_bytes;
    moved_log_bytes.extend_from_slice(b"y\n");
    assert!(!log_file_path.exists(), "the full file is moved away");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// Another writer already moved the file: the file at the path is the new,
// small one, and a rotation leaves it and the `.1` as they are. A file that is
// gone stays gone.
#[test]
fn a_rotation_reads_the_size_at_the_path_again_and_leaves_a_new_file_alone() {
    let test_directory_path = create_empty_test_directory("rotate-recheck");
    let log_directory_path = test_directory_path.join("logs");
    std::fs::create_dir_all(&log_directory_path).expect("create the logs directory");
    let log_file_path = log_directory_path.join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    let moved_log_bytes = build_filler_bytes(b'd', DEFAULT_MAX_LOG_FILE_SIZE_BYTES + 1);
    std::fs::write(&previous_log_file_path, &moved_log_bytes).expect("write the moved file");
    std::fs::write(&log_file_path, b"fresh\n").expect("write the new file");

    rotate_log_file(&log_file_path, DEFAULT_MAX_LOG_FILE_SIZE_BYTES)
        .expect("a small file needs no move");
    assert_log_file_bytes(&log_file_path, b"fresh\n");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

    std::fs::remove_file(&log_file_path).expect("remove the new file");
    rotate_log_file(&log_file_path, DEFAULT_MAX_LOG_FILE_SIZE_BYTES)
        .expect("a file that is gone needs no move");
    assert!(!log_file_path.exists(), "a rotation creates no file");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A handle opened before the move writes on into the moved file: its line
// lands in `.1`, and the next line starts a new file.
#[test]
fn a_line_through_a_handle_opened_before_the_move_lands_in_dot_one() {
    use std::io::Write as _;

    let test_directory_path = create_empty_test_directory("rotate-held-handle");
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };
    log_writer
        .write_all(b"first\n")
        .expect("the first line creates the file");
    let mut held_log_file = OpenOptions::new()
        .append(true)
        .open(&log_file_path)
        .expect("another writer opens the file");

    let full_log_bytes = build_filler_bytes(b'e', DEFAULT_MAX_LOG_FILE_SIZE_BYTES);
    log_writer
        .write_all(&full_log_bytes)
        .expect("the line over the size is written");
    held_log_file
        .write_all(b"late\n")
        .expect("the held handle writes after the move");
    drop(held_log_file);
    log_writer
        .write_all(b"after\n")
        .expect("the next line starts a new file");

    let mut moved_log_bytes = b"first\n".to_vec();
    moved_log_bytes.extend_from_slice(&full_log_bytes);
    moved_log_bytes.extend_from_slice(b"late\n");
    assert_log_file_bytes(&previous_log_file_path, &moved_log_bytes);
    assert_log_file_bytes(&log_file_path, b"after\n");

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// Four writers at 1 MiB, each with its own handle per line as separate
// processes have, write 1.25 MiB of lines across one move. Every line is in
// `.1` or the new file, whole and once.
#[test]
fn concurrent_log_file_writers_lose_no_line_across_a_move() {
    use std::io::Write as _;

    const WRITER_THREAD_COUNT: usize = 4;
    const LINE_COUNT_PER_WRITER: usize = 40;
    const LINE_BYTE_COUNT: usize = 8 * 1024;

    let test_directory_path = create_empty_test_directory("rotate-concurrent");
    let log_file_path = test_directory_path
        .join("logs")
        .join("koshi-log-router.log");
    let previous_log_file_path = build_previous_log_file_path(&log_file_path);
    let build_log_line = |writer_index: usize, line_index: usize| {
        let mut log_line = format!("{writer_index:02}-{line_index:03}-");
        log_line.push_str(&"x".repeat(LINE_BYTE_COUNT - 1 - log_line.len()));
        log_line
    };

    let writer_threads: Vec<std::thread::JoinHandle<()>> = (0..WRITER_THREAD_COUNT)
        .map(|writer_index| {
            let mut log_writer = LogFileWriter {
                log_file_path: log_file_path.clone(),
                max_log_file_size_bytes: LOWEST_MAX_LOG_FILE_SIZE_BYTES,
            };
            std::thread::spawn(move || {
                for line_index in 0..LINE_COUNT_PER_WRITER {
                    let log_line = build_log_line(writer_index, line_index);
                    log_writer
                        .write_all(format!("{log_line}\n").as_bytes())
                        .expect("every line is written");
                }
            })
        })
        .collect();
    for writer_thread in writer_threads {
        writer_thread.join().expect("the writer runs to completion");
    }

    let moved_log_text =
        std::fs::read_to_string(&previous_log_file_path).expect("one move happened");
    // A move that comes after the last line leaves no new file.
    let current_log_text = match std::fs::read_to_string(&log_file_path) {
        Ok(current_log_text) => current_log_text,
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(read_error) => panic!("the new file cannot be read: {read_error}"),
    };
    let mut written_log_lines: Vec<&str> = moved_log_text
        .lines()
        .chain(current_log_text.lines())
        .collect();
    written_log_lines.sort_unstable();
    let mut expected_log_lines: Vec<String> = (0..WRITER_THREAD_COUNT)
        .flat_map(|writer_index| {
            (0..LINE_COUNT_PER_WRITER)
                .map(move |line_index| build_log_line(writer_index, line_index))
        })
        .collect();
    expected_log_lines.sort_unstable();
    assert_eq!(written_log_lines.len(), expected_log_lines.len());
    assert!(
        written_log_lines == expected_log_lines,
        "the lines in .1 and the new file differ from the lines written"
    );
    assert!(
        u64::try_from(current_log_text.len()).unwrap() <= LOWEST_MAX_LOG_FILE_SIZE_BYTES,
        "the new file stays at most the size"
    );

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

/// Create the empty file `log_file_name` in `log_directory_path`, and set its
/// modification time to `written_at`.
fn write_log_file_at(log_directory_path: &Path, log_file_name: &str, written_at: SystemTime) {
    let log_file =
        std::fs::File::create(log_directory_path.join(log_file_name)).expect("create the log file");
    log_file
        .set_modified(written_at)
        .expect("set the log file's modification time");
}

/// Every name in `directory_path`, sorted.
fn list_sorted_file_names(directory_path: &Path) -> Vec<String> {
    let mut file_names: Vec<String> = std::fs::read_dir(directory_path)
        .expect("read the directory")
        .map(|directory_entry| {
            directory_entry
                .expect("read a directory entry")
                .file_name()
                .into_string()
                .expect("a UTF-8 name")
        })
        .collect();
    file_names.sort_unstable();
    file_names
}

// 23 other sessions: the 20 written last stay, the 3 written first go, with
// their `.1`. A session's newest file dates it: an old `.log` with a new `.1`
// stays. The caller's own files, the router's files, and every name that is
// not exactly `koshi-log-<uuid>.log` or `.log.1` stay, however old.
#[test]
fn deleting_stale_session_logs_keeps_the_newest_twenty_own_router_and_other_names() {
    let test_directory_path = create_empty_test_directory("prune-keep-twenty");
    let log_directory_path = test_directory_path.join("logs");
    std::fs::create_dir_all(&log_directory_path).expect("create the logs directory");
    let first_written_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let written_seconds_after =
        |second_count: u64| first_written_at + std::time::Duration::from_secs(second_count);
    let own_session_id = SessionId::new();
    let other_session_ids: Vec<SessionId> = (0..23).map(|_| SessionId::new()).collect();
    let format_session_log_file_name =
        |session_id: SessionId| format_log_file_name(LogFileOwner::Session { session_id });

    for (session_index, session_id) in other_session_ids.iter().enumerate() {
        let session_index = u64::try_from(session_index).unwrap();
        write_log_file_at(
            &log_directory_path,
            &format_session_log_file_name(*session_id),
            written_seconds_after(10 * (session_index + 1)),
        );
    }
    let newest_by_dot_one_session_log_file_name =
        format_session_log_file_name(other_session_ids[0]);
    write_log_file_at(
        &log_directory_path,
        &format!("{newest_by_dot_one_session_log_file_name}.1"),
        written_seconds_after(1000),
    );
    let older_dot_one_session_log_file_name = format_session_log_file_name(other_session_ids[1]);
    write_log_file_at(
        &log_directory_path,
        &format!("{older_dot_one_session_log_file_name}.1"),
        written_seconds_after(1),
    );
    let own_session_log_file_name = format_session_log_file_name(own_session_id);
    write_log_file_at(
        &log_directory_path,
        &own_session_log_file_name,
        first_written_at,
    );
    write_log_file_at(
        &log_directory_path,
        &format!("{own_session_log_file_name}.1"),
        first_written_at,
    );
    let unmatched_session_uuid_text = SessionId::new().get_uuid().to_string();
    let unmatched_file_names = [
        "koshi-log-router.log".to_string(),
        "koshi-log-router.log.1".to_string(),
        "rotate.lock".to_string(),
        "notes.txt".to_string(),
        "koshi-log-not-a-uuid.log".to_string(),
        format!(
            "koshi-log-{}.log",
            unmatched_session_uuid_text.to_uppercase()
        ),
        format!("koshi-log-{unmatched_session_uuid_text}.log.2"),
        format!("koshi-log-{unmatched_session_uuid_text}.txt"),
    ];
    for unmatched_file_name in &unmatched_file_names {
        write_log_file_at(&log_directory_path, unmatched_file_name, first_written_at);
    }
    let directory_with_log_file_name = format_session_log_file_name(SessionId::new());
    std::fs::create_dir(log_directory_path.join(&directory_with_log_file_name))
        .expect("create a directory with a log file's name");
    let mut expected_file_names = list_sorted_file_names(&log_directory_path);
    let deleted_file_names = [
        format_session_log_file_name(other_session_ids[1]),
        format!("{older_dot_one_session_log_file_name}.1"),
        format_session_log_file_name(other_session_ids[2]),
        format_session_log_file_name(other_session_ids[3]),
    ];
    expected_file_names.retain(|file_name| !deleted_file_names.contains(file_name));

    delete_stale_session_log_files_in_directory(&log_directory_path, own_session_id)
        .expect("every stale file is deleted");

    assert_eq!(
        list_sorted_file_names(&log_directory_path),
        expected_file_names
    );
    assert_eq!(expected_file_names.len(), 36 - 4);

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

#[test]
fn deleting_stale_session_logs_with_no_log_directory_creates_nothing() {
    let test_directory_path = create_empty_test_directory("prune-no-directory");
    let log_directory_path = test_directory_path.join("logs");

    delete_stale_session_log_files_in_directory(&log_directory_path, SessionId::new())
        .expect("a missing directory has nothing to delete");

    assert!(!log_directory_path.exists(), "no directory is created");

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A regular file where the log directory must be: the read fails and the
// error names that path.
#[test]
fn deleting_stale_session_logs_names_a_log_directory_that_cannot_be_read() {
    let test_directory_path = create_empty_test_directory("prune-unreadable");
    let log_directory_path = test_directory_path.join("logs");
    std::fs::write(&log_directory_path, b"not a directory").expect("write the blocking file");
    let directory_read_error_kind = std::fs::read_dir(&log_directory_path)
        .expect_err("a regular file is no directory")
        .kind();

    let session_log_delete_error =
        delete_stale_session_log_files_in_directory(&log_directory_path, SessionId::new())
            .expect_err("the directory cannot be read");

    assert_eq!(session_log_delete_error.log_path, log_directory_path);
    assert_eq!(
        session_log_delete_error.io_error.kind(),
        directory_read_error_kind
    );

    let _ = std::fs::remove_dir_all(&test_directory_path);
}

// A regular file where the `logs/` directory must go: the open fails, the
// recreate fails with `AlreadyExists`, and the write reports that error and
// creates nothing. With the file gone, the next write succeeds.
#[test]
fn log_file_writer_reports_the_error_when_its_parent_is_a_regular_file() {
    use std::io::Write as _;

    let test_directory_path =
        std::env::temp_dir().join(format!("koshi-writer-blocked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&test_directory_path);
    std::fs::create_dir_all(&test_directory_path).expect("create the test directory");
    let logs_directory_blocking_file_path = test_directory_path.join("logs");
    std::fs::write(&logs_directory_blocking_file_path, b"not a directory")
        .expect("create the file that blocks the logs directory");
    let log_file_path = logs_directory_blocking_file_path.join("koshi-log-writer.log");

    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
    };
    let write_error = log_writer
        .write(b"line one\n")
        .expect_err("a file where the directory must go fails the write");
    assert_eq!(write_error.kind(), io::ErrorKind::AlreadyExists);
    assert!(
        !log_file_path.exists(),
        "nothing is created under a regular file"
    );

    std::fs::remove_file(&logs_directory_blocking_file_path)
        .expect("remove the file that blocks the logs directory");
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

    let mut log_writer = LogFileWriter {
        log_file_path: log_file_path.clone(),
        max_log_file_size_bytes: DEFAULT_MAX_LOG_FILE_SIZE_BYTES,
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
// `contents`, `lines` and the next write recover the captured output.
#[test]
fn captured_logs_recover_after_a_writer_thread_panics_while_holding_the_lock() {
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
