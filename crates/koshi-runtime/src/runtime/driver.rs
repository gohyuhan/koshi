//! The loop-facing surface: the thin methods the binary's event loop calls to
//! route one inbox event to its handler, time renders, decide when to
//! repaint, tell whether any pane is still live, and group-kill every child
//! when the loop panics (the normal quit path takes the staged
//! [`Server::shutdown`]). They wrap the render scheduler, PTY maps, and
//! handlers, which are crate-private, so the loop can live outside this
//! crate. Explicit quit marks zero-grace teardown before the loop exits.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_core::process::KillPolicy;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

impl Server {
    /// Route one inbox event to its handler, publishing whatever events the
    /// handler emits. Returns [`ControlFlow::Break`] when the event is a quit
    /// request, so the loop stops. A [`RuntimeEvent::Quit`] is a terminal
    /// hangup — explicit quit travels through the `core:quit` command — so it
    /// breaks the loop and leaves teardown on the graceful path.
    pub fn handle_runtime_event(&mut self, event: RuntimeEvent) -> ControlFlow<()> {
        match event {
            RuntimeEvent::Quit => return ControlFlow::Break(()),
            RuntimeEvent::PtyOutput { pane_id, bytes } => self.handle_pty_output(pane_id, &bytes),
            RuntimeEvent::ChildExit {
                pane_id,
                status,
                exited_at,
            } => {
                let events = self.handle_child_exit(pane_id, status, exited_at);
                self.publish_events(&events);
            }
            // A raw chord is the viewer's to read: it holds the keymap, the
            // input mode and any open sequence, and hands the session either a
            // resolved action or a press to write. One arriving here belongs to
            // no attached viewer, and the session has no keymap to judge it
            // with, so it is dropped rather than guessed at.
            RuntimeEvent::KeyInput { client_id, .. } => {
                tracing::debug!(%client_id, "dropping a key no attached viewer resolved");
            }
            // An attached client's viewer already read this chord against its
            // keymap and bound nothing to it, so it is written to the pane that
            // client is typing into.
            RuntimeEvent::ClientKeyPress { client_id, chord } => {
                self.handle_key_press(client_id, chord);
            }
            // An attached client's viewer already read this mouse event against
            // the frame it painted, so the round names every pane it touches.
            // The round is answered on that client's own queue.
            RuntimeEvent::ClientMouse {
                client_id,
                request_id,
                actions,
            } => {
                self.run_client_mouse(client_id, request_id, actions);
            }
            // A mouse event is the viewer's for the same reason: only the frame
            // it painted says which pane the pointer is over and which gesture
            // is under way. One arriving here belongs to no attached viewer.
            RuntimeEvent::MouseInput { client_id, .. } => {
                tracing::debug!(%client_id, "dropping a mouse event no attached viewer answered");
            }
            RuntimeEvent::HostPaste { client_id, text } => {
                self.handle_host_paste(client_id, &text);
            }
            RuntimeEvent::ClientAttached {
                session_id,
                client_id,
                viewport,
                active_tab,
                attached_at,
            } => {
                let events = self.handle_client_attach(
                    session_id,
                    client_id,
                    viewport,
                    active_tab,
                    attached_at,
                );
                self.publish_events(&events);
            }
            RuntimeEvent::ClientDetached { client_id } => {
                let events = self.handle_client_detach(client_id);
                self.publish_events(&events);
            }
            RuntimeEvent::Resize { client_id, size } => {
                let events = self.handle_client_resize(client_id, size);
                self.publish_events(&events);
            }
            // The only deadline the session ever woke for was a key sequence's,
            // and that sequence now lives on the viewer, which expires it
            // itself. The variant stays as the loop's generic wake-up.
            RuntimeEvent::Timer => {}
            RuntimeEvent::Ipc { envelope, reply } => {
                let result = self.submit_command(envelope);
                // A closed reply channel means the connection thread is gone;
                // the command has already applied, so there is nothing to undo.
                let _ = reply.send(result);
            }
            RuntimeEvent::IpcAttach {
                viewport,
                filter,
                attached_at,
                reply,
            } => {
                // The client and its subscription are registered together here,
                // so the structure in the answer and the queue's first event
                // describe one continuous state.
                let _ = reply.send(self.handle_ipc_attach(viewport, filter, attached_at));
            }
            RuntimeEvent::IpcDiscovery { reply } => {
                let _ = reply.send(self.build_overview());
            }
            RuntimeEvent::Plugin(envelope) => {
                let _ = self.submit_command(envelope);
            }
        }
        ControlFlow::Continue(())
    }
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
    /// the staged [`Server::shutdown`].
    pub fn kill_all_panes(&mut self) {
        let backend = Arc::clone(self.pty_backend());
        for pane_id in self.pty_handles.keys().copied() {
            let _ = backend.kill(pane_id, KillPolicy::Tree);
        }
    }
}

#[cfg(test)]
mod tests;
