//! Staged process teardown for the normal quit path, in a fixed order.
//!
//! The event loop calls [`Runtime::shutdown`] once it exits (quit chord,
//! hangup, or the last pane closing). Stages 1–4 run here; stages 5 (restore
//! the outer terminal) and 6 (flush logs) run after this returns, as the
//! binary's cleanup guard and tracing guard drop in that order. The panic path
//! does not come here — it takes the abrupt [`Runtime::kill_all_panes`].

use std::sync::Arc;
use std::thread;

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::process::KillPolicy;

use crate::runtime::state::Runtime;

impl Runtime {
    /// Tear the process down in a fixed staged order:
    /// 1. enter draining mode (reject new IPC/plugin commands),
    /// 2. notify plugins of imminent shutdown *(seam — no host yet)*,
    /// 3. graceful-then-group-kill every pane's child,
    /// 4. persist the session snapshot *(seam — no storage yet)*.
    ///
    /// Stages 5–6 (restore terminal, flush logs) are left to the caller's
    /// guards, which drop in that order after this returns.
    pub fn shutdown(&mut self) {
        // Stage 1 — draining: reject any newly-arriving IPC/plugin command so
        // nothing mutates state mid-teardown.
        self.draining = true;

        // Stage 2 — notify plugins of imminent shutdown.
        // SEAM: no plugin host exists yet. When it lands, broadcast the
        // shutdown notice here, ahead of the kill, so plugins can flush.

        // Stage 3 — ask every pane's process group to exit, then group-kill so
        // no descendant is orphaned. Parallel across panes and joined, so the
        // wait is bounded by one grace window, not the sum across panes.
        self.graceful_kill_all_panes();

        // Stage 4 — persist the session snapshot.
        // SEAM: storage is a no-op placeholder. When a real storage layer
        // lands, serialize each session here and write it under the data dir,
        // skipping gracefully when persistence is unavailable. Ordered after the
        // kill so it records the final session state.

        // Stages 5 (restore terminal) and 6 (flush logs) run after this returns,
        // as the caller's cleanup guard and tracing guard drop in that order.
    }

    /// Graceful-then-group-kill every live pane's child, in parallel. Each pane
    /// gets its own thread so every pane's group receives the stop request at
    /// once; joining them holds the process open until the children are reaped
    /// (or group-killed at the deadline), bounding the total wait to ~one
    /// window.
    fn graceful_kill_all_panes(&self) {
        let backend = Arc::clone(self.pty_backend());
        let handles: Vec<_> = self
            .pty_handles
            .keys()
            .copied()
            .map(|pane_id| {
                let backend = Arc::clone(&backend);
                thread::spawn(move || {
                    let _ = backend.kill(
                        pane_id,
                        KillPolicy::GracefulTree {
                            timeout: GRACEFUL_TIMEOUT_DURATION,
                        },
                    );
                })
            })
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
}
