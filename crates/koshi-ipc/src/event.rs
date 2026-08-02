//! What an attached client is told after it attaches: the session's structure
//! changing.
//!
//! A client is handed
//! [`AttachedSessionStructureSnapshot`](crate::attach::AttachedSessionStructureSnapshot)
//! once, in the attach reply. [`SessionEvent`](crate::event::SessionEvent) is
//! everything after it: one frame per change to which tabs exist, how each
//! tab's panes are arranged, which pane a client focused, and which panes are
//! alive.
//!
//! Nothing here describes pane content: no grid, no cursor, no scrollback, no
//! colors.
//!
//! [`Resync`](crate::event::SessionEvent::Resync) is the one frame that is not
//! a session fact. The server sends it when a client's queue overflowed and
//! dropped an event the stream cannot skip. A client that reads it connects
//! again and attaches again, which hands it a fresh structure.

use koshi_core::ids::{ClientId, PaneId, TabId};
use serde::{Deserialize, Serialize};

/// One frame on an attached client's event stream.
///
/// Decoding rejects any field the build does not know, so a misspelled name is
/// an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SessionEvent {
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
    /// The client's queue overflowed and dropped an event the stream cannot
    /// skip. The client connects again and attaches again for a fresh
    /// structure.
    Resync {
        /// How many events the client missed.
        dropped_count: u64,
    },
}

#[cfg(test)]
mod tests;
