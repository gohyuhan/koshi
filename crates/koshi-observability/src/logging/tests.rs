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
fn default_log_path_is_under_state_dir() {
    let path = default_log_path();
    assert!(
        path.ends_with("koshi/koshi.log"),
        "unexpected default path: {}",
        path.display()
    );
}
