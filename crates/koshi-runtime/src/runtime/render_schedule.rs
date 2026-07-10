//! Invalidation-driven render scheduling.
//!
//! The dispatcher thread does not repaint on a blind loop. After it handles a
//! [`RuntimeEvent`](crate::runtime::event::RuntimeEvent) it marks *why* the
//! screen is stale with [`RenderScheduler::invalidate`], then asks
//! [`RenderScheduler::poll`] whether it is time to render. The scheduler
//! **coalesces** a burst of invalidations into a single repaint and **gates**
//! how often that repaint may happen, so a chatty child produces one frame per
//! tick instead of one per write, and an idle koshi burns ~0% CPU.
//!
//! # Two cadences
//!
//! A real change (cell output, layout, focus, resize, …) may render as fast as
//! [`FRAME_INTERVAL`] (~one frame / 16 ms). When the *only* pending reason is
//! the cursor blink, the scheduler drops to the far slower [`BLINK_INTERVAL`]
//! (~500 ms) — that lane is what keeps an idle session near 0% CPU while the
//! cursor still blinks.
//!
//! # Time is injected, never read
//!
//! The scheduler never calls `Instant::now()`. The event loop passes the
//! current [`Instant`] into every decision, so the gate is a pure function of
//! its inputs: monotonic (only ever moves forward, immune to wall-clock jumps
//! from clock-sync corrections (NTP, Network Time Protocol) or daylight-saving
//! changes (DST) that a `SystemTime` gate would suffer) and deterministic to
//! test with a synthetic timeline. `last_render` is dispatcher-thread-local
//! and never serialized, so `Instant` — not the boundary-only `SystemTime` —
//! is the correct clock.

use std::time::{Duration, Instant};

/// Fastest cadence for a real (non-blink) change: ~one frame per 16 ms tick.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Cadence when only the cursor blink is pending: slow enough that an idle
/// session stays near 0% CPU.
pub const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Why the rendered frame is stale.
///
/// One variant per source the dispatcher reacts to. Each occupies one bit of
/// [`RenderScheduler`]'s pending mask via `bit`, whose exhaustive match caps
/// the set at eight variants (one `u8`) at compile time;
/// [`BlinkTick`](InvalidationReason::BlinkTick) is special-cased into the slow
/// cadence, every other reason takes the fast one.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationReason {
    /// A live pane's child wrote output bytes, staling that pane's cells,
    /// cursor, or modes.
    PtyOutput = 0,
    /// The layout tree changed (split, close, resize, reflow).
    LayoutChanged = 1,
    /// Focus moved to a different pane.
    FocusChanged = 2,
    /// The active tab changed.
    TabChanged = 3,
    /// A client's outer terminal changed size.
    TerminalResize = 4,
    /// The tabline status section (mode tag, scroll indicator) changed.
    StatusChanged = 5,
    /// A plugin updated one of its UI surfaces.
    PluginUiUpdated = 6,
    /// The periodic cursor-blink tick — the slow, idle-friendly cadence.
    BlinkTick = 7,
}

impl InvalidationReason {
    /// The bit this reason occupies in [`RenderScheduler`]'s pending mask.
    ///
    /// Exhaustive match: a new variant must add an arm here, and an arm whose
    /// bit index passes 7 fails to compile (`u8` shift overflow), keeping the
    /// mask within its eight-bit capacity.
    const fn bit(self) -> u8 {
        match self {
            InvalidationReason::PtyOutput => 1 << 0,
            InvalidationReason::LayoutChanged => 1 << 1,
            InvalidationReason::FocusChanged => 1 << 2,
            InvalidationReason::TabChanged => 1 << 3,
            InvalidationReason::TerminalResize => 1 << 4,
            InvalidationReason::StatusChanged => 1 << 5,
            InvalidationReason::PluginUiUpdated => 1 << 6,
            InvalidationReason::BlinkTick => 1 << 7,
        }
    }
}

/// The bit [`InvalidationReason::BlinkTick`] occupies in the pending mask.
const BLINK_BIT: u8 = InvalidationReason::BlinkTick.bit();

/// Decides when the dispatcher thread repaints.
///
/// Producers mark reasons with [`invalidate`](Self::invalidate); the loop drives
/// [`poll`](Self::poll) to learn whether to render now and [`next_wakeup`](Self::next_wakeup)
/// to learn how long it may block on the inbox before it must wake to flush a
/// pending frame or fire the blink. Lives on the dispatcher thread; never shared.
#[derive(Debug)]
pub struct RenderScheduler {
    /// Bitmask of pending [`InvalidationReason`]s, one bit per reason as given
    /// by `InvalidationReason::bit`. Zero means nothing is pending.
    pending: u8,
    /// When the last frame was rendered. `None` until the first render, which
    /// makes any pending reason render immediately.
    last_render: Option<Instant>,
}

impl RenderScheduler {
    /// Build a scheduler with nothing pending and no prior render.
    pub fn new() -> Self {
        RenderScheduler {
            pending: 0,
            last_render: None,
        }
    }

    /// Mark `reason` as pending. Idempotent within a coalescing window: marking
    /// the same reason twice before a render still yields one render.
    pub fn invalidate(&mut self, reason: InvalidationReason) {
        self.pending |= reason.bit();
    }

    /// Whether a repaint is due at `now`, without changing state. `true` when
    /// something is pending and the cadence for what is pending has elapsed
    /// since the last render (or nothing has rendered yet).
    fn is_due(&self, now: Instant) -> bool {
        if self.pending == 0 {
            return false;
        }
        match self.last_render {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.required_interval(),
        }
    }

    /// The cadence for the current pending set: [`FRAME_INTERVAL`] if any real
    /// reason is pending, else [`BLINK_INTERVAL`] when only the blink is.
    fn required_interval(&self) -> Duration {
        if self.pending & !BLINK_BIT != 0 {
            FRAME_INTERVAL
        } else {
            BLINK_INTERVAL
        }
    }

    /// Ask whether to render at `now`. On `true`, records `now` as the last
    /// render and clears every pending reason — the caller then repaints. On
    /// `false`, leaves the pending set intact for a later poll.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.is_due(now) {
            self.last_render = Some(now);
            self.pending = 0;
            true
        } else {
            false
        }
    }

    /// How long the loop may block on the inbox before it must wake to render.
    ///
    /// `None` when nothing is pending — the loop sleeps until an event arrives.
    /// `Some(Duration::ZERO)` when a render is already due. Otherwise the
    /// remaining time until the current pending set's cadence elapses.
    pub fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        if self.pending == 0 {
            return None;
        }
        match self.last_render {
            None => Some(Duration::ZERO),
            Some(last) => {
                let elapsed = now.saturating_duration_since(last);
                Some(self.required_interval().saturating_sub(elapsed))
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
