//! Staged process teardown for the normal quit path, in a fixed order.
//!
//! The event loop calls [`Server::shutdown`] once it exits. A quit with no
//! issuing client — `kill-session` — group-kills immediately; every other
//! ending group-kills gracefully. Stages 1–5 run here; stages 6 (restore the
//! outer terminal) and 7 (flush logs) run after this returns, as the binary's
//! cleanup guard and tracing guard drop in that order. The panic path does not
//! come here — it takes the abrupt [`Server::kill_all_panes`].

use std::sync::Arc;
use std::thread;

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::process::KillPolicy;

use crate::server::Server;

impl Server {
    /// Tear the process down in a fixed staged order:
    /// 1. set the draining flag,
    /// 2. stop the control socket and withdraw its endpoint file,
    /// 3. plugin notification — a no-op, no plugin host is wired,
    /// 4. group-kill immediately for a quit with no issuing client, otherwise
    ///    graceful kill,
    /// 5. session-snapshot persistence — a no-op, the storage service is
    ///    [`NullStorage`](crate::placeholder::NullStorage).
    ///
    /// Stages 6–7 (restore terminal, flush logs) are left to the caller's
    /// guards, which drop in that order after this returns.
    pub fn shutdown(&mut self) {
        // Stage 1 — record that teardown started. The event loop has already
        // exited, so no further IPC or plugin command reaches dispatch.
        self.draining = true;

        // Stage 2 — stop answering the control socket, then remove the socket
        // file, the endpoint file and the advert that name this session.
        if let Some(ipc_server) = self.ipc_server.take() {
            ipc_server.shutdown();
        }

        // Stage 3 — plugin notification: a no-op, no plugin host is wired.

        // Stage 4 — a quit with no issuing client is immediate; every other
        // ending keeps the graceful process-group window. Both paths reap
        // descendants.
        if self.immediate_shutdown {
            self.kill_all_panes();
        } else {
            self.graceful_kill_all_panes();
        }

        // Stage 5 — session-snapshot persistence: a no-op, the storage service
        // is the NullStorage stand-in.
    }

    /// Graceful-then-group-kill every live pane's child
    /// ([`KillPolicy::GracefulTree`] with [`GRACEFUL_TIMEOUT_DURATION`]), one
    /// thread per pane, so every pane's group receives the stop request at
    /// once. Joins every thread, which holds the process open until the
    /// children are reaped or group-killed at the deadline; the total wait is
    /// one such window. A pane whose kill fails is skipped and the rest still
    /// run.
    fn graceful_kill_all_panes(&self) {
        let handles: Vec<_> = self
            .pty_handles
            .keys()
            .copied()
            .map(|pane_id| {
                let backend = Arc::clone(self.pty_backend());
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
