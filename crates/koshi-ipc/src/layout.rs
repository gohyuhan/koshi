//! One session's layout, as the session server reports it on request.
//!
//! [`SessionLayout`](crate::layout::SessionLayout) describes how a session
//! arranges its panes right now: each tab's split tree, and the rectangles
//! that tree solves to.
//!
//! Trees travel unsolved, as on attach. Each tab carries one
//! [`SolvedTab`](crate::layout::SolvedTab) per client viewing it, solved
//! against the size those clients share and that client's own layout mode. A
//! tab no client is viewing carries its tree and no solved entry.
//!
//! Nothing here describes pane content: no grid, no cursor, no scrollback, no
//! colors.

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::LayoutNode;
use serde::{Deserialize, Serialize};

/// One session's layout: every tab it holds, and where each attached client
/// is looking.
///
/// Decoding ignores a field this build does not know, in this record and in
/// every record under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayout {
    /// The session's stable id.
    pub id: SessionId,
    /// The session's display name.
    pub name: String,
    /// The tabs described, in tab-bar order. Holds every tab, or the one tab
    /// the request named.
    pub tabs: Vec<TabLayout>,
    /// Every attached client, and what it has focused.
    pub clients: Vec<ClientFocus>,
}

/// One tab: its split tree, and the rectangles each viewing client solved
/// that tree to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabLayout {
    /// The tab's stable id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The tab's position in the bar, starting at 0.
    pub index: usize,
    /// The tab's layout tree, unsolved.
    pub tree: LayoutNode,
    /// One entry per client viewing this tab. Empty when no client views it.
    pub solved: Vec<SolvedTab>,
}

/// One client's solve of one tab's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvedTab {
    /// The client this solve belongs to.
    pub client: ClientId,
    /// The size the tree solves against. The rectangles live in that space,
    /// starting at origin `(0, 0)`. Every client viewing the tab shares this
    /// size: the smallest viewing terminal on each axis, minus the two chrome
    /// rows.
    pub viewport: Size,
    /// Whether this client sees the tab tiled or sees one pane fullscreen.
    pub mode: LayoutMode,
    /// Every leaf pane exactly once, in layout order, with its rectangle. A
    /// collapsed stack member's rectangle is its one-row header strip. A
    /// zero-area rectangle is a pane that is not visible.
    pub panes: Vec<SolvedPane>,
    /// The panes clipped to zero area when the layout does not fit. A pane
    /// that is zero-area for another reason is not listed: one behind a
    /// fullscreen pane, or a leaf under a collapsed stack member's header.
    pub suppressed: Vec<PaneId>,
    /// `true` when `suppressed` is not empty and every rectangle in `panes`
    /// is zero-area.
    pub all_suppressed: bool,
    /// One header strip per collapsed stack member, in layout order.
    pub stack_headers: Vec<StackHeader>,
}

/// One pane's place in a solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvedPane {
    /// The pane this rectangle places.
    pub id: PaneId,
    /// The pane's rectangle, including its 1-cell border on each side.
    pub rect: Rect,
}

/// One attached client's focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientFocus {
    /// The client's stable id.
    pub id: ClientId,
    /// The tab the client is viewing.
    pub active_tab: TabId,
    /// The client's focused pane in the tab it is viewing. Absent until the
    /// client focuses one.
    pub focused_pane: Option<PaneId>,
}

#[cfg(test)]
mod tests;
