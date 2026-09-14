//! Tests for the process-wide beta-feature gate and the warning a blocked
//! entry point writes.
//!
//! The gate is one process-wide flag, so a test that asserts its untouched
//! initial value cannot live beside a test that sets it. That assertion is
//! `tests/starts_closed.rs`, a test binary of its own.

use super::*;

/// One test walks every state of the gate, which is one process-wide flag.
#[test]
fn the_gate_follows_what_it_is_set_to() {
    set_beta_features_allowed(true);
    assert!(are_beta_features_allowed());

    set_beta_features_allowed(true);
    assert!(are_beta_features_allowed());

    set_beta_features_allowed(false);
    assert!(!are_beta_features_allowed());

    set_beta_features_allowed(false);
    assert!(!are_beta_features_allowed());

    let thread_gate_status = std::thread::spawn(are_beta_features_allowed)
        .join()
        .unwrap();
    assert!(!thread_gate_status);

    set_beta_features_allowed(true);
    let thread_gate_status = std::thread::spawn(are_beta_features_allowed)
        .join()
        .unwrap();
    assert!(thread_gate_status);

    set_beta_features_allowed(false);
}

/// `log_blocked_feature_warning` itself logs on every call; the once-per-site limit lives in
/// the generated code, not here.
#[test]
fn log_blocked_emits_one_warn_record_per_call_with_the_function_field() {
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    log_blocked_feature_warning("attach");
    log_blocked_feature_warning("attach");

    let log_lines = logs.lines();
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
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    log_blocked_feature_warning("");
    log_blocked_feature_warning("a`b{c}");

    let log_lines = logs.lines();
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
