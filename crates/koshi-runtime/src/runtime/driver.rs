//! The loop-facing surface: the thin methods the binary's event loop calls to
//! time renders, decide when to repaint, tell whether any pane is still live,
//! and group-kill every child when the loop panics (the normal quit path takes
//! the staged [`Runtime::shutdown`]). They wrap the render scheduler and PTY
//! maps, which are crate-private, so the loop can live outside this crate.
//! Explicit quit marks zero-grace teardown before the loop exits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_core::process::KillPolicy;

use crate::runtime::state::Runtime;

impl Runtime {
    /// How long the loop may block before the next render is due: `None` to
    /// sleep until an event, `Some(ZERO)` to render now, else the time left on
    /// the current cadence.
    pub fn next_render_wakeup(&self, now: Instant) -> Option<Duration> {
        self.render_scheduler.next_wakeup(now)
    }

    /// Whether a render is due at `now`. When `true`, the scheduler records the
    /// render and clears its pending reasons, so the caller must repaint.
    pub fn poll_render(&mut self, now: Instant) -> bool {
        self.render_scheduler.poll(now)
    }

    /// Whether any pane's PTY is still live — the loop exits once none remain.
    pub fn has_active_panes(&self) -> bool {
        !self.pty_handles.is_empty()
    }

    /// Immediately group-kill every live pane's child (`KillPolicy::Tree`),
    /// reaping any descendants so none is orphaned. The abrupt teardown for the
    /// panic path — no grace window while unwinding; the normal quit path takes
    /// the staged [`Runtime::shutdown`].
    pub fn kill_all_panes(&mut self) {
        let backend = Arc::clone(self.pty_backend());
        for pane_id in self.pty_handles.keys().copied() {
            let _ = backend.kill(pane_id, KillPolicy::Tree);
        }
    }
}

#[cfg(test)]
mod tests;
