//! Test suite: verify that diagnostic messages clearly report what failed,
//! where it failed, and how to fix it.

use super::*;
use miette::Diagnostic;

// Snapshot each diagnostic variant by its message, stable code, and help line —
// the three parts a user reads: what/where failed and how to fix it.

/// A diagnostic's stable error code, or `None` when it has none.
fn code_of(d: &impl Diagnostic) -> Option<String> {
    d.code().map(|c| c.to_string())
}

/// A diagnostic's help message, or `None` when it has none.
fn help_of(d: &impl Diagnostic) -> Option<String> {
    d.help().map(|h| h.to_string())
}

#[test]
fn config_diagnostic_reports_path_key_and_help() {
    let d = config_diagnostic(
        "~/.config/koshi/config.kdl",
        "layout",
        "unknown value `grid`",
        "use one of: tiled, stacked",
    );
    assert_eq!(
        d.to_string(),
        "invalid config at ~/.config/koshi/config.kdl: key `layout` unknown value `grid`"
    );
    assert_eq!(code_of(&d).as_deref(), Some("koshi::config"));
    assert_eq!(help_of(&d).as_deref(), Some("use one of: tiled, stacked"));
}

#[test]
fn config_diagnostic_with_empty_parts_keeps_the_template_around_them() {
    let d = config_diagnostic("", "", "", "");
    assert_eq!(d.to_string(), "invalid config at : key `` ");
    assert_eq!(help_of(&d).as_deref(), Some(""));
}

// Values land in the message and the help exactly as given: `{}` and `{key}`
// inside a value are text, not format placeholders.
#[test]
fn config_diagnostic_renders_braces_in_its_values_literally() {
    let d = config_diagnostic("{path}", "{0}", "unknown value `{}`", "use {a} or {b}");
    assert_eq!(
        d.to_string(),
        "invalid config at {path}: key `{0}` unknown value `{}`"
    );
    assert_eq!(help_of(&d).as_deref(), Some("use {a} or {b}"));
}

#[test]
fn command_reject_diagnostic_renders_braces_in_its_context_literally() {
    let d = command_reject_diagnostic(RejectReason::TargetGone, "close pane {id}");
    assert_eq!(
        d.to_string(),
        "cannot close pane {id}: target no longer exists"
    );
}

#[test]
fn command_reject_diagnostic_reports_context_reason_and_help() {
    let d = command_reject_diagnostic(RejectReason::TargetNotFound, "focus pane");
    assert_eq!(d.to_string(), "cannot focus pane: no target matched");
    assert_eq!(code_of(&d).as_deref(), Some("koshi::command"));
    assert_eq!(
        help_of(&d).as_deref(),
        Some("check the target id and try again")
    );
}

#[test]
fn command_reject_diagnostic_with_an_empty_context_keeps_the_template() {
    let d = command_reject_diagnostic(RejectReason::InvalidState, "");
    assert_eq!(d.to_string(), "cannot : invalid in the current state");
}

#[test]
fn resize_min_size_diagnostic_reports_direction_and_sizes() {
    let d = resize_min_size_diagnostic(Direction::Left, 3, 2);
    assert_eq!(
        d.to_string(),
        "cannot resize pane left: would drop a pane below minimum size 2 (current 3)"
    );
    assert_eq!(code_of(&d).as_deref(), Some("koshi::resize"));
    assert_eq!(
        help_of(&d).as_deref(),
        Some("free space by resizing or closing a neighboring pane")
    );
}

#[test]
fn resize_min_size_diagnostic_renders_the_ends_of_the_u16_range() {
    let d = resize_min_size_diagnostic(Direction::Up, 0, u16::MAX);
    assert_eq!(
        d.to_string(),
        "cannot resize pane up: would drop a pane below minimum size 65535 (current 0)"
    );
}

#[test]
fn reject_reason_converts_to_report_without_context() {
    let report = reject_report(RejectReason::TargetNotFound);
    assert_eq!(
        report.to_string(),
        "cannot complete command: no target matched"
    );
}

#[test]
fn reject_report_carries_the_command_code_and_the_reasons_help() {
    let report = reject_report(RejectReason::Unauthorized);
    assert_eq!(
        report.to_string(),
        "cannot complete command: command not permitted"
    );
    assert_eq!(
        report.code().map(|c| c.to_string()).as_deref(),
        Some("koshi::command")
    );
    assert_eq!(
        report.help().map(|h| h.to_string()).as_deref(),
        Some("this command requires additional capability")
    );
}

#[test]
fn every_reject_reason_has_distinct_help() {
    use std::collections::BTreeSet;
    let reasons = [
        RejectReason::TargetGone,
        RejectReason::TargetAmbiguous,
        RejectReason::TargetNotFound,
        RejectReason::SourceClientStale,
        RejectReason::Unauthorized,
        RejectReason::InvalidState,
        RejectReason::MinSize,
    ];
    let helps: BTreeSet<&str> = reasons.iter().map(|r| reject_help(*r)).collect();
    assert_eq!(helps.len(), reasons.len(), "each reason needs unique help");
}

// Distinctness alone does not catch two reasons' help text being swapped with
// each other; pin the exact message and help pair per reason.
#[test]
fn command_reject_diagnostic_reports_exact_message_and_help_for_every_reason() {
    let cases = [
        (
            RejectReason::TargetGone,
            "cannot close pane: target no longer exists",
            "the target closed; re-run against a current target",
        ),
        (
            RejectReason::TargetAmbiguous,
            "cannot close pane: target matched more than one; specify an explicit id",
            "specify an explicit pane or tab id",
        ),
        (
            RejectReason::TargetNotFound,
            "cannot close pane: no target matched",
            "check the target id and try again",
        ),
        (
            RejectReason::SourceClientStale,
            "cannot close pane: source client has detached",
            "reconnect the client and retry",
        ),
        (
            RejectReason::Unauthorized,
            "cannot close pane: command not permitted",
            "this command requires additional capability",
        ),
        (
            RejectReason::InvalidState,
            "cannot close pane: invalid in the current state",
            "the command is not valid in the current state",
        ),
        (
            RejectReason::MinSize,
            "cannot close pane: below minimum size",
            "free space by resizing or closing a neighboring pane",
        ),
    ];
    for (reason, message, help) in cases {
        let d = command_reject_diagnostic(reason, "close pane");
        assert_eq!(d.to_string(), message, "{reason:?}");
        assert_eq!(help_of(&d).as_deref(), Some(help), "{reason:?}");
    }
}

#[test]
fn resize_min_size_diagnostic_reports_every_direction_word() {
    let cases = [
        (Direction::Left, "left"),
        (Direction::Right, "right"),
        (Direction::Up, "up"),
        (Direction::Down, "down"),
    ];
    for (direction, word) in cases {
        let d = resize_min_size_diagnostic(direction, 5, 3);
        assert_eq!(
            d.to_string(),
            format!(
                "cannot resize pane {word}: would drop a pane below minimum size 3 (current 5)"
            ),
            "{direction:?}"
        );
    }
}
