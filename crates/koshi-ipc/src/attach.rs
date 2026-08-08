//! What the server sends a client when it attaches: the session's structure.
//!
//! A client runs in its own process and draws the session itself, so before it
//! can paint anything it needs to know what the session contains — which tabs
//! exist, how each tab's panes are arranged, and which pane each tab focused.
//! [`crate::attach::AttachedSessionStructureSnapshot`] carries exactly that,
//! once, in the handshake reply. Everything after it arrives as events.
//!
//! Layout trees travel unsolved: each client solves the tree against its own
//! terminal size. Two attached clients of different sizes place the same panes
//! differently.
//!
//! Nothing here is written to disk, and nothing here describes pane content:
//! no grid, no cursor, no scrollback, no colors. The viewing client's own
//! state — its viewport, lock mode and focus — is not part of this either; the
//! server assigns those and reports them through events and painted frames.

use koshi_core::ids::{PaneId, SessionId, TabId};
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneKind;
use serde::{Deserialize, Serialize};

/// One session's structure, as handed to a client on attach.
///
/// Every tab in the session is present, so a client draws any tab it switches
/// to from what it already holds.
///
/// A field this build does not know is ignored, in this record and every one
/// under it, so a snapshot from a newer koshi still attaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedSessionStructureSnapshot {
    /// The session's stable id.
    pub id: SessionId,
    /// The session's display name, shown in the status line.
    pub name: String,
    /// Every tab in the session, in display order.
    pub tabs: Vec<TabStructure>,
    /// Every pane in the session, ordered by [`PaneId`]. A layout leaf names a
    /// `PaneId`; the matching entry here says what backs it.
    pub panes: Vec<PaneStructure>,
}

/// One tab: what to label it in the tab bar, how its panes are arranged, and
/// which pane it focused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabStructure {
    /// The tab's stable id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The tab's position in the bar, starting at 0.
    pub index: usize,
    /// The tab's layout tree, unsolved. The client solves it against its own
    /// terminal size.
    pub layout: LayoutNode,
    /// Panes this tab has focused, most-recent first, across every client that
    /// has viewed it. The head is the tab's most-recently-focused pane; focus
    /// recovery walks the rest as panes close. Empty when nothing in the tab
    /// has been focused yet.
    pub focus_mru: Vec<PaneId>,
}

/// One pane: its id, and what backs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneStructure {
    /// The pane's stable id, matching its layout leaf.
    pub id: PaneId,
    /// Whether a terminal or a plugin draws this pane.
    pub kind: PaneKind,
}

#[cfg(test)]
mod tests;
