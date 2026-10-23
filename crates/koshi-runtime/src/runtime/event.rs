//! The runtime event inbox.
//!
//! [`RuntimeEvent`] is the single typed channel the dispatcher thread drains.
//! Every asynchronous trigger the runtime must react to — a pane with child
//! output or a child exit to read, a client resize, terminal input, an IPC
//! command — arrives as one variant. The dispatcher consumes every trigger from
//! one shared `std::sync::mpsc` inbox. A pane's output and exit themselves wait
//! in that pane's [`PaneOutputQueue`].
//!
//! These are *input* triggers, distinct from the *output* facts the dispatcher
//! emits ([`koshi_core::event::Event`]): an exit taken from a
//! [`RuntimeEvent::PtyReadable`] queue is the raw notification that a child
//! died, while the emitted `PaneProcessExited` is the resulting domain fact.
//!
//! The inbox stays in-process: producers send into it directly, and
//! `RuntimeEvent` is not `Serialize`.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Instant, SystemTime};

use koshi_core::{
    command::{CommandEnvelope, CommandResult, TiledPlacementTarget},
    discovery::SessionOverview,
    geometry::{PaneArea, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
    key::KeyInput,
    mouse::MouseInput,
};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::placement::PanePlacementPreviewTarget;
use koshi_ipc::protocol::{ConnectionToken, WireMouseAction};
use koshi_layout::placement::PlacedTabLayout;

use crate::runtime::bus::WriterHandoff;
use crate::runtime::delivery_queue::DeliveryReceiver;
use crate::runtime::pty_inbox::PaneOutputQueue;

/// A trigger the dispatcher thread reacts to, drained from the runtime inbox.
///
/// One variant per runtime event source. Construction is the producer's job
/// (the per-pane PTY threads, the input reader, the IPC server); the
/// dispatcher matches on the variant to decide what to mutate and which
/// [`koshi_core::event::Event`] facts to emit.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// A pane's output queue went from empty to holding child output or the
    /// child's exit. The inbox holds at most one of these per queue.
    PtyReadable {
        /// The queue holding the pane's output and exit.
        pane_output_queue: Arc<PaneOutputQueue>,
    },
    /// A client's outer terminal changed size.
    Resize {
        /// Client whose terminal was resized.
        client_id: ClientId,
        /// The client's new size in cells, before size reconciliation.
        viewport_size: Size,
        /// The pane region the client draws the tab's panes in at the new
        /// size; `None` replaces any earlier report.
        pane_area: Option<PaneArea>,
        /// The cell dimensions measured for the new viewport, or `None` when
        /// the previous measurement must be cleared.
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
    },
    /// A client measured the pixel dimensions of one terminal cell.
    CellSize {
        /// The client that supplied the measurement.
        client_id: ClientId,
        /// The nonzero pixel dimensions of one cell.
        cell_size: koshi_core::geometry::PixelCellSize,
    },
    /// A client left, stopping its view of whatever tab it held.
    ClientDetached {
        /// The departing client.
        client_id: ClientId,
        /// When the producer saw the connection end. The handler reads this
        /// value, not the clock.
        detached_at: SystemTime,
        /// Whether the connection carrying this client reached its event
        /// stream. `false` from the attach reply failing to write, which hands
        /// the client no token; the view it was looking at is dropped.
        is_streamed: bool,
        /// Whether the client may attach again: the `is_returning` of the
        /// `Leaving` request that ended the connection. With no such request,
        /// it is `true` for a connection from another machine. For a
        /// connection on this machine, it is `true` when the connection ended
        /// before its first request after an `Attach` that named
        /// `resume_client_id`, and `false` otherwise. `true` removes the client
        /// with no `auto-close-session` quit. `false` lets `auto-close-session`
        /// end a session the client leaves with no client.
        is_returning: bool,
    },
    /// A request to stop the event loop and shut the process down. Produced
    /// when reading a client's outer terminal fails, which is that terminal
    /// reaching end of stream. Explicit quit travels through the `core:quit`
    /// command instead.
    Quit,
    /// One decoded outer-terminal key awaiting keybinding resolution.
    ///
    /// Carries everything the terminal reported, including the key kinds and
    /// fields no chord can hold. The viewer projects it to a chord for the
    /// keymap lookup, and a key that projects to no chord resolves nothing.
    KeyInput {
        /// Client whose terminal produced the key.
        client_id: ClientId,
        /// The complete key event the client's terminal reported.
        key_input: KeyInput,
    },
    /// One keyboard event an attached client sent for the pane it is typing
    /// into: its keymap bound nothing to the event, or no chord could name it.
    ///
    /// Carries every field the client's terminal reported, a release and a key
    /// no keybinding can name included.
    ClientKeyboard {
        /// Client whose keymap left the event unbound.
        client_id: ClientId,
        /// The keyboard event the client read from its terminal.
        key_input: KeyInput,
    },
    /// One round of mouse actions an attached client's viewer decided for one
    /// host mouse event, in the order the session must run them.
    ///
    /// The round is answered exactly once, on the client's own event queue,
    /// and the answer carries `request_id` back.
    ClientMouse {
        /// Client whose viewer decided the round.
        client_id: ClientId,
        /// The `request_id` the round arrived under, repeated in its answer.
        request_id: u64,
        /// What to run, in order. An empty round is answered like any other.
        mouse_actions: Vec<WireMouseAction>,
    },
    /// One decoded outer-terminal mouse event awaiting the viewer's answer.
    /// Carries the event alone: which pane it lands on, which gesture it
    /// continues, and what it means are all read from the frame the viewer
    /// painted.
    ///
    /// It travels the same inbox as [`KeyInput`](Self::KeyInput): the two arrive
    /// in the order the user produced them. Toggling mouse-select and then
    /// pressing is answered in that order.
    MouseInput {
        /// Client whose terminal produced the mouse event.
        client_id: ClientId,
        /// The decoded event: kind, cell position, and modifiers.
        mouse_input: MouseInput,
    },
    /// The outer terminal lost focus while this client was attached.
    OuterTerminalFocusLost {
        /// Client whose outer terminal lost focus.
        client_id: ClientId,
    },
    /// Text the client's outer terminal pasted — the OS paste key pressed in
    /// the terminal koshi runs in, delivered whole. No character of it fires a
    /// keybinding.
    HostPaste {
        /// Client whose terminal pasted.
        client_id: ClientId,
        /// The pasted text, exactly as the outer terminal delivered it.
        pasted_text: String,
    },
    /// A command delivered over the IPC socket, from external or in-session
    /// CLI. Carries the reply sender the connection thread waits on: the
    /// dispatcher sends the command's result into it, and the connection
    /// thread writes that result back over the socket.
    Ipc {
        /// The command as it arrived over the socket.
        command_envelope: Box<CommandEnvelope>,
        /// Where the dispatcher sends the command's result.
        response_sender: Sender<CommandResult>,
    },
    /// An attach request delivered over the IPC socket: the caller asks to
    /// join the running session as a viewing client. Carries the reply sender
    /// the connection thread waits on; the dispatcher registers the client and
    /// its event subscription in one turn and answers with
    /// [`AttachAccepted`], or with `None` when no session is running.
    IpcAttach {
        /// The client record the caller asks to come back as, after the
        /// session replaced its own process image. The dispatcher hands that
        /// record back when it still holds it, the tab that record was viewing
        /// still exists, no connection is streaming for it, and `resume_token`
        /// is the token that record's last attach minted, and mints a fresh
        /// client otherwise.
        resume_client_id: Option<ClientId>,
        /// The token the caller's last attach minted, presented to get that
        /// attach's view back
        /// ([`SavedView`](crate::runtime::saved_view::SavedView)), and to
        /// prove a `resume_client_id` claim. The dispatcher hands that view
        /// back when it still holds one under this token, and mints a fresh
        /// view otherwise. Absent on a first attach.
        resume_token: Option<ConnectionToken>,
        /// The caller's terminal size in cells, recorded as the client's
        /// viewport.
        viewport_size: Size,
        /// The pane region the client reported, recorded on its record.
        pane_area: Option<PaneArea>,
        /// The cell dimensions measured before this attach, if available.
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
        /// When the producer received the request. The handler reads this
        /// value, not the clock.
        attached_at: SystemTime,
        /// Whether the connection carrying this attach reached the session
        /// from another machine. The client is minted with it as its origin.
        /// The router marks the Hello it sends for a remote caller; every
        /// other connection leaves it `false`.
        is_remote: bool,
        /// The most frames a second the session sends this client, from `30`
        /// to `120`: the client's subscription gets a frame at most once per
        /// [`compute_frame_interval`](koshi_core::constant::compute_frame_interval)
        /// of it.
        max_frames_per_second: u16,
        /// The placement target the client previews from its first frame on
        /// (`Server::set_pane_placement_preview`), or `None`.
        placement_preview_target: Option<PanePlacementPreviewTarget>,
        /// Where the dispatcher sends what it minted.
        response_sender: Sender<Option<AttachAccepted>>,
    },
    /// A discovery request delivered over the IPC socket: the caller asks
    /// this process to describe its session. Carries the reply sender the
    /// connection thread waits on; the dispatcher answers with the overview
    /// built from live state, or `None` when no session is running.
    IpcDiscovery {
        /// Where the dispatcher sends the overview.
        response_sender: Sender<Option<SessionOverview>>,
    },
    /// A layout request delivered over the IPC socket: the caller asks this
    /// process to describe how its session arranges panes. Carries the reply
    /// sender the connection thread waits on; the dispatcher answers with the
    /// layout built from live state, or `None` when no session is running.
    IpcLayout {
        /// The one tab to describe, or every tab when absent.
        tab_id: Option<TabId>,
        /// Where the dispatcher sends the layout.
        response_sender: Sender<Option<SessionLayout>>,
    },
    /// A client names the placement target whose preview its frames carry,
    /// or ends its preview with `None`
    /// (`Server::set_pane_placement_preview`).
    SetPanePlacementPreview {
        /// The client whose frames carry the preview.
        client_id: ClientId,
        /// The pane and the destination tab the client previews.
        placement_target: Option<PanePlacementPreviewTarget>,
    },
    /// A client confirms one tiled pane placement that it previews
    /// (`Server::confirm_pane_placement`).
    ConfirmPanePlacement {
        /// The client that confirmed the placement.
        client_id: ClientId,
        /// The id the client chose for the placement.
        command_id: CommandId,
        /// The pane being placed.
        source_pane_id: PaneId,
        /// Where the pane goes.
        placement_target: TiledPlacementTarget,
        /// The destination tab as the client draws it after the placement.
        expected_placed_tab_layout: Box<PlacedTabLayout>,
    },
    /// A restart request whose restart check passed on the thread that asks:
    /// the caller asks this process to replace its own image with the binary at
    /// the path it started from. The dispatcher sets `is_restart_requested` and
    /// answers `()` on `response_sender`. The swap runs after the event loop
    /// ends.
    IpcRestart {
        /// Where the dispatcher answers that it took the restart.
        response_sender: Sender<()>,
    },
    /// A stand-in clipboard tool asks whether the session takes one more part
    /// of a copy, which brings the bytes the connection holds for that copy to
    /// `held_copy_byte_count`. The dispatcher answers `true` while
    /// `pane-clipboard-writes` allows writes and `held_copy_byte_count` is at
    /// most `max-copy-bytes`.
    IpcClipboardCopyPart {
        /// The bytes of the copy the connection would hold with the part.
        held_copy_byte_count: usize,
        /// Where the dispatcher sends its answer.
        response_sender: Sender<bool>,
    },
    /// A caller hands the session a request to hold until its `Commit`. The
    /// dispatcher answers at once.
    IpcPrepare {
        /// Where the dispatcher sends its answer.
        response_sender: Sender<()>,
    },
    /// A stand-in clipboard tool in `pane_id` hands the session a copy of
    /// `copied_byte_count` bytes to hold until its `Commit`. The dispatcher
    /// answers whether a client would take the copy: `true` when
    /// `pane-clipboard-writes` allows writes, `copied_byte_count` is at most
    /// `max-copy-bytes`, and a client on another machine focuses `pane_id`.
    IpcClipboardCopyPrepare {
        /// The pane the tool runs in.
        pane_id: PaneId,
        /// Every byte of the copy, counted.
        copied_byte_count: usize,
        /// Where the dispatcher sends its answer.
        response_sender: Sender<bool>,
    },
    /// A stand-in clipboard tool in `pane_id` hands the session a whole copy.
    /// Each client on another machine whose focused pane is `pane_id` takes the
    /// copy as an `OSC 52` host write. The dispatcher answers whether the tool
    /// runs the real clipboard tool: `false` when at least one client focuses
    /// `pane_id`, every one of them is on another machine, and each one took
    /// the host write, and `true` in every other case.
    IpcClipboardCopy {
        /// The pane the tool runs in.
        pane_id: PaneId,
        /// The `OSC 52` targets the tool names, before the session filters them.
        clipboard_targets: String,
        /// Every byte of the copy.
        copied_bytes: Vec<u8>,
        /// Where the dispatcher sends `should_run_clipboard_tool`.
        response_sender: Sender<bool>,
    },
    /// The grace window for the clients whose records came across an image
    /// swap has closed. The dispatcher detaches every one of those clients
    /// that has not attached again, and does nothing when they all have.
    DropUnclaimedClients {
        /// When the window closed, supplied by the producer. The handler reads
        /// this value, not the clock, and files each dropped client's view
        /// as detached at it.
        unclaimed_client_deadline: SystemTime,
    },
    /// The thread that writes one client's connection read a delivery off a
    /// queue where a frame or resync snapshot had found no room
    /// ([`WriterHandoff::take_owed_frame_mark`]). The event starts a pass of
    /// the serve loop, which handles nothing for it. The frame that did not
    /// fit left its view change in place. The pass's
    /// [`Server::push_due_frames`](crate::server::Server::push_due_frames)
    /// then puts the client's current frame on the queue once its frame pace
    /// lets a frame go. A paused subscriber takes its resync snapshot on the same
    /// pass instead.
    QueueRoomFreed,
}

/// What the dispatcher minted for one [`RuntimeEvent::IpcAttach`]: the client
/// record, the session it joined, the structure built for the attach reply, the
/// receiving end of the client's own event queue, what waits beside that queue
/// for the client's writing thread, and the session's shared ending notice.
///
/// The whole of it comes out of one dispatcher turn: the structure names the
/// same state the queue's first event follows.
#[derive(Debug)]
pub struct AttachAccepted {
    /// The id the dispatcher minted for this client.
    pub client_id: ClientId,
    /// The session the client joined.
    pub session_id: SessionId,
    /// What the session contains, built for this reply.
    pub session_structure: AttachedSessionStructureSnapshot,
    /// The client's event queue. Dropping it ends the subscription.
    pub deliveries: DeliveryReceiver,
    /// Shared with the session. This client's writing thread takes the bytes
    /// that wait for the client's own terminal from it before each delivery,
    /// and the owed-frame mark after each delivery.
    pub writer_handoff: Arc<WriterHandoff>,
    /// Shared with the session. This client's writing thread reads the session
    /// ending from it even when the queue above is full, and the session reads
    /// from it when that thread has written the last frame.
    pub ending_notice: Arc<EndingNotice>,
    /// The fresh secret this attach minted. Presenting it on the next attach
    /// takes back the view this client leaves behind when it detaches.
    pub resume_token: ConnectionToken,
    /// The pane region the session holds for this client, exactly as the
    /// attach reported it.
    pub pane_area: Option<PaneArea>,
}

/// How a client's event stream ends: the last frame that client's writing
/// thread writes before it stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnding {
    /// The session ended.
    Quit,
    /// The session is replacing its own process image. A client that reads this
    /// waits for the session's new socket and attaches again on it.
    Restarting,
}

/// A count of running work, such as client writing threads or requests
/// waiting for their answer to be written, that a thread can wait on until it
/// reaches zero.
///
/// Example: two writing threads run, and one ends while the other blocks in
/// its write → [`wait_until_none_run`](Self::wait_until_none_run) returns `1`
/// at its deadline.
#[derive(Debug, Default)]
pub(crate) struct RunningCount {
    /// How many are running.
    running_count: Mutex<usize>,
    /// Notified each time one ends.
    run_ended: Condvar,
}

impl RunningCount {
    /// Count one more as running. Paired with
    /// [`record_ended`](Self::record_ended).
    pub(crate) fn record_started(&self) {
        *self.lock_running_count() += 1;
    }

    /// Count one as ended, and wake every thread in
    /// [`wait_until_none_run`](Self::wait_until_none_run).
    pub(crate) fn record_ended(&self) {
        *self.lock_running_count() -= 1;
        self.run_ended.notify_all();
    }

    /// Wait until none runs, or until `deadline` passes, and return how many
    /// still run. A `deadline` already past reads the count without waiting.
    pub(crate) fn wait_until_none_run(&self, deadline: Instant) -> usize {
        let (running_count, _) = self
            .run_ended
            .wait_timeout_while(
                self.lock_running_count(),
                deadline.saturating_duration_since(Instant::now()),
                |running_count| *running_count > 0,
            )
            .unwrap_or_else(PoisonError::into_inner);
        *running_count
    }

    /// The lock on the count, taken back from a thread that panicked while it
    /// held it.
    fn lock_running_count(&self) -> MutexGuard<'_, usize> {
        self.running_count
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the session and every attached client's writing thread share about the
/// session's last frame.
///
/// Publishing that frame raises the notice. A writing thread reads it at the
/// top of each turn: raised, it drops whatever is still queued for that client,
/// writes the frame the notice names, and ends. The queue each client reads is
/// bounded: a published last frame does not reach a client whose queue is
/// full, and the notice reaches that client instead. A client whose queue the
/// server closed already left the session, and its writing thread writes its
/// own goodbye.
///
/// The notice counts the writing threads that have not ended. Each one ends
/// right after it writes the last frame. The session waits in
/// [`wait_until_writers_end`](Self::wait_until_writers_end) for the count to
/// reach zero before it replaces its own process image or tears the process
/// down.
///
/// Before → after: a client's queue holds its full 1024 deliveries when the
/// session quits → the quit does not fit the queue, the writing thread reads
/// the raised notice at its next turn, writes the quit frame, and the client
/// says the session ended instead of reading end of stream.
#[derive(Debug, Default)]
pub struct EndingNotice {
    /// Which frame ends every attached client's stream; empty while the session
    /// serves. Set once: the session is ending by then.
    session_ending: OnceLock<SessionEnding>,
    /// How many client writing threads have started and not yet ended.
    running_writers: RunningCount,
}

impl EndingNotice {
    /// Raise the notice: every attached client's writing thread writes
    /// `session_ending`'s frame at its next turn. The notice keeps the ending it was
    /// raised with first.
    pub fn raise_session_ending(&self, session_ending: SessionEnding) {
        let _ = self.session_ending.set(session_ending);
    }

    /// How every attached client's stream ends, or `None` while the session
    /// serves.
    #[must_use]
    pub fn get_session_ending(&self) -> Option<SessionEnding> {
        self.session_ending.get().copied()
    }

    /// Count one client writing thread as started. Paired with
    /// [`record_writer_ended`](Self::record_writer_ended).
    pub fn record_writer_started(&self) {
        self.running_writers.record_started();
    }

    /// Count one client writing thread as ended, and wake every thread in
    /// [`wait_until_writers_end`](Self::wait_until_writers_end).
    pub fn record_writer_ended(&self) {
        self.running_writers.record_ended();
    }

    /// Wait until no client writing thread is running, or until `deadline`
    /// passes, and return how many still run. A `deadline` already past reads
    /// the count without waiting.
    ///
    /// Example: two writing threads run, and one ends while the other blocks in
    /// its write → the call returns `1` at `deadline`.
    #[must_use]
    pub fn wait_until_writers_end(&self, deadline: Instant) -> usize {
        self.running_writers.wait_until_none_run(deadline)
    }
}

#[cfg(test)]
mod tests;
