use super::*;
use miette::Diagnostic;

// Snapshot each diagnostic variant by its message, stable code, and help line —
// the three parts a user reads: what/where failed and how to fix it.

fn code_of(d: &impl Diagnostic) -> String {
    d.code().map(|c| c.to_string()).unwrap_or_default()
}

fn help_of(d: &impl Diagnostic) -> String {
    d.help().map(|h| h.to_string()).unwrap_or_default()
}

#[test]
fn config_diagnostic_reports_path_key_and_help() {
    let d = config_diagnostic(
        "~/.config/tile/config.kdl",
        "layout",
        "unknown value `grid`",
        "use one of: tiled, stacked",
    );
    assert_eq!(
        d.to_string(),
        "invalid config at ~/.config/tile/config.kdl: key `layout` unknown value `grid`"
    );
    assert_eq!(code_of(&d), "tile::config");
    assert_eq!(help_of(&d), "use one of: tiled, stacked");
}

#[test]
fn command_reject_diagnostic_reports_context_reason_and_help() {
    let d = command_reject_diagnostic(RejectReason::TargetNotFound, "focus pane");
    assert_eq!(d.to_string(), "cannot focus pane: no target matched");
    assert_eq!(code_of(&d), "tile::command");
    assert_eq!(help_of(&d), "check the target id and try again");
}

#[test]
fn resize_min_size_diagnostic_reports_direction_and_sizes() {
    let d = resize_min_size_diagnostic(Direction::Left, 3, 2);
    assert_eq!(
        d.to_string(),
        "cannot resize pane left: would drop a pane below minimum size 2 (current 3)"
    );
    assert_eq!(code_of(&d), "tile::resize");
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
