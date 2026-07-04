//! The render snapshot: an immutable, read-only picture of one frame that the
//! runtime hands to the renderer.
//!
//! The runtime owns the live, mutating state (sessions, tabs, panes, terminal
//! grids, cursor, focus, layout). The renderer only draws. To keep those two
//! apart — no mid-frame tearing, no renderer-driven mutation — the runtime
//! freezes the current instant into a [`RenderSnapshot`] and passes it over.
//! The renderer reads the snapshot and nothing else; it cannot reach the
//! engine, so it cannot change it.
//!
//! Everything here is a plain data package: scalar copies of the live state,
//! plus an [`Arc`]-shared [`Grid`] so freezing the screen buffer is a reference
//! bump rather than a deep copy. The snapshot is built and read in the same
//! process (the terminal `Grid`/`Cursor` types are not serializable); a
//! detached client is served by the separate session-persistence path.
//!
//! This module defines the *shape*. The runtime-side builder that fills a
//! snapshot from live state, and the renderer code that draws each field, live
//! in later tasks; the fields carried here are the contract between them.

use std::collections::HashMap;
use std::sync::Arc;

use tile_core::geometry::{Rect, Size};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_core::lock::LockMode;
use tile_layout::mode::LayoutMode;
use tile_layout::solver::StackHeader;
use tile_pane::pane::state::PaneKind;
use tile_terminal::grid::state::Grid;

/// One frozen frame: the full read-only view the renderer draws from.
///
/// The renderer joins [`panes`](Self::panes) to the [`PaneSlot`]s in
/// [`session`](Self::session)'s active tab by [`PaneId`]: a slot says *where* a
/// pane sits, its [`PaneSnapshot`] says *what* is inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSnapshot {
    /// The session being viewed: its identity, active tab, and tab list.
    pub session: SessionSnapshot,
    /// Per-pane content (grid, cursor, title), one entry per live pane in the
    /// active tab, matched to a [`PaneSlot`] by [`PaneId`].
    pub panes: Vec<PaneSnapshot>,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client: ClientSnapshot,
    /// Plugin-contributed UI (statusline/tabline segments, notifications,
    /// overlays). Empty for a stock, plugin-free Tile.
    pub plugin_ui: PluginUiSnapshot,
}

/// The session-scoped part of a frame: identity plus the active tab and the
/// metadata needed to draw the tab bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The session's stable id.
    pub id: SessionId,
    /// The session's display name.
    pub name: String,
    /// The tab currently shown, solved and ready to draw.
    pub active_tab: TabSnapshot,
    /// Lightweight entry per tab for the tab bar (index, name, active marker).
    pub tabs_metadata: Vec<TabMeta>,
}

/// One tab's entry in the tab bar: enough to draw the tab list without its full
/// layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabMeta {
    /// The tab's stable id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The tab's ordinal position in the bar, starting at 0.
    pub index: usize,
    /// Whether this is the client's active tab (drawn with the active marker).
    pub active: bool,
}

/// The active tab, with its layout already solved into placed pane slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSnapshot {
    /// The tab's stable id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The solved layout: one [`PaneSlot`] per pane, giving outer and content
    /// rects and coarse status.
    pub layout_solved: Vec<PaneSlot>,
    /// Header strips for stacked panes (title bars for collapsed stack members).
    pub stack_headers: Vec<StackHeader>,
    /// Whether the tab is tiled or a single pane is fullscreen.
    pub layout_mode: LayoutMode,
    /// True when every pane is suppressed because the tab has no room to draw —
    /// the renderer shows the "terminal too small" overlay instead of panes.
    pub all_suppressed: bool,
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`PaneSnapshot`] by
/// [`pane_id`](Self::pane_id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    pub rect: Rect,
    /// The content area inside the border — the layout-owned rect the PTY was
    /// sized from, taken verbatim from `tile_layout::content_rects`. `None`
    /// when the pane shows no content (suppressed, hidden, or a collapsed stack
    /// member). The renderer draws cells and places the cursor here and never
    /// re-computes the inset.
    pub inner_rect: Option<Rect>,
    /// Whether the pane runs a terminal or a plugin.
    pub kind: PaneKind,
    /// Whether the pane is currently shown.
    pub visible: bool,
    /// Whether the pane is suppressed for lack of room.
    pub suppressed: bool,
    /// Whether the pane's process has exited (drawn dimmed / with a marker).
    pub dead: bool,
}

/// One pane's content: what the renderer paints inside the matching
/// [`PaneSlot`]'s content rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    /// The pane this content belongs to, matched to a [`PaneSlot`] by id.
    pub id: PaneId,
    /// The pane's title (from the terminal's title sequence), if any.
    pub title: Option<String>,
    /// The cursor's position and visibility within the content area.
    pub cursor: CursorSnapshot,
    /// The visible terminal cells. `None` for a pane with no terminal content
    /// (a plugin pane, or a slot showing nothing this frame).
    pub grid_view: Option<GridView>,
    /// Scrollback state for the scroll-position indicator.
    pub scrollback: ScrollbackMeta,
    /// Whether this pane holds the viewing client's focus.
    pub focused: bool,
}

/// The cursor's on-screen position, relative to the content area's origin, plus
/// whether it is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    /// The cursor's row within the content area, starting at 0.
    pub row: u16,
    /// The cursor's column within the content area, starting at 0.
    pub col: u16,
    /// Whether the cursor is visible (the app may hide it).
    pub visible: bool,
}

/// A cheap-to-clone view of the visible cells: the screen grid shared by
/// reference, plus how far the view is scrolled back from the live tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridView {
    /// The visible screen buffer, shared by reference so cloning the snapshot
    /// bumps a refcount instead of copying every cell.
    pub grid: Arc<Grid>,
    /// Rows scrolled up from the live tail; `0` shows the live bottom of the
    /// buffer.
    pub view_offset: usize,
}

/// Scrollback state the renderer needs for the scroll-position indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackMeta {
    /// Whether the buffer reached its cap and dropped its oldest lines.
    pub truncated: bool,
    /// How many scrollback lines are currently retained.
    pub retained_lines: usize,
}

/// The viewing client's own state: what this client sees and how it is moded.
///
/// A projection of the client's live state — the fields are copied out, the
/// live client model is not embedded — so each attached client renders its own
/// viewport independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSnapshot {
    /// The client's stable id.
    pub id: ClientId,
    /// The client's terminal size in cells.
    pub viewport: Size,
    /// The tab the client is currently viewing.
    pub active_tab: TabId,
    /// The client's focused pane in each tab it has visited.
    pub focused_pane_per_tab: HashMap<TabId, PaneId>,
    /// The client's input mode (drives the mode tag and keybind resolution).
    pub lock_mode: LockMode,
}

/// Plugin-contributed UI for one frame. All slots are empty for a stock,
/// plugin-free Tile; UI tasks populate them once the plugin host lands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginUiSnapshot {
    /// Segments injected into the statusline slots.
    pub statusline_segments: Vec<Segment>,
    /// Segments injected into the tabline slots.
    pub tabline_segments: Vec<Segment>,
    /// Transient notifications / toasts to draw.
    pub notifications: Vec<NotificationView>,
    /// Floating overlays to draw above the layout.
    pub overlays: Vec<OverlayView>,
}

/// A plugin-contributed statusline or tabline segment. Placeholder shape; UI
/// tasks flesh it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The segment's rendered text.
    pub text: String,
}

/// A plugin-contributed notification. Placeholder shape; UI tasks flesh it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationView {
    /// The notification's rendered text.
    pub text: String,
}

/// A plugin-contributed floating overlay. Placeholder shape; UI tasks flesh it
/// out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayView {
    /// The overlay's rendered text.
    pub text: String,
}

#[cfg(test)]
mod tests;
