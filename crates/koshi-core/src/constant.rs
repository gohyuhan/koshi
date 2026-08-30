//! Numeric bounds that more than one crate reads.

use std::time::Duration;

/// Cap on a tab's most-recently-focused pane list. Each tab keeps the panes it
/// focused, newest first and one entry per pane; once it holds this many,
/// recording another drops the oldest.
pub const MAX_TAB_FOCUS_MRU: u16 = 16;

/// Default timeout of a `Graceful` close: the time a child gets to exit on its
/// own before the close escalates to a forced kill.
pub const GRACEFUL_TIMEOUT_DURATION: Duration = Duration::from_secs(3);
