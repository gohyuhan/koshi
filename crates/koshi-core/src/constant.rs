//! Numeric bounds and fixed sizes that more than one crate reads, and the
//! heartbeat interval a link timeout gives.

use std::time::Duration;

use crate::geometry::Size;

/// Cap on a tab's most-recently-focused pane list. Each tab keeps the panes it
/// focused, newest first and one entry per pane; once it holds this many,
/// recording another drops the oldest.
pub const MAX_TAB_FOCUS_MRU_ENTRY_COUNT: u16 = 16;

/// The most floating panes one session holds at once.
pub const MAX_FLOATING_PANES_PER_SESSION: usize = 12;

/// The cells a floating pane's chrome takes from its outer size: one border
/// column on each side, and the top border, titlebar, separator and bottom
/// border rows. A `40x12` floating pane holds `38x8` cells of content.
pub const FLOATING_PANE_CHROME_SIZE: Size = Size {
    column_count: 2,
    row_count: 4,
};

/// Default timeout of a `Graceful` close: the time a child gets to exit on its
/// own before the close escalates to a forced kill.
pub const GRACEFUL_TIMEOUT_DURATION: Duration = Duration::from_secs(3);

/// The least link timeout, in milliseconds: the least
/// `remote-link-timeout-ms` that `koshi.kdl` takes, and the least
/// `link_timeout_ms` that a session takes in an `Attach` request.
pub const MIN_LINK_TIMEOUT_MS: u32 = 1000;

/// The heartbeat interval of a link whose link timeout is `link_timeout_ms`:
/// a third of it. A viewer on another machine sends a `Ping` once every
/// heartbeat interval, and the session writes the viewer a `Pong` whenever it
/// has written it nothing for one. `30000` gives 10 seconds, and `1000` gives
/// 333333333 nanoseconds.
#[must_use]
pub fn compute_heartbeat_interval(link_timeout_ms: u32) -> Duration {
    Duration::from_millis(u64::from(link_timeout_ms)) / 3
}

#[cfg(test)]
mod tests;
