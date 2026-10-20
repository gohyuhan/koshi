//! Platform terminal mode, output, input, resize, and wake handling.

use std::time::Duration;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(super) use unix::{
    read_window_size, EventSource as PlatformEventSource, TerminalDevice, Waker as PlatformWaker,
};
#[cfg(windows)]
pub(super) use windows::{
    read_window_size, EventSource as PlatformEventSource, TerminalDevice, Waker as PlatformWaker,
};

/// How long the input parser waits, after the last input bytes, for the rest
/// of an unfinished escape sequence before it takes the bytes as they stand:
/// 25 ms. Example: a lone `ESC` becomes the Escape key 25 ms after it arrives.
const ESCAPE_SEQUENCE_TIMEOUT_DURATION: Duration = Duration::from_millis(25);

/// The bytes the terminal output writer holds before it writes them to the
/// terminal: 4 KiB.
const OUTPUT_BUFFER_BYTE_COUNT: usize = 4_096;

/// Return the shorter of `first_timeout_duration` and
/// `second_timeout_duration`, where `None` is no limit. Example: `Some(25 ms)`
/// and `None` give `Some(25 ms)`; `None` and `None` give `None`.
fn choose_shorter_timeout_duration(
    first_timeout_duration: Option<Duration>,
    second_timeout_duration: Option<Duration>,
) -> Option<Duration> {
    match (first_timeout_duration, second_timeout_duration) {
        (Some(first_timeout_duration), Some(second_timeout_duration)) => {
            Some(first_timeout_duration.min(second_timeout_duration))
        }
        (Some(timeout_duration), None) | (None, Some(timeout_duration)) => Some(timeout_duration),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests;
