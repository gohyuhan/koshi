//! Shared tuning constants — fixed numeric bounds the model enforces, kept in
//! `tile-core` so every crate that references one agrees on a single value.

use std::time::Duration;

/// Cap on a tab's most-recently-focused pane list. Each tab keeps the panes
/// it focused, newest first and one entry per pane; once it holds this many,
/// recording another drops the oldest. Bounds per-tab memory over a
/// long-lived session while keeping the recent "where was I" trail that focus
/// recovery walks.
pub const MAX_TAB_FOCUS_MRU: u16 = 16;

/// Default grace period for a `Graceful` close: how long the model waits for a
/// child to exit on its own before a caller escalates to a forced kill.
pub const GRACEFUL_TIMEOUT_DURATION: Duration = Duration::new(3, 0);
