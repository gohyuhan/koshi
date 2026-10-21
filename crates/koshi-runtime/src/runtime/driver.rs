//! The loop-facing surface: the thin methods the binary's event loop calls to
//! run one dispatch pass, route one inbox event to its handler, time renders,
//! decide when to repaint, push the frames that are due, tell whether any pane
//! is still live, and group-kill every child when the loop panics (the normal
//! quit path takes the staged [`Server::shutdown`]). They wrap the frame
//! paces, the timed-render deadlines, the PTY maps, and the handlers, which
//! are crate-private; the loop lives outside this crate.

use std::ops::ControlFlow;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koshi_core::command::{Command, CommandResult, CommandSource};
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::process::KillPolicy;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// What one [`run_dispatch_pass`](Server::run_dispatch_pass) ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPassOutcome {
    /// The pass ran, and no event it handled was a [`RuntimeEvent::Quit`].
    Served,
    /// The pass ran, and an event it handled was a [`RuntimeEvent::Quit`].
    QuitEventHandled,
    /// The inbox lost its last sender while the pass waited on it. The pass
    /// handled nothing.
    InboxClosed,
}

impl Server {
    /// Run one dispatch pass, in this order:
    ///
    /// 1. When no pane's output queue holds output or an exit, wait for one
    ///    inbox event, at most until
    ///    [`compute_next_render_wakeup`](Self::compute_next_render_wakeup).
    /// 2. Handle that event, then every event the inbox already holds: keys,
    ///    IPC requests, resizes, timers, and output wakes. A run of
    ///    [`RuntimeEvent::Resize`] events with no other event between them
    ///    applies each client's last resize of the run only.
    /// 3. Give the first pane in the turn rotation one turn of at most
    ///    [`PANE_OUTPUT_TURN_BYTE_LIMIT`](crate::runtime::pty_inbox::PANE_OUTPUT_TURN_BYTE_LIMIT)
    ///    bytes, then handle every event the inbox holds again, as step 2
    ///    does. A pane whose queue still holds output goes back to the end of
    ///    the rotation, behind every pane whose wake arrived during its turn. A
    ///    pass that handled a [`RuntimeEvent::Quit`] in step 2 gives no turn.
    /// 4. Push the frames that are due ([`push_due_frames`](Self::push_due_frames)).
    ///
    /// Example — pane `A` holds 1 MiB, and the wake of pane `B`, which holds
    /// 10 bytes, arrives during `A`'s turn: the next pass gives `B` its turn.
    ///
    /// Example — 30 resizes from client `1` wait in the inbox: the pass applies
    /// the 30th size only, and each pane's PTY and terminal grid change once.
    pub fn run_dispatch_pass(&mut self) -> DispatchPassOutcome {
        let mut waited_event = None;
        if self.ready_output_pane_ids.is_empty() {
            waited_event = match self.compute_next_render_wakeup(Instant::now()) {
                Some(render_wakeup_timeout) => {
                    match self
                        .get_inbox_receiver()
                        .recv_timeout(render_wakeup_timeout)
                    {
                        Ok(runtime_event) => Some(runtime_event),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => {
                            return DispatchPassOutcome::InboxClosed
                        }
                    }
                }
                None => match self.get_inbox_receiver().recv() {
                    Ok(runtime_event) => Some(runtime_event),
                    Err(_) => return DispatchPassOutcome::InboxClosed,
                },
            };
        }
        let mut has_quit_event = self.handle_runtime_events(waited_event);
        if !has_quit_event {
            let served_pane_id = self.serve_output_turn();
            has_quit_event |= self.handle_runtime_events(None);
            if let Some(served_pane_id) = served_pane_id {
                self.ready_output_pane_ids.push_back(served_pane_id);
            }
        }
        self.push_due_frames(Instant::now());
        if has_quit_event {
            DispatchPassOutcome::QuitEventHandled
        } else {
            DispatchPassOutcome::Served
        }
    }

    /// Handle `first_runtime_event`, then every event the inbox holds now,
    /// without waiting, in arrival order. Returns whether one of them was a
    /// [`RuntimeEvent::Quit`].
    ///
    /// A [`RuntimeEvent::Resize`] waits until the next event of another kind
    /// arrives or the inbox is empty. Each waiting resize replaces the one
    /// waiting for the same client, and the waiting resizes then apply in the
    /// order of their arrival. Example — resizes `A1`, `B1`, `A2`, then a key:
    /// `B1` applies, then `A2`, then the key.
    fn handle_runtime_events(&mut self, mut first_runtime_event: Option<RuntimeEvent>) -> bool {
        let mut has_quit_event = false;
        let mut waiting_resize_events: Vec<(ClientId, RuntimeEvent)> = Vec::new();
        loop {
            let Some(runtime_event) = first_runtime_event
                .take()
                .or_else(|| self.get_inbox_receiver().try_recv().ok())
            else {
                break;
            };
            if let RuntimeEvent::Resize { client_id, .. } = runtime_event {
                waiting_resize_events
                    .retain(|(waiting_client_id, _)| *waiting_client_id != client_id);
                waiting_resize_events.push((client_id, runtime_event));
                continue;
            }
            for (_, resize_event) in waiting_resize_events.drain(..) {
                has_quit_event |= self.handle_runtime_event(resize_event).is_break();
            }
            has_quit_event |= self.handle_runtime_event(runtime_event).is_break();
        }
        for (_, resize_event) in waiting_resize_events {
            has_quit_event |= self.handle_runtime_event(resize_event).is_break();
        }
        has_quit_event
    }

    /// Route one inbox event to its handler, publishing whatever events the
    /// handler emits. Returns [`ControlFlow::Break`] when the event is a quit
    /// request, which each loop reads on its own terms. A [`RuntimeEvent::Quit`]
    /// is a terminal hangup: it breaks the loop and leaves teardown on the
    /// graceful path. Explicit quit travels through the `core:quit` command.
    pub fn handle_runtime_event(&mut self, runtime_event: RuntimeEvent) -> ControlFlow<()> {
        match runtime_event {
            RuntimeEvent::Quit => return ControlFlow::Break(()),
            // The output itself waits in the queue; the pane gets its turns in
            // the dispatch pass.
            RuntimeEvent::PtyReadable { pane_output_queue } => {
                self.handle_pty_readable(pane_output_queue);
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
            // reads the whole event: the receiving pane's keyboard flags decide
            // which of its fields reach that pane.
            RuntimeEvent::ClientKeyboard {
                client_id,
                key_input,
            } => {
                self.allow_client_frame_now(client_id);
                self.handle_key_input(client_id, &key_input);
            }
            // An attached client's viewer already read this mouse event against
            // the frame it painted: the round names every pane it touches. The
            // round is answered on that client's own queue.
            RuntimeEvent::ClientMouse {
                client_id,
                request_id,
                mouse_actions,
            } => {
                self.allow_client_frame_now(client_id);
                self.run_client_mouse(client_id, request_id, mouse_actions);
            }
            // A raw mouse event is the viewer's to read: only the frame it
            // painted says which pane the pointer is over and which gesture is
            // under way. One arriving here belongs to no attached viewer, and
            // is dropped.
            RuntimeEvent::MouseInput { client_id, .. } => {
                tracing::debug!(%client_id, "dropping a mouse event no attached viewer answered");
            }
            RuntimeEvent::OuterTerminalFocusLost { client_id } => {
                tracing::debug!(%client_id, "dropping an outer-terminal focus event");
            }
            RuntimeEvent::HostPaste {
                client_id,
                pasted_text,
            } => {
                self.allow_client_frame_now(client_id);
                self.handle_host_paste(client_id, &pasted_text);
            }
            RuntimeEvent::ClientDetached {
                client_id,
                detached_at,
                is_streamed,
                is_returning,
            } => {
                // The view is filed before the detach, which removes the record
                // it is read from.
                if is_streamed {
                    self.save_client_view(client_id, detached_at);
                } else {
                    self.saved_view_store.forget_client_resume_token(client_id);
                }
                let client_detach_events = if is_returning {
                    self.handle_returning_client_detach(client_id)
                } else {
                    self.handle_client_detach(client_id)
                };
                self.publish_events(&client_detach_events);
            }
            RuntimeEvent::Resize {
                client_id,
                viewport_size,
                pane_area,
                cell_size,
            } => {
                let resize_events =
                    self.handle_client_resize(client_id, viewport_size, pane_area, cell_size);
                self.publish_events(&resize_events);
            }
            RuntimeEvent::CellSize {
                client_id,
                cell_size,
            } => self.handle_client_cell_size(client_id, cell_size),
            RuntimeEvent::Ipc {
                command_envelope,
                response_sender,
            } => {
                if let CommandSource::KeyBinding { client_id }
                | CommandSource::Mouse { client_id } = command_envelope.command_source
                {
                    self.allow_client_frame_now(client_id);
                }
                let placement_client_id =
                    match (&command_envelope.command_source, &command_envelope.command) {
                        (
                            CommandSource::KeyBinding { client_id },
                            Command::PlacePane(_) | Command::MoveFloatingPane(_),
                        ) => Some(*client_id),
                        _ => None,
                    };
                let command_result = self.submit_command(*command_envelope);
                if let (Some(client_id), CommandResult::Rejected { command_id, .. }) =
                    (placement_client_id, &command_result)
                {
                    let _ = self.send_placement_command_rejection(client_id, *command_id);
                }
                // A closed reply channel means the connection thread is gone.
                // The command has already applied and stays applied.
                let _ = response_sender.send(command_result);
            }
            RuntimeEvent::IpcAttach {
                resume_client_id,
                resume_token,
                viewport_size,
                pane_area,
                cell_size,
                attached_at,
                is_remote,
                max_frames_per_second,
                response_sender,
            } => {
                // The client and its subscription are registered together here:
                // the structure in the answer and the queue's first event
                // describe one continuous state.
                let _ = response_sender.send(self.handle_ipc_attach(
                    resume_client_id,
                    resume_token,
                    viewport_size,
                    pane_area,
                    cell_size,
                    attached_at,
                    is_remote,
                    max_frames_per_second,
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
            RuntimeEvent::ReadPanePlacement {
                client_id,
                request_id,
                source_pane_id,
                destination_tab_id,
            } => {
                self.send_placement_snapshot(
                    client_id,
                    request_id,
                    source_pane_id,
                    destination_tab_id,
                );
            }
            // The restart is taken here, and the swap runs after the loop
            // ends. The caller reads the reply on a socket that is still up.
            RuntimeEvent::IpcRestart { response_sender } => {
                self.is_restart_requested = true;
                let _ = response_sender.send(());
            }
            RuntimeEvent::IpcClipboardCopyPart {
                held_copy_byte_count,
                response_sender,
            } => {
                let _ =
                    response_sender.send(self.can_take_clipboard_tool_copy(held_copy_byte_count));
            }
            RuntimeEvent::IpcPrepare { response_sender } => {
                let _ = response_sender.send(());
            }
            RuntimeEvent::IpcClipboardCopyPrepare {
                pane_id,
                copied_byte_count,
                response_sender,
            } => {
                let _ = response_sender
                    .send(self.can_route_clipboard_tool_copy(pane_id, copied_byte_count));
            }
            RuntimeEvent::IpcClipboardCopy {
                pane_id,
                clipboard_targets,
                copied_bytes,
                response_sender,
            } => {
                let is_routed =
                    self.route_clipboard_tool_copy(pane_id, &clipboard_targets, &copied_bytes);
                let _ = response_sender.send(!is_routed);
            }
            RuntimeEvent::DropUnclaimedClients {
                unclaimed_client_deadline,
            } => {
                let unclaimed_client_drop_events =
                    self.handle_drop_unclaimed_clients(unclaimed_client_deadline);
                self.publish_events(&unclaimed_client_drop_events);
            }
            // The frame the subscriber is owed goes from this pass's
            // `push_due_frames`, at the subscriber's pace.
            RuntimeEvent::QueueRoomFreed => {}
        }
        ControlFlow::Continue(())
    }

    /// How long the loop may block at `current_time` before
    /// [`push_due_frames`](Self::push_due_frames) must run: `None` to sleep
    /// until an event, else the shortest of three waits (`Some(ZERO)` to run
    /// now):
    ///
    /// 1. For each subscriber that can take a frame and whose client's view
    ///    changed, the time until its frame pace lets a frame go.
    /// 2. The time until the earliest end of any pane's open
    ///    synchronized-output group.
    /// 3. For each pane with a running image animation that the client of a
    ///    subscriber that can take a frame shows, the first time on one of
    ///    those subscribers' frame grids
    ///    (`FramePace::find_grid_frame_time`)
    ///    at or after both its next animation frame and `current_time`. An
    ///    animation whose clock has not started is due at once. An animation
    ///    that no such client shows sets no wait.
    ///
    /// Reads no terminal engine.
    ///
    /// Example — one client at 60 frames a second, which got a frame at
    /// `t`, shows a pane whose animation frame lasts 8 ms: the wait at `t` is
    /// 16666667 ns, not 8 ms. A wake at which the animation shows the same
    /// frame again sends nothing, and the next wait runs to the next grid
    /// time, `t + 33333334 ns`.
    pub fn compute_next_render_wakeup(&self, current_time: Instant) -> Option<Duration> {
        let frame_wakeup = self
            .subscriptions
            .iter()
            .filter(|frame_subscription| {
                self.event_bus
                    .can_take_frame(frame_subscription.subscriber_id)
            })
            .filter_map(|frame_subscription| {
                frame_subscription
                    .frame_pace
                    .compute_frame_wait(current_time)
            })
            .min();
        let synchronized_output_wakeup = self
            .pane_synchronized_output_deadlines
            .find_earliest_render_deadline()
            .map(|render_deadline| render_deadline.saturating_duration_since(current_time));
        let animation_wakeup = self
            .find_shown_animation_wakeup_at(current_time)
            .map(|animation_wakeup_at| animation_wakeup_at.saturating_duration_since(current_time));
        [frame_wakeup, synchronized_output_wakeup, animation_wakeup]
            .into_iter()
            .flatten()
            .min()
    }

    /// The earliest time at which an animation frame can reach a client, as
    /// step 3 of [`compute_next_render_wakeup`](Self::compute_next_render_wakeup)
    /// states, or `None` when no client that can take a frame shows a pane
    /// with a running animation. Reads the panes in deadline order, and stops
    /// at the first deadline that is not earlier than the best time found.
    fn find_shown_animation_wakeup_at(&self, current_time: Instant) -> Option<Instant> {
        let mut earliest_wakeup_at: Option<Instant> = None;
        for (animation_deadline, pane_id) in self.pane_animation_deadlines.list_render_deadlines() {
            if earliest_wakeup_at.is_some_and(|wakeup_at| animation_deadline >= wakeup_at) {
                break;
            }
            let grid_wakeup_at = self
                .subscriptions
                .iter()
                .filter(|frame_subscription| {
                    self.event_bus
                        .can_take_frame(frame_subscription.subscriber_id)
                        && self.is_pane_shown_to_client(frame_subscription.client_id, pane_id)
                })
                .map(|frame_subscription| {
                    frame_subscription
                        .frame_pace
                        .find_grid_frame_time(animation_deadline.max(current_time))
                })
                .min();
            let Some(wakeup_at) = grid_wakeup_at else {
                continue;
            };
            if earliest_wakeup_at.is_none_or(|earliest_wakeup_at| wakeup_at < earliest_wakeup_at) {
                earliest_wakeup_at = Some(wakeup_at);
            }
        }
        earliest_wakeup_at
    }

    /// Run every pane's timed render that is due at `current_time`: for each
    /// pane whose synchronized-output end or animation frame is due, releases
    /// its open synchronized-output group when its time ran out, advances its
    /// image animations to `current_time` (an animation whose clock has not
    /// started starts it at `current_time`), and records its next timed
    /// renders. A changed animation frame marks the view of each subscriber
    /// whose client shows the pane
    /// (`mark_views_showing_pane`).
    ///
    /// No other pane's engine is read. A due pane that no client shows
    /// advances here too, on a pass that runs for another reason. A client
    /// that starts to show it sees the animation frame its full elapsed time
    /// selects.
    pub fn advance_due_pane_renders(&mut self, current_time: Instant) {
        let mut due_pane_ids = self
            .pane_synchronized_output_deadlines
            .take_due_pane_ids(current_time);
        for pane_id in self
            .pane_animation_deadlines
            .take_due_pane_ids(current_time)
        {
            if !due_pane_ids.contains(&pane_id) {
                due_pane_ids.push(pane_id);
            }
        }
        for pane_id in due_pane_ids {
            let is_synchronized_output_expired = self
                .terminal_engine_by_pane_id
                .get(&pane_id)
                .is_some_and(|terminal_engine| {
                    terminal_engine.get_next_synchronized_output_delay(current_time)
                        == Some(Duration::ZERO)
                });
            if is_synchronized_output_expired {
                self.expire_synchronized_output(pane_id, current_time);
            }
            let has_animation_change = self
                .terminal_engine_by_pane_id
                .get_mut(&pane_id)
                .is_some_and(|terminal_engine| {
                    terminal_engine.advance_image_animations(current_time)
                });
            if has_animation_change {
                self.mark_views_showing_pane(pane_id);
            }
            self.refresh_pane_render_deadlines(pane_id, current_time);
        }
    }

    /// Record `pane_id`'s next timed renders, read from its engine at
    /// `current_time`: its next image animation frame, and the end of its open
    /// synchronized-output group. A pane with neither, or with no engine, has
    /// no timed render.
    pub(crate) fn refresh_pane_render_deadlines(&mut self, pane_id: PaneId, current_time: Instant) {
        let terminal_engine = self.terminal_engine_by_pane_id.get(&pane_id);
        let animation_deadline = terminal_engine
            .and_then(|terminal_engine| {
                terminal_engine.get_next_image_animation_delay(current_time)
            })
            .map(|animation_delay| current_time + animation_delay);
        let synchronized_output_deadline = terminal_engine
            .and_then(|terminal_engine| {
                terminal_engine.get_next_synchronized_output_delay(current_time)
            })
            .map(|synchronized_output_delay| current_time + synchronized_output_delay);
        self.pane_animation_deadlines
            .set_pane_render_deadline(pane_id, animation_deadline);
        self.pane_synchronized_output_deadlines
            .set_pane_render_deadline(pane_id, synchronized_output_deadline);
    }

    /// Run the timed renders due at `current_time`
    /// ([`advance_due_pane_renders`](Self::advance_due_pane_renders)), hand a
    /// fresh snapshot to each paused subscriber that can take one
    /// ([`resync_lagged`](Self::resync_lagged)), then put each client's
    /// current frame on its queue when its frame is due
    /// ([`push_frames`](Self::push_frames)). The snapshots and the frames are
    /// built from the same state. A serve loop calls it once per pass, after
    /// the events it read.
    pub fn push_due_frames(&mut self, current_time: Instant) {
        self.advance_due_pane_renders(current_time);
        self.resync_lagged(current_time);
        self.push_frames(current_time);
    }

    /// Whether any pane's PTY is still live — the loop exits once none remain.
    pub fn has_active_panes(&self) -> bool {
        !self.live_pane_ids.is_empty()
    }

    /// Immediately group-kill every live pane's child (`KillPolicy::Tree`),
    /// reaping every descendant. The abrupt teardown for the panic path — no
    /// grace window while unwinding; the normal quit path takes the staged
    /// [`Server::shutdown`].
    pub fn kill_all_panes(&mut self) {
        let pty_backend = Arc::clone(self.get_pty_backend());
        for pane_id in self.live_pane_ids.iter().copied() {
            let _ = pty_backend.kill_pane(pane_id, KillPolicy::Tree);
        }
    }
}

#[cfg(test)]
mod tests;
