//! What an attached client is told after it attaches: the picture to draw, and
//! the session's structure changing.
//!
//! A client is handed
//! [`AttachedSessionStructureSnapshot`](crate::attach::AttachedSessionStructureSnapshot)
//! once, in the attach reply. [`SessionEvent`](crate::event::SessionEvent) is
//! everything after it: the painted frames the client draws, plus one frame per
//! change to which tabs exist, how each tab's panes are arranged, which pane a
//! client focused, and which panes are alive.
//!
//! Pane content travels in [`Painted`](crate::event::SessionEvent::Painted).
//! Image placements in that frame name connection-local records. A record the
//! client has not received follows as one
//! [`ImageContentStart`](crate::event::SessionEvent::ImageContentStart) and its
//! [`ImageContentChunk`](crate::event::SessionEvent::ImageContentChunk)
//! events. An unchanged record is referenced again without sending its RGBA
//! bytes again.
//!
//! Five frames here are not session facts.
//! [`Resync`](crate::event::SessionEvent::Resync) is the first: the server
//! sends it when a client's queue overflowed and dropped an event the stream
//! cannot skip, and it names how many events went missing.
//! [`MouseAnswer`](crate::event::SessionEvent::MouseAnswer) is the second: it
//! answers one [`IpcRequestKind::Mouse`](crate::protocol::IpcRequestKind::Mouse)
//! request and is addressed to the client that sent it.
//! [`HostWrite`](crate::event::SessionEvent::HostWrite) is the third: bytes a
//! pane aimed at the terminal the client runs in, such as an OSC 52 clipboard
//! write.
//! [`SwitchTo`](crate::event::SessionEvent::SwitchTo) is the fourth: it names
//! the session the client leaves this one for.
//! [`PlacementCommandRejected`](crate::event::SessionEvent::PlacementCommandRejected)
//! is the fifth: it names a placement command that the session rejected.

use koshi_core::command::PanePlacementTarget;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use serde::{Deserialize, Serialize};

use crate::frame::{FrameImageChunk, FrameImageTransfer, PaintedFrame};
use crate::placement::PanePlacementSnapshot;
use crate::protocol::IpcErrorPayload;
use crate::wire::{MaybeKnown, WireName, WireVariants};

/// An event as a client reads it: it may name a frame this build does not
/// have.
pub type IncomingEvent = MaybeKnown<SessionEvent>;

/// One frame on an attached client's event stream.
///
/// A field this build does not know is ignored: a frame from a newer koshi
/// reads. A whole frame this build has no name for arrives as
/// [`MaybeKnown::Unknown`] through [`IncomingEvent`], and the client skips it
/// and keeps reading. A frame missing a field this build needs is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// The picture the session composed for this client, drawn whole.
    Painted {
        /// The frame to draw.
        frame: Box<PaintedFrame>,
    },
    /// Drop every connection-local image record before the next painted frame.
    ImageCacheReset,
    /// Metadata for one image record whose pixels follow in chunks.
    ImageContentStart {
        /// The image record and its exact RGBA byte count.
        image_transfer: FrameImageTransfer,
    },
    /// One bounded piece of an image record.
    ImageContentChunk {
        /// The image bytes and their position in the transfer.
        image_chunk: FrameImageChunk,
    },
    /// A read-only placement preview for one request.
    PanePlacementSnapshot {
        /// The request this preview answers.
        request_id: u64,
        /// The bounded source and destination snapshot.
        snapshot: Box<PanePlacementSnapshot>,
    },
    /// A read-only placement preview request was refused.
    PanePlacementRefused {
        /// The request this refusal answers.
        request_id: u64,
        /// The typed refusal and its human-readable message.
        error: IpcErrorPayload,
    },
    /// A pane was created and registered.
    PaneCreated {
        /// The new pane.
        pane_id: PaneId,
        /// The tab it belongs to.
        tab_id: TabId,
    },
    /// A pane's child process exited. The pane stays in the layout until it is
    /// removed.
    PaneProcessExited {
        /// The pane whose process exited.
        pane_id: PaneId,
        /// The process exit code; `None` when a signal terminated the process.
        exit_code: Option<i32>,
        /// The signal number that terminated the process; `None` when the
        /// process exited with a code. A Windows session server sends `None`.
        /// A peer that sends no `signal` field is read as `None`.
        #[serde(default)]
        signal: Option<i32>,
    },
    /// A pane's close transaction started.
    PaneClosing {
        /// The pane whose close transaction started.
        pane_id: PaneId,
    },
    /// A pane leaf left the layout and registry.
    PaneRemoved {
        /// The pane removed from the layout and registry.
        pane_id: PaneId,
        /// The tab it was removed from.
        tab_id: TabId,
    },
    /// Focus moved to a pane.
    PaneFocused {
        /// The client whose focus moved.
        client_id: ClientId,
        /// The tab the focus moved in.
        tab_id: TabId,
        /// The newly focused pane.
        pane_id: PaneId,
        /// The pane that held this client's focus in the tab before, if any.
        previous_pane_id: Option<PaneId>,
    },
    /// A tab's layout tree changed.
    LayoutChanged {
        /// The tab whose layout tree changed.
        tab_id: TabId,
    },
    /// A checked pane placement committed in the session.
    PanePlacementCommitted {
        /// The command whose transaction committed this placement.
        command_id: CommandId,
        /// The pane placed in the destination layout.
        source_pane_id: PaneId,
        /// The tab that owned the pane before the placement.
        source_tab_id: TabId,
        /// The tab that owns the pane after the placement.
        destination_tab_id: TabId,
        /// The checked swap or insertion target used by the transaction.
        placement_target: PanePlacementTarget,
    },
    /// A tab was created.
    TabCreated {
        /// The new tab.
        tab_id: TabId,
    },
    /// A tab was closed.
    TabClosed {
        /// The closed tab.
        tab_id: TabId,
    },
    /// Focus moved to a tab.
    TabFocused {
        /// The client whose active tab changed.
        client_id: ClientId,
        /// The newly focused tab.
        tab_id: TabId,
        /// The tab the client was viewing before the switch. When the switch
        /// was forced by a tab close, this is the closed tab.
        previous_tab_id: TabId,
    },
    /// A tab moved to a new index.
    TabMoved {
        /// The moved tab.
        tab_id: TabId,
        /// The tab's previous zero-based index.
        previous_tab_index: usize,
        /// The tab's new zero-based index.
        new_tab_index: usize,
    },
    /// The session is over: its last tab closed, a quit command was applied,
    /// or its last pane's child exited. A terminal frame — nothing follows it.
    Quit,
    /// The session server is replacing its own process image with the binary
    /// now on disk. Its socket goes away and comes back under a new connection
    /// token, and the client attaches again naming the client id it holds. A
    /// terminal frame — nothing follows it.
    Restarting,
    /// The server detached this client. The last frame the server writes on
    /// this connection; the session keeps running and the client may attach
    /// again.
    Detached,
    /// The client's queue overflowed and dropped an event the stream cannot
    /// skip.
    Resync {
        /// How many events the client missed.
        dropped_event_count: u64,
    },
    /// What one round of mouse actions did. Sent once per
    /// [`IpcRequestKind::Mouse`](crate::protocol::IpcRequestKind::Mouse)
    /// request.
    MouseAnswer {
        /// The `request_id` of the round being answered.
        request_id: u64,
        /// One entry per action in the round that had something to report, in
        /// the order those actions ran. An empty list is the normal case: the
        /// session ran the round and had nothing to say.
        mouse_answers: Vec<koshi_core::mouse::MouseAnswer>,
    },
    /// Bytes for the terminal this client runs in, written to it verbatim.
    HostWrite {
        /// The bytes to write, in the order the session queued them. Written
        /// as one base64 string: the two bytes `[104, 105]` are `"aGk="`. Read
        /// from that string or from a list of numbers, the shape a session
        /// server speaking session protocol 2 writes.
        #[serde(with = "crate::bytes::base64_or_list")]
        host_output_bytes: Vec<u8>,
    },
    /// The client drops this session and attaches to the named one.
    SwitchTo {
        /// The session to attach to. The client reads that session's socket
        /// and connection token from the endpoint file keyed by this id.
        session_id: SessionId,
    },
    /// A pane placement command from this client was rejected.
    PlacementCommandRejected {
        /// The rejected command.
        command_id: CommandId,
    },
}

impl SessionEvent {
    /// The frame's name, e.g. `"Painted"`, with none of its payload. Written
    /// on log lines.
    #[must_use]
    pub fn get_event_name(&self) -> &'static str {
        match self {
            SessionEvent::Painted { .. } => "Painted",
            SessionEvent::ImageCacheReset => "ImageCacheReset",
            SessionEvent::ImageContentStart { .. } => "ImageContentStart",
            SessionEvent::ImageContentChunk { .. } => "ImageContentChunk",
            SessionEvent::PanePlacementSnapshot { .. } => "PanePlacementSnapshot",
            SessionEvent::PanePlacementRefused { .. } => "PanePlacementRefused",
            SessionEvent::PaneCreated { .. } => "PaneCreated",
            SessionEvent::PaneProcessExited { .. } => "PaneProcessExited",
            SessionEvent::PaneClosing { .. } => "PaneClosing",
            SessionEvent::PaneRemoved { .. } => "PaneRemoved",
            SessionEvent::PaneFocused { .. } => "PaneFocused",
            SessionEvent::LayoutChanged { .. } => "LayoutChanged",
            SessionEvent::PanePlacementCommitted { .. } => "PanePlacementCommitted",
            SessionEvent::TabCreated { .. } => "TabCreated",
            SessionEvent::TabClosed { .. } => "TabClosed",
            SessionEvent::TabFocused { .. } => "TabFocused",
            SessionEvent::TabMoved { .. } => "TabMoved",
            SessionEvent::Quit => "Quit",
            SessionEvent::Restarting => "Restarting",
            SessionEvent::Detached => "Detached",
            SessionEvent::Resync { .. } => "Resync",
            SessionEvent::MouseAnswer { .. } => "MouseAnswer",
            SessionEvent::HostWrite { .. } => "HostWrite",
            SessionEvent::SwitchTo { .. } => "SwitchTo",
            SessionEvent::PlacementCommandRejected { .. } => "PlacementCommandRejected",
        }
    }
}

impl WireVariants for SessionEvent {
    /// Every frame this build has. A variant added to [`SessionEvent`] is
    /// added here and to [`SessionEvent::get_event_name`] in the same change.
    const VARIANTS: &'static [&'static str] = &[
        "Painted",
        "ImageCacheReset",
        "ImageContentStart",
        "ImageContentChunk",
        "PanePlacementSnapshot",
        "PanePlacementRefused",
        "PaneCreated",
        "PaneProcessExited",
        "PaneClosing",
        "PaneRemoved",
        "PaneFocused",
        "LayoutChanged",
        "PanePlacementCommitted",
        "TabCreated",
        "TabClosed",
        "TabFocused",
        "TabMoved",
        "Quit",
        "Restarting",
        "Detached",
        "Resync",
        "MouseAnswer",
        "HostWrite",
        "SwitchTo",
        "PlacementCommandRejected",
    ];
}

impl WireName for SessionEvent {
    fn wire_name(&self) -> &'static str {
        self.get_event_name()
    }
}

#[cfg(test)]
mod tests;
