//! Test suite: verify that diagnostic messages clearly report what failed,
//! where it failed, and how to fix it.

use super::*;
use miette::Diagnostic;

// Snapshot each diagnostic variant by its message, stable code, and help line —
// the three parts a user reads: what/where failed and how to fix it.

/// Extract a diagnostic's stable error code, or an empty string if absent.
fn code_of(d: &impl Diagnostic) -> String {
    d.code().map(|c| c.to_string()).unwrap_or_default()
}

/// Extract a diagnostic's help message, or an empty string if absent.
fn help_of(d: &impl Diagnostic) -> String {
    d.help().map(|h| h.to_string()).unwrap_or_default()
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
    assert_eq!(code_of(&d), "koshi::config");
    assert_eq!(help_of(&d), "use one of: tiled, stacked");
}

#[test]
fn command_reject_diagnostic_reports_context_reason_and_help() {
    let d = command_reject_diagnostic(RejectReason::TargetNotFound, "focus pane");
    assert_eq!(d.to_string(), "cannot focus pane: no target matched");
    assert_eq!(code_of(&d), "koshi::command");
    assert_eq!(help_of(&d), "check the target id and try again");
}

#[test]
fn resize_min_size_diagnostic_reports_direction_and_sizes() {
    let d = resize_min_size_diagnostic(Direction::Left, 3, 2);
    assert_eq!(
        d.to_string(),
        "cannot resize pane left: would drop a pane below minimum size 2 (current 3)"
    );
    assert_eq!(code_of(&d), "koshi::resize");
    assert_eq!(
        help_of(&d),
        "free space by resizing or closing a neighboring pane"
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
        assert_eq!(help_of(&d), help, "{reason:?}");
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
