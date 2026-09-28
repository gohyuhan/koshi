//! Tests for the process-wide beta-feature gate and the warning a blocked
//! entry point writes.
//!
//! The gate is one process-wide flag, so a test that asserts its untouched
//! initial value cannot live beside a test that sets it. That assertion is
//! `tests/starts_closed.rs`, a test binary of its own.

use super::*;

/// One test walks every state of the gate, which is one process-wide flag.
#[test]
fn beta_feature_gate_returns_the_latest_setting() {
    set_beta_features_allowed(true);
    assert!(should_allow_beta_features());

    set_beta_features_allowed(true);
    assert!(should_allow_beta_features());

    set_beta_features_allowed(false);
    assert!(!should_allow_beta_features());

    set_beta_features_allowed(false);
    assert!(!should_allow_beta_features());

    let should_allow_beta_features_in_thread = std::thread::spawn(should_allow_beta_features)
        .join()
        .unwrap();
    assert!(!should_allow_beta_features_in_thread);

    set_beta_features_allowed(true);
    let should_allow_beta_features_in_thread = std::thread::spawn(should_allow_beta_features)
        .join()
        .unwrap();
    assert!(should_allow_beta_features_in_thread);

    set_beta_features_allowed(false);
}

/// `log_blocked_feature_warning` itself logs on every call; the once-per-site limit lives in
/// the generated code, not here.
#[test]
fn log_blocked_emits_one_warn_record_per_call_with_the_function_field() {
    let (_logging_guard, captured_log_output) = koshi_observability::logging::with_test_writer();

    log_blocked_feature_warning("attach");
    log_blocked_feature_warning("attach");

    let log_lines = captured_log_output.lines();
    assert_eq!(log_lines.len(), 2, "{log_lines:?}");
    for log_line in &log_lines {
        assert!(log_line.contains(r#""level":"WARN""#), "{log_line}");
        assert!(log_line.contains(r#""target":"koshi_beta""#), "{log_line}");
        assert!(log_line.contains(r#""function":"attach""#), "{log_line}");
        assert!(
            log_line.contains(
                r#""message":"`attach` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
            ),
            "{log_line}"
        );
    }
}

/// The function name goes into the message unchanged: an empty name gives empty
/// backticks and a name holding backticks or braces is not escaped.
#[test]
fn log_blocked_keeps_the_function_name_byte_for_byte() {
    let (_logging_guard, captured_log_output) = koshi_observability::logging::with_test_writer();

    log_blocked_feature_warning("");
    log_blocked_feature_warning("a`b{c}");

    let log_lines = captured_log_output.lines();
    assert_eq!(log_lines.len(), 2, "{log_lines:?}");
    assert!(log_lines[0].contains(r#""function":"""#), "{log_lines:?}");
    assert!(
        log_lines[1].contains(r#""function":"a`b{c}""#),
        "{log_lines:?}"
    );
    assert!(
        log_lines[0].contains(
            r#""message":"`` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
        ),
        "{log_lines:?}"
    );
    assert!(
        log_lines[1].contains(
            r#""message":"`a`b{c}` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
        ),
        "{log_lines:?}"
    );
}
