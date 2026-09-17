//! The loop-facing surface: the thin methods the binary's event loop calls to
//! route one inbox event to its handler, time renders, decide when to
//! repaint, tell whether any pane is still live, and group-kill every child
//! when the loop panics (the normal quit path takes the staged
//! [`Server::shutdown`]). They wrap the render scheduler, PTY maps, and
//! handlers, which are crate-private; the loop lives outside this crate.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::KillPolicy;
use koshi_terminal::engine::TerminalEngine;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

impl Server {
    /// Route one inbox event to its handler, publishing whatever events the
    /// handler emits. Returns [`ControlFlow::Break`] when the event is a quit
    /// request, which each loop reads on its own terms. A [`RuntimeEvent::Quit`]
    /// is a terminal hangup: it breaks the loop and leaves teardown on the
    /// graceful path. Explicit quit travels through the `core:quit` command.
    pub fn handle_runtime_event(&mut self, runtime_event: RuntimeEvent) -> ControlFlow<()> {
        match runtime_event {
            RuntimeEvent::Quit => return ControlFlow::Break(()),
            RuntimeEvent::PtyOutput {
                pane_id,
                output_bytes,
            } => self.handle_pty_output(pane_id, &output_bytes),
            RuntimeEvent::ChildExit {
                pane_id,
                exit_status,
            } => {
                let events = self.handle_child_exit(pane_id, exit_status);
                self.publish_events(&events);
            }
            // A raw chord is the viewer's to read: it holds the keymap, the
            // input mode and any open sequence, and hands the session either a
            // resolved action or a press to write. One arriving here belongs to
            // no attached viewer, and is dropped.
            RuntimeEvent::KeyInput { client_id, .. } => {
                tracing::debug!(%client_id, "dropping a key no attached viewer resolved");
            }
            // An attached client's viewer already read this event: its keymap
            // bound nothing to it, or no chord could name it. The pane write
            // takes a chord, so a release and a key no chord can name write
            // nothing.
            RuntimeEvent::ClientKeyboard {
                client_id,
                key_input,
            } => {
                if let Some(chord) = key_input.to_binding_chord() {
                    self.handle_key_press(client_id, chord);
                }
            }
            // An attached client's viewer already read this mouse event against
            // the frame it painted, so the round names every pane it touches.
            // The round is answered on that client's own queue.
            RuntimeEvent::ClientMouse {
                client_id,
                request_id,
                mouse_actions,
            } => {
                self.run_client_mouse(client_id, request_id, mouse_actions);
            }
            // A mouse event is the viewer's for the same reason: only the frame
            // it painted says which pane the pointer is over and which gesture
            // is under way. One arriving here belongs to no attached viewer, and
            // is dropped.
            RuntimeEvent::MouseInput { client_id, .. } => {
                tracing::debug!(%client_id, "dropping a mouse event no attached viewer answered");
            }
            RuntimeEvent::HostPaste {
                client_id,
                pasted_text,
            } => {
                self.handle_host_paste(client_id, &pasted_text);
            }
            RuntimeEvent::ClientDetached {
                client_id,
                detached_at,
                is_streamed,
            } => {
                // The view is filed before the detach, which removes the record
                // it is read from.
                if is_streamed {
                    self.save_client_view(client_id, detached_at);
                } else {
                    self.saved_view_store.forget_client_resume_token(client_id);
                }
                let events = self.handle_client_detach(client_id);
                self.publish_events(&events);
            }
            RuntimeEvent::Resize {
                client_id,
                viewport_size,
                pane_area,
                cell_size,
            } => {
                let resize_events = self.handle_client_resize_with_cell_size(
                    client_id,
                    viewport_size,
                    pane_area,
                    cell_size,
                );
                self.publish_events(&resize_events);
            }
            RuntimeEvent::CellSize {
                client_id,
                cell_size,
            } => self.handle_client_cell_size(client_id, cell_size),
            // The loop's generic wake-up. The session holds no deadline of its
            // own: a key sequence expires on the viewer that opened it.
            RuntimeEvent::Timer => {}
            RuntimeEvent::Ipc {
                envelope,
                response_sender,
            } => {
                let command_result = self.submit_command(envelope);
                // A closed reply channel means the connection thread is gone;
                // the command has already applied, so there is nothing to undo.
                let _ = response_sender.send(command_result);
            }
            RuntimeEvent::IpcAttach {
                resume_client_id,
                resume_token,
                viewport_size,
                pane_area,
                cell_size,
                event_filter,
                attached_at,
                is_remote,
                response_sender,
            } => {
                // The client and its subscription are registered together here,
                // so the structure in the answer and the queue's first event
                // describe one continuous state.
                let _ = response_sender.send(self.handle_ipc_attach_with_cell_size(
                    resume_client_id,
                    resume_token,
                    viewport_size,
                    pane_area,
                    cell_size,
                    event_filter,
                    attached_at,
                    is_remote,
                ));
            }
            RuntimeEvent::IpcDiscovery { response_sender } => {
                let _ = response_sender.send(self.build_overview());
            }
            RuntimeEvent::IpcLayout {
                tab_id,
                response_sender,
            } => {
                let _ = response_sender.send(self.build_session_layout(tab_id));
            }
            // The verdict is answered here and the swap runs after the loop
            // ends, so the caller reads the reply on a socket that is still up.
            RuntimeEvent::IpcRestart { response_sender } => {
                let _ = response_sender.send(self.handle_ipc_restart());
            }
            RuntimeEvent::DropUnclaimedClients {
                unclaimed_client_deadline,
            } => {
                let events = self.handle_drop_unclaimed_clients(unclaimed_client_deadline);
                self.publish_events(&events);
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
    pub fn next_render_wakeup(&self, current_time: Instant) -> Option<Duration> {
        let animation_wakeup = self
            .terminal_engine_by_pane_id
            .values()
            .filter_map(TerminalEngine::get_next_image_animation_delay)
            .map(|delay| {
                delay.saturating_sub(current_time.saturating_duration_since(self.animation_clock))
            })
            .min();
        let synchronized_output_wakeup = self
            .terminal_engine_by_pane_id
            .values()
            .filter_map(|terminal_engine| {
                terminal_engine.get_next_synchronized_output_delay(current_time)
            })
            .min();
        [
            self.render_scheduler.next_wakeup(current_time),
            animation_wakeup,
            synchronized_output_wakeup,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether a render is due at `current_time`. When `true`, the scheduler records the
    /// render and clears its pending reasons, so the caller must repaint.
    pub fn poll_render(&mut self, current_time: Instant) -> bool {
        let expired_pane_ids: Vec<PaneId> = self
            .terminal_engine_by_pane_id
            .iter()
            .filter_map(|(pane_id, engine)| {
                (engine.get_next_synchronized_output_delay(current_time) == Some(Duration::ZERO))
                    .then_some(*pane_id)
            })
            .collect();
        for pane_id in expired_pane_ids {
            self.expire_synchronized_output(pane_id, current_time);
        }
        let elapsed_duration = current_time.saturating_duration_since(self.animation_clock);
        self.animation_clock = current_time;
        let has_animation_changes = self
            .terminal_engine_by_pane_id
            .values_mut()
            .any(|terminal_engine| terminal_engine.advance_image_animations(elapsed_duration));
        if has_animation_changes {
            self.render_scheduler.invalidate();
        }
        self.render_scheduler.poll(current_time)
    }

    /// Whether any pane's PTY is still live — the loop exits once none remain.
    pub fn has_active_panes(&self) -> bool {
        !self.pty_handle_by_pane_id.is_empty()
    }

    /// Immediately group-kill every live pane's child (`KillPolicy::Tree`),
    /// reaping any descendants so none is orphaned. The abrupt teardown for the
    /// panic path — no grace window while unwinding; the normal quit path takes
    /// the staged [`Server::shutdown`].
    pub fn kill_all_panes(&mut self) {
        let backend = Arc::clone(self.get_pty_backend());
        for pane_id in self.pty_handle_by_pane_id.keys().copied() {
            let _ = backend.kill_pane(pane_id, KillPolicy::Tree);
        }
    }
}

#[cfg(test)]
mod tests;
