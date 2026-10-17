//! One warning that repeats, written to the log at most once inside
//! [`LOG_WINDOW_DURATION`](crate::repeated_warning::LOG_WINDOW_DURATION).

use std::time::{Duration, Instant};

/// How often one repeated warning is written at most: once in 60 seconds.
pub const LOG_WINDOW_DURATION: Duration = Duration::from_secs(60);

/// One repeated warning, written at most once inside [`LOG_WINDOW_DURATION`].
#[derive(Debug, Default)]
pub struct WarningRateLimiter {
    /// When the last line was written, or `None` when none has been.
    last_warning_at: Option<Instant>,
}

impl WarningRateLimiter {
    /// A warning that has not been written yet.
    #[must_use]
    pub fn new() -> WarningRateLimiter {
        Self {
            last_warning_at: None,
        }
    }

    /// Record `current_time` as the moment the line is written when the line is
    /// due, and return whether it was due: true when no line has been written,
    /// and when the last one was written [`LOG_WINDOW_DURATION`] or longer ago.
    /// The caller writes the line when this returns true.
    ///
    /// Example — with [`LOG_WINDOW_DURATION`] at 60 seconds, ten thousand calls spread
    /// over five minutes return true five times.
    pub fn record_warning_if_due(&mut self, current_time: Instant) -> bool {
        if self.last_warning_at.is_some_and(|warning_written_at| {
            current_time.duration_since(warning_written_at) < LOG_WINDOW_DURATION
        }) {
            return false;
        }
        self.last_warning_at = Some(current_time);
        true
    }
}

#[cfg(test)]
mod tests;
