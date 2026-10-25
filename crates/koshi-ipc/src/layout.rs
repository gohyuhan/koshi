//! One session's layout, as the session server reports it on request.
//!
//! [`SessionLayout`](crate::layout::SessionLayout) describes how a session
//! arranges its panes right now: each tab's split tree, the rectangles that
//! tree solves to, and each floating pane with what each attached client draws
//! for it.
//!
//! Trees travel unsolved, as on attach. Each tab carries one
//! [`SolvedTab`](crate::layout::SolvedTab) per client viewing it, solved
//! against the size those clients share and that client's own layout mode. A
//! tab no client is viewing carries its tree and no solved entry.
//!
//! Nothing here describes pane content: no grid, no cursor, no scrollback, no
//! colors.

use koshi_core::geometry::{FloatingPanePosition, FloatingPaneSize, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::LayoutNode;
use serde::{Deserialize, Serialize};

/// One session's layout: every tab it holds, every floating pane it holds, and
/// where each attached client is looking.
///
/// Decoding ignores a field this build does not know, in this record and in
/// every record under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayout {
    /// The session's stable id.
    pub session_id: SessionId,
    /// The session's display name.
    pub session_name: String,
    /// The tabs described, in tab-bar order. Holds every tab, or the one tab
    /// the request named.
    pub tabs: Vec<TabLayout>,
    /// Every floating pane of the session, in the order the panes became
    /// floating, also when the request named one tab.
    pub floating_panes: Vec<FloatingPaneLayout>,
    /// Every attached client, and what it has focused.
    pub clients: Vec<ClientFocus>,
}

/// One tab: its split tree, and the rectangles each viewing client solved
/// that tree to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabLayout {
    /// The tab's stable id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The tab's position in the bar, starting at 0.
    pub tab_index: usize,
    /// The tab's layout tree, unsolved.
    pub layout_tree: LayoutNode,
    /// One entry per client viewing this tab. Empty when no client views it.
    pub solved_tabs: Vec<SolvedTab>,
}

/// One client's solve of one tab's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvedTab {
    /// The client this solve belongs to.
    pub client_id: ClientId,
    /// The size the tree solves against. The rectangles live in that space,
    /// starting at origin `(0, 0)`. Every client viewing the tab shares this
    /// size: the smallest viewing terminal on each axis, minus the two chrome
    /// rows.
    pub viewport_size: Size,
    /// Whether this client sees the tab tiled or sees one pane fullscreen.
    pub layout_mode: LayoutMode,
    /// Every leaf pane exactly once, in layout order, with its rectangle. A
    /// collapsed stack member's rectangle is its one-row header strip. A
    /// zero-area rectangle is a pane that is not visible.
    pub pane_rects: Vec<SolvedPane>,
    /// The panes clipped to zero area when the layout does not fit. A pane
    /// that is zero-area for another reason is not listed: one behind a
    /// fullscreen pane, or a leaf under a collapsed stack member's header.
    pub suppressed_pane_ids: Vec<PaneId>,
    /// `true` when `suppressed_pane_ids` is not empty and every rectangle in
    /// `pane_rects` is zero-area.
    pub is_every_pane_suppressed: bool,
    /// One header strip per collapsed stack member, in layout order.
    pub stack_headers: Vec<StackHeader>,
}

/// One pane's place in a solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvedPane {
    /// The pane this rectangle places.
    pub pane_id: PaneId,
    /// The pane's rectangle, including its 1-cell border on each side.
    pub outer_rect: Rect,
}

/// One floating pane: the size it asks for, the size it holds, and what each
/// attached client draws for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingPaneLayout {
    /// The floating pane.
    pub pane_id: PaneId,
    /// The size the pane asks for, per axis: a number of cells, or a percent
    /// of the smallest floating viewport of the attached clients. `60%` by
    /// `60%` for a pane that `koshi new-pane --floating` opened with no `--cols`
    /// and no `--rows`.
    pub desired_size: FloatingPaneSize,
    /// The pane's outer size in cells, its chrome included: the size every
    /// client draws it at. `60%` by `60%` with an `80x22` smallest floating
    /// viewport → `48x13`.
    pub solved_size: Size,
    /// One view per attached client, in client id order.
    pub client_views: Vec<FloatingPaneClientView>,
}

/// One attached client's view of one floating pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingPaneClientView {
    /// The client holding the view.
    pub client_id: ClientId,
    /// The cell the client stored for the pane: none for the default
    /// placement, a moved cell, or a pinned cell. A `40`-column pane pinned at
    /// column `20` keeps `Pinned` at column `20` while a `50`-column pane area
    /// draws it at column `10`.
    pub position: FloatingPanePosition,
    /// What the client draws for the pane.
    pub visibility: FloatingPaneVisibility,
}

/// What one attached client draws for one floating pane.
///
/// Encodes as `{"Drawn":{"top_left_cell":{"column":16,"row":4},"layer_index":0}}`
/// or `"Minimized"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FloatingPaneVisibility {
    /// The client shows the pane and draws it: a box of
    /// [`FloatingPaneLayout::solved_size`] cells whose top-left cell is
    /// `top_left_cell`, counted from the client's pane-area origin. A
    /// `48x13` pane at the default placement, alone in the session, on an
    /// `80x22` pane area → `top_left_cell` `(16, 4)`.
    Drawn {
        /// The top-left cell of the pane's box, its chrome included.
        top_left_cell: Point,
        /// The pane's place in the order the client draws the floating panes
        /// it shows: `0` draws first, at the back, and each higher index
        /// draws over the lower ones.
        layer_index: usize,
    },
    /// The client holds the pane minimized and draws nothing for it.
    Minimized,
}

/// One attached client's focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientFocus {
    /// The client's stable id.
    pub client_id: ClientId,
    /// The tab the client is viewing.
    pub active_tab_id: TabId,
    /// The pane that takes the client's input: its focused floating pane,
    /// else its focused pane in the tab it is viewing. Absent until the client
    /// focuses one.
    pub focused_pane_id: Option<PaneId>,
}

#[cfg(test)]
mod tests;
