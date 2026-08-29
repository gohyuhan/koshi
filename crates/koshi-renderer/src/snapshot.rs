//! The render snapshot: an immutable, read-only picture of one frame that the
//! runtime hands to the renderer.
//!
//! The runtime owns the live, mutating state (sessions, tabs, panes, terminal
//! grids, cursor, focus, layout). The renderer only draws. The runtime freezes
//! the current instant into a [`RenderSnapshot`] and passes it over; the
//! renderer reads the snapshot and nothing else, so it cannot reach or change
//! the engine.
//!
//! Everything here is a plain data package: scalar copies of the live state,
//! plus the screen [`Grid`] behind an [`Arc`] so cloning a built snapshot
//! shares the buffer by reference. The snapshot is built and read in the same
//! process (the terminal `Grid`/`Cursor` types are not serializable); a client
//! in another process is served the same frame as a [`Delivery::Frame`], which
//! its connection thread turns into koshi-ipc's wire form.
//!
//! This module defines the *shape*. The runtime-side builder fills it from
//! live state and renderer modules draw from it. This DTO is their contract.
//!
//! A frame also carries a few fields nothing draws: the terminal modes on
//! [`PaneSnapshot`] that say where a mouse event over a pane goes, and the line
//! number its top visible row is. A viewer copies them into a [`MouseFrame`] as
//! it paints and answers the next mouse event from that.
//!
//! Three things about a frame come from the viewer instead, as a
//! [`ViewerChrome`]: the pane its pointer is over, where its tab strip is
//! scrolled to, and whether it is dialing the session again. The session
//! stores none of them.

use std::sync::Arc;

use koshi_core::event::{Event, SubscriberLagged};
use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::RegionSolve;
use koshi_layout::solver::StackHeader;
use koshi_terminal::grid::state::Grid;
use koshi_terminal::state::CursorShape;

use crate::region::{core_region_solve, NavigatorLayout};

/// The hint-bar data types live with the keymap that produces them; the
/// renderer only draws them, and re-exports them here so a caller painting a
/// frame resolves them from one place.
pub use koshi_config::hints::{HintBinding, KeymapHints};

/// What a pane runs, as [`PaneSlot::kind`] reports it. Re-exported so a caller
/// reading a frame resolves it from here.
pub use koshi_pane::pane::state::PaneKind;

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
}

impl RenderSnapshot {
    /// Borrow the server-provided parts of this frame that say where things
    /// sit, with `viewer` supplying what the session does not hold. The client
    /// adds its committed region solve when it builds a [`MouseFrame`].
    #[must_use]
    pub fn layout(&self, viewer: ViewerChrome) -> FrameLayout<'_> {
        FrameLayout {
            session: &self.session,
            client: &self.client,
            viewer,
            committed_regions: None,
        }
    }
}

/// The region geometry that was committed with one painted frame.
///
/// `viewport` is the client-local terminal size used to solve `solve`.
/// `input_revision` changes when the region inputs change. The renderer and
/// mouse path read the same value from the last painted frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedRegions {
    /// The client viewport used for this solve.
    pub viewport: Size,
    /// The ordered region rectangles and the pane rectangle left by them.
    pub solve: RegionSolve,
    /// The region-input revision that produced this solve.
    pub input_revision: u64,
}

impl CommittedRegions {
    /// Build a committed region value from an exact solve and its input revision.
    #[must_use]
    pub fn new(viewport: Size, solve: RegionSolve, input_revision: u64) -> Self {
        Self {
            viewport,
            solve,
            input_revision,
        }
    }

    /// Build the compiled-in navigator and statusline solve.
    #[must_use]
    pub fn core(viewport: Size, input_revision: u64) -> Self {
        Self::new(viewport, core_region_solve(viewport), input_revision)
    }
}

/// One item on a subscriber's queue: a live event, the frame composed for the
/// subscriber's client, a fresh frame that resyncs a subscriber whose queue
/// overflowed, the answers to one round of mouse actions, bytes for the
/// subscriber's own terminal, or the session its client moves to.
///
/// All of them ride the same queue in order, so a subscriber that missed events
/// reads the backlog it already had, then the frame, then live events again. A
/// frame is boxed to keep this type close to the size of an [`Event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// A live event, as published.
    Event(Event),
    /// The frame the session composed for the subscriber's client, to be drawn
    /// whole.
    Frame(Box<RenderSnapshot>),
    /// A frame the subscriber resumes from, with the report of what it missed.
    Snapshot {
        /// The state the subscriber picks up from, replacing the events it did
        /// not receive.
        snapshot: Box<RenderSnapshot>,
        /// Which subscriber lagged, how many events were dropped, and their
        /// class.
        lagged: SubscriberLagged,
    },
    /// What one round of mouse actions did, for the client that asked for the
    /// round.
    MouseAnswer {
        /// The `request_id` of the round being answered.
        request_id: u64,
        /// One entry per action in the round that had something to report, in
        /// the order those actions ran. Empty when the round had nothing to
        /// say.
        answers: Vec<koshi_core::mouse::MouseAnswer>,
    },
    /// Bytes for the terminal the subscriber's client runs in, written to it
    /// verbatim.
    HostWrite(Vec<u8>),
    /// The session the subscriber's client leaves this one for.
    SwitchTo(SessionId),
}

/// The three things about a frame the viewer decides, not the session: which
/// pane its pointer is over, where its tab strip is scrolled to, and whether it
/// is dialing the session again.
///
/// All three belong to one viewer. None is stored on the session or carried in
/// a snapshot; the viewer hands them in when it hit-tests a frame and again
/// when it paints one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewerChrome {
    /// The pane the viewer's pointer is over, or `None` over koshi's own chrome.
    /// The renderer draws an *unfocused* pane under the pointer in the hover
    /// color so the wheel target is visible; the focused pane keeps its focus
    /// color.
    pub hovered_pane: Option<PaneId>,
    /// Where the viewer's tab strip is scrolled: `None` follows the active tab —
    /// the strip always reveals it — while `Some(i)` peeks from tab index `i`
    /// without changing focus. The renderer windows the tab list from this and
    /// clamps an index past the last tab.
    pub tabline_offset: Option<usize>,
    /// Where the viewer's dialing stands while it has no link to the session,
    /// and `None` while it has one. The tabline draws
    /// `RECONNECTING (attempt 4, retry in 8s)` from a
    /// `Reconnecting { attempt: 4, retry_in_seconds: 8 }`.
    pub reconnecting: Option<Reconnecting>,
}

/// How far a viewer with no link has got: which dial comes next, and how many
/// seconds are left before it goes out.
///
/// The viewer replaces this once a second while it waits, so the tag it draws
/// counts down: `retry_in_seconds` 3, then 2, then 1, then the dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconnecting {
    /// Which dial comes next, counting from 1. Drawn as the `attempt 4` part of
    /// `RECONNECTING (attempt 4, retry in 8s)`.
    pub attempt: u32,
    /// Whole seconds left before that dial goes out. Drawn as the `retry in 8s`
    /// part of `RECONNECTING (attempt 4, retry in 8s)`.
    pub retry_in_seconds: u32,
}

/// Where a frame's surfaces sit, borrowed: the session with its solved active
/// tab, the viewing client, the viewer's own chrome state, and an optional
/// client-side region commit. Carries no pane content and no colors.
///
/// Hit-testing reads the session, client, viewer, and committed region solve
/// from this value. The `navigator` method returns the session name, tabs, lock
/// mode, mouse-selection state, reconnect state, and tab offset used by the
/// tabline solve. Hit-testing and tabline solving use cell coordinates. This
/// value has no theme; renderers apply colors when they draw cells. A
/// client-side layout carries the committed region solve that placed the pane
/// area; a server layout uses the default whole-area compatibility geometry.
///
/// A caller that already holds a [`RenderSnapshot`] borrows one out of it with
/// [`RenderSnapshot::layout`]. A client-side [`MouseFrame`] adds its committed
/// regions before hit-testing. A server layout view has no client commit and
/// uses the built-in whole-area compatibility geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout<'a> {
    /// The session being viewed, including its solved active tab.
    pub session: &'a SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client: &'a ClientSnapshot,
    /// The viewer's pointer, tab-strip, and link state.
    pub viewer: ViewerChrome,
    /// The region solve committed with the painted frame, when this layout is
    /// the client-side view of a painted frame.
    pub(crate) committed_regions: Option<&'a CommittedRegions>,
}

impl<'a> FrameLayout<'a> {
    /// Borrow the tab-row facts from this frame without carrying pane data.
    ///
    /// A frame named `work` with tabs `shell` and `logs` yields those names and
    /// their tab state, but it does not yield a pane slot or terminal grid.
    #[must_use]
    pub(crate) fn navigator(&self) -> NavigatorLayout<'_> {
        NavigatorLayout {
            session_name: &self.session.name,
            tabs: &self.session.tabs_metadata,
            lock_mode: self.client.lock_mode,
            mouse_select: self.client.mouse_select,
            reconnecting: self.viewer.reconnecting,
            tabline_offset: self.viewer.tabline_offset,
        }
    }
}

/// The owned form of [`FrameLayout`], for a caller that builds these two
/// itself instead of borrowing them out of a [`RenderSnapshot`].
///
/// Answering a mouse event needs to know where the surfaces are and nothing
/// about what is inside them, so the mouse path builds one of these and calls
/// [`layout`](Self::layout) to hand it to the hit-testing functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFrameLayout {
    /// The session being viewed, including its solved active tab.
    pub session: SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client: ClientSnapshot,
}

impl OwnedFrameLayout {
    /// Borrow these two as a [`FrameLayout`], with `viewer` supplying the rest.
    #[must_use]
    pub fn layout(&self, viewer: ViewerChrome) -> FrameLayout<'_> {
        FrameLayout {
            session: &self.session,
            client: &self.client,
            viewer,
            committed_regions: None,
        }
    }
}

/// A painted frame cut down to what answering a mouse event reads: where the
/// surfaces sit, the committed region solve, and the few per-pane fields that
/// say which line each pane's top row shows and where an event over it goes.
///
/// It carries no cells, no cursor and no titles, so a viewer holding one
/// between paints holds no pane's [`Grid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseFrame {
    /// The session being viewed, including its solved active tab.
    pub session: SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client: ClientSnapshot,
    /// One entry per pane the frame carried content for, matched to a
    /// [`PaneSlot`] by id.
    pub panes: Vec<MousePane>,
    /// The region solve and input revision that were painted with this frame.
    pub committed_regions: CommittedRegions,
}

impl MouseFrame {
    /// Borrow the parts of this painted frame that say where things sit, with
    /// `viewer` supplying the pointer and tab-strip state.
    #[must_use]
    pub fn layout(&self, viewer: ViewerChrome) -> FrameLayout<'_> {
        FrameLayout {
            session: &self.session,
            client: &self.client,
            viewer,
            committed_regions: Some(&self.committed_regions),
        }
    }

    /// Build the mouse frame with the exact region solve that was painted.
    #[must_use]
    pub fn with_regions(snapshot: RenderSnapshot, committed_regions: CommittedRegions) -> Self {
        Self {
            panes: snapshot.panes.iter().map(MousePane::from).collect(),
            session: snapshot.session,
            client: snapshot.client,
            committed_regions,
        }
    }
}

impl From<RenderSnapshot> for MouseFrame {
    /// Takes the frame by value and uses the compiled-in region solve.
    fn from(snapshot: RenderSnapshot) -> Self {
        let committed_regions = CommittedRegions::core(snapshot.client.viewport, 0);
        Self::with_regions(snapshot, committed_regions)
    }
}

/// One pane as a mouse event reads it: which pane, which line its top visible
/// row is, and what decides where a wheel tick over it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePane {
    /// The pane this entry describes, matched to a [`PaneSlot`] by id.
    pub id: PaneId,
    /// The absolute line number of the pane's top visible row, copied from
    /// [`PaneSnapshot::view_top_row`].
    pub view_top_row: u64,
    /// Which mouse events the pane's program asked to be told about, copied
    /// from [`PaneSnapshot::mouse_tracking`].
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on, copied from
    /// [`PaneSnapshot::alt_scroll`].
    pub alt_scroll: bool,
    /// Whether the pane is showing the alternate screen, copied from
    /// [`PaneSnapshot::on_alt_screen`].
    pub on_alt_screen: bool,
    /// Whether the viewing client has a highlight in the pane, copied from
    /// [`PaneSnapshot::has_selection`].
    pub has_selection: bool,
}

impl From<&PaneSnapshot> for MousePane {
    fn from(pane: &PaneSnapshot) -> Self {
        Self {
            id: pane.id,
            view_top_row: pane.view_top_row,
            mouse_tracking: pane.mouse_tracking,
            alt_scroll: pane.alt_scroll,
            on_alt_screen: pane.on_alt_screen,
            has_selection: pane.has_selection,
        }
    }
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
    /// Blank cells between two panes that meet along a horizontal or
    /// vertical split, in the [`layout_solved`](Self::layout_solved) space.
    pub gap: u16,
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
/// [`PaneSlot`]'s content rect, plus what a mouse event over this pane is
/// answered from.
///
/// Those last fields are not painted. [`mouse_tracking`](Self::mouse_tracking),
/// [`alt_scroll`](Self::alt_scroll), [`on_alt_screen`](Self::on_alt_screen),
/// [`has_selection`](Self::has_selection) and
/// [`view_top_row`](Self::view_top_row) are copied into a [`MousePane`] as the
/// frame is painted, and that is what the viewer's decision reads.
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
    /// Which mouse events the pane's program asked to be told about
    /// (`?9`/`?1000`/`?1002`/`?1003`). An event a pane asked for is the
    /// program's; anything it did not ask for is koshi's.
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on: on the alternate screen a
    /// wheel tick becomes cursor arrow keys.
    pub alt_scroll: bool,
    /// Whether the pane is showing the alternate screen. The alternate screen
    /// keeps no scrollback, so there is no view to scroll there.
    pub on_alt_screen: bool,
    /// The absolute line number of the top row this frame shows for the pane —
    /// the same numbering [`koshi_core::command::GridPos::row`] uses, counting
    /// every line the pane has ever pushed into scrollback.
    ///
    /// A press on the pane's `n`-th visible row names line `view_top_row + n`.
    /// Absolute line numbers never move, and that answer keeps naming the same
    /// text after more output arrives.
    pub view_top_row: u64,
    /// The viewing client's highlighted text in this pane, already cut down to
    /// the rows this frame shows. `None` when the client has nothing highlighted
    /// here, or when the highlight is entirely outside the visible rows.
    pub selection: Option<SelectionSpans>,
    /// Whether the viewing client has a highlight in this pane at all, including
    /// one scrolled entirely out of the visible rows, where
    /// [`selection`](Self::selection) is `None`.
    pub has_selection: bool,
    /// Scrollback state for the scroll-position indicator.
    pub scrollback: ScrollbackMeta,
}

/// Which cells of a pane are highlighted this frame, as a column range per
/// visible row.
///
/// The highlight is resolved to the rendered window's own rows and columns
/// before it gets here, so the renderer paints the rows it is handed. Rows are
/// in ascending order, and a row the highlight does not touch has no entry.
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
/// Only a pane that asked for a look overrides the cursor the user configured
/// in their own terminal. A plain shell never sends DECSCUSR, so focusing one
/// leaves the user's cursor alone.
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
    /// The client's input mode, as the session has it: it drives the mode tag,
    /// decides whether a paste from the client's own terminal reaches the pane,
    /// and is what `koshi list-clients` reports.
    pub lock_mode: LockMode,
    /// Whether this client grabs the mouse for text selection. Adds the `SELECT`
    /// tag to the mode indicator; orthogonal to [`lock_mode`](Self::lock_mode),
    /// so both can be on at once. The viewer also reads it off a painted frame
    /// to decide whether a press in a mouse-aware pane begins a highlight.
    pub mouse_select: bool,
}

/// Plugin-contributed UI for one frame. All slots are empty for a stock,
/// plugin-free Koshi.
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

/// A plugin-contributed statusline or tabline segment. Placeholder shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The segment's rendered text.
    pub text: String,
}

/// A plugin-contributed notification. Placeholder shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationView {
    /// The notification's rendered text.
    pub text: String,
}

/// A plugin-contributed floating overlay. Placeholder shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayView {
    /// The overlay's rendered text.
    pub text: String,
}

#[cfg(test)]
mod tests;
