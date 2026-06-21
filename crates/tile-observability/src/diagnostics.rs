//! `diagnostics` domain — user-facing diagnostics.
//!
//! These builders turn an internal failure into a [`miette::Diagnostic`] that
//! tells the user what failed, where, and how to fix it. Each carries a stable
//! `code`, a one-line message, and a `help` suggestion. The binary enables
//! miette's `fancy` feature to render them; libraries only construct them.

// miette-derive's generated impls re-bind each message field, tripping
// `unused_assignments` in code we don't own; silence it for this module only.
#![allow(unused_assignments)]

use miette::Diagnostic;
use thiserror::Error;

use tile_core::event::RejectReason;
use tile_core::geometry::Direction;

pub mod state;

/// A configuration value the user must correct, naming the file and key.
#[derive(Debug, Error, Diagnostic)]
#[error("invalid config at {path}: key `{key}` {reason}")]
#[diagnostic(code(tile::config), help("{help}"))]
pub struct ConfigDiagnostic {
    /// The config file path.
    path: String,
    /// The invalid configuration key.
    key: String,
    /// What is wrong with the value.
    reason: String,
    /// How to fix it.
    help: String,
}

/// A command the runtime declined to apply, with the reason and a suggestion.
#[derive(Debug, Error, Diagnostic)]
#[error("cannot {context}: {reason}")]
#[diagnostic(code(tile::command), help("{help}"))]
pub struct CommandRejectDiagnostic {
    /// The action the user attempted (e.g. "focus pane").
    context: String,
    /// Why the runtime rejected it.
    reason: RejectReason,
    /// How to fix it or work around it.
    help: String,
}

/// A resize refused because it would drop a pane below its minimum size.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "cannot resize pane {direction}: would drop a pane below minimum size {min} (current {current})"
)]
#[diagnostic(
    code(tile::resize),
    help("free space by resizing or closing a neighboring pane")
)]
pub struct ResizeMinSizeDiagnostic {
    /// The direction of the attempted resize.
    direction: &'static str,
    /// The pane's current size.
    current: u16,
    /// The minimum size the resize would breach.
    min: u16,
}

/// Build a diagnostic for an invalid configuration value.
pub fn config_diagnostic(
    path: impl Into<String>,
    key: impl Into<String>,
    reason: impl Into<String>,
    help: impl Into<String>,
) -> ConfigDiagnostic {
    ConfigDiagnostic {
        path: path.into(),
        key: key.into(),
        reason: reason.into(),
        help: help.into(),
    }
}

/// Build a diagnostic for a rejected command. `context` names the attempted
/// action (e.g. `"focus pane"`).
pub fn command_reject_diagnostic(
    reason: RejectReason,
    context: impl Into<String>,
) -> CommandRejectDiagnostic {
    CommandRejectDiagnostic {
        context: context.into(),
        reason,
        help: reject_help(reason).to_string(),
    }
}

/// Build a diagnostic for a resize that would breach a minimum-size constraint.
/// `current` is the pane's size now; `min` is the floor the resize would breach.
pub fn resize_min_size_diagnostic(
    direction: Direction,
    current: u16,
    min: u16,
) -> ResizeMinSizeDiagnostic {
    ResizeMinSizeDiagnostic {
        direction: direction_word(direction),
        current,
        min,
    }
}

/// Turn any rejected command into a user-facing report, even without a known
/// action context. This is a free function rather than `From<RejectReason> for
/// miette::Report` because both types are foreign here: the orphan rule forbids
/// that impl, and `tile-core` must not depend on miette to host it.
pub fn reject_report(reason: RejectReason) -> miette::Report {
    command_reject_diagnostic(reason, "complete command").into()
}

/// A fix suggestion tailored to each rejection reason.
fn reject_help(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::TargetGone => "the target closed; re-run against a current target",
        RejectReason::TargetAmbiguous => "specify an explicit pane or tab id",
        RejectReason::TargetNotFound => "check the target id and try again",
        RejectReason::SourceClientStale => "reconnect the client and retry",
        RejectReason::Unauthorized => "this command requires additional capability",
        RejectReason::InvalidState => "the command is not valid in the current state",
        RejectReason::MinSize => "free space by resizing or closing a neighboring pane",
    }
}

/// The lowercase user-facing word for a direction.
fn direction_word(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

#[cfg(test)]
mod tests;
