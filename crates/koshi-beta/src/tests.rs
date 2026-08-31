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
    set_allowed(true);
    assert!(allowed());

    set_allowed(true);
    assert!(allowed());

    set_allowed(false);
    assert!(!allowed());

    set_allowed(false);
    assert!(!allowed());
}

/// `log_blocked` itself logs on every call; the once-per-site limit lives in
/// the generated code, not here.
#[test]
fn log_blocked_emits_one_warn_record_per_call_with_the_function_field() {
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    log_blocked("attach");
    log_blocked("attach");

    let lines = logs.lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    for line in &lines {
        assert!(line.contains(r#""level":"WARN""#), "{line}");
        assert!(line.contains(r#""target":"koshi_beta""#), "{line}");
        assert!(line.contains(r#""function":"attach""#), "{line}");
        assert!(
            line.contains(
                r#""message":"`attach` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
            ),
            "{line}"
        );
    }
}

/// The name goes into the message unchanged: an empty name gives empty
/// backticks and a name holding backticks or braces is not escaped.
#[test]
fn log_blocked_keeps_the_function_name_byte_for_byte() {
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    log_blocked("");
    log_blocked("a`b{c}");

    let lines = logs.lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].contains(
            r#""message":"`` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
        ),
        "{lines:?}"
    );
    assert!(
        lines[1].contains(
            r#""message":"`a`b{c}` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
        ),
        "{lines:?}"
    );
}
