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
//! Pane content travels in
//! [`Painted`](crate::event::SessionEvent::Painted) alone, as a whole
//! [`PaintedFrame`](crate::frame::PaintedFrame). No other frame here carries a
//! grid, a cursor, scrollback, or colors.
//!
//! Four frames here are not session facts.
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

use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use serde::{Deserialize, Serialize};

use crate::frame::PaintedFrame;
use crate::wire::{MaybeKnown, WireName, WireVariants};

/// An event as a client reads it: it may name a frame this build does not
/// have.
pub type IncomingEvent = MaybeKnown<SessionEvent>;

/// One frame on an attached client's event stream.
///
/// A field this build does not know is ignored, so a frame from a newer koshi
/// still reads. A whole frame this build has no name for arrives as
/// [`MaybeKnown::Unknown`] through
/// [`IncomingEvent`], and the client skips it and keeps reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// The picture the session composed for this client, drawn whole.
    Painted {
        /// The frame to draw.
        frame: Box<PaintedFrame>,
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
        /// The process exit code; `None` when terminated by a signal or
        /// unknown.
        exit_code: Option<i32>,
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
        prior_pane: Option<PaneId>,
    },
    /// A tab's layout tree changed.
    LayoutChanged {
        /// The tab whose layout tree changed.
        tab_id: TabId,
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
        prior_tab: TabId,
    },
    /// A tab moved to a new index.
    TabMoved {
        /// The moved tab.
        tab_id: TabId,
        /// The tab's previous zero-based index.
        old_index: usize,
        /// The tab's new zero-based index.
        new_index: usize,
    },
    /// The session is shutting down: its last tab closed, so the program
    /// quits. A terminal frame — nothing follows it.
    Quit,
    /// The server detached this client. The last frame the server writes on
    /// this connection; the session keeps running and the client may attach
    /// again.
    Detached,
    /// The client's queue overflowed and dropped an event the stream cannot
    /// skip.
    Resync {
        /// How many events the client missed.
        dropped_count: u64,
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
        answers: Vec<koshi_core::mouse::MouseAnswer>,
    },
    /// Bytes for the terminal this client runs in, written to it verbatim.
    HostWrite {
        /// The bytes to write, in the order the session queued them.
        bytes: Vec<u8>,
    },
    /// The client drops this session and attaches to the named one.
    SwitchTo {
        /// The session to attach to. The client reads that session's socket
        /// and connection token from the endpoint file keyed by this id.
        session_id: SessionId,
    },
}

impl SessionEvent {
    /// The frame's name, e.g. `"Painted"`. Carries no payload, so it is safe
    /// on a log line even though a payload can hold pane content or bytes a
    /// pane wrote.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            SessionEvent::Painted { .. } => "Painted",
            SessionEvent::PaneCreated { .. } => "PaneCreated",
            SessionEvent::PaneProcessExited { .. } => "PaneProcessExited",
            SessionEvent::PaneClosing { .. } => "PaneClosing",
            SessionEvent::PaneRemoved { .. } => "PaneRemoved",
            SessionEvent::PaneFocused { .. } => "PaneFocused",
            SessionEvent::LayoutChanged { .. } => "LayoutChanged",
            SessionEvent::TabCreated { .. } => "TabCreated",
            SessionEvent::TabClosed { .. } => "TabClosed",
            SessionEvent::TabFocused { .. } => "TabFocused",
            SessionEvent::TabMoved { .. } => "TabMoved",
            SessionEvent::Quit => "Quit",
            SessionEvent::Detached => "Detached",
            SessionEvent::Resync { .. } => "Resync",
            SessionEvent::MouseAnswer { .. } => "MouseAnswer",
            SessionEvent::HostWrite { .. } => "HostWrite",
            SessionEvent::SwitchTo { .. } => "SwitchTo",
        }
    }
}

impl WireVariants for SessionEvent {
    /// Every frame this build has. A variant added to [`SessionEvent`] is
    /// added here and to [`SessionEvent::name`] in the same change.
    const VARIANTS: &'static [&'static str] = &[
        "Painted",
        "PaneCreated",
        "PaneProcessExited",
        "PaneClosing",
        "PaneRemoved",
        "PaneFocused",
        "LayoutChanged",
        "TabCreated",
        "TabClosed",
        "TabFocused",
        "TabMoved",
        "Quit",
        "Detached",
        "Resync",
        "MouseAnswer",
        "HostWrite",
        "SwitchTo",
    ];
}

impl WireName for SessionEvent {
    fn wire_name(&self) -> &'static str {
        self.name()
    }
}

#[cfg(test)]
mod tests;
