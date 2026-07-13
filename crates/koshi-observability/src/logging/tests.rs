//! Tests for the tracing subscriber bootstrap and redaction machinery.
//!
//! Coverage includes format parsing, environment-variable option mapping, filter validation,
//! subscriber installation (global and thread-local), log file creation and
//! rotation, and environment variable redaction.

use super::*;

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
        event_id = "evt-1",
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
fn log_format_parses_value() {
    // Test the pure mapping directly so the suite never reads or writes the
    // process-global `KOSHI_LOG_FORMAT`, which would be race-prone under parallel
    // tests.
    assert_eq!(LogFormat::parse(Some("json")), LogFormat::Json);
    assert_eq!(LogFormat::parse(Some("pretty")), LogFormat::Pretty);
    assert_eq!(LogFormat::parse(Some("anything-else")), LogFormat::Pretty);
    assert_eq!(LogFormat::parse(None), LogFormat::Pretty);
}

#[test]
fn init_tracing_writes_to_file_and_installs_once() {
    // This is the only test that completes a global install, so its calls are
    // deterministic regardless of parallelism: no other test races the global
    // slot. `init_tracing_rejects_bad_filter` returns before `try_init`, and the
    // `with_test_writer` tests use a thread-local subscriber.
    let dir = std::env::temp_dir().join(format!("koshi-log-test-{}", std::process::id()));
    let path = dir.join("koshi.log");
    let _ = std::fs::remove_dir_all(&dir);

    let guard = init_tracing(TracingOptions {
        format: LogFormat::Json,
        filter: "info".to_string(),
        destination: LogDestination::File(path.clone()),
        max_log_files: 7,
    })
    .expect("first install succeeds");

    tracing::info!(session_id = "sess-file", "file sink event");

    // A second install fails: a process has a single global subscriber.
    let second = init_tracing(TracingOptions {
        format: LogFormat::Json,
        filter: "info".to_string(),
        destination: LogDestination::Stderr,
        max_log_files: 7,
    });
    assert!(matches!(second, Err(TracingError::AlreadyInitialized)));

    // Dropping the guard flushes the non-blocking writer to disk.
    drop(guard);

    // Daily rotation appends a date suffix to the prefix, so read whichever
    // `koshi.log*` file the appender created.
    let contents = std::fs::read_dir(&dir)
        .expect("log dir exists")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("koshi.log"))
        .map(|entry| std::fs::read_to_string(entry.path()).expect("log file readable"))
        .expect("a rotated log file was written");
    assert!(contents.contains("session_id"), "missing canonical field");
    assert!(contents.contains("file sink event"), "missing log message");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_tracing_rejects_bad_filter() {
    let err = init_tracing(TracingOptions {
        format: LogFormat::Json,
        filter: "this is not a valid==filter".to_string(),
        destination: LogDestination::Stderr,
        max_log_files: 7,
    });
    assert!(matches!(err, Err(TracingError::Filter(_))));
}

#[test]
fn init_tracing_disabled_is_noop() {
    // Disabled installs no global subscriber, so it never conflicts with the one
    // global install and is safely idempotent — both calls succeed regardless of
    // test order.
    let opts = || TracingOptions {
        format: LogFormat::Json,
        filter: "info".to_string(),
        destination: LogDestination::Disabled,
        max_log_files: 7,
    };
    assert!(init_tracing(opts()).is_ok());
    assert!(init_tracing(opts()).is_ok());
}

#[test]
fn default_log_path_is_the_logs_folder_of_the_state_dir() {
    let path = default_log_path();
    // The state dir's own tail differs per OS (`koshi` on Linux/macOS,
    // `koshi\data` on Windows), so pin the `logs/koshi.log` tail plus the
    // state-dir prefix instead of a full literal path.
    assert!(
        path.ends_with("logs/koshi.log"),
        "unexpected default path: {}",
        path.display()
    );
    if let Some(state) = koshi_paths::state_dir() {
        assert_eq!(path, state.join("logs").join("koshi.log"));
    }
}

#[test]
fn no_filter_disables_logging() {
    let opts = TracingOptions::from_filter(None);
    assert_eq!(opts.destination, LogDestination::Disabled);
    assert_eq!(opts.filter, "info");
    assert_eq!(opts.max_log_files, DEFAULT_MAX_LOG_FILES);
}

#[test]
fn a_filter_enables_the_standard_log_file() {
    let opts = TracingOptions::from_filter(Some("koshi=debug".to_string()));
    assert_eq!(opts.destination, LogDestination::File(default_log_path()));
    assert_eq!(opts.filter, "koshi=debug");
    assert_eq!(opts.max_log_files, DEFAULT_MAX_LOG_FILES);
}

// `from_env` pre-filters an empty `KOSHI_LOG` to `None` before calling this,
// but `from_filter` is a public, directly-callable mapping in its own right:
// called with `Some(String::new())` it must NOT fall back to `Disabled`
// (only `None` does), and must keep the empty string rather than substituting
// the "info" default (only a missing filter substitutes that).
#[test]
fn from_filter_with_empty_string_keeps_empty_filter_and_enables_file_destination() {
    let opts = TracingOptions::from_filter(Some(String::new()));
    assert_eq!(opts.destination, LogDestination::File(default_log_path()));
    assert_eq!(opts.filter, "");
    assert_eq!(opts.max_log_files, DEFAULT_MAX_LOG_FILES);
}

#[test]
fn log_format_parse_is_case_sensitive_and_rejects_empty_string() {
    assert_eq!(LogFormat::parse(Some("JSON")), LogFormat::Pretty);
    assert_eq!(LogFormat::parse(Some("")), LogFormat::Pretty);
}

#[test]
fn redacted_env_field_of_empty_map_is_empty_string() {
    let env: BTreeMap<String, String> = BTreeMap::new();
    assert_eq!(redacted_env_field(&env), "");
}

// Boundary: a zero retention count is a legal `usize` input. Called directly
// (bypassing `init_tracing`'s single-shot global subscriber, which only one
// test in this file may claim), this proves sink construction still succeeds
// at that boundary.
#[test]
fn file_writer_accepts_zero_max_log_files_without_erroring() {
    let dir = std::env::temp_dir().join(format!("koshi-log-clamp-test-{}", std::process::id()));
    let path = dir.join("koshi.log");
    let _ = std::fs::remove_dir_all(&dir);

    let result = file_writer(&path, 0);
    assert!(
        result.is_ok(),
        "max_log_files=0 must clamp to 1, not fail sink construction"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_writer_with_no_file_name_returns_sink_error() {
    // `Path::file_name()` returns `None` when a path terminates in `..`,
    // regardless of platform; this exercises the `ok_or_else` error arm
    // before any directory is created.
    let path = PathBuf::from("..");
    match file_writer(&path, 7) {
        Err(TracingError::Sink(msg)) => {
            assert_eq!(
                msg,
                format!("log path has no file name: {}", path.display())
            );
        }
        other => panic!("expected Sink error, got {other:?}"),
    }
}
