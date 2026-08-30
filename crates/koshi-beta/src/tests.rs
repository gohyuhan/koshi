//! Tests for the process-wide beta-feature gate, the blocked message, and the
//! warning that carries it.

use super::*;

/// One test walks every state of the gate, which is one process-wide flag.
#[test]
fn the_gate_starts_closed_and_follows_what_it_is_set_to() {
    assert!(!allowed());

    set_allowed(true);
    assert!(allowed());

    set_allowed(true);
    assert!(allowed());

    set_allowed(false);
    assert!(!allowed());

    set_allowed(false);
    assert!(!allowed());
}

#[test]
fn blocked_message_names_the_function_and_the_line_to_add() {
    assert_eq!(
        blocked_message("attach"),
        "`attach` is a beta feature and did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    );
}

/// The name goes into the message unchanged: an empty name gives empty
/// backticks and a name with backticks or braces is not escaped.
#[test]
fn blocked_message_keeps_the_name_byte_for_byte() {
    assert_eq!(
        blocked_message(""),
        "`` is a beta feature and did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    );
    assert_eq!(
        blocked_message("a`b{c}"),
        "`a`b{c}` is a beta feature and did nothing; add a top-level \
         `allow-beta-features #true` line to koshi.kdl to run it"
    );
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
