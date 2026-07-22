//! Staged process teardown for the normal quit path, in a fixed order.
//!
//! The event loop calls [`Server::shutdown`] once it exits. Explicit quit
//! group-kills immediately; hangup or last-pane exit keeps graceful teardown.
//! Stages 1–5 run here; stages 6 (restore
//! the outer terminal) and 7 (flush logs) run after this returns, as the
//! binary's cleanup guard and tracing guard drop in that order. The panic path
//! does not come here — it takes the abrupt [`Server::kill_all_panes`].

use std::sync::Arc;
use std::thread;

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::process::KillPolicy;

use crate::server::Server;

impl Server {
    /// Tear the process down in a fixed staged order:
    /// 1. enter draining mode (reject new IPC/plugin commands),
    /// 2. stop the control socket and withdraw its endpoint file,
    /// 3. notify plugins of imminent shutdown *(seam — no host yet)*,
    /// 4. group-kill immediately for explicit quit, otherwise graceful kill,
    /// 5. persist the session snapshot *(seam — no storage yet)*.
    ///
    /// Stages 6–7 (restore terminal, flush logs) are left to the caller's
    /// guards, which drop in that order after this returns.
    pub fn shutdown(&mut self) {
        // Stage 1 — draining: reject any newly-arriving IPC/plugin command so
        // nothing mutates state mid-teardown.
        self.draining = true;

        // Stage 2 — stop answering the control socket and remove the socket
        // and endpoint file, so nothing advertises a session that is ending.
        if let Some(ipc_server) = self.ipc_server.take() {
            ipc_server.shutdown();
        }

        // Stage 3 — notify plugins of imminent shutdown.
        // SEAM: no plugin host exists yet. When it lands, broadcast the
        // shutdown notice here, ahead of the kill, so plugins can flush.

        // Stage 4 — explicit user quit is immediate; natural loop endings keep
        // the graceful process-group window. Both paths reap descendants.
        if self.immediate_shutdown {
            self.kill_all_panes();
        } else {
            self.graceful_kill_all_panes();
        }

        // Stage 5 — persist the session snapshot.
        // SEAM: storage is a no-op placeholder. When a real storage layer
        // lands, serialize each session here and write it under the data dir,
        // skipping gracefully when persistence is unavailable. Ordered after the
        // kill so it records the final session state.

        // Stages 6 (restore terminal) and 7 (flush logs) run after this returns,
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

#[cfg(test)]
mod tests;
