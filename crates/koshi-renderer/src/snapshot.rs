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
//! plus the screen [`Grid`] behind an [`Arc`] so cloning a built snapshot
//! shares the buffer by reference. The snapshot is built and read in the same
//! process (the terminal `Grid`/`Cursor` types are not serializable); a
//! detached client is served by the separate session-persistence path.
//!
//! This module defines the *shape*. The runtime-side builder fills it from
//! live state; renderer modules draw only these fields. This DTO is their
//! contract.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;
use koshi_terminal::grid::state::Grid;
use koshi_terminal::state::CursorShape;

use crate::theme::Theme;

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
    /// overlays). Empty for a stock, plugin-free Koshi.
    pub plugin_ui: PluginUiSnapshot,
    /// The keybinding data the hint bar draws for the client's current mode.
    pub keymap_hints: KeymapHints,
    /// The resolved chrome theme every koshi-owned surface draws its colors
    /// from this frame.
    pub theme: Theme,
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
    /// The viewport size the layout was solved for: the tab's effective size,
    /// the element-wise minimum viewport across the clients viewing this tab.
    /// The [`layout_solved`](Self::layout_solved) rects live in this space with
    /// origin `(0, 0)`. A client whose own [`viewport`](ClientSnapshot::viewport)
    /// is larger draws this layout centered and letterboxes the surrounding
    /// margin; a client at exactly this size draws it edge to edge.
    pub effective_size: Size,
    /// Header strips for stacked panes (title bars for collapsed stack members).
    pub stack_headers: Vec<StackHeader>,
    /// Whether **this snapshot's client** sees the tab tiled, or sees a single
    /// pane zoomed to fill it. Zoom is per-client, so another client viewing the
    /// same tab in the same frame can carry a different value here.
    pub layout_mode: LayoutMode,
    /// True when every pane is suppressed because the tab has no room to draw —
    /// the renderer fills the whole frame with the "terminal too small" overlay.
    pub all_suppressed: bool,
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`PaneSnapshot`] by
/// [`pane_id`](Self::pane_id).
///
/// The builder keeps these fields consistent: [`visible`](Self::visible) is
/// true exactly when [`inner_rect`](Self::inner_rect) is `Some` (the pane has a
/// content area to draw), and a [`suppressed`](Self::suppressed) pane is not
/// visible. [`dead`](Self::dead) is an orthogonal axis: it does not by itself
/// change visibility — an exited pane stays laid out, drawn dimmed, until it is
/// removed. `inner_rect` is `None` for three distinct reasons — no room,
/// hidden, or a collapsed stack member — and [`suppressed`](Self::suppressed)
/// marks the no-room case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    pub rect: Rect,
    /// The content area inside the border — the layout-owned rect the PTY was
    /// sized from, taken verbatim from
    /// [`content_rects`](koshi_layout::content::content_rects). `None` when the
    /// pane shows no content (suppressed, hidden, or a collapsed stack member).
    /// The renderer draws cells and places the cursor here and never re-computes
    /// the inset.
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
    /// The pane's resolved display title: on the alternate screen the running
    /// app's OSC 0/1/2 title; on the primary screen the shell's OSC 7 working
    /// directory (`~`-shortened), falling back to the OSC title. `None` when
    /// the pane has reported neither.
    pub title: Option<String>,
    /// The cursor's position and visibility within the content area.
    pub cursor: CursorSnapshot,
    /// The visible terminal cells. `None` for a pane with no terminal content
    /// (a plugin pane, or a slot showing nothing this frame).
    pub grid_view: Option<GridView>,
    /// Whether the whole screen is in reverse video (DECSCNM): the renderer
    /// swaps the default foreground and background for every cell.
    pub reverse_video: bool,
    /// The viewing client's highlighted text in this pane, already cut down to
    /// the rows this frame shows. `None` when the client has nothing highlighted
    /// here, or when the highlight is entirely outside the visible rows.
    pub selection: Option<SelectionSpans>,
    /// Scrollback state for the scroll-position indicator.
    pub scrollback: ScrollbackMeta,
}

/// Which cells of a pane are highlighted this frame, as a column range per
/// visible row.
///
/// The highlight is resolved to the rendered window's own rows and columns
/// before it gets here, so the renderer never has to know how a selection is
/// stored or how far the view is scrolled — it paints the rows it is handed.
/// Rows are in ascending order, and a row the highlight does not touch has no
/// entry.
///
/// A highlight running from mid-way along row 4 to mid-way along row 6 of an
/// 80-column pane arrives as `[(4, 12, 79), (5, 0, 79), (6, 0, 33)]`: the first
/// row from the start column to its end, whole rows in between, the last row up
/// to its end column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSpans {
    /// One entry per highlighted row: the row, then the first and last
    /// highlighted column on it. Both columns are inclusive.
    pub rows: Vec<(u16, u16, u16)>,
}

impl SelectionSpans {
    /// The highlighted column range on `row`, or `None` if it has none.
    #[must_use]
    pub fn row_span(&self, row: u16) -> Option<(u16, u16)> {
        self.rows
            .iter()
            .find(|(candidate, _, _)| *candidate == row)
            .map(|&(_, start, end)| (start, end))
    }
}

/// The cursor's on-screen position, relative to the content area's origin, plus
/// how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    /// The cursor's row within the content area, starting at 0.
    pub row: u16,
    /// The cursor's column within the content area, starting at 0.
    pub col: u16,
    /// Whether the cursor is visible (the app may hide it).
    pub visible: bool,
    /// Whether the cursor blinks.
    pub blink: bool,
    /// The shape the cursor is drawn as (DECSCUSR) — a program in the pane
    /// switches it to show its own mode, as vim does between a normal-mode
    /// block and an insert-mode bar — or `None` while the pane has asked for no
    /// shape at all.
    pub shape: Option<CursorShape>,
}

/// How the outer terminal's cursor should look for one frame.
///
/// The distinction that matters is between a pane that *asked* for a look and a
/// pane that asked for nothing: only the first may override the cursor the user
/// configured in their own terminal. A plain shell never sends DECSCUSR, so
/// focusing one must not repaint the user's blinking bar into a steady block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    /// The pane asked for no style — the user's own configured cursor stands.
    UserDefault,
    /// The pane asked for this shape, blinking or steady.
    Shaped {
        /// The requested shape.
        shape: CursorShape,
        /// Whether the requested cursor blinks.
        blink: bool,
    },
}

/// The visible cells for one pane: the live screen grid, plus how far the view
/// is scrolled back from the tail.
///
/// The grid is held behind an [`Arc`], so cloning a built [`GridView`] shares
/// the buffer by reference. The history rows for a non-zero
/// [`view_offset`](Self::view_offset) are supplied by the scroll feature that
/// sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridView {
    /// The live screen buffer.
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
    /// The client's focused pane in the active tab, or `None` when the tab has
    /// no focusable pane. The renderer highlights the pane whose
    /// [`PaneSlot::pane_id`] matches, and places the cursor there.
    pub focused_pane: Option<PaneId>,
    /// The pane the client's pointer is hovering over, or `None` when it is over
    /// chrome. The renderer draws an *unfocused* pane under the pointer in the
    /// hover color so the wheel target is visible; the focused pane keeps its
    /// focus color.
    pub hovered_pane: Option<PaneId>,
    /// The client's input mode (drives the mode tag and keybind resolution).
    pub lock_mode: LockMode,
    /// Whether this client grabs the mouse for text selection. Adds the `SELECT`
    /// tag to the mode indicator; orthogonal to [`lock_mode`](Self::lock_mode),
    /// so both can be on at once.
    pub mouse_select: bool,
    /// The chords of a multi-chord binding pressed so far, or `None` when no
    /// sequence is pending. The hint bar switches from the mode's top-level
    /// hints to the continuations of this prefix while it is `Some`.
    pub pending_sequence: Option<KeySequence>,
    /// This client's tabline scroll position: `None` follows the active tab —
    /// the tab strip always scrolls to reveal it — while `Some(i)` peeks from
    /// tab index `i` without changing focus. The renderer windows the tab list
    /// from this; mouse scroll, arrow clicks, and drag set it.
    pub tabline_offset: Option<usize>,
}

/// The keybinding data behind the hint bar, projected for one client's
/// current input mode.
///
/// Everything is plain data resolved by the runtime: the merged keymap's
/// bindings for the mode, each already joined to its action's display name.
/// The per-mode collections travel behind [`Arc`]s — the runtime computes
/// them once per keymap change, and every frame's snapshot shares them by
/// reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapHints {
    /// Every binding in the client's current mode, sorted by key sequence.
    pub entries: Arc<Vec<HintBinding>>,
    /// Display labels for prefix chords whose sequence group is untouched
    /// defaults (`<C-p>` → `PANE`). A group with any user-authored entry, or
    /// a user removal under it, ignores this and shows a `+N` marker instead.
    pub prefix_labels: Arc<BTreeMap<KeyChord, String>>,
    /// Every key a user surface removed in the current mode. A removal under
    /// a labeled prefix voids the label: the shipped name no longer describes
    /// the group.
    pub removed: Arc<BTreeSet<KeySequence>>,
    /// True when the user keymap was reverted to defaults over a key
    /// collision: the bar shows a conflict marker, and the hints listed are
    /// the reverted-to defaults.
    pub reverted: bool,
}

/// One binding the hint bar can show: a key sequence, the display name of the
/// action it fires, and the flags the bar's grouping and ordering read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintBinding {
    /// The chords pressed to fire the binding.
    pub sequence: KeySequence,
    /// The bound action's human-facing name, from its registry metadata.
    pub label: String,
    /// Whether a user surface authored the winning entry (a default shows
    /// `false`). Any `true` entry under a prefix voids the prefix's label.
    pub user_set: bool,
    /// Whether the bar must keep this hint visible ahead of every other —
    /// set on the reserved unlock binding in locked mode, which truncation
    /// never drops.
    pub pinned: bool,
}

/// Plugin-contributed UI for one frame. All slots are empty for a stock,
/// plugin-free Koshi; UI tasks populate them once the plugin host lands.
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
