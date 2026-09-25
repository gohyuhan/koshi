//! The render snapshot: an immutable, read-only picture of one frame that the
//! runtime hands to the renderer.
//!
//! The runtime owns the live, mutating state (sessions, tabs, panes, terminal
//! grids, cursor, focus, layout). The renderer only draws. The runtime freezes
//! the current instant into a [`RenderSnapshot`] and passes it over; the
//! renderer reads the snapshot and nothing else.
//!
//! Everything here is a plain data package: scalar copies of the live state,
//! plus the screen [`Grid`] behind an [`Arc`] so cloning a built snapshot
//! shares the buffer by reference. The snapshot is built and read in the same
//! process; a client in another process is served the same frame as a
//! [`Delivery::Frame`], which its connection thread turns into koshi-ipc's
//! wire form.
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
use koshi_core::geometry::{PaneArea, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::SolvedRegions;
use koshi_layout::solver::{PaneSizing, StackHeader};
use koshi_layout::tree::LayoutNode;
use koshi_terminal::graphics::{
    ImageAction, ImageRecord, MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
};
use koshi_terminal::grid::state::Grid;
use koshi_terminal::state::{CursorShape, ImagePlacement, ImagePlacementId};

use crate::region::{solve_core_regions, TablineInputs};

/// The statusline data the renderer draws, re-exported from the keymap crate
/// that produces it: `koshi_config::hints`.
pub use koshi_config::hints::{HintBinding, KeymapHints};

/// What a pane runs, as [`PaneSlot::pane_kind`] reports it. Re-exported from
/// `koshi_pane::pane::state`.
pub use koshi_pane::pane::state::PaneKind;

/// One frozen frame: the full read-only view the renderer draws from.
///
/// The renderer joins [`pane_snapshots`](Self::pane_snapshots) to the
/// [`PaneSlot`]s in [`session_snapshot`](Self::session_snapshot)'s active tab
/// by [`PaneId`]: a slot says where a pane sits, and its [`PaneSnapshot`] says
/// what is inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSnapshot {
    /// The session being viewed: its identity, active tab, and tab list.
    pub session_snapshot: SessionSnapshot,
    /// Per-pane content (grid, cursor, title), one entry per live pane in the
    /// active tab, matched to a [`PaneSlot`] by [`PaneId`].
    pub pane_snapshots: Vec<PaneSnapshot>,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: ClientSnapshot,
    /// Plugin-contributed UI (statusline/tabline segments, notifications,
    /// overlays). Empty for a stock, plugin-free Koshi.
    pub plugin_ui_snapshot: PluginUiSnapshot,
}

/// A read-only placement preview built from one client's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementSnapshot {
    /// The session that produced the preview.
    pub session_id: SessionId,
    /// The pane named by the request.
    pub source_pane_id: PaneId,
    /// The tab that currently contains `source_pane_id`.
    pub source_tab_id: TabId,
    /// The tab named as the destination.
    pub destination_tab_id: TabId,
    /// The session placement revision used for this preview.
    pub session_placement_revision: u64,
    /// The client placement revision used for this preview.
    pub client_placement_revision: u64,
    /// The source tab's solved layout, tree, and visible pane content.
    pub source_tab_snapshot: PlacementTabSnapshot,
    /// The destination tab's solved layout, tree, and visible pane content.
    /// `None` means that the destination is the source tab.
    pub destination_tab_snapshot: Option<PlacementTabSnapshot>,
    /// The client's retained view inputs.
    pub client_snapshot: PlacementClientSnapshot,
    /// The shared sizing inputs used by the solver.
    pub pane_sizing: PaneSizing,
}

/// The presentation state of the viewer's placement statusline entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementStatusKind {
    /// The selected destination snapshot is still being read.
    Loading,
    /// The viewer has no confirmed destination to submit.
    Invalid,
    /// The viewer has a destination that can be confirmed.
    Valid,
}

/// One viewer-local placement statusline entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementStatus {
    /// The style and validation state shown with `status_text`.
    pub placement_status_kind: PlacementStatusKind,
    /// The one-line text shown in the statusline.
    pub status_text: String,
}

/// One source or destination tab in a placement preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementTabSnapshot {
    /// The unsolved layout tree.
    pub layout_tree: LayoutNode,
    /// The solved tab layout.
    pub tab_snapshot: TabSnapshot,
    /// Visible content matched to the tab's pane slots by pane id.
    pub pane_snapshots: Vec<PlacementPaneSnapshot>,
}

/// One pane's bounded content in a placement preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementPaneSnapshot {
    /// The pane this content belongs to.
    pub pane_id: PaneId,
    /// The visible terminal cells, without scrollback metadata.
    pub terminal_grid_view: Option<GridView>,
    /// Native-image placements whose records may follow in bounded events.
    pub image_placement_snapshots: Vec<ImagePlacementSnapshot>,
}

/// The client view values that are needed to reproduce a placement preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementClientSnapshot {
    /// The requesting client.
    pub client_snapshot: ClientSnapshot,
    /// The pane area this client reported, if any.
    pub reported_pane_area: Option<PaneArea>,
}

/// The typed reasons a placement preview cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementSnapshotErrorCode {
    /// The source pane or destination tab does not exist.
    NotFound,
    /// The requested preview exceeds a bounded resource limit.
    ResourceLimit,
}

/// A placement preview refusal that travels through the attached event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementSnapshotError {
    /// The typed refusal code.
    pub code: PlacementSnapshotErrorCode,
    /// The human-readable refusal message.
    pub message: String,
}

impl RenderSnapshot {
    /// Borrow the server-provided parts of this frame that say where things
    /// sit, with `viewer_chrome` supplying what the session does not hold. The client
    /// adds its committed region solve when it builds a [`MouseFrame`].
    #[must_use]
    pub fn build_frame_layout(&self, viewer_chrome: ViewerChrome) -> FrameLayout<'_> {
        FrameLayout {
            session_snapshot: &self.session_snapshot,
            client_snapshot: &self.client_snapshot,
            viewer_chrome,
            committed_regions: None,
        }
    }
}

/// The region geometry that was committed with one painted frame.
///
/// `viewport_size` is the client-local terminal size used to solve the
/// `solved_regions`. `region_input_revision` changes when the region inputs
/// change. The renderer and mouse path read the same value from the last
/// painted frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedRegions {
    /// The client viewport used for this solve.
    pub viewport_size: Size,
    /// The ordered region rectangles and the pane rectangle left by them.
    pub solved_regions: SolvedRegions,
    /// The region-input revision that produced this solve.
    pub region_input_revision: u64,
}

impl CommittedRegions {
    /// Build a committed region value from an exact solve and its input revision.
    #[must_use]
    pub fn from_solved_regions(
        viewport_size: Size,
        solved_regions: SolvedRegions,
        region_input_revision: u64,
    ) -> Self {
        Self {
            viewport_size,
            solved_regions,
            region_input_revision,
        }
    }

    /// Build a committed region value whose solve is the compiled-in tabline
    /// and statusline solve for `viewport`, tagged with `input_revision`.
    #[must_use]
    pub fn core(viewport_size: Size, region_input_revision: u64) -> Self {
        Self::from_solved_regions(
            viewport_size,
            solve_core_regions(viewport_size),
            region_input_revision,
        )
    }
}

/// One item on a subscriber's queue: a live event, the frame composed for the
/// subscriber's client, a fresh frame that resyncs a subscriber whose queue
/// overflowed, the answers to one round of mouse actions, bytes for the
/// subscriber's own terminal, or the session its client moves to.
///
/// All of them ride the same queue in order: a subscriber that missed events
/// reads the backlog it already had, then the frame, then live events again.
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
        render_snapshot: Box<RenderSnapshot>,
        /// Which subscriber lagged, how many events were dropped, and their
        /// class.
        lag_report: SubscriberLagged,
    },
    /// A bounded read-only placement preview for the client that asked for it.
    PanePlacementSnapshot {
        /// The request this preview answers.
        request_id: u64,
        /// The source and destination snapshot.
        snapshot: Box<PlacementSnapshot>,
    },
    /// A placement preview request was refused.
    PanePlacementRefused {
        /// The request this refusal answers.
        request_id: u64,
        /// The typed refusal.
        error: PlacementSnapshotError,
    },
    /// What one round of mouse actions did, for the client that asked for the
    /// round.
    MouseAnswer {
        /// The `request_id` of the round being answered.
        request_id: u64,
        /// One entry per action in the round that had something to report, in
        /// the order those actions ran. Empty when the round had nothing to
        /// say.
        mouse_answers: Vec<koshi_core::mouse::MouseAnswer>,
    },
    /// Bytes for the terminal the subscriber's client runs in, written to it
    /// verbatim.
    HostWrite(Vec<u8>),
    /// The session the subscriber's client leaves this one for.
    SwitchTo(SessionId),
    /// A pane placement command from the subscriber was rejected.
    PlacementCommandRejected(CommandId),
}

/// The viewer-owned frame state: which pane the pointer is over, which top
/// border exposes a placement handle, which input mode owns the keymap, where
/// the tab strip is scrolled, and whether the viewer is dialing the session
/// again. While pane placement is shown, including while a confirmation waits,
/// the viewer clears the pointer and handle fields before hit-testing and
/// painting, and the renderer ignores them: hovering `pane-123` leaves its
/// border color unchanged.
///
/// These values belong to one viewer. None is stored on the session or carried
/// in a snapshot; the viewer hands them in when it hit-tests a frame and again
/// when it paints one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewerChrome {
    /// The pane the viewer's pointer is over, or `None` over koshi's own chrome.
    /// The renderer draws an unfocused pane under the pointer in the hover color
    /// outside pane placement mode; the focused pane keeps its focus color.
    pub hovered_pane_id: Option<PaneId>,
    /// The pane whose top border currently exposes the placement handle.
    pub placement_handle_pane_id: Option<PaneId>,
    /// The effective input mode for the viewer's keymap and mode tag. `None`
    /// lets generic frame consumers use the mode carried by the session frame.
    pub active_input_mode: Option<LockMode>,
    /// Whether pane borders and stack headers show pane ids and suppress hover
    /// styling for this painted frame.
    pub is_pane_placement_visible: bool,
    /// The pane being placed, whose border keeps the focus color.
    pub placement_source_pane_id: Option<PaneId>,
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
/// from this value. The `tabline` method returns the session name, tabs, lock
/// mode, mouse-selection state, reconnect state, and tab offset used by the
/// tabline solve. Hit-testing and tabline solving use cell coordinates. This
/// value has no theme; renderers apply colors when they draw cells.
///
/// A caller that already holds a [`RenderSnapshot`] borrows one out of it with
/// [`RenderSnapshot::build_frame_layout`], and an [`OwnedFrameLayout`] with
/// [`OwnedFrameLayout::build_frame_layout`]; both leave the committed region solve unset,
/// and hit-testing then works from the built-in geometry. A [`MouseFrame`]
/// fills the solve in with the one that placed the pane area when the frame was
/// painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout<'a> {
    /// The session being viewed, including its solved active tab.
    pub session_snapshot: &'a SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: &'a ClientSnapshot,
    /// The viewer's pointer, tab-strip, and link state.
    pub viewer_chrome: ViewerChrome,
    /// The region solve committed with the painted frame. `None` leaves
    /// hit-testing on the built-in geometry: the pane area is the whole area,
    /// the tabline its top row, the statusline its bottom row.
    pub(crate) committed_regions: Option<&'a CommittedRegions>,
}

impl<'a> FrameLayout<'a> {
    /// Borrow the tab-row facts from this frame without carrying pane data.
    ///
    /// A frame named `work` with tabs `shell` and `logs` yields those names and
    /// their tab state, but it does not yield a pane slot or terminal grid.
    #[must_use]
    pub(crate) fn get_tabline_inputs(&self) -> TablineInputs<'a> {
        TablineInputs {
            session_name: &self.session_snapshot.session_name,
            tabs_metadata: &self.session_snapshot.tabs_metadata,
            lock_mode: self
                .viewer_chrome
                .active_input_mode
                .unwrap_or(self.client_snapshot.lock_mode),
            is_mouse_selection_enabled: self.client_snapshot.is_mouse_selection_enabled,
            reconnecting: self.viewer_chrome.reconnecting,
            tabline_offset: self.viewer_chrome.tabline_offset,
        }
    }
}

/// The owned form of [`FrameLayout`], for a caller that builds these two
/// itself instead of borrowing them out of a [`RenderSnapshot`].
///
/// Answering a mouse event needs to know where the surfaces are and nothing
/// about what is inside them, so the mouse path builds one of these and calls
/// [`build_frame_layout`](Self::build_frame_layout) to hand it to the hit-testing functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFrameLayout {
    /// The session being viewed, including its solved active tab.
    pub session_snapshot: SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: ClientSnapshot,
}

impl OwnedFrameLayout {
    /// Borrow these two as a [`FrameLayout`], with `viewer_chrome` supplying the rest.
    #[must_use]
    pub fn build_frame_layout(&self, viewer_chrome: ViewerChrome) -> FrameLayout<'_> {
        FrameLayout {
            session_snapshot: &self.session_snapshot,
            client_snapshot: &self.client_snapshot,
            viewer_chrome,
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
    pub session_snapshot: SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: ClientSnapshot,
    /// One entry per pane the frame carried content for, matched to a
    /// [`PaneSlot`] by pane id.
    pub mouse_panes: Vec<MousePane>,
    /// The region solve and input revision that were painted with this frame.
    pub committed_regions: CommittedRegions,
}

impl MouseFrame {
    /// Borrow the parts of this painted frame that say where things sit, with
    /// `viewer_chrome` supplying the pointer and tab-strip state.
    #[must_use]
    pub fn build_frame_layout(&self, viewer_chrome: ViewerChrome) -> FrameLayout<'_> {
        FrameLayout {
            session_snapshot: &self.session_snapshot,
            client_snapshot: &self.client_snapshot,
            viewer_chrome,
            committed_regions: Some(&self.committed_regions),
        }
    }

    /// Build a mouse frame from a borrowed render snapshot and its painted regions.
    ///
    /// The mouse frame copies session, client, and per-pane input data. It does
    /// not copy pane grids.
    #[must_use]
    pub fn from_snapshot(
        render_snapshot: &RenderSnapshot,
        committed_regions: CommittedRegions,
    ) -> Self {
        Self {
            mouse_panes: render_snapshot
                .pane_snapshots
                .iter()
                .map(MousePane::from)
                .collect(),
            session_snapshot: render_snapshot.session_snapshot.clone(),
            client_snapshot: render_snapshot.client_snapshot.clone(),
            committed_regions,
        }
    }

    /// Build the mouse frame with the exact region solve that was painted.
    #[must_use]
    pub fn from_snapshot_with_regions(
        render_snapshot: RenderSnapshot,
        committed_regions: CommittedRegions,
    ) -> Self {
        Self {
            mouse_panes: render_snapshot
                .pane_snapshots
                .iter()
                .map(MousePane::from)
                .collect(),
            session_snapshot: render_snapshot.session_snapshot,
            client_snapshot: render_snapshot.client_snapshot,
            committed_regions,
        }
    }
}

impl From<RenderSnapshot> for MouseFrame {
    /// Takes the frame by value and uses the compiled-in region solve for the
    /// client's viewport, at input revision `0`.
    fn from(render_snapshot: RenderSnapshot) -> Self {
        let committed_regions =
            CommittedRegions::core(render_snapshot.client_snapshot.viewport_size, 0);
        Self::from_snapshot_with_regions(render_snapshot, committed_regions)
    }
}

/// One pane as a mouse event reads it: which pane, which line its top visible
/// row is, and what decides where a wheel tick over it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePane {
    /// The pane this entry describes, matched to a [`PaneSlot`] by id.
    pub pane_id: PaneId,
    /// The absolute line number of the pane's top visible row, copied from
    /// [`PaneSnapshot::view_top_row_index`].
    pub view_top_row_index: u64,
    /// Which mouse events the pane's program asked to be told about, copied
    /// from [`PaneSnapshot::mouse_tracking`].
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on, copied from
    /// [`PaneSnapshot::is_alternate_scroll_enabled`].
    pub is_alternate_scroll_enabled: bool,
    /// Whether the pane is showing the alternate screen, copied from
    /// [`PaneSnapshot::is_on_alternate_screen`].
    pub is_on_alternate_screen: bool,
    /// Whether the viewing client has a highlight in the pane, copied from
    /// [`PaneSnapshot::has_selection`].
    pub has_selection: bool,
}

impl From<&PaneSnapshot> for MousePane {
    fn from(pane_snapshot: &PaneSnapshot) -> Self {
        Self {
            pane_id: pane_snapshot.pane_id,
            view_top_row_index: pane_snapshot.view_top_row_index,
            mouse_tracking: pane_snapshot.mouse_tracking,
            is_alternate_scroll_enabled: pane_snapshot.is_alternate_scroll_enabled,
            is_on_alternate_screen: pane_snapshot.is_on_alternate_screen,
            has_selection: pane_snapshot.has_selection,
        }
    }
}

/// The session-scoped part of a frame: identity plus the active tab and the
/// metadata needed to draw the tab bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The session's stable id.
    pub session_id: SessionId,
    /// The session's committed layout, membership, and shared-sizing revision.
    pub session_revision: u64,
    /// The session's display name.
    pub session_name: String,
    /// The tab currently shown, solved and ready to draw.
    pub active_tab_snapshot: TabSnapshot,
    /// Lightweight entry per tab for the tab bar (index, name, active marker).
    pub tabs_metadata: Vec<TabMeta>,
}

/// One tab's entry in the tab bar: enough to draw the tab list without its full
/// layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabMeta {
    /// The tab's stable id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The tab's ordinal position in the bar, starting at 0.
    pub tab_index: usize,
    /// Whether this is the client's active tab (drawn with the active marker).
    pub is_active: bool,
}

/// The active tab, with its layout already solved into placed pane slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSnapshot {
    /// The tab's stable id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The solved layout: one [`PaneSlot`] per pane, giving outer and content
    /// rects and coarse status.
    pub pane_slots: Vec<PaneSlot>,
    /// The viewport size the layout was solved for: the tab's effective size,
    /// the element-wise minimum viewport across the clients viewing this tab.
    /// The [`pane_slots`](Self::pane_slots) rects live in this space with
    /// origin `(0, 0)`. A client whose own
    /// [`viewport_size`](ClientSnapshot::viewport_size)
    /// is larger draws this layout centered and letterboxes the surrounding
    /// margin; a client at exactly this size draws it edge to edge.
    pub effective_cell_size: Size,
    /// Header strips for stacked panes (title bars for collapsed stack members).
    pub stack_headers: Vec<StackHeader>,
    /// Whether **this snapshot's client** sees the tab tiled, or sees a single
    /// pane zoomed to fill it. Zoom is per-client, so another client viewing the
    /// same tab in the same frame can carry a different value here.
    pub layout_mode: LayoutMode,
    /// True when every pane is suppressed because the tab has no room to draw —
    /// the renderer fills the whole frame with the "terminal too small" overlay.
    pub are_all_panes_suppressed: bool,
    /// Blank cells between two panes that meet along a horizontal or
    /// vertical split, in the [`effective_cell_size`](Self::effective_cell_size) space.
    pub gap_cell_count: u16,
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`PaneSnapshot`] by
/// [`pane_id`](Self::pane_id).
///
/// The builder keeps these fields consistent: [`is_visible`](Self::is_visible)
/// is true exactly when [`content_rect`](Self::content_rect) is `Some` (the pane has a
/// content area to draw), and an [`is_suppressed`](Self::is_suppressed) pane is not
/// visible. [`is_dead`](Self::is_dead) is an orthogonal axis: it does not by itself
/// change visibility — an exited pane stays laid out, drawn like any other,
/// until it is removed. `content_rect` is `None` for three distinct reasons — no room,
/// hidden, or a collapsed stack member — and [`is_suppressed`](Self::is_suppressed)
/// marks the no-room case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    pub outer_rect: Rect,
    /// The content area inside the border — the layout-owned rect the PTY was
    /// sized from, taken verbatim from
    /// [`list_content_rects`](koshi_layout::content::list_content_rects). `None` when the
    /// pane shows no content (suppressed, hidden, or a collapsed stack member).
    /// The renderer draws cells and places the cursor here and never re-computes
    /// the inset.
    pub content_rect: Option<Rect>,
    /// Whether the pane runs a terminal or a plugin.
    pub pane_kind: PaneKind,
    /// Whether the pane is currently shown.
    pub is_visible: bool,
    /// Whether the pane is suppressed for lack of room.
    pub is_suppressed: bool,
    /// Whether the pane's process has exited. The renderer paints an exited
    /// pane the same as a live one.
    pub is_dead: bool,
}

/// One pane's content: what the renderer paints inside the matching
/// [`PaneSlot`]'s content rect, plus what a mouse event over this pane is
/// answered from.
///
/// Those last fields are not painted. [`mouse_tracking`](Self::mouse_tracking),
/// [`is_alternate_scroll_enabled`](Self::is_alternate_scroll_enabled),
/// [`is_on_alternate_screen`](Self::is_on_alternate_screen),
/// [`has_selection`](Self::has_selection) and
/// [`view_top_row_index`](Self::view_top_row_index) are copied into a
/// [`MousePane`] as the
/// frame is painted, and that is what the viewer's decision reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    /// The pane this content belongs to, matched to a [`PaneSlot`] by id.
    pub pane_id: PaneId,
    /// The pane's resolved display title: on the alternate screen the running
    /// app's OSC 0/1/2 title; on the primary screen the shell's OSC 7 working
    /// directory (`~`-shortened), falling back to the OSC title. `None` when
    /// the pane has reported neither.
    pub pane_title: Option<String>,
    /// The cursor's position and visibility within the content area.
    pub cursor_snapshot: CursorSnapshot,
    /// The visible terminal cells. `None` for a pane with no terminal content
    /// (a plugin pane, or a slot showing nothing this frame).
    pub terminal_grid_view: Option<GridView>,
    /// The image placements whose rectangles fit inside this view. A remote
    /// viewer can hold the rectangle before its image record arrives. Their
    /// anchors use the same pane-local rows and columns as `terminal_grid_view`.
    pub image_placement_snapshots: Vec<ImagePlacementSnapshot>,
    /// Whether the whole screen is in reverse video (DECSCNM): the renderer
    /// swaps the default foreground and background for every cell.
    pub is_reverse_video: bool,
    /// Which mouse events the pane's program asked to be told about
    /// (`?9`/`?1000`/`?1002`/`?1003`). An event a pane asked for is the
    /// program's; anything it did not ask for is koshi's.
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on: on the alternate screen a
    /// wheel tick becomes cursor arrow keys.
    pub is_alternate_scroll_enabled: bool,
    /// Whether the pane is showing the alternate screen. The alternate screen
    /// keeps no scrollback, so there is no view to scroll there.
    pub is_on_alternate_screen: bool,
    /// The absolute line number of the top row this frame shows for the pane —
    /// the same numbering [`koshi_core::command::GridPosition::row_index`] uses, counting
    /// every line the pane has ever pushed into scrollback.
    ///
    /// A press on the pane's `n`-th visible row names line `view_top_row_index + n`.
    /// Absolute line numbers never move, and that answer keeps naming the same
    /// text after more output arrives.
    pub view_top_row_index: u64,
    /// The viewing client's highlighted text in this pane, already cut down to
    /// the rows this frame shows. `None` when the client has nothing highlighted
    /// here, or when the highlight is entirely outside the visible rows.
    pub selection_spans: Option<SelectionSpans>,
    /// Whether the viewing client has a highlight in this pane at all, including
    /// one scrolled entirely out of the visible rows, where
    /// [`selection_spans`](Self::selection_spans) is `None`.
    pub has_selection: bool,
    /// Scrollback state for the scroll-position indicator.
    pub scrollback_meta: ScrollbackMeta,
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
    pub row_spans: Vec<(u16, u16, u16)>,
}

impl SelectionSpans {
    /// The highlighted column range on `row`, or `None` if it has none.
    #[must_use]
    pub fn find_row_span(&self, row_index: u16) -> Option<(u16, u16)> {
        self.row_spans
            .iter()
            .find(|(candidate_row_index, _, _)| *candidate_row_index == row_index)
            .map(|&(_, start, end)| (start, end))
    }
}

/// The cursor's on-screen position, relative to the content area's origin, plus
/// how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    /// The cursor's row within the content area, starting at 0.
    pub row_index: u16,
    /// The cursor's column within the content area, starting at 0.
    pub column_index: u16,
    /// Whether the cursor is visible (the app may hide it).
    pub is_visible: bool,
    /// Whether the cursor blinks.
    pub is_blinking: bool,
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
/// [`view_row_offset`](Self::view_row_offset) are supplied by the scroll feature that
/// sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridView {
    /// The live screen buffer.
    pub grid: Arc<Grid>,
    /// Rows scrolled up from the live tail; `0` shows the live bottom of the
    /// buffer.
    pub view_row_offset: usize,
}

/// One validated terminal image placement carried in a read-only frame.
///
/// The image record is shared so a local snapshot does not copy RGBA pixels.
/// A remote frame first carries the placement without a record, then rebuilds
/// the record after its bounded content events pass the wire checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacementSnapshot {
    /// The complete cell size and the clipped top and left cells.
    cell_geometry: koshi_core::geometry::ImageCellGeometry,
    /// The terminal-local placement identity.
    placement_id: ImagePlacementId,
    /// The connection-local identity of the image record.
    image_content_id: u64,
    /// The decoded image and its display metadata, when this viewer has it.
    image_record: Option<Arc<ImageRecord>>,
    /// The zero-based row and column of the upper-left covered cell.
    anchor_cell: (u16, u16),
    /// The number of covered columns.
    column_count: u16,
    /// The number of covered rows.
    row_count: u16,
}

impl ImagePlacementSnapshot {
    /// Build a placement with a complete image record and connection-local
    /// content id equal to `image_content_id`.
    ///
    /// Returns `None` when an identity or dimension is zero, the placement
    /// has an anchor plus a row or column count greater than `u16::MAX + 1`,
    /// the record action is `Transmit`, `record.compute_source_rect()` returns an
    /// error, or the decoded image has zero width or height, exceeds
    /// `MAX_IMAGE_SIDE_PIXEL_COUNT`, `MAX_IMAGE_PIXEL_COUNT`, or `MAX_IMAGE_BYTE_COUNT`, or has an
    /// RGBA length other than `width * height * 4`.
    #[must_use]
    pub fn from_image_record(
        placement_id: ImagePlacementId,
        image_record: Arc<ImageRecord>,
        anchor_cell: (u16, u16),
        column_count: u16,
        row_count: u16,
    ) -> Option<Self> {
        Self::with_content_id(
            placement_id,
            placement_id,
            image_record,
            anchor_cell,
            column_count,
            row_count,
        )
    }

    /// Build a placement with a complete image record and `image_content_id`.
    ///
    /// Returns `None` when an identity or dimension is zero, the placement
    /// has an anchor plus a row or column count greater than `u16::MAX + 1`,
    /// the record action is `Transmit`, `record.compute_source_rect()` returns an
    /// error, or the decoded image has zero width or height, exceeds
    /// `MAX_IMAGE_SIDE_PIXEL_COUNT`, `MAX_IMAGE_PIXEL_COUNT`, or `MAX_IMAGE_BYTE_COUNT`, or has an
    /// RGBA length other than `width * height * 4`.
    #[must_use]
    pub fn with_content_id(
        placement_id: ImagePlacementId,
        image_content_id: u64,
        image_record: Arc<ImageRecord>,
        anchor_cell: (u16, u16),
        column_count: u16,
        row_count: u16,
    ) -> Option<Self> {
        if !is_valid_placement(
            placement_id,
            image_content_id,
            anchor_cell,
            column_count,
            row_count,
        ) || image_record.action == ImageAction::Transmit
            || image_record.compute_source_rect().is_err()
            || !is_valid_image_record(&image_record)
        {
            return None;
        }
        Some(Self {
            placement_id,
            image_content_id,
            image_record: Some(image_record),
            anchor_cell,
            column_count,
            row_count,
            cell_geometry: compute_full_image_geometry(column_count, row_count),
        })
    }

    /// Build a placement whose image record is unavailable to this viewer.
    ///
    /// Returns `None` when an identity or dimension is zero, or the placement
    /// has an anchor plus a row or column count greater than `u16::MAX + 1`.
    #[must_use]
    pub fn unavailable(
        placement_id: ImagePlacementId,
        image_content_id: u64,
        anchor_cell: (u16, u16),
        column_count: u16,
        row_count: u16,
    ) -> Option<Self> {
        is_valid_placement(
            placement_id,
            image_content_id,
            anchor_cell,
            column_count,
            row_count,
        )
        .then_some(Self {
            placement_id,
            image_content_id,
            image_record: None,
            anchor_cell,
            column_count,
            row_count,
            cell_geometry: compute_full_image_geometry(column_count, row_count),
        })
    }

    /// Return the terminal-local placement identity.
    #[must_use]
    pub fn get_placement_id(&self) -> ImagePlacementId {
        self.placement_id
    }

    /// Return the connection-local identity of the image record.
    #[must_use]
    pub fn get_image_content_id(&self) -> u64 {
        self.image_content_id
    }

    /// Return the complete image record when this viewer received it.
    #[must_use]
    pub fn get_image_record(&self) -> Option<&ImageRecord> {
        self.image_record.as_deref()
    }

    /// Return the shared complete image record.
    #[must_use]
    pub fn clone_image_record(&self) -> Option<Arc<ImageRecord>> {
        self.image_record.as_ref().map(Arc::clone)
    }

    /// Return the zero-based row and column of the placement anchor.
    #[must_use]
    pub fn get_anchor_cell(&self) -> (u16, u16) {
        self.anchor_cell
    }

    /// Return the placement dimensions as `(row_count, column_count)`.
    #[must_use]
    pub fn get_cell_dimensions(&self) -> (u16, u16) {
        (self.row_count, self.column_count)
    }

    /// Set clipping geometry when the visible rectangle fits the complete image.
    ///
    /// Returns `None` when `geometry` does not contain this placement's full
    /// cell size.
    #[must_use]
    pub fn with_cell_geometry(
        mut self,
        cell_geometry: koshi_core::geometry::ImageCellGeometry,
    ) -> Option<Self> {
        if !cell_geometry.is_visible_size_contained(koshi_core::geometry::Size {
            column_count: self.column_count,
            row_count: self.row_count,
        }) {
            return None;
        }
        self.cell_geometry = cell_geometry;
        Some(self)
    }

    /// Return the complete cell size and the clipped top and left cells.
    #[must_use]
    pub fn get_cell_geometry(&self) -> koshi_core::geometry::ImageCellGeometry {
        self.cell_geometry
    }

    /// Copy a terminal placement into the frame while sharing its image record.
    ///
    /// Panics when the terminal supplies an invalid placement or clipping
    /// geometry.
    #[must_use]
    pub fn from_placement(placement: &ImagePlacement) -> Self {
        let (row_count, column_count) = placement.get_image_cell_dimensions();
        Self::with_content_id(
            placement.get_image_placement_id(),
            placement.get_image_content_id(),
            placement.clone_image_record(),
            placement.get_image_anchor(),
            column_count,
            row_count,
        )
        .expect("terminal image placement is valid")
        .with_cell_geometry(placement.get_image_geometry())
        .expect("terminal image clipping is valid")
    }
}

fn compute_full_image_geometry(
    column_count: u16,
    row_count: u16,
) -> koshi_core::geometry::ImageCellGeometry {
    koshi_core::geometry::ImageCellGeometry {
        full_size: koshi_core::geometry::Size {
            column_count,
            row_count,
        },
        cell_offset: koshi_core::geometry::Point { column: 0, row: 0 },
    }
}

fn is_valid_placement(
    placement_id: ImagePlacementId,
    image_content_id: u64,
    anchor_cell: (u16, u16),
    column_count: u16,
    row_count: u16,
) -> bool {
    placement_id != 0
        && image_content_id != 0
        && column_count != 0
        && row_count != 0
        && u32::from(anchor_cell.0) + u32::from(row_count) <= u32::from(u16::MAX) + 1
        && u32::from(anchor_cell.1) + u32::from(column_count) <= u32::from(u16::MAX) + 1
}

fn is_valid_image_record(image_record: &ImageRecord) -> bool {
    let Ok(pixel_width) = usize::try_from(image_record.image.pixel_width) else {
        return false;
    };
    let Ok(pixel_height) = usize::try_from(image_record.image.pixel_height) else {
        return false;
    };
    let Some(pixel_count) = pixel_width.checked_mul(pixel_height) else {
        return false;
    };
    let Some(expected_byte_count) = pixel_count.checked_mul(4) else {
        return false;
    };
    pixel_width > 0
        && pixel_height > 0
        && pixel_width <= MAX_IMAGE_SIDE_PIXEL_COUNT
        && pixel_height <= MAX_IMAGE_SIDE_PIXEL_COUNT
        && pixel_count <= MAX_IMAGE_PIXEL_COUNT
        && expected_byte_count <= MAX_IMAGE_BYTE_COUNT
        && image_record.image.rgba_bytes.len() == expected_byte_count
}

/// A pane's scrollback state. The renderer draws
/// [`retained_line_count`](Self::retained_line_count) as the total in the
/// scroll-position indicator and paints nothing from
/// [`is_truncated`](Self::is_truncated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackMeta {
    /// Whether the buffer reached its cap and dropped its oldest lines.
    pub is_truncated: bool,
    /// How many scrollback lines are currently retained.
    pub retained_line_count: usize,
}

/// The viewing client's own state: what this client sees and how it is moded.
///
/// A projection of the client's live state — the fields are copied out, the
/// live client model is not embedded — so each attached client renders its own
/// viewport independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSnapshot {
    /// The client's stable identifier.
    pub client_id: ClientId,
    /// The client's committed geometry and view revision.
    pub client_revision: u64,
    /// The client's terminal size in cells.
    pub viewport_size: Size,
    /// The tab the client is currently viewing.
    pub active_tab_id: TabId,
    /// The client's focused pane in the active tab, or `None` when the tab has
    /// no focusable pane. The renderer highlights the pane whose
    /// [`PaneSlot::pane_id`] matches, and places the cursor there.
    pub focused_pane_id: Option<PaneId>,
    /// The client's input mode, as the session has it: it drives the mode tag,
    /// decides whether a paste from the client's own terminal reaches the pane,
    /// and is what `koshi list-clients` reports.
    pub lock_mode: LockMode,
    /// Whether this client grabs the mouse for text selection. Adds the `SELECT`
    /// tag to the mode indicator; orthogonal to [`lock_mode`](Self::lock_mode),
    /// so both can be on at once. The viewer also reads it off a painted frame
    /// to decide whether a press in a mouse-aware pane begins a highlight.
    pub is_mouse_selection_enabled: bool,
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

/// A plugin-contributed statusline or tabline segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The segment's rendered text.
    pub rendered_text: String,
}

/// A plugin-contributed notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationView {
    /// The notification's rendered text.
    pub rendered_text: String,
}

/// A plugin-contributed floating overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayView {
    /// The overlay's rendered text.
    pub rendered_text: String,
}

#[cfg(test)]
mod tests;
