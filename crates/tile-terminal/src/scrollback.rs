//! Per-pane scrollback history.
//!
//! Placeholder: the bounded buffer — a `VecDeque` of lines with line- and
//! byte-count caps and truncation accounting — is added in a later task. It
//! exists now only so [`TerminalState`](crate::state::TerminalState) can own
//! the field.

/// The scrollback buffer for one pane. Currently retains no lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scrollback {}
