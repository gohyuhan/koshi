//! The runtime event inbox.
//!
//! [`RuntimeEvent`] is the single typed channel the dispatcher thread drains.
//! Every asynchronous trigger the runtime must react to — child output, child
//! exit, a client resize, a periodic tick, terminal input, an IPC command, a
//! plugin command — arrives as one variant, so the dispatcher consumes every
//! trigger from one shared `std::sync::mpsc` inbox.
//!
//! These are *input* triggers, distinct from the *output* facts the dispatcher
//! emits ([`koshi_core::event::Event`]): a [`RuntimeEvent::ChildExit`] is the raw
//! notification that a child died, while the emitted `PaneProcessExited` is the
//! resulting domain fact.
//!
//! The inbox stays in-process — producers send into it directly — so
//! `RuntimeEvent` is not `Serialize`, unlike the command and event vocabulary
//! that crosses the IPC socket.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use koshi_core::{
    command::{CommandEnvelope, CommandResult},
    discovery::SessionOverview,
    geometry::{PaneArea, Size},
    ids::{ClientId, PaneId, SessionId, TabId},
    key::KeyChord,
    mouse::MouseInput,
    process::ExitStatus,
};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{ConnectionToken, WireMouseAction};
use koshi_renderer::snapshot::Delivery;

use crate::runtime::bus::EventFilter;

/// A trigger the dispatcher thread reacts to, drained from the runtime inbox.
///
/// One variant per runtime event source. Construction is the producer's job
/// (the per-pane PTY threads, the input reader, the IPC server, the plugin
/// host, the timer); the dispatcher matches on the variant to decide what to
/// mutate and which [`koshi_core::event::Event`] facts to emit.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// Raw bytes a child process wrote to its PTY.
    PtyOutput {
        /// Pane whose child produced the output.
        pane_id: PaneId,
        /// The bytes read from the PTY, fed verbatim to the pane's terminal.
        bytes: Vec<u8>,
    },
    /// A child process ended.
    ChildExit {
        /// Pane whose child exited.
        pane_id: PaneId,
        /// How the child ended: an exit code or a terminating signal.
        status: ExitStatus,
    },
    /// A client's outer terminal changed size.
    Resize {
        /// Client whose terminal was resized.
        client_id: ClientId,
        /// The client's new size in cells, before size reconciliation.
        size: Size,
        /// The pane region the client draws the tab's panes in at the new
        /// size; `None` replaces any earlier report.
        pane_area: Option<PaneArea>,
    },
    /// A client left, stopping its view of whatever tab it held.
    ClientDetached {
        /// The departing client.
        client_id: ClientId,
        /// When the producer saw the connection end, carried on the event so
        /// the handler never reads the clock itself.
        detached_at: SystemTime,
        /// Whether the connection carrying this client reached its event
        /// stream. `false` from the attach reply failing to write, which hands
        /// the client no token; the view it was looking at is dropped.
        streamed: bool,
    },
    /// A periodic tick for time-driven refreshes such as cursor blink.
    Timer,
    /// A request to stop the event loop and shut the process down. Produced
    /// when reading a client's outer terminal fails, which is that terminal
    /// reaching end of stream. Explicit quit travels through the `core:quit`
    /// command instead.
    Quit,
    /// One decoded outer-terminal key awaiting keybinding resolution. Carries
    /// the chord alone: the bytes a fallthrough writes are encoded from it
    /// when they are written, against the receiving pane's mode.
    KeyInput {
        /// Client whose terminal produced the key.
        client_id: ClientId,
        /// Canonical chord used for keymap lookup.
        chord: KeyChord,
    },
    /// One key press an attached client's keymap did not bind, for the pane
    /// that client is typing into.
    ClientKeyPress {
        /// Client whose keymap left the press unbound.
        client_id: ClientId,
        /// The chord the client read from its terminal.
        chord: KeyChord,
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
        actions: Vec<WireMouseAction>,
    },
    /// One decoded outer-terminal mouse event awaiting the viewer's answer.
    /// Carries the event alone: which pane it lands on, which gesture it
    /// continues, and what it means are all read from the frame the viewer
    /// painted.
    ///
    /// It travels the same inbox as [`KeyInput`](Self::KeyInput) so the two stay
    /// in the order the user produced them: toggling mouse-select and then
    /// pressing must be answered in that order.
    MouseInput {
        /// Client whose terminal produced the mouse event.
        client_id: ClientId,
        /// The decoded event: kind, cell position, and modifiers.
        mouse: MouseInput,
    },
    /// Text the client's outer terminal pasted — the OS paste key pressed in
    /// the terminal koshi runs in, delivered whole so no character of it can
    /// fire a keybinding.
    HostPaste {
        /// Client whose terminal pasted.
        client_id: ClientId,
        /// The pasted text, exactly as the outer terminal delivered it.
        text: String,
    },
    /// A command delivered over the IPC socket, from external or in-session
    /// CLI. Carries the reply sender the connection thread waits on: the
    /// dispatcher sends the command's result into it, and the connection
    /// thread writes that result back over the socket.
    Ipc {
        /// The command as it arrived over the socket.
        envelope: CommandEnvelope,
        /// Where the dispatcher sends the command's result.
        reply: Sender<CommandResult>,
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
        /// still exists, and no connection is streaming for it, and mints a
        /// fresh client otherwise.
        resume: Option<ClientId>,
        /// The token the caller's last attach minted, presented to get that
        /// attach's view back: the active tab, the focused pane of each tab,
        /// the zoomed pane of each tab, and the scroll offset of each pane. The
        /// dispatcher hands that view back when it still holds one under this
        /// token, and mints a fresh view otherwise. Absent on a first attach.
        resume_token: Option<ConnectionToken>,
        /// The caller's terminal size in cells, recorded as the client's
        /// viewport.
        viewport: Size,
        /// The pane region the client reported, recorded on its record.
        pane_area: Option<PaneArea>,
        /// Which of the session's events the client receives.
        filter: EventFilter,
        /// When the producer received the request, carried on the event so the
        /// handler never reads the clock itself.
        attached_at: SystemTime,
        /// Whether the connection carrying this attach reached the session
        /// from another machine. The client is minted with it as its origin.
        /// The router marks the Hello it sends for a remote caller; every
        /// other connection leaves it `false`.
        remote: bool,
        /// Where the dispatcher sends what it minted.
        reply: Sender<Option<AttachAccepted>>,
    },
    /// A discovery request delivered over the IPC socket: the caller asks
    /// this process to describe its session. Carries the reply sender the
    /// connection thread waits on; the dispatcher answers with the overview
    /// built from live state, or `None` when no session is running.
    IpcDiscovery {
        /// Where the dispatcher sends the overview.
        reply: Sender<Option<SessionOverview>>,
    },
    /// A layout request delivered over the IPC socket: the caller asks this
    /// process to describe how its session arranges panes. Carries the reply
    /// sender the connection thread waits on; the dispatcher answers with the
    /// layout built from live state, or `None` when no session is running.
    IpcLayout {
        /// The one tab to describe, or every tab when absent.
        tab: Option<TabId>,
        /// Where the dispatcher sends the layout.
        reply: Sender<Option<SessionLayout>>,
    },
    /// A restart request delivered over the IPC socket: the caller asks this
    /// process to replace its own image with the binary at the path it started
    /// from. Carries the reply sender the connection thread waits on; the
    /// dispatcher checks what the swap needs and answers `Ok(())` when the
    /// restart is accepted, or `Err` carrying the sentence naming what is
    /// wrong. A refused restart changes nothing and the session keeps serving.
    IpcRestart {
        /// Where the dispatcher sends its verdict.
        reply: Sender<Result<(), String>>,
    },
    /// The grace window for the clients whose records came across an image
    /// swap has closed. The dispatcher detaches every one of those clients
    /// that has not attached again, and does nothing when they all have.
    DropUnclaimedClients {
        /// When the window closed, supplied by the producer so the handler
        /// never reads the clock itself.
        deadline: Instant,
    },
    /// A capability-checked command issued by a plugin.
    Plugin(CommandEnvelope),
}

/// What the dispatcher minted for one [`RuntimeEvent::IpcAttach`]: the client
/// record, the session it joined, the structure built for the attach reply, the
/// receiving end of the client's own event queue, and the session's shared
/// ending notice.
///
/// The whole of it comes out of one dispatcher turn, so the structure names
/// the same state the queue's first event follows.
#[derive(Debug)]
pub struct AttachAccepted {
    /// The id the dispatcher minted for this client.
    pub client_id: ClientId,
    /// The session the client joined.
    pub session_id: SessionId,
    /// What the session contains, built for this reply.
    pub structure: AttachedSessionStructureSnapshot,
    /// The client's event queue. Dropping it ends the subscription.
    pub events: Receiver<Delivery>,
    /// Shared with the session, so this client's writing thread learns that the
    /// session is ending even when the queue above is full, and so the session
    /// learns when that thread has written the last frame.
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

/// What the session and every attached client's writing thread share about the
/// session's last frame.
///
/// Publishing that frame raises the notice. A writing thread reads it at the
/// top of each turn: raised, it drops whatever is still queued for that client,
/// writes the frame the notice names, and ends. The queue each client reads is
/// bounded, so a published last frame does not reach a client whose queue is
/// full; this is what reaches that client instead. A client whose queue the
/// server closed already left the session, so its writing thread keeps to its
/// own goodbye.
///
/// [`writers_running`](Self::writers_running) counts the writing threads that
/// have not ended. Each one ends right after it writes the last frame, so the
/// session waits for the count to reach zero before it replaces its own process
/// image or tears the process down.
///
/// Before → after: a client's queue holds its full 1024 deliveries when the
/// session quits → the quit does not fit the queue, the writing thread reads
/// the raised notice at its next turn, writes the quit frame, and the client
/// says the session ended instead of reading end of stream.
#[derive(Debug, Default)]
pub struct EndingNotice {
    /// Which frame ends every attached client's stream; empty while the session
    /// serves. Set once: the session is ending by then.
    ending: OnceLock<SessionEnding>,
    /// How many client writing threads have started and not yet ended.
    writers: AtomicUsize,
}

impl EndingNotice {
    /// Raise the notice: every attached client's writing thread writes
    /// `ending`'s frame at its next turn. The notice keeps the ending it was
    /// raised with first.
    pub fn raise(&self, ending: SessionEnding) {
        let _ = self.ending.set(ending);
    }

    /// How every attached client's stream ends, or `None` while the session
    /// serves.
    #[must_use]
    pub fn raised(&self) -> Option<SessionEnding> {
        self.ending.get().copied()
    }

    /// Count one client writing thread as started. Paired with
    /// [`writer_ended`](Self::writer_ended).
    pub fn writer_started(&self) {
        self.writers.fetch_add(1, Ordering::SeqCst);
    }

    /// Count one client writing thread as ended.
    pub fn writer_ended(&self) {
        self.writers.fetch_sub(1, Ordering::SeqCst);
    }

    /// How many client writing threads have started and not yet ended.
    #[must_use]
    pub fn writers_running(&self) -> usize {
        self.writers.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests;
