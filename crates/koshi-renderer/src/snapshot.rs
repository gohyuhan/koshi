//! The render snapshot: an immutable, read-only picture of one frame that the
//! runtime hands to the renderer.
//!
//! The runtime owns the live, mutating state (sessions, tabs, panes, terminal
//! grids, cursor, focus, layout). The renderer only draws. The runtime freezes
//! the current instant into a [`RenderSnapshot`] and passes it over; the
//! renderer reads the snapshot and nothing else.
//!
//! Everything here is a plain data package: scalar copies of the live state,
//! plus each visible row as a shared [`RowSnapshot`], so cloning a built
//! snapshot copies no cell. The snapshot is built and read in the same
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
//! it paints, with the [`ViewerChrome`] it paints, and answers the next mouse
//! event from that.
//!
//! Other things about a frame come from the viewer instead, as a
//! [`ViewerChrome`]: the pane its pointer is over, the placement handle it
//! shows or marks, its input mode, its pane placement preview, where its tab strip is
//! scrolled to, whether it is dialing the session again, and where it draws a
//! floating pane it moves. The session stores none of them.

use std::collections::HashSet;
use std::sync::Arc;

use koshi_core::event::{Event, SubscriberLagged};
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::content::list_content_rects;
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::{PlacementTabSizes, SwapTabSizes};
use koshi_layout::regions::SolvedRegions;
use koshi_layout::solver::{LayoutSolve, PaneSizing, StackHeader};
use koshi_layout::tree::LayoutNode;
use koshi_terminal::graphics::{
    ImageAction, ImageRecord, MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
};
use koshi_terminal::grid::row_snapshot::RowSnapshot;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::state::{CursorShape, ImagePlacement, ImagePlacementId};

use crate::floating_pane_chrome::compute_floating_pane_content_rect;
use crate::hit_test::{compute_frame_pane_area, compute_viewport_area};
use crate::region::{solve_core_regions, TablineInputs};

/// The statusline data the renderer draws, re-exported from the keymap crate
/// that produces it: `koshi_config::hints`.
pub use koshi_config::hints::{HintBinding, KeymapHints};

/// One frozen frame: the full read-only view the renderer draws from.
///
/// The renderer joins [`pane_snapshots`](Self::pane_snapshots) to the
/// [`PaneSlot`]s in [`session_snapshot`](Self::session_snapshot)'s active tab
/// and to its [`FloatingPaneSlot`]s by [`PaneId`]: a slot says where a pane
/// sits, and its [`PaneSnapshot`] says what is inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSnapshot {
    /// Whether a failed session restore opened a new shell whose recovery
    /// notice remains visible on the statusline.
    pub is_recovery_notice_visible: bool,
    /// The session being viewed: its identity, active tab, tab list, and the
    /// floating panes the viewing client shows.
    pub session_snapshot: SessionSnapshot,
    /// Per-pane content (grid, cursor, title): one entry per pane slot in the
    /// active tab and one per floating pane slot, matched to its slot by
    /// [`PaneId`].
    pub pane_snapshots: Vec<PaneSnapshot>,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: ClientSnapshot,
    /// The pane placement preview of the viewing client's placement target
    /// ([`PlacementPreview`]), or `None` while the client names no target.
    /// Frames built from the same preview share one value: a preview the
    /// session built again with no change keeps its `Arc`.
    pub placement_preview: Option<Arc<PlacementPreview>>,
}

/// The pane placement preview of one placement target: a source pane and the
/// destination tab it may move to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementPreview {
    /// The layouts the target's source pane and destination tab have now.
    Available(Box<PlacementSnapshot>),
    /// The session holds the target's source pane in no tab and among no
    /// floating panes, or holds no tab with its destination tab id.
    Unavailable {
        /// The source pane of the target.
        source_pane_id: PaneId,
        /// The destination tab of the target.
        destination_tab_id: TabId,
    },
}

impl PlacementPreview {
    /// The target's source pane and destination tab, as
    /// `(source_pane_id, destination_tab_id)`.
    #[must_use]
    pub fn get_placement_target(&self) -> (PaneId, TabId) {
        match self {
            PlacementPreview::Available(placement_snapshot) => (
                placement_snapshot.source_pane_id,
                placement_snapshot.destination_tab_id,
            ),
            PlacementPreview::Unavailable {
                source_pane_id,
                destination_tab_id,
            } => (*source_pane_id, *destination_tab_id),
        }
    }

    /// The layouts of an available preview, or `None` for an unavailable one.
    #[must_use]
    pub fn find_placement_snapshot(&self) -> Option<&PlacementSnapshot> {
        match self {
            PlacementPreview::Available(placement_snapshot) => Some(placement_snapshot),
            PlacementPreview::Unavailable { .. } => None,
        }
    }
}

/// The layouts one client's pane placement preview is built from. The panes'
/// content travels in the frame's own [`PaneSnapshot`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementSnapshot {
    /// The pane being placed.
    pub source_pane_id: PaneId,
    /// Where `source_pane_id` sits: the tab whose layout holds it, or
    /// [`PlacementSource::Floating`].
    pub source: PlacementSource,
    /// The tab the pane may move to.
    pub destination_tab_id: TabId,
    /// The destination tab's tree and solved layout. `None` exactly when the
    /// destination is the tab of a [`PlacementSource::Tiled`] source.
    pub destination_tab_snapshot: Option<PlacementTabSnapshot>,
    /// The shared sizing inputs used by the solver.
    pub pane_sizing: PaneSizing,
    /// The tab sizes the session checks and solves a split or a stack at, and
    /// any placement within the source pane's own tab or of a floating source
    /// pane.
    pub insertion_tab_sizes: PlacementTabSizes,
    /// The tab sizes of a swap with each destination pane, in the destination
    /// tree's leaf order, when the source pane is tiled in another tab than
    /// the destination. Empty otherwise.
    pub swap_tab_sizes: Vec<SwapTabSizes>,
}

/// Where the source pane of a placement preview sits before the placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementSource {
    /// A leaf of this tab's layout: the source tab's tree and solved layout.
    Tiled(PlacementTabSnapshot),
    /// A floating pane.
    Floating,
}

impl PlacementSnapshot {
    /// The tab snapshot whose layout holds the source pane, or `None` when the
    /// source pane floats.
    #[must_use]
    pub fn find_source_tab_snapshot(&self) -> Option<&PlacementTabSnapshot> {
        match &self.source {
            PlacementSource::Tiled(source_tab_snapshot) => Some(source_tab_snapshot),
            PlacementSource::Floating => None,
        }
    }

    /// The tab the preview shows: the source tab snapshot when the destination
    /// is the source pane's tab, else the destination tab snapshot. `None` for
    /// another tab with no destination snapshot.
    #[must_use]
    pub fn find_displayed_tab_snapshot(&self) -> Option<&PlacementTabSnapshot> {
        match self.find_source_tab_snapshot() {
            Some(source_tab_snapshot)
                if source_tab_snapshot.tab_snapshot.tab_id == self.destination_tab_id =>
            {
                Some(source_tab_snapshot)
            }
            _ => self.destination_tab_snapshot.as_ref(),
        }
    }
}

/// Text drawn inside one softened pane during placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanePlacementMessage {
    /// The sentence shown when the area has room for it.
    pub full_text: String,
    /// The short sentence shown in a narrow area.
    pub compact_text: &'static str,
    /// The second line shown when the area has room for it.
    pub detail_text: Option<String>,
}

/// One viewer's visual explanation of a pane placement proposal or confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanePlacementPresentation {
    /// The pane the viewer is moving.
    pub source_pane_id: PaneId,
    /// Panes in the selected destination span, excluding the source pane.
    pub target_pane_ids: Vec<PaneId>,
    /// The moving pane's message, shown in uncovered visible content when no
    /// stack header for this pane is displayed.
    pub source_message: PanePlacementMessage,
    /// The destination message, shown in one affected pane's uncovered visible
    /// content when an area is available.
    pub target_message: Option<PanePlacementMessage>,
}

impl PanePlacementPresentation {
    /// Whether the pane is the moving pane or belongs to the destination span.
    #[must_use]
    pub fn is_pane_affected(&self, pane_id: PaneId) -> bool {
        pane_id == self.source_pane_id || self.target_pane_ids.contains(&pane_id)
    }
}

/// One source or destination tab in a placement preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementTabSnapshot {
    /// The tab's unsolved layout tree, each stack at its shared active member.
    pub layout_tree: LayoutNode,
    /// The requesting client's focus history in the tab, newest first, with
    /// only panes of `layout_tree`. Each stack of `tab_snapshot` shows the
    /// member this history gives ([`LayoutNode::build_shown_layout_tree`]).
    pub focus_history: Vec<PaneId>,
    /// The solved tab layout.
    pub tab_snapshot: TabSnapshot,
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

    /// Borrow this frame as the client paints it: [`Self::build_frame_layout`]
    /// with `committed_regions`, the region solve the paint places the
    /// tabline, the statusline and the pane area with.
    #[must_use]
    pub fn build_painted_frame_layout<'a>(
        &'a self,
        viewer_chrome: ViewerChrome,
        committed_regions: &'a CommittedRegions,
    ) -> FrameLayout<'a> {
        FrameLayout {
            committed_regions: Some(committed_regions),
            ..self.build_frame_layout(viewer_chrome)
        }
    }

    /// The content snapshot of the pane `pane_id` in this frame, or `None`
    /// when the frame carries no content for it.
    #[must_use]
    pub fn find_pane_snapshot(&self, pane_id: PaneId) -> Option<&PaneSnapshot> {
        self.pane_snapshots
            .iter()
            .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
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
    /// and statusline solve for `viewport_size`, tagged with
    /// `region_input_revision`.
    #[must_use]
    pub fn build_core(viewport_size: Size, region_input_revision: u64) -> Self {
        Self::from_solved_regions(
            viewport_size,
            solve_core_regions(viewport_size),
            region_input_revision,
        )
    }
}

/// One item on a subscriber's queue: a live event, the frame composed for the
/// subscriber's client, a fresh frame that resyncs a subscriber whose queue
/// overflowed, the answers to one round of mouse actions, a wake-up for the bytes that wait for the
/// subscriber's own terminal, the session its client moves to, or a pane
/// placement command or floating pane move the session rejected.
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
    /// Bytes for the terminal the subscriber's client runs in wait beside the
    /// queue, and the thread that writes the client's connection writes them
    /// verbatim. This item carries none of them: it wakes that thread.
    HostWrite,
    /// The session the subscriber's client leaves this one for.
    SwitchTo {
        /// The session the client attaches to next.
        session_id: SessionId,
        /// The user id whose folder of the shared directory advertises
        /// `session_id`, or `None` for a session the client looks up by its id
        /// alone.
        owner_user_id: Option<u32>,
    },
    /// A pane placement command from the subscriber's key binding, or a
    /// floating pane move from its key binding or its mouse round, was
    /// rejected.
    PlacementCommandRejected(CommandId),
}

/// The viewer-owned frame state: which pane the pointer is over, which pane
/// shows its placement handle and whether the pointer rests on that handle,
/// which input mode owns the keymap, where
/// the tab strip is scrolled, whether the viewer holds a link to its session,
/// where it draws a floating pane it is moving, and its floating pane tray. While pane placement is
/// shown, including while a confirmation waits, the viewer clears the pointer
/// and handle fields before hit-testing and painting, and the renderer ignores
/// the pointer field: hovering `pane-123` leaves its border color unchanged.
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
    /// The placement handle hover this frame shows, as
    /// [`find_shown_placement_handle_hover`] gives it, or `None`. Hit-testing
    /// and painting read it as it is.
    pub placement_handle_hover: Option<PlacementHandleHover>,
    /// The effective input mode for the viewer's keymap and mode tag. `None`
    /// lets generic frame consumers use the mode carried by the session frame.
    pub active_input_mode: Option<LockMode>,
    /// Whether pane placement suppresses hover styling for this painted frame.
    pub is_pane_placement_visible: bool,
    /// The pane being placed, used for border styling before the preview treatment.
    pub placement_source_pane_id: Option<PaneId>,
    /// Where the viewer's tab strip is scrolled: `None` follows the active tab —
    /// the strip always reveals it — while `Some(i)` peeks from tab index `i`
    /// without changing focus. The renderer windows the tab list from this and
    /// clamps an index past the last tab.
    pub tabline_offset: Option<usize>,
    /// Whether the viewer holds a link to its session, and how it is getting
    /// one back when it does not. The tabline draws
    /// `RECONNECTING (attempt 4, retry in 8s)` from a
    /// `Redialing(Reconnecting { attempt: 4, retry_in_seconds: Some(8) })`,
    /// and `RECONNECTING (attempt 4, dialing)` from a
    /// `Redialing(Reconnecting { attempt: 4, retry_in_seconds: None })`.
    pub link_status: LinkStatus,
    /// The floating pane this viewer is moving, drawn and hit-tested at the
    /// draft's top-left cell, moved inside the pane area, instead of the cell
    /// the frame places it at. A pinned pane stays at the frame's cell. `None`
    /// while no move is under way.
    pub floating_pane_move_draft: Option<FloatingPaneMoveDraft>,
    /// The viewer's open floating pane tray, drawn under the tabline's tray
    /// button and hit-tested over the floating panes. `None` while the tray
    /// is closed.
    pub floating_pane_tray: Option<FloatingPaneTray>,
}

/// One viewer's open floating pane tray: the tile its keys act on, and the
/// tile under its pointer.
///
/// A tile index counts the session's
/// [`floating_panes`](SessionSnapshot::floating_panes) from `0`: tile `1`
/// shows the second floating pane. The viewer keeps both indexes below the
/// number of floating panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneTray {
    /// The tile that `core:toggle-selected-tray-tile` acts on, drawn with the
    /// hover border.
    pub selected_tile_index: usize,
    /// The tile under the viewer's pointer, or `None` off every tile. The
    /// tray's header shows this tile's title, and the selected tile's title
    /// while it is `None`.
    pub hovered_tile_index: Option<usize>,
}

/// The placement handle hover a viewer shows for `pointer_hover`, the handle or
/// tiled top border its pointer rests on: `pointer_hover` while a left press on
/// that handle starts a drag of its pane, and `None` otherwise.
/// `active_input_mode` is the mode that owns the viewer's keyboard.
/// `is_dock_undock_command_pending` is `true` while dock/undock mode waits for
/// the session's answer to the undock or the dock it sent.
/// `is_floating_pane_move_waiting` is `true` while a floating pane move that
/// the viewer sent waits for the session's answer.
///
/// - While `is_pane_placement_visible` is `true`, or `link_status` is not
///   [`Linked`](LinkStatus::Linked), it is `None`.
/// - A [`TiledTopBorder`](PlacementHandleHover::TiledTopBorder) or
///   [`TiledHandle`](PlacementHandleHover::TiledHandle) hover shows in
///   `Normal`, `Locked`, `PaneMoveChoice` and `FloatingPaneTray`, and in
///   `DockUndock` while `is_dock_undock_command_pending` is `false`.
/// - A [`FloatingHandle`](PlacementHandleHover::FloatingHandle) hover of an
///   unpinned pane shows in `Normal`, `Locked`, `PaneMoveChoice`,
///   `FloatingPaneMove` and `FloatingPaneTray` while
///   `is_floating_pane_move_waiting` is `false`. In
///   `DockUndock`, a pinned or unpinned pane's hover shows while
///   `is_dock_undock_command_pending` and `is_floating_pane_move_waiting` are
///   both `false`.
/// - No hover shows in `PanePlacement`.
///
/// `TiledTopBorder { pane_id: pane-123 }` gives itself in `Normal` and `None`
/// in `FloatingPaneMove`.
#[must_use]
pub fn find_shown_placement_handle_hover(
    pointer_hover: Option<PlacementHandleHover>,
    active_input_mode: LockMode,
    link_status: LinkStatus,
    is_pane_placement_visible: bool,
    is_dock_undock_command_pending: bool,
    is_floating_pane_move_waiting: bool,
) -> Option<PlacementHandleHover> {
    let placement_handle_hover = pointer_hover?;
    if is_pane_placement_visible || link_status != LinkStatus::Linked {
        return None;
    }
    let is_drag_started_by_press = match placement_handle_hover {
        PlacementHandleHover::TiledTopBorder { .. } | PlacementHandleHover::TiledHandle { .. } => {
            match active_input_mode {
                LockMode::Normal
                | LockMode::Locked
                | LockMode::PaneMoveChoice
                | LockMode::FloatingPaneTray => true,
                LockMode::DockUndock => !is_dock_undock_command_pending,
                LockMode::PanePlacement | LockMode::FloatingPaneMove => false,
            }
        }
        PlacementHandleHover::FloatingHandle { is_pinned, .. } => match active_input_mode {
            LockMode::Normal
            | LockMode::Locked
            | LockMode::PaneMoveChoice
            | LockMode::FloatingPaneMove
            | LockMode::FloatingPaneTray => !is_pinned && !is_floating_pane_move_waiting,
            LockMode::DockUndock => {
                !is_dock_undock_command_pending && !is_floating_pane_move_waiting
            }
            LockMode::PanePlacement => false,
        },
    };
    is_drag_started_by_press.then_some(placement_handle_hover)
}

/// Where a viewer's pointer rests on one pane's placement handle. A tiled pane
/// draws its handle `⠿⠿⠿` under both tiled variants. Only
/// [`TiledHandle`](PlacementHandleHover::TiledHandle) and
/// [`FloatingHandle`](PlacementHandleHover::FloatingHandle) give the pane the
/// placement hover outline and tint.
///
/// The pointer on column `10` of tiled `pane-123`'s top border is
/// `TiledTopBorder { pane_id: pane-123 }`, and on column `2` it is
/// `TiledHandle { pane_id: pane-123 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementHandleHover {
    /// The pointer rests on a tiled pane's top border, between its corners and
    /// off its handle cells.
    TiledTopBorder {
        /// The tiled pane whose top border the pointer rests on.
        pane_id: PaneId,
    },
    /// The pointer rests on a tiled pane's handle cells `⠿⠿⠿`.
    TiledHandle {
        /// The tiled pane whose handle the pointer rests on.
        pane_id: PaneId,
    },
    /// The pointer rests on a floating pane's grab handle `⣿⣿`.
    FloatingHandle {
        /// The floating pane whose grab handle the pointer rests on.
        pane_id: PaneId,
        /// Whether the frame the pointer moved over drew the pane pinned.
        is_pinned: bool,
    },
}

/// Whether a viewer holds a link to its session, and how it is getting one back
/// when it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkStatus {
    /// The viewer's requests reach the session, and its frames arrive.
    #[default]
    Linked,
    /// The session is replacing its own process image, and the viewer waits
    /// to join its new image. The tabline draws no tag.
    AwaitingRestartedSession,
    /// The link to a session on a server dropped, and the viewer dials that
    /// server again. The tabline draws the tag this value describes.
    Redialing(Reconnecting),
}

/// A floating pane this viewer draws at a cell of its own choosing, before the
/// session places it there.
///
/// The frame's slot for `pane_id` keeps its size and pin state; only its
/// top-left cell becomes `top_left_cell`, moved left and up until the slot lies
/// inside the pane area ([`FrameLayout::list_drawn_floating_pane_slots`]). In
/// a `120x38` pane area, a slot at `(24, 8)` `72x22` with a draft at `(29, 8)`
/// draws at `(29, 8)` `72x22`, its content at `(30, 11)`. A pinned slot keeps
/// the cell the frame places it at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneMoveDraft {
    /// The floating pane being moved.
    pub pane_id: PaneId,
    /// The pane's top-left cell that the viewer asks for, counted from the
    /// viewer's pane-area origin. It may lie past the pane area.
    pub top_left_cell: Point,
}

/// How far a viewer that dials its server again has got: which dial runs or
/// comes next, and how many seconds are left before it goes out.
///
/// The viewer replaces this each time the whole seconds left change, so the
/// tag it draws counts down: `retry_in_seconds` `Some(3)`, then `Some(2)`, then
/// `Some(1)`, then `None` while the dial runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconnecting {
    /// Which dial runs or comes next, counting from 1. Drawn as the
    /// `attempt 4` part of `RECONNECTING (attempt 4, retry in 8s)`.
    pub attempt: u32,
    /// Whole seconds left before that dial goes out, rounded up, drawn as the
    /// `retry in 8s` part of `RECONNECTING (attempt 4, retry in 8s)`. `None`
    /// while the dial runs, drawn as the `dialing` part of
    /// `RECONNECTING (attempt 4, dialing)`.
    pub retry_in_seconds: Option<u32>,
}

/// Where a frame's surfaces sit, borrowed: the session with its solved active
/// tab, the viewing client, the viewer's own chrome state, and an optional
/// client-side region commit. Carries no pane content and no colors.
///
/// Hit-testing reads the session, client, viewer, and committed region solve
/// from this value. `FrameLayout::get_tabline_inputs` returns the session
/// name, tabs, lock mode, mouse-selection state, reconnect state, and tab
/// offset used by the tabline solve. Hit-testing and tabline solving use cell
/// coordinates. This value has no theme; renderers apply colors when they draw
/// cells.
///
/// A caller that already holds a [`RenderSnapshot`] borrows one out of it with
/// [`RenderSnapshot::build_frame_layout`], which leaves the committed region
/// solve unset, and hit-testing then works from the built-in geometry. A [`MouseFrame`]
/// fills the solve in with the one that placed the pane area when the frame was
/// painted, and the viewer chrome with the one painted with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout<'a> {
    /// The session being viewed, including its solved active tab.
    pub session_snapshot: &'a SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: &'a ClientSnapshot,
    /// The viewer's own chrome: the hovered pane, the marked placement handle,
    /// the input mode, the pane placement preview, the tab strip's scroll, the
    /// reconnect state, the floating pane move draft, and the floating pane
    /// tray.
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
    /// The lock mode is the viewer's `active_input_mode` when one is set, and
    /// the client's `lock_mode` otherwise. The tray button shows while the
    /// session holds a floating pane, highlighted while the viewer's tray is
    /// open.
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
            reconnecting: match self.viewer_chrome.link_status {
                LinkStatus::Redialing(reconnecting) => Some(reconnecting),
                LinkStatus::Linked | LinkStatus::AwaitingRestartedSession => None,
            },
            tabline_offset: self.viewer_chrome.tabline_offset,
            has_floating_panes: !self.session_snapshot.floating_panes.is_empty(),
            is_floating_pane_tray_open: self.viewer_chrome.floating_pane_tray.is_some(),
        }
    }

    /// The floating panes this viewer draws in this frame, back to front:
    /// every slot in the session's
    /// [`floating_pane_slots`](SessionSnapshot::floating_pane_slots). The
    /// unpinned slot that `viewer_chrome.floating_pane_move_draft` names starts
    /// at the draft's top-left cell, then moves left and up until it lies
    /// inside this frame's pane area ([`Rect::compute_moved_inside`]). While
    /// `viewer_chrome.is_pane_placement_visible` is `true`, the only slot is
    /// that of `viewer_chrome.placement_source_pane_id`, and only while the
    /// active tab holds no slot of that pane. None while the active tab's
    /// `is_every_pane_suppressed` is `true`: the frame shows
    /// `Terminal too small — enlarge window` instead.
    ///
    /// The pane area comes from `committed_regions`, or is the whole viewport
    /// when they are `None`. Example: a `72x22` slot with a draft at `(60, 20)`
    /// in a `120x38` pane area draws at `(48, 16)`.
    pub fn list_drawn_floating_pane_slots(
        &self,
    ) -> impl DoubleEndedIterator<Item = FloatingPaneSlot> + 'a {
        let floating_pane_slots: &'a [FloatingPaneSlot] = if self
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
        {
            &[]
        } else {
            &self.session_snapshot.floating_pane_slots
        };
        let frame_layout = *self;
        let is_drawn_floating_pane_slot = move |floating_pane_slot: &&FloatingPaneSlot| {
            !frame_layout.viewer_chrome.is_pane_placement_visible
                || (frame_layout.viewer_chrome.placement_source_pane_id
                    == Some(floating_pane_slot.pane_id)
                    && !frame_layout
                        .session_snapshot
                        .active_tab_snapshot
                        .pane_slots
                        .iter()
                        .any(|pane_slot| pane_slot.pane_id == floating_pane_slot.pane_id))
        };
        floating_pane_slots
            .iter()
            .filter(is_drawn_floating_pane_slot)
            .map(move |floating_pane_slot| {
                match frame_layout.viewer_chrome.floating_pane_move_draft {
                    Some(floating_pane_move_draft)
                        if floating_pane_move_draft.pane_id == floating_pane_slot.pane_id
                            && !floating_pane_slot.is_pinned =>
                    {
                        let frame_pane_area = compute_frame_pane_area(
                            frame_layout,
                            compute_viewport_area(frame_layout),
                        );
                        FloatingPaneSlot::from_outer_rect(
                            floating_pane_slot.pane_id,
                            Rect::from_origin_and_size(
                                floating_pane_move_draft.top_left_cell,
                                floating_pane_slot.outer_rect.size,
                            )
                            .compute_moved_inside(Size {
                                column_count: frame_pane_area.width,
                                row_count: frame_pane_area.height,
                            }),
                            floating_pane_slot.is_pinned,
                        )
                    }
                    _ => *floating_pane_slot,
                }
            })
    }

    /// The slot of the floating pane `pane_id` as this frame draws it
    /// ([`list_drawn_floating_pane_slots`](Self::list_drawn_floating_pane_slots)),
    /// at its move draft's cell while one names it, or `None` when the frame
    /// draws no such floating pane.
    #[must_use]
    pub fn find_drawn_floating_pane_slot(&self, pane_id: PaneId) -> Option<FloatingPaneSlot> {
        self.list_drawn_floating_pane_slots()
            .find(|floating_pane_slot| floating_pane_slot.pane_id == pane_id)
    }
}

/// A painted frame cut down to what answering a mouse event reads: where the
/// surfaces sit, the committed region solve, the viewer chrome it was painted
/// with, and the few per-pane fields that say which line each pane's top row
/// shows and where an event over it goes.
///
/// It carries no cells, no cursor and no pane content titles, so a viewer
/// holding one between paints holds no pane's rows. The session's floating
/// pane list keeps its titles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseFrame {
    /// The session being viewed, including its solved active tab.
    pub session_snapshot: SessionSnapshot,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: ClientSnapshot,
    /// One entry per pane the frame carried content for, matched to a
    /// [`PaneSlot`] or a [`FloatingPaneSlot`] by pane id.
    pub mouse_panes: Vec<MousePane>,
    /// The region solve and input revision that were painted with this frame.
    pub committed_regions: CommittedRegions,
    /// The viewer chrome that was painted with this frame: the hovered pane,
    /// the marked placement handle, the tab strip's scroll, the input mode and
    /// reconnect tags, the pane placement preview, the floating pane move
    /// draft, and the floating pane tray.
    pub viewer_chrome: ViewerChrome,
}

impl MouseFrame {
    /// Borrow the parts of this painted frame that say where things sit, with
    /// the viewer chrome it was painted with.
    #[must_use]
    pub fn build_frame_layout(&self) -> FrameLayout<'_> {
        FrameLayout {
            session_snapshot: &self.session_snapshot,
            client_snapshot: &self.client_snapshot,
            viewer_chrome: self.viewer_chrome,
            committed_regions: Some(&self.committed_regions),
        }
    }

    /// Build a mouse frame from a borrowed render snapshot, the regions it was
    /// painted with, and the viewer chrome it was painted with.
    ///
    /// The mouse frame copies session, client, and per-pane input data. It does
    /// not copy pane grids.
    #[must_use]
    pub fn from_snapshot(
        render_snapshot: &RenderSnapshot,
        committed_regions: CommittedRegions,
        viewer_chrome: ViewerChrome,
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
            viewer_chrome,
        }
    }
}

/// One pane as a mouse event reads it: which pane, which line its top visible
/// row is, and what decides where a wheel tick over it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePane {
    /// The pane this entry describes, matched to a [`PaneSlot`] or a
    /// [`FloatingPaneSlot`] by id.
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

/// The session-scoped part of a frame: identity plus the active tab, the
/// metadata needed to draw the tab bar, and the floating panes the viewing
/// client shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The session's stable id.
    pub session_id: SessionId,
    /// The session's display name.
    pub session_name: String,
    /// The tab currently shown, solved and ready to draw.
    pub active_tab_snapshot: TabSnapshot,
    /// Lightweight entry per tab for the tab bar (index, name, active marker).
    pub tabs_metadata: Vec<TabMetadata>,
    /// The floating panes the viewing client shows, back to front: the pane
    /// shown longest first, the focused one last. Empty when the client has no
    /// floating viewport: it reports [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving), or a pane area
    /// narrower than 5 columns or shorter than 6 rows.
    pub floating_pane_slots: Vec<FloatingPaneSlot>,
    /// Every floating pane of the session, in the order the panes became
    /// floating, whether or not the viewing client shows it, each with its
    /// title and whether the viewing client minimized it. With floats `a`,
    /// `b` and `c` made in that order and `b` minimized, it lists `a`, `b`
    /// and `c`, and only `b` has `is_minimized` set.
    pub floating_panes: Vec<FloatingPane>,
}

impl SessionSnapshot {
    /// The index of `pane_id` in the session's
    /// [`floating_panes`](Self::floating_panes), or `None` when it is no
    /// floating pane. With floating panes `htop` and `logs`, `logs` gives
    /// `Some(1)`.
    #[must_use]
    pub fn find_floating_pane_index(&self, pane_id: PaneId) -> Option<usize> {
        self.floating_panes
            .iter()
            .position(|floating_pane| floating_pane.pane_id == pane_id)
    }

    /// Whether `pane_id` is one of the session's
    /// [`floating_panes`](Self::floating_panes), shown or minimized.
    #[must_use]
    pub fn is_floating_pane(&self, pane_id: PaneId) -> bool {
        self.find_floating_pane_index(pane_id).is_some()
    }

    /// Whether `pane_id` is one of the session's
    /// [`floating_panes`](Self::floating_panes) and the viewing client
    /// minimized it.
    #[must_use]
    pub fn is_floating_pane_minimized(&self, pane_id: PaneId) -> bool {
        self.floating_panes
            .iter()
            .any(|floating_pane| floating_pane.pane_id == pane_id && floating_pane.is_minimized)
    }

    /// Whether the viewing client minimized every one of the session's
    /// [`floating_panes`](Self::floating_panes). `true` when the session
    /// holds no floating pane.
    #[must_use]
    pub fn are_all_floating_panes_minimized(&self) -> bool {
        self.floating_panes
            .iter()
            .all(|floating_pane| floating_pane.is_minimized)
    }
}

/// One floating pane of the session, shown or minimized for the viewing
/// client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatingPane {
    /// The floating pane.
    pub pane_id: PaneId,
    /// The pane's resolved display title, by the rule of
    /// [`PaneSnapshot::pane_title`]. `None` when the pane has reported neither
    /// a working directory nor a title.
    pub pane_title: Option<String>,
    /// Whether the viewing client minimized the pane. The session gives a
    /// minimized pane no slot in
    /// [`floating_pane_slots`](SessionSnapshot::floating_pane_slots), and a
    /// pane the client did not minimize a slot.
    pub is_minimized: bool,
}

/// One tab's entry in the tab bar: enough to draw the tab list without its full
/// layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabMetadata {
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
    /// The size the tab's layout was solved for: the per-axis minimum pane
    /// area across the clients viewing this tab, or `0x0` while each of them
    /// reports [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving).
    /// The [`pane_slots`](Self::pane_slots) rects live in this space with
    /// origin `(0, 0)`. A client whose own
    /// [`viewport_size`](ClientSnapshot::viewport_size)
    /// is larger draws this layout centered and letterboxes the surrounding
    /// margin; a client at exactly this size draws it edge to edge.
    pub tab_size: Size,
    /// Header strips for stacked panes (title bars for collapsed stack members).
    pub stack_headers: Vec<StackHeader>,
    /// Whether **this snapshot's client** sees the tab tiled, or sees a single
    /// pane zoomed to fill it. Zoom is per-client, so another client viewing the
    /// same tab in the same frame can carry a different value here.
    pub layout_mode: LayoutMode,
    /// True when every pane is suppressed because the tab has no room to draw —
    /// the renderer fills the whole frame with the "terminal too small" overlay.
    pub is_every_pane_suppressed: bool,
    /// Blank cells between two panes that meet along a horizontal or
    /// vertical split, in the [`tab_size`](Self::tab_size) space.
    pub gap_cell_count: u16,
}

impl TabSnapshot {
    /// The slot of `pane_id` when its [`is_visible`](PaneSlot::is_visible) is
    /// `true`. `None` when the tab has no slot for `pane_id`, or the slot is
    /// hidden.
    #[must_use]
    pub fn find_visible_pane_slot(&self, pane_id: PaneId) -> Option<&PaneSlot> {
        self.pane_slots
            .iter()
            .find(|pane_slot| pane_slot.is_visible && pane_slot.pane_id == pane_id)
    }
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`PaneSnapshot`] by
/// [`pane_id`](Self::pane_id).
///
/// The builder keeps these fields consistent: [`is_visible`](Self::is_visible)
/// is true exactly when [`content_rect`](Self::content_rect) is `Some` (the pane has a
/// content area to draw), and an [`is_suppressed`](Self::is_suppressed) pane is not
/// visible. `content_rect` is `None` for three distinct reasons — no room,
/// hidden, or a collapsed stack member — and [`is_suppressed`](Self::is_suppressed)
/// marks the no-room case. A placement slide can keep a pane box visible while
/// its content rect is absent and a stack header for the same pane is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    pub outer_rect: Rect,
    /// The content area inside the border — the layout-owned rect the PTY was
    /// sized from, taken verbatim from [`list_content_rects`]. `None` when the
    /// pane shows no content (suppressed, hidden, or a collapsed stack member).
    /// The renderer draws cells and places the cursor here and never re-computes
    /// the inset.
    pub content_rect: Option<Rect>,
    /// Whether the pane is currently shown.
    pub is_visible: bool,
    /// Whether the pane is suppressed for lack of room.
    pub is_suppressed: bool,
}

/// One [`PaneSlot`] per leaf of `layout_solve`, in solve order: the outer rect
/// from the solve, the content rect from [`list_content_rects`], and
/// `is_suppressed` for each pane in
/// [`suppressed_pane_ids`](LayoutSolve::suppressed_pane_ids).
#[must_use]
pub fn list_pane_slots(layout_solve: &LayoutSolve) -> Vec<PaneSlot> {
    let suppressed_pane_ids: HashSet<PaneId> =
        layout_solve.suppressed_pane_ids.iter().copied().collect();
    layout_solve
        .pane_rects
        .iter()
        .zip(list_content_rects(layout_solve))
        .map(
            |(&(pane_id, outer_rect), (content_pane_id, content_rect))| {
                debug_assert_eq!(pane_id, content_pane_id);
                PaneSlot {
                    pane_id,
                    outer_rect,
                    content_rect,
                    is_visible: content_rect.is_some(),
                    is_suppressed: suppressed_pane_ids.contains(&pane_id),
                }
            },
        )
        .collect()
}

/// One floating pane the viewing client shows, placed for that client.
/// Paired with a [`PaneSnapshot`] by [`pane_id`](Self::pane_id).
///
/// Both rects count cells from the client's pane-area origin, with no
/// letterbox offset: an outer rect at column `150` draws at pane-area column
/// `150`, also when the client's tiled layout starts at column `60`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneSlot {
    /// The floating pane this slot places.
    pub pane_id: PaneId,
    /// The pane's box with its chrome: the border ring, the titlebar row and
    /// the separator row.
    pub outer_rect: Rect,
    /// The cells inside the chrome, the size of the pane's PTY
    /// ([`compute_floating_pane_content_rect`]). The renderer draws cells and
    /// places the cursor here.
    pub content_rect: Rect,
    /// Whether the viewing client pinned the pane at its cell.
    pub is_pinned: bool,
}

impl FloatingPaneSlot {
    /// The slot of the floating pane `pane_id` drawn in the box `outer_rect`,
    /// its content rect taken from that box
    /// ([`compute_floating_pane_content_rect`]). Outer `(24, 8)` `72x22` gives
    /// content `(25, 11)` `70x18`.
    #[must_use]
    pub fn from_outer_rect(pane_id: PaneId, outer_rect: Rect, is_pinned: bool) -> Self {
        Self {
            pane_id,
            outer_rect,
            content_rect: compute_floating_pane_content_rect(outer_rect),
            is_pinned,
        }
    }
}

/// One pane's content: what the renderer paints inside the matching
/// [`PaneSlot`]'s or [`FloatingPaneSlot`]'s content rect, plus what a mouse
/// event over this pane is answered from.
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
    /// The pane this content belongs to, matched to a [`PaneSlot`] or a
    /// [`FloatingPaneSlot`] by id.
    pub pane_id: PaneId,
    /// The pane's resolved display title: on the alternate screen the running
    /// app's OSC 0/1/2 title; on the primary screen the shell's OSC 7 working
    /// directory (`~`-shortened), falling back to the OSC title. `None` when
    /// the pane has reported neither.
    pub pane_title: Option<String>,
    /// The cursor's position and visibility within the content area.
    pub cursor_snapshot: CursorSnapshot,
    /// The visible terminal cells. `None` for a pane that shows nothing this
    /// frame.
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
    pub scrollback_metadata: ScrollbackMetadata,
}

impl PaneSnapshot {
    /// A snapshot of `pane_id` that shows nothing: no title, no grid, no
    /// images, a hidden non-blinking cursor at `(0, 0)` with no shape, mouse
    /// tracking off, the primary screen at view row `0`, no highlight, and no
    /// retained scrollback lines.
    #[must_use]
    pub fn from_empty_pane(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            pane_title: None,
            cursor_snapshot: CursorSnapshot {
                row_index: 0,
                column_index: 0,
                is_visible: false,
                is_blinking: false,
                shape: None,
            },
            terminal_grid_view: None,
            image_placement_snapshots: Vec::new(),
            is_reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            is_alternate_scroll_enabled: false,
            is_on_alternate_screen: false,
            view_top_row_index: 0,
            selection_spans: None,
            has_selection: false,
            scrollback_metadata: ScrollbackMetadata {
                retained_line_count: 0,
            },
        }
    }
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
    /// The highlighted column range on `row_index`, or `None` if it has none.
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
        is_blinking: bool,
    },
}

/// The visible rows of one pane, top to bottom, plus how far the view is
/// scrolled back from the tail.
///
/// Each row is a shared [`RowSnapshot`], so cloning a built [`GridView`]
/// copies no cell. A view scrolled back by `view_row_offset` lists that many
/// history rows above the top of the live screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridView {
    /// The visible rows, top to bottom.
    pub row_snapshots: Vec<Arc<RowSnapshot>>,
    /// The column count of the pane. A row holding fewer cells shows nothing
    /// past its last cell, and a row holding more shows its first
    /// `column_count` cells.
    pub column_count: u16,
    /// Rows scrolled up from the live tail; `0` shows the live bottom of the
    /// buffer.
    pub view_row_offset: usize,
}

impl GridView {
    /// The view of every row of `terminal_grid`, scrolled back by
    /// `view_row_offset`.
    #[must_use]
    pub fn from_grid(terminal_grid: &Grid, view_row_offset: usize) -> Self {
        let (_, column_count) = terminal_grid.get_grid_dimensions();
        GridView {
            row_snapshots: terminal_grid.list_row_snapshots().collect(),
            column_count,
            view_row_offset,
        }
    }

    /// The `(row_count, column_count)` the view shows.
    #[must_use]
    pub fn get_view_dimensions(&self) -> (u16, u16) {
        (
            u16::try_from(self.row_snapshots.len()).unwrap_or(u16::MAX),
            self.column_count,
        )
    }

    /// The cell at `row_index`/`column_index`, or `None` past the view's
    /// rows, past its column count, or past the row's last cell.
    #[must_use]
    pub fn get_cell(&self, row_index: u16, column_index: u16) -> Option<&Cell> {
        if column_index >= self.column_count {
            return None;
        }
        self.row_snapshots
            .get(usize::from(row_index))?
            .list_row_cells()
            .get(usize::from(column_index))
    }
}

/// One validated terminal image placement carried in a read-only frame.
///
/// A local snapshot shares the image record and copies no RGBA pixels.
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
    pub fn build_unavailable(
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
/// scroll-position indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackMetadata {
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
    /// The client's terminal size in cells.
    pub viewport_size: Size,
    /// The tab the client is currently viewing.
    pub active_tab_id: TabId,
    /// The pane that takes the client's input: its focused floating pane,
    /// else its focused pane in the active tab. `None` when the client focuses
    /// neither. The renderer highlights the pane whose [`PaneSlot::pane_id`]
    /// or [`FloatingPaneSlot::pane_id`] matches, and places the cursor there.
    pub focused_pane_id: Option<PaneId>,
    /// The pane the client focused in its active tab, also while a focused
    /// floating pane takes its input. `None` when the client focuses no pane
    /// in that tab. Pane placement mode places this pane.
    pub active_tab_focused_pane_id: Option<PaneId>,
    /// The client's input mode, as the session has it: it drives the mode tag,
    /// decides whether a paste from the client's own terminal reaches the pane,
    /// and is what `koshi list-clients` reports.
    pub lock_mode: LockMode,
    /// The last lock command that the client's own connection sent and the
    /// session ran before it built this snapshot. `None` before the session
    /// runs one, and again after the session restarts. A viewer that sent a
    /// lock command keeps its own lock mode until a frame names that command
    /// here.
    pub last_applied_lock_command_id: Option<CommandId>,
    /// The last floating pane move that the client's own connection sent,
    /// from its mouse or a key binding, and the session ran before it built
    /// this snapshot. `None` before the session runs one, and again after the
    /// session restarts. A viewer that sent a move keeps drawing the pane at
    /// the move's cell until a frame names that move here.
    pub last_applied_floating_pane_move_command_id: Option<CommandId>,
    /// The last tiled pane placement that the client's own connection
    /// confirmed and the session ran before it built this snapshot. `None`
    /// before the session runs one, and again after the session restarts. A
    /// viewer that confirmed a placement waits until a frame names it here,
    /// and that frame shows the placement.
    pub last_applied_placement_command_id: Option<CommandId>,
    /// Whether this client grabs the mouse for text selection. Adds the `SELECT`
    /// tag to the mode indicator. It is independent of
    /// [`lock_mode`](Self::lock_mode): both can be on at once. The viewer also
    /// reads it off a painted frame to decide whether a press in a mouse-aware
    /// pane begins a highlight.
    pub is_mouse_selection_enabled: bool,
}

#[cfg(test)]
mod tests;
