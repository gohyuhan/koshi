//! Numeric bounds and fixed sizes that more than one crate reads, the
//! heartbeat interval a link timeout gives, and the frame interval a frame
//! rate gives.

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

/// The timeout of every `Graceful` close that koshi requests: the time a child
/// gets to exit on its own before the close escalates to a forced kill.
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

/// The most cells a client viewport holds: 2097152, which is `2^21`. A
/// session refuses an `Attach` whose viewport holds more, and drops a `Resize`
/// whose viewport holds more. A `1920x1080` viewport holds 2073600 cells and
/// fits.
pub const MAX_VIEWPORT_CELL_COUNT: u32 = 2_097_152;

/// `viewport_size` cut to at most [`MAX_VIEWPORT_CELL_COUNT`] cells: every
/// column stays, and the rows past the limit go. `65535x65535` →
/// `65535x32`, and `200x50` stays `200x50`.
#[must_use]
pub fn limit_viewport_size(viewport_size: Size) -> Size {
    if viewport_size.compute_cell_count() <= MAX_VIEWPORT_CELL_COUNT {
        return viewport_size;
    }
    // Over the limit, `column_count` is 33 or more and the quotient is 63550
    // or less.
    let limited_row_count = MAX_VIEWPORT_CELL_COUNT / u32::from(viewport_size.column_count);
    Size {
        column_count: viewport_size.column_count,
        row_count: limited_row_count as u16,
    }
}

/// The least `max-frames-per-second` that `koshi.kdl` takes, and the least
/// `max_frames_per_second` that a session takes in an `Attach` request.
pub const LOWEST_MAX_FRAMES_PER_SECOND: u16 = 30;

/// The greatest `max-frames-per-second` that `koshi.kdl` takes, and the
/// greatest `max_frames_per_second` that a session takes in an `Attach`
/// request.
pub const HIGHEST_MAX_FRAMES_PER_SECOND: u16 = 120;

/// The `max-frames-per-second` of a `koshi.kdl` that sets none.
pub const DEFAULT_MAX_FRAMES_PER_SECOND: u16 = 60;

/// The least time between two frames at `max_frames_per_second` frames a
/// second: one second divided by it, rounded up to the next nanosecond. No
/// second holds more frames than `max_frames_per_second`. `60` gives 16666667
/// nanoseconds, `120` gives 8333334, and `30` gives 33333334.
///
/// # Panics
/// When `max_frames_per_second` is `0`.
#[must_use]
pub fn compute_frame_interval(max_frames_per_second: u16) -> Duration {
    Duration::from_nanos(1_000_000_000_u64.div_ceil(u64::from(max_frames_per_second)))
}

#[cfg(test)]
mod tests;
