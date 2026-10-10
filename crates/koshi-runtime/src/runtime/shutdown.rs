//! Staged process teardown for the normal quit path, in a fixed order.
//!
//! The event loop calls [`Server::shutdown`] once it exits. A quit with no
//! issuing client — `kill-session` — group-kills immediately; every other
//! ending group-kills gracefully. Either way, the kills of panes that left the
//! session earlier end before [`Server::shutdown`] returns. Stages 1–2 run
//! here; stages 3 (restore the outer terminal) and 4 (flush logs) run after
//! this returns, as the binary's cleanup guard and tracing guard drop in that
//! order. The panic path does not come here — it takes the abrupt
//! [`Server::kill_all_panes`].

use std::sync::Arc;
use std::thread;

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::process::KillPolicy;

use crate::server::Server;

impl Server {
    /// Tear the process down in a fixed staged order:
    /// 1. stop the control socket and withdraw its endpoint file,
    /// 2. group-kill immediately for a quit with no issuing client, otherwise
    ///    graceful kill, then wait for the kills of panes that left the
    ///    session earlier ([`Self::wait_for_pane_kills`]).
    ///
    /// Stages 3–4 (restore terminal, flush logs) are left to the caller's
    /// guards, which drop in that order after this returns.
    pub fn shutdown(&mut self) {
        // Stage 1 — stop answering the control socket, then remove the socket
        // file, the endpoint file and the advert that name this session.
        if let Some(ipc_server) = self.ipc_server.take() {
            ipc_server.shutdown();
        }

        // Stage 2 — a quit with no issuing client is immediate; every other
        // ending keeps the graceful process-group window. Both paths reap
        // descendants.
        if self.should_shutdown_immediately {
            self.kill_all_panes();
        } else {
            self.kill_all_panes_gracefully();
        }
        self.wait_for_pane_kills();
    }

    /// Join every thread in `pane_kill_threads`, each one killing the child of
    /// a pane that left the session, and empty that list. Each thread waits at
    /// most its kill policy's grace window for the child to exit before it
    /// force-kills it.
    pub fn wait_for_pane_kills(&mut self) {
        for pane_kill_thread in self.pane_kill_threads.drain(..) {
            let _ = pane_kill_thread.join();
        }
    }

    /// Graceful-then-group-kill every live pane's child
    /// ([`KillPolicy::GracefulTree`] with [`GRACEFUL_TIMEOUT_DURATION`]), one
    /// thread per pane, so every pane's group receives the stop request at
    /// once. Joins every thread, which holds the process open until the
    /// children are reaped or group-killed at the deadline; the total wait is
    /// one such window. A pane whose kill fails is skipped and the rest still
    /// run.
    fn kill_all_panes_gracefully(&self) {
        let kill_thread_handles: Vec<_> = self
            .live_pane_ids
            .iter()
            .copied()
            .map(|pane_id| {
                let pty_backend = Arc::clone(self.get_pty_backend());
                thread::spawn(move || {
                    let _ = pty_backend.kill_pane(
                        pane_id,
                        KillPolicy::GracefulTree {
                            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
                        },
                    );
                })
            })
            .collect();
        for kill_thread_handle in kill_thread_handles {
            let _ = kill_thread_handle.join();
        }
    }
}

#[cfg(test)]
mod tests;
