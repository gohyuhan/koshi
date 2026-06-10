use super::*;

// A sample event must carry the canonical join fields, and any env-derived value
// must arrive already scrubbed — the token must never appear in raw output.
#[test]
fn sample_event_has_canonical_fields_and_redacts_token() {
    let (_guard, logs) = with_test_writer();

    let mut env = BTreeMap::new();
    env.insert("TILE_CONTEXT_TOKEN".to_string(), "super-secret".to_string());
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
    // process-global `TILE_LOG_FORMAT`, which would be race-prone under parallel
    // tests.
    assert_eq!(LogFormat::parse(Some("json")), LogFormat::Json);
    assert_eq!(LogFormat::parse(Some("pretty")), LogFormat::Pretty);
    assert_eq!(LogFormat::parse(Some("anything-else")), LogFormat::Pretty);
    assert_eq!(LogFormat::parse(None), LogFormat::Pretty);
}

#[test]
fn init_tracing_installs_once() {
    // First install succeeds; a second install fails because the process has a
    // single global subscriber.
    let first = init_tracing(TracingOptions {
        format: LogFormat::Json,
        filter: "info".to_string(),
    });
    assert!(first.is_ok());

    let second = init_tracing(TracingOptions {
        format: LogFormat::Json,
        filter: "info".to_string(),
    });
    assert!(matches!(second, Err(TracingError::AlreadyInitialized)));
}

#[test]
fn init_tracing_rejects_bad_filter() {
    let err = init_tracing(TracingOptions {
        format: LogFormat::Json,
        filter: "this is not a valid==filter".to_string(),
    });
    assert!(matches!(err, Err(TracingError::Filter(_))));
}
