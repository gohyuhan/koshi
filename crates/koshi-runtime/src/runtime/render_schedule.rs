//! Invalidation-driven render scheduling.
//!
//! The dispatcher thread does not repaint on a blind loop. After it handles a
//! [`RuntimeEvent`](crate::runtime::event::RuntimeEvent) it marks the screen
//! stale with [`RenderScheduler::invalidate`], then asks
//! [`RenderScheduler::poll`] whether it is time to render. The scheduler
//! **coalesces** a burst of invalidations into a single repaint and **gates**
//! how often that repaint may happen at [`FRAME_INTERVAL`]: a chatty child
//! produces one frame per tick instead of one per write, and an idle koshi
//! burns ~0% CPU.
//!
//! # Time is injected, never read
//!
//! The scheduler never calls `Instant::now()`. The event loop passes the
//! current [`Instant`] into every decision, and the gate is a pure function of
//! its inputs. An [`Instant`] is monotonic: it only ever moves forward, and a
//! wall-clock jump from a clock-sync correction (NTP, Network Time Protocol)
//! or a daylight-saving change (DST) does not move it. A test drives the gate
//! with a synthetic timeline. `last_render` stays on the dispatcher thread and
//! is never serialized.

use std::time::{Duration, Instant};

/// Fastest cadence a repaint may happen at: ~one frame per 8 ms tick.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// Decides when the dispatcher thread repaints.
///
/// Producers mark the screen stale with [`invalidate`](Self::invalidate); the
/// loop drives [`poll`](Self::poll) to learn whether to render now and
/// [`next_wakeup`](Self::next_wakeup) to learn how long it may block on the
/// inbox before it must wake to flush a pending frame. Lives on the dispatcher
/// thread; never shared.
#[derive(Debug)]
pub struct RenderScheduler {
    /// Whether a change is waiting to be painted. A render clears it.
    pending: bool,
    /// When the last frame was rendered. `None` until the first render, which
    /// makes a pending change render immediately.
    last_render: Option<Instant>,
}

impl RenderScheduler {
    /// Build a scheduler with nothing pending and no prior render.
    pub fn new() -> Self {
        RenderScheduler {
            pending: false,
            last_render: None,
        }
    }

    /// Mark the screen stale. Idempotent within a coalescing window: marking
    /// twice before a render still yields one render.
    pub fn invalidate(&mut self) {
        self.pending = true;
    }

    /// Whether a repaint is due at `now`, without changing state. `true` when
    /// something is pending and [`FRAME_INTERVAL`] has elapsed since the last
    /// render, or nothing has rendered yet.
    fn is_due(&self, now: Instant) -> bool {
        if !self.pending {
            return false;
        }
        match self.last_render {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= FRAME_INTERVAL,
        }
    }

    /// Ask whether to render at `now`. On `true`, records `now` as the last
    /// render and clears the pending mark — the caller then repaints. On
    /// `false`, leaves the mark in place for a later poll.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.is_due(now) {
            self.last_render = Some(now);
            self.pending = false;
            true
        } else {
            false
        }
    }

    /// How long the loop may block on the inbox before it must wake to render.
    ///
    /// `None` when nothing is pending — the loop sleeps until an event arrives.
    /// `Some(Duration::ZERO)` when a render is already due. Otherwise the
    /// remaining time until [`FRAME_INTERVAL`] elapses.
    pub fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        if !self.pending {
            return None;
        }
        match self.last_render {
            None => Some(Duration::ZERO),
            Some(last) => {
                let elapsed = now.saturating_duration_since(last);
                Some(FRAME_INTERVAL.saturating_sub(elapsed))
            }
        }
    }
}

impl Default for RenderScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
