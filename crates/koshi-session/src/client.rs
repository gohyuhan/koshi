//! Attached clients: the identity and per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport, input modes and
//! the view of each floating pane live on each client; the session holds only
//! this registry. Each client also carries what the server set at attach: its
//! origin, its generated label and its color.

use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    time::SystemTime,
};

pub use koshi_core::client::ClientOrigin;
use koshi_core::{
    command::Selection,
    constant::FLOATING_PANE_CHROME_SIZE,
    geometry::{compute_percent_cell_count, PaneArea, PixelCellSize, Point, Rect, Size},
    ids::{ClientId, CommandId, PaneId, SessionId, TabId},
    lock::LockMode,
};
use koshi_layout::{
    mode::LayoutMode,
    solver::{LayoutSolve, MIN_PANE_SIZE},
};
use serde::{Deserialize, Serialize};

/// The smallest outer size of a floating pane: [`MIN_PANE_SIZE`] of content
/// plus [`FLOATING_PANE_CHROME_SIZE`], `4x5`. A client whose
/// [`compute_floating_pane_size_cap`] of its pane area is smaller on either
/// axis has no floating viewport ([`Client::get_floating_viewport`]).
pub const MIN_FLOATING_PANE_SIZE: Size = Size {
    column_count: MIN_PANE_SIZE.column_count + FLOATING_PANE_CHROME_SIZE.column_count,
    row_count: MIN_PANE_SIZE.row_count + FLOATING_PANE_CHROME_SIZE.row_count,
};

/// The percent of a floating viewport's axis that a floating pane spans at
/// most: `90`.
pub const MAX_FLOATING_PANE_AXIS_PERCENT: u8 = 90;

/// The largest outer size a floating pane takes in `floating_viewport`:
/// [`MAX_FLOATING_PANE_AXIS_PERCENT`] of each axis, rounded down. `80x22` →
/// `72x19`; `5x6` → `4x5`.
#[must_use]
pub fn compute_floating_pane_size_cap(floating_viewport: Size) -> Size {
    Size {
        column_count: compute_percent_cell_count(
            floating_viewport.column_count,
            MAX_FLOATING_PANE_AXIS_PERCENT,
        ),
        row_count: compute_percent_cell_count(
            floating_viewport.row_count,
            MAX_FLOATING_PANE_AXIS_PERCENT,
        ),
    }
}

/// The pane region of a client that reported none: the full viewport minus
/// one top tabline row and one bottom key-hint row. `80x24` → `80x22`; a
/// viewport two rows tall or shorter gives `0` rows.
#[must_use]
pub const fn compute_default_pane_area_size(viewport_size: Size) -> Size {
    Size {
        column_count: viewport_size.column_count,
        row_count: viewport_size.row_count.saturating_sub(2),
    }
}

/// One client's view of one floating pane: where this client draws it, and
/// whether it is minimized for this client.
///
/// Every client holds one view for every floating pane of its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingPaneView {
    /// Where this client draws the pane.
    /// [`Client::set_floating_pane_position`] refuses a pinned pane.
    pub position: FloatingPanePosition,
    /// Whether the pane is minimized for this client. `false` exactly when
    /// the pane is in this client's
    /// [`list_shown_floating_pane_ids`](Client::list_shown_floating_pane_ids).
    pub is_minimized: bool,
}

/// What [`Client::focus_tiled_pane`] changed for one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TiledPaneFocusChange {
    /// The floating pane that held this client's input when the input left
    /// it, else the pane this client focused in the tab before.
    pub previous_pane_id: Option<PaneId>,
    /// The floating panes this client showed before the call, oldest first.
    /// After the call, each is minimized for this client.
    pub minimized_floating_pane_ids: Vec<PaneId>,
}

/// Where one client draws one floating pane: the default placement, or a
/// stored top-left cell counted from that client's pane-area origin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FloatingPanePosition {
    /// The client stored no cell: the pane draws at the default placement
    /// ([`place_floating_pane`]).
    #[default]
    Default,
    /// The pane's top-left cell, stored by a move, a resize, or the
    /// `top_left_cell` of the command that created the pane.
    Moved(Point),
    /// The pane's top-left cell, locked: a move and a resize of the pane's
    /// left or top edge are refused.
    Pinned(Point),
}

/// The rectangle one client draws a floating pane in, counted in that client's
/// pane-area cells.
///
/// The top-left cell is the cell `position` stores, or for
/// [`FloatingPanePosition::Default`] the default placement: `size` centered in
/// `client_viewport` ([`Size::compute_centered_origin`]), then moved `(2, 1)`
/// per cascade step.
/// The step is `cascade_index % (max_steps + 1)`, where `max_steps` is
/// `min(right_margin / 2, bottom_margin)` and the margins count the cells
/// right of and below the centered rectangle. The rectangle then moves left
/// and up until it lies inside `client_viewport`
/// ([`Rect::compute_moved_inside`]); on an axis where `size` is larger than
/// `client_viewport`, it starts at `0`. `position` itself is not changed.
///
/// A `48x13` pane on an `80x22` viewport centers at `(16, 4)`, and `max_steps`
/// is `min(16 / 2, 5) = 5`: `cascade_index` `1` → `(18, 5)`, `6` → `(16, 4)`.
/// `Moved((70, 2))` for a `20x10` pane on an `80x22` viewport → `(60, 2)`.
#[must_use]
pub fn place_floating_pane(
    position: FloatingPanePosition,
    size: Size,
    cascade_index: usize,
    client_viewport: Size,
) -> Rect {
    let free_column_count = client_viewport
        .column_count
        .saturating_sub(size.column_count);
    let free_row_count = client_viewport.row_count.saturating_sub(size.row_count);
    let unclamped_top_left_cell = match position {
        FloatingPanePosition::Moved(top_left_cell)
        | FloatingPanePosition::Pinned(top_left_cell) => top_left_cell,
        FloatingPanePosition::Default => {
            let centered_cell = client_viewport.compute_centered_origin(size);
            let right_margin = free_column_count - centered_cell.column;
            let bottom_margin = free_row_count - centered_cell.row;
            let max_cascade_steps = usize::from((right_margin / 2).min(bottom_margin));
            let cascade_step = (cascade_index % (max_cascade_steps + 1)) as u16;
            Point {
                column: centered_cell.column + 2 * cascade_step,
                row: centered_cell.row + cascade_step,
            }
        }
    };
    Rect::from_origin_and_size(unclamped_top_left_cell, size).compute_moved_inside(client_viewport)
}

/// One attached client: a single terminal connected to a session, holding the
/// identity the server gave it at attach and the view state that is the
/// client's alone. Two clients on the same session — and even viewing the same
/// tab — keep independent focus, lock mode, viewport and reported pane area.
#[derive(Debug, Serialize, Deserialize)]
pub struct Client {
    client_id: ClientId,
    session_id: SessionId,
    attached_at: SystemTime,
    viewport_size: Size,
    /// The client's measured terminal cell size in pixels.
    cell_size: Option<PixelCellSize>,
    /// The pane region this client reported for the tab it views. `None`
    /// when the client reported none.
    pane_area: Option<PaneArea>,
    active_tab_id: TabId,
    /// Where this client connected from, set by the server at attach.
    origin: ClientOrigin,
    /// This client's display name, `C-<adjective>-<noun>`, generated at
    /// attach and never changed.
    label: String,
    /// Which palette entry paints this client's identity in the UI, chosen by
    /// the caller at attach.
    color_index: u8,
    /// The panes this client focused in each tab, newest first, each pane
    /// once. The first pane is the one this client focuses in that tab, and
    /// the active tab's first pane takes this client's input while no
    /// floating pane is focused. A tab with no entry has no focused pane.
    ///
    /// A stack shows this client the member holding its newest pane of that
    /// stack ([`SplitNode::get_shown_child_index`](koshi_layout::tree::SplitNode::get_shown_child_index)).
    focus_history_by_tab_id: HashMap<TabId, Vec<PaneId>>,
    lock_mode: LockMode,
    /// The last lock command that this client's own connection sent and the
    /// session ran, also when the command left the lock mode as it was. `None`
    /// before the session runs one, and again after the session restarts: the
    /// resume file does not carry it.
    #[serde(skip)]
    last_applied_lock_command_id: Option<CommandId>,
    /// The last floating pane move that this client's mouse or key binding
    /// sent and the session ran, also when the move left the pane where it
    /// was. `None` before the session runs one, and again after the session
    /// restarts: the resume file does not carry it.
    #[serde(skip)]
    last_applied_floating_pane_move_command_id: Option<CommandId>,
    /// The last pane placement that this client's own connection confirmed
    /// and the session ran. `None` before the session runs one, and again
    /// after the session restarts: the resume file does not carry it.
    #[serde(skip)]
    last_applied_placement_command_id: Option<CommandId>,
    /// Whether this client grabs the mouse for text selection: while on, a drag
    /// highlights in koshi even over a program that asked for the mouse. Toggled
    /// by `core:mouse-select`; independent of [`lock_mode`](Self::lock_mode).
    is_mouse_selection_enabled: bool,
    /// This client's scrollback view position per pane: lines scrolled up from
    /// the live bottom. A pane absent from the map (the default) sits at the live
    /// bottom, offset `0`; only scrolled-up panes have an entry, always with a
    /// non-zero offset. Scrolling is per client: two clients scroll a shared pane
    /// independently.
    ///
    /// This is the position alone. Whether the view is *held* there — showing the
    /// same text as output arrives, rather than following the newest line — is
    /// derived by [`is_view_held`](Self::is_view_held).
    scroll_offset_by_pane_id: HashMap<PaneId, usize>,
    /// This client's highlighted text, keyed by the pane it is in. A highlight
    /// in a pane is visual mode for that pane, and clearing the highlight
    /// leaves visual mode. A pane absent from the map has no highlight.
    ///
    /// **A highlight belongs to one pane, and panes keep their own.** Highlighting
    /// in a second pane leaves the first pane's highlight where it is: several
    /// can be up at once. Only input that reaches a pane's own child clears that
    /// pane's highlight.
    ///
    /// Highlighting is per client: two clients viewing one pane select in it
    /// independently, and neither sees the other's highlight.
    selection_by_pane_id: HashMap<PaneId, Selection>,
    /// The pane this client has zoomed in each tab: the one pane filling the tab
    /// while the others are hidden. A tab absent from the map (the default) is
    /// tiled for this client.
    ///
    /// Zoom is per client: one client zooming a pane leaves another's tiled view
    /// as it is. A zoom changes how this client solves the tab's tree; the tree
    /// itself stays unchanged.
    zoomed_pane_id_by_tab_id: HashMap<TabId, PaneId>,
    /// The solve this client draws its active tab with, kept from the first
    /// read ([`Client::get_own_active_tab_solve`]): `Some` when that solve is
    /// this client's own, for a zoom or for a stack that shows this client
    /// another member than its active one, and `None` when this client draws
    /// the tab's own solve. Every change to this client's active tab, focus
    /// history or zoom empties it, and so does every solve of its active tab
    /// ([`ClientRegistry::clear_own_active_tab_solves`]). The resume file does
    /// not carry it.
    #[serde(skip)]
    own_active_tab_solve: OnceCell<Option<LayoutSolve>>,
    /// This client's view of each floating pane of its session, keyed by pane
    /// id: one entry per floating pane.
    floating_pane_view_by_pane_id: HashMap<PaneId, FloatingPaneView>,
    /// The floating panes this client shows, oldest first: the order this
    /// client draws them in, the newest on top. Focusing or restoring a pane
    /// moves it to the end, and minimizing it removes it. A pane is listed
    /// exactly when its view is not minimized.
    shown_floating_pane_ids: Vec<PaneId>,
    /// The floating pane that holds this client's focus, or `None` when no
    /// floating pane does. A set value is the last entry of
    /// `shown_floating_pane_ids`, and an empty list leaves it `None`. A set
    /// value takes this client's input instead of the pane this client
    /// focused in its active tab
    /// ([`get_active_focused_pane_id`](Self::get_active_focused_pane_id)).
    focused_floating_pane_id: Option<PaneId>,
    /// Whether state that this client's own frame shows changed since the
    /// last [`ClientRegistry::take_changed_client_ids`].
    /// [`ClientRegistry::attach_client`] sets it. The resume file does not
    /// carry it.
    #[serde(skip)]
    is_view_changed: bool,
}

impl Client {
    /// A newly attached client viewing `active_tab_id` at `viewport_size`, with
    /// no per-tab focus recorded yet, no floating pane view, and in
    /// [`LockMode::Normal`]. The caller supplies `attached_at`, `origin`,
    /// `label` and `color_index`; this never reads the clock itself.
    // Carries the whole of one attach: the client's identity (`client_id`,
    // `session_id`, `origin`, `label`, `color_index`) and its first view
    // (`attached_at`, `viewport_size`, `pane_area`, `active_tab_id`).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn from_attachment(
        client_id: ClientId,
        session_id: SessionId,
        attached_at: SystemTime,
        viewport_size: Size,
        pane_area: Option<PaneArea>,
        active_tab_id: TabId,
        origin: ClientOrigin,
        label: String,
        color_index: u8,
    ) -> Self {
        Client {
            client_id,
            session_id,
            attached_at,
            viewport_size,
            cell_size: None,
            pane_area,
            active_tab_id,
            origin,
            label,
            color_index,
            focus_history_by_tab_id: HashMap::new(),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled: false,
            scroll_offset_by_pane_id: HashMap::new(),
            selection_by_pane_id: HashMap::new(),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            own_active_tab_solve: OnceCell::new(),
            floating_pane_view_by_pane_id: HashMap::new(),
            shown_floating_pane_ids: Vec::new(),
            focused_floating_pane_id: None,
            is_view_changed: false,
        }
    }

    /// This client's stable identifier.
    #[must_use]
    pub fn get_client_id(&self) -> ClientId {
        self.client_id
    }

    /// The session this client is attached to.
    #[must_use]
    pub fn get_session_id(&self) -> SessionId {
        self.session_id
    }

    /// When this client attached.
    #[must_use]
    pub fn get_attached_at(&self) -> SystemTime {
        self.attached_at
    }

    /// Where this client connected from.
    #[must_use]
    pub fn get_origin(&self) -> ClientOrigin {
        self.origin
    }

    /// This client's generated display name.
    #[must_use]
    pub fn get_label(&self) -> &str {
        &self.label
    }

    /// Which palette entry paints this client's identity.
    #[must_use]
    pub fn get_color_index(&self) -> u8 {
        self.color_index
    }

    /// This client's current viewport size.
    #[must_use]
    pub fn get_viewport_size(&self) -> Size {
        self.viewport_size
    }

    /// Return this client's measured terminal cell size in pixels.
    #[must_use]
    pub fn get_cell_size(&self) -> Option<PixelCellSize> {
        self.cell_size
    }

    /// Replace this client's measured terminal cell size in pixels. `None`
    /// clears it.
    pub fn update_cell_size(&mut self, cell_size: Option<PixelCellSize>) {
        self.cell_size = cell_size;
    }

    /// The tab this client is currently viewing. Once the session's last tab
    /// closes (the session is quitting), this keeps naming the closed tab until
    /// the transport disconnects the client.
    #[must_use]
    pub fn get_active_tab_id(&self) -> TabId {
        self.active_tab_id
    }

    /// This client's lock mode.
    #[must_use]
    pub fn get_lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// The pane this client has focused in `tab_id`, or `None` if it has not
    /// focused one there.
    #[must_use]
    pub fn get_focused_pane_id(&self, tab_id: TabId) -> Option<PaneId> {
        self.list_focus_history(tab_id).first().copied()
    }

    /// The panes this client focused in `tab_id`, newest first, each pane
    /// once. The first is [`Client::get_focused_pane_id`]. Empty when it
    /// focuses no pane there.
    ///
    /// Focusing `a`, then `b`, then `a` again in one tab gives `[a, b]`.
    #[must_use]
    pub fn list_focus_history(&self, tab_id: TabId) -> &[PaneId] {
        self.focus_history_by_tab_id
            .get(&tab_id)
            .map_or(&[], Vec::as_slice)
    }

    /// The pane that takes this client's input: the focused floating pane,
    /// else the pane this client focused in its active tab. `None` when
    /// neither is set.
    #[must_use]
    pub fn get_active_focused_pane_id(&self) -> Option<PaneId> {
        self.focused_floating_pane_id
            .or_else(|| self.get_focused_pane_id(self.active_tab_id))
    }

    /// Every tab's focus history this client holds
    /// ([`Client::list_focus_history`]), keyed by tab id.
    #[must_use]
    pub fn list_focus_histories(&self) -> &HashMap<TabId, Vec<PaneId>> {
        &self.focus_history_by_tab_id
    }

    /// How `tab_id` is laid out **for this client**: zoomed on one pane, or
    /// tiled. The tab's tree is the same either way; this only says how this
    /// client solves it. Another client can be tiled on the same tab at the
    /// same moment.
    #[must_use]
    pub fn get_layout_mode(&self, tab_id: TabId) -> LayoutMode {
        self.zoomed_pane_id_by_tab_id
            .get(&tab_id)
            .map_or(LayoutMode::Tiled, |&focused_pane_id| {
                LayoutMode::Fullscreen { focused_pane_id }
            })
    }

    /// The pane this client has zoomed in `tab_id`, if any.
    #[must_use]
    pub fn get_zoomed_pane_id(&self, tab_id: TabId) -> Option<PaneId> {
        self.zoomed_pane_id_by_tab_id.get(&tab_id).copied()
    }

    /// Every pane this client has zoomed, keyed by tab id. A tab with no entry is
    /// tiled for this client.
    #[must_use]
    pub fn list_zoomed_pane_ids(&self) -> &HashMap<TabId, PaneId> {
        &self.zoomed_pane_id_by_tab_id
    }

    /// Zoom `pane_id` for this client in `tab_id`: it fills the tab and the
    /// tab's other panes are hidden, for this client's view alone.
    pub fn zoom_pane(&mut self, tab_id: TabId, pane_id: PaneId) {
        let previous_zoomed_pane_id = self.zoomed_pane_id_by_tab_id.insert(tab_id, pane_id);
        self.is_view_changed |= previous_zoomed_pane_id != Some(pane_id);
        self.clear_own_active_tab_solve();
    }

    /// Leave zoom in `tab_id`: this client sees the tab tiled again.
    pub fn clear_zoom(&mut self, tab_id: TabId) {
        self.is_view_changed |= self.zoomed_pane_id_by_tab_id.remove(&tab_id).is_some();
        self.clear_own_active_tab_solve();
    }

    /// Leave zoom in every tab where this client was zoomed on `pane_id`. The
    /// client sees those tabs tiled again.
    ///
    /// Called when a pane is removed.
    pub fn clear_zoom_of_pane(&mut self, pane_id: PaneId) {
        let zoomed_tab_count = self.zoomed_pane_id_by_tab_id.len();
        self.zoomed_pane_id_by_tab_id
            .retain(|_, zoomed_pane_id| *zoomed_pane_id != pane_id);
        self.is_view_changed |= self.zoomed_pane_id_by_tab_id.len() != zoomed_tab_count;
        self.clear_own_active_tab_solve();
    }

    /// Returns how many lines `pane_id` is scrolled above the live bottom.
    /// Returns `0` for an unscrolled pane or a view at the newest line; `3`
    /// means three lines above the live bottom.
    #[must_use]
    pub fn get_scroll_offset(&self, pane_id: PaneId) -> usize {
        self.scroll_offset_by_pane_id
            .get(&pane_id)
            .copied()
            .unwrap_or_default()
    }

    /// Set where this client's view of `pane_id` sits, `scroll_offset` lines
    /// above the live bottom. An offset of `0` removes the entry: the map holds
    /// only scrolled-up panes.
    pub fn set_scroll_offset(&mut self, pane_id: PaneId, scroll_offset: usize) {
        self.is_view_changed |= self.get_scroll_offset(pane_id) != scroll_offset;
        if scroll_offset == 0 {
            self.scroll_offset_by_pane_id.remove(&pane_id);
        } else {
            self.scroll_offset_by_pane_id.insert(pane_id, scroll_offset);
        }
    }

    /// Every scrolled-up pane this client remembers, keyed by pane id, each
    /// value the lines scrolled up from the live bottom. A pane with no entry
    /// sits at the live bottom.
    #[must_use]
    pub fn list_scroll_offsets(&self) -> &HashMap<PaneId, usize> {
        &self.scroll_offset_by_pane_id
    }

    /// This client's highlight in `pane_id`, or `None` if it has none there.
    #[must_use]
    pub fn get_selection(&self, pane_id: PaneId) -> Option<Selection> {
        self.selection_by_pane_id.get(&pane_id).copied()
    }

    /// Highlight `selection` in `pane_id` for this client, replacing any highlight
    /// it already had there. Other panes' highlights are untouched — each pane
    /// keeps its own.
    pub fn set_selection(&mut self, pane_id: PaneId, selection: Selection) {
        let previous_selection = self.selection_by_pane_id.insert(pane_id, selection);
        self.is_view_changed |= previous_selection != Some(selection);
    }

    /// Drop this client's highlight in `pane_id`, leaving visual mode for that
    /// pane. Clearing a pane with no highlight changes nothing. Other panes'
    /// highlights are untouched.
    pub fn clear_selection(&mut self, pane_id: PaneId) {
        self.is_view_changed |= self.selection_by_pane_id.remove(&pane_id).is_some();
    }

    /// Whether this client's view of `pane_id` is **held**: showing the same text
    /// as new output arrives, rather than following the newest line.
    ///
    /// Two independent things hold a view, and this is the only place they are
    /// combined:
    ///
    /// - **Scrolled up** (`scroll_offset > 0`) — the ordinary terminal rule: at
    ///   the bottom you are carried along, one line up you stay put. It ends when
    ///   the view is scrolled back to the bottom.
    /// - **Visual mode** (a highlight is up in this pane) — new output leaves the
    ///   text being selected where it is. It ends when the highlight clears.
    ///
    /// The answer is derived from those two facts on every call, never stored.
    ///
    /// Example: highlight up in this pane at offset `0` → held, so three lines of
    /// output move the offset to `3` and the same text stays on screen. Clicking
    /// into the pane clears the highlight; the view is now at offset `3`, so it is
    /// still held — by being scrolled up. Scrolling back to the bottom follows
    /// live again.
    #[must_use]
    pub fn is_view_held(&self, pane_id: PaneId) -> bool {
        self.get_scroll_offset(pane_id) > 0 || self.selection_by_pane_id.contains_key(&pane_id)
    }

    /// Update this client's lock mode.
    pub fn update_lock_mode(&mut self, lock_mode: LockMode) {
        self.is_view_changed |= self.lock_mode != lock_mode;
        self.lock_mode = lock_mode
    }

    /// The last lock command that this client's own connection sent and the
    /// session ran, or `None` before the session runs one.
    #[must_use]
    pub fn get_last_applied_lock_command_id(&self) -> Option<CommandId> {
        self.last_applied_lock_command_id
    }

    /// Record `lock_command_id` as the last lock command that this client's
    /// own connection sent and the session ran.
    pub fn set_last_applied_lock_command_id(&mut self, lock_command_id: CommandId) {
        self.is_view_changed |= self.last_applied_lock_command_id != Some(lock_command_id);
        self.last_applied_lock_command_id = Some(lock_command_id);
    }

    /// The last floating pane move that this client's mouse or key binding
    /// sent and the session ran, or `None` before the session runs one.
    #[must_use]
    pub fn get_last_applied_floating_pane_move_command_id(&self) -> Option<CommandId> {
        self.last_applied_floating_pane_move_command_id
    }

    /// Record `move_command_id` as the last floating pane move that this
    /// client's mouse or key binding sent and the session ran.
    pub fn set_last_applied_floating_pane_move_command_id(&mut self, move_command_id: CommandId) {
        self.is_view_changed |=
            self.last_applied_floating_pane_move_command_id != Some(move_command_id);
        self.last_applied_floating_pane_move_command_id = Some(move_command_id);
    }

    /// The last pane placement that this client's own connection confirmed
    /// and the session ran, or `None` before the session runs one.
    #[must_use]
    pub fn get_last_applied_placement_command_id(&self) -> Option<CommandId> {
        self.last_applied_placement_command_id
    }

    /// Record `placement_command_id` as the last pane placement that this
    /// client's own connection confirmed and the session ran.
    pub fn set_last_applied_placement_command_id(&mut self, placement_command_id: CommandId) {
        self.is_view_changed |=
            self.last_applied_placement_command_id != Some(placement_command_id);
        self.last_applied_placement_command_id = Some(placement_command_id);
    }

    /// Whether this client grabs the mouse for text selection.
    #[must_use]
    pub fn is_mouse_selection_enabled(&self) -> bool {
        self.is_mouse_selection_enabled
    }

    /// Flip [`is_mouse_selection_enabled`](Self::is_mouse_selection_enabled)
    /// and return the new value.
    pub fn toggle_mouse_selection(&mut self) -> bool {
        self.is_view_changed = true;
        self.is_mouse_selection_enabled = !self.is_mouse_selection_enabled;
        self.is_mouse_selection_enabled
    }

    /// Set the pane this client has focused in `tab_id`, returning the prior pane if one was set.
    /// The pane moves to the front of the tab's focus history
    /// ([`Client::list_focus_history`]).
    ///
    /// **Zoom follows focus.** When this client has `tab_id` zoomed, the zoom
    /// moves to the newly focused pane: the zoomed view swaps its content and
    /// stays on. Every path that moves focus — a keybinding, a `focus-pane`
    /// command, focus repair after a close — runs through here.
    ///
    /// A focused floating pane keeps this client's input, and every floating
    /// pane this client shows stays shown.
    /// [`focus_tiled_pane`](Self::focus_tiled_pane) also takes the input.
    pub fn update_focused_pane(&mut self, tab_id: TabId, pane_id: PaneId) -> Option<PaneId> {
        if let Some(zoomed_pane_id) = self.zoomed_pane_id_by_tab_id.get_mut(&tab_id) {
            self.is_view_changed |= *zoomed_pane_id != pane_id;
            *zoomed_pane_id = pane_id;
        }
        let focus_history = self.focus_history_by_tab_id.entry(tab_id).or_default();
        let previous_pane_id = focus_history.first().copied();
        focus_history.retain(|&history_pane_id| history_pane_id != pane_id);
        focus_history.insert(0, pane_id);
        self.is_view_changed |= previous_pane_id != Some(pane_id);
        self.clear_own_active_tab_solve();
        previous_pane_id
    }

    /// Take `pane_id` out of this client's focus history in `tab_id`. When
    /// `pane_id` is the focused pane, the next pane of the history becomes
    /// the focused one, and an empty history leaves no focused pane.
    ///
    /// The history `[a, b, c]` without `b` is `[a, c]`.
    pub fn remove_pane_from_focus_history(&mut self, tab_id: TabId, pane_id: PaneId) {
        let Some(focus_history) = self.focus_history_by_tab_id.get_mut(&tab_id) else {
            return;
        };
        let focus_history_length = focus_history.len();
        focus_history.retain(|&history_pane_id| history_pane_id != pane_id);
        self.is_view_changed |= focus_history.len() != focus_history_length;
        if focus_history.is_empty() {
            self.focus_history_by_tab_id.remove(&tab_id);
        }
        self.clear_own_active_tab_solve();
    }

    /// Focus `pane_id` in `tab_id` for this client, as
    /// [`update_focused_pane`](Self::update_focused_pane) does. When `tab_id`
    /// is this client's active tab, `pane_id` also takes this client's input:
    /// every floating pane this client shows is minimized for it
    /// ([`minimize_shown_floating_panes`](Self::minimize_shown_floating_panes)),
    /// and no floating pane is focused afterwards.
    ///
    /// With `[htop, logs]` shown and `logs` focused, focusing tiled pane `a` in
    /// the active tab gives `previous_pane_id: Some(logs)` and
    /// `minimized_floating_pane_ids: [htop, logs]`.
    pub fn focus_tiled_pane(&mut self, tab_id: TabId, pane_id: PaneId) -> TiledPaneFocusChange {
        let previous_floating_pane_id = self.focused_floating_pane_id;
        let minimized_floating_pane_ids = if tab_id == self.active_tab_id {
            self.minimize_shown_floating_panes()
        } else {
            Vec::new()
        };
        let previous_tab_pane_id = self.update_focused_pane(tab_id, pane_id);
        let previous_pane_id = if tab_id == self.active_tab_id {
            previous_floating_pane_id.or(previous_tab_pane_id)
        } else {
            previous_tab_pane_id
        };
        TiledPaneFocusChange {
            previous_pane_id,
            minimized_floating_pane_ids,
        }
    }

    /// Forget every pane this client focused in `tab_id`, and leave any zoom
    /// in that tab.
    pub fn clear_focus_history(&mut self, tab_id: TabId) {
        let has_removed_focus_history = self.focus_history_by_tab_id.remove(&tab_id).is_some();
        let has_removed_zoom = self.zoomed_pane_id_by_tab_id.remove(&tab_id).is_some();
        self.is_view_changed |= has_removed_focus_history || has_removed_zoom;
        self.clear_own_active_tab_solve();
    }

    /// Switch this client to viewing `tab_id`. The highlights it made in the
    /// tab it leaves stay where they are, and it finds them again on switching
    /// back.
    pub fn update_active_tab_id(&mut self, tab_id: TabId) {
        self.is_view_changed |= self.active_tab_id != tab_id;
        self.active_tab_id = tab_id;
        self.clear_own_active_tab_solve();
    }

    /// The solve this client draws its active tab with when that solve is its
    /// own, or `None` when it draws the tab's own solve. The first call after
    /// the kept solve was emptied runs `build_own_solve` and keeps its result;
    /// each call after it returns the kept result and runs nothing.
    pub(crate) fn get_own_active_tab_solve(
        &self,
        build_own_solve: impl FnOnce() -> Option<LayoutSolve>,
    ) -> Option<&LayoutSolve> {
        self.own_active_tab_solve
            .get_or_init(build_own_solve)
            .as_ref()
    }

    /// Empty the kept solve of this client's active tab
    /// ([`Client::get_own_active_tab_solve`]).
    fn clear_own_active_tab_solve(&mut self) {
        self.own_active_tab_solve = OnceCell::new();
    }

    /// Set this client's viewport size to `viewport_size`.
    pub fn update_viewport_size(&mut self, viewport_size: Size) {
        self.is_view_changed |= self.viewport_size != viewport_size;
        self.viewport_size = viewport_size
    }

    /// The pane region this client's tab is sized against, in cells. `None`
    /// when the client reported [`PaneArea::Starving`]; that client takes no
    /// part in any size minimum.
    ///
    /// No report → [`compute_default_pane_area_size`] of the viewport (`80x24` → `80x22`).
    /// [`PaneArea::Reported`] → that size clamped per axis to the viewport
    /// (`200x50` reported on an `80x24` viewport → `80x24`).
    #[must_use]
    pub fn get_pane_area(&self) -> Option<Size> {
        match self.pane_area {
            None => Some(compute_default_pane_area_size(self.viewport_size)),
            Some(PaneArea::Reported(size)) => Some(size.compute_minimum_axes(self.viewport_size)),
            Some(PaneArea::Starving) => None,
        }
    }

    /// The pane region exactly as this client reported it; `None` when it
    /// reported none.
    #[must_use]
    pub fn get_reported_pane_area(&self) -> Option<PaneArea> {
        self.pane_area
    }

    /// The pane region this client positions floating panes in: its
    /// [`get_pane_area`](Self::get_pane_area) when
    /// [`compute_floating_pane_size_cap`] of it holds
    /// [`MIN_FLOATING_PANE_SIZE`] on both axes, a pane area of at least
    /// `5x6`. `None` when this client reported [`PaneArea::Starving`], or a
    /// smaller pane area. `80x22` → `Some(80x22)`; `5x6` → `Some(5x6)`; `4x6`
    /// → `None`.
    #[must_use]
    pub fn get_floating_viewport(&self) -> Option<Size> {
        self.get_pane_area().filter(|pane_area| {
            MIN_FLOATING_PANE_SIZE.can_fit_inside(compute_floating_pane_size_cap(*pane_area))
        })
    }

    /// Replace this client's reported pane region with `pane_area`, `None`
    /// included.
    pub fn update_pane_area(&mut self, pane_area: Option<PaneArea>) {
        self.is_view_changed |= self.pane_area != pane_area;
        self.pane_area = pane_area
    }

    /// Set where this client's current connection came from.
    pub fn update_origin(&mut self, origin: ClientOrigin) {
        self.origin = origin;
    }

    /// This client's view of the floating pane `pane_id`, or `None` when this
    /// client holds no view of it: `pane_id` is no floating pane of its
    /// session.
    #[must_use]
    pub fn get_floating_pane_view(&self, pane_id: PaneId) -> Option<FloatingPaneView> {
        self.floating_pane_view_by_pane_id.get(&pane_id).copied()
    }

    /// Every floating pane view this client holds, keyed by pane id: one per
    /// floating pane of its session.
    #[must_use]
    pub fn list_floating_pane_views(&self) -> &HashMap<PaneId, FloatingPaneView> {
        &self.floating_pane_view_by_pane_id
    }

    /// The floating panes this client shows, oldest first: the order it draws
    /// them in, the newest on top.
    #[must_use]
    pub fn list_shown_floating_pane_ids(&self) -> &[PaneId] {
        &self.shown_floating_pane_ids
    }

    /// The floating pane that holds this client's focus, or `None` when no
    /// floating pane does.
    #[must_use]
    pub fn get_focused_floating_pane_id(&self) -> Option<PaneId> {
        self.focused_floating_pane_id
    }

    /// Whether this client shows the floating pane `pane_id`: this client
    /// holds a view of it that is not minimized.
    #[must_use]
    pub fn is_floating_pane_shown(&self, pane_id: PaneId) -> bool {
        self.get_floating_pane_view(pane_id)
            .is_some_and(|floating_pane_view| !floating_pane_view.is_minimized)
    }

    /// Add a view of the floating pane `pane_id` at `position`, minimized for
    /// this client. A view this client already holds of `pane_id` stays as it
    /// is.
    pub fn add_floating_pane_view(&mut self, pane_id: PaneId, position: FloatingPanePosition) {
        self.floating_pane_view_by_pane_id
            .entry(pane_id)
            .or_insert(FloatingPaneView {
                position,
                is_minimized: true,
            });
    }

    /// Show `pane_id` for this client, draw it above its other floating panes
    /// and focus it: the pane moves to the end of the shown floating panes, or
    /// is appended when it was minimized. `[a, b, c]` focusing `b` →
    /// `[a, c, b]`; `[a]` focusing minimized `c` → `[a, c]`.
    ///
    /// Returns whether the pane was minimized before the call. A pane this
    /// client holds no view of changes nothing and returns `false`.
    #[must_use]
    pub fn focus_floating_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(floating_pane_view) = self.floating_pane_view_by_pane_id.get_mut(&pane_id) else {
            return false;
        };
        let is_pane_restored = floating_pane_view.is_minimized;
        floating_pane_view.is_minimized = false;
        self.is_view_changed |= is_pane_restored
            || self.shown_floating_pane_ids.last() != Some(&pane_id)
            || self.focused_floating_pane_id != Some(pane_id);
        self.shown_floating_pane_ids
            .retain(|&shown_pane_id| shown_pane_id != pane_id);
        self.shown_floating_pane_ids.push(pane_id);
        self.focused_floating_pane_id = Some(pane_id);
        is_pane_restored
    }

    /// Minimize `pane_id` for this client when this client shows it: its view
    /// turns minimized and it leaves the shown floating panes. A floating
    /// focus on `pane_id` moves to the newest pane this client still shows;
    /// with none, no floating pane is focused. `[a, b, c]` with `c` focused
    /// gives `[a, b]` with `b` focused.
    ///
    /// Returns `false`, changing nothing, when this client does not show
    /// `pane_id`.
    #[must_use]
    pub fn minimize_floating_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(floating_pane_view) = self.floating_pane_view_by_pane_id.get_mut(&pane_id) else {
            return false;
        };
        if floating_pane_view.is_minimized {
            return false;
        }
        floating_pane_view.is_minimized = true;
        self.remove_shown_floating_pane(pane_id);
        true
    }

    /// Minimize every floating pane this client shows: each view turns
    /// minimized, the shown floating panes empty, and no floating pane is
    /// focused.
    ///
    /// Returns the panes minimized, oldest first: `[htop, logs]` shown →
    /// `[htop, logs]`, and `[]` when this client shows none.
    pub fn minimize_shown_floating_panes(&mut self) -> Vec<PaneId> {
        let minimized_pane_ids = std::mem::take(&mut self.shown_floating_pane_ids);
        for pane_id in &minimized_pane_ids {
            if let Some(floating_pane_view) = self.floating_pane_view_by_pane_id.get_mut(pane_id) {
                floating_pane_view.is_minimized = true;
            }
        }
        self.is_view_changed |=
            !minimized_pane_ids.is_empty() || self.focused_floating_pane_id.is_some();
        self.focused_floating_pane_id = None;
        minimized_pane_ids
    }

    /// Pin `pane_id` for this client at `top_left_cell`, counted from this
    /// client's pane-area origin. A pinned pane refuses
    /// [`set_floating_pane_position`](Self::set_floating_pane_position). The
    /// focus and the shown floating panes stay as they are. A pane this
    /// client holds no view of changes nothing.
    pub fn pin_floating_pane(&mut self, pane_id: PaneId, top_left_cell: Point) {
        self.update_floating_pane_position(pane_id, FloatingPanePosition::Pinned(top_left_cell));
    }

    /// Unpin `pane_id` for this client: a pane pinned at a cell stays at that
    /// cell, unpinned. A pane this client did not pin, and a pane it holds no
    /// view of, is left as it is.
    pub fn unpin_floating_pane(&mut self, pane_id: PaneId) {
        if let Some(FloatingPaneView {
            position: FloatingPanePosition::Pinned(top_left_cell),
            ..
        }) = self.get_floating_pane_view(pane_id)
        {
            self.update_floating_pane_position(pane_id, FloatingPanePosition::Moved(top_left_cell));
        }
    }

    /// Store `top_left_cell` for `pane_id` in this client's view, counted from
    /// this client's pane-area origin.
    ///
    /// Returns `false`, changing nothing, when this client pinned `pane_id` or
    /// holds no view of it.
    #[must_use]
    pub fn set_floating_pane_position(&mut self, pane_id: PaneId, top_left_cell: Point) -> bool {
        let Some(floating_pane_view) = self.get_floating_pane_view(pane_id) else {
            return false;
        };
        if let FloatingPanePosition::Pinned(_) = floating_pane_view.position {
            return false;
        }
        self.update_floating_pane_position(pane_id, FloatingPanePosition::Moved(top_left_cell));
        true
    }

    /// Set the position of this client's view of `pane_id` to `position`. The
    /// view changes for this client's frame when the position differs and the
    /// pane is shown. A pane this client holds no view of changes nothing.
    fn update_floating_pane_position(&mut self, pane_id: PaneId, position: FloatingPanePosition) {
        let Some(floating_pane_view) = self.floating_pane_view_by_pane_id.get_mut(&pane_id) else {
            return;
        };
        self.is_view_changed |=
            floating_pane_view.position != position && !floating_pane_view.is_minimized;
        floating_pane_view.position = position;
    }

    /// Drop `pane_id` from this client's floating view: its view and its
    /// shown floating pane entry. A floating focus on `pane_id` moves as
    /// [`minimize_floating_pane`](Self::minimize_floating_pane) moves it.
    pub(crate) fn remove_floating_pane_view(&mut self, pane_id: PaneId) {
        self.floating_pane_view_by_pane_id.remove(&pane_id);
        self.remove_shown_floating_pane(pane_id);
    }

    /// Take `pane_id` out of the shown floating panes. A floating focus on
    /// `pane_id` moves to the newest pane this client still shows; with none,
    /// no floating pane is focused.
    fn remove_shown_floating_pane(&mut self, pane_id: PaneId) {
        let shown_floating_pane_count = self.shown_floating_pane_ids.len();
        self.shown_floating_pane_ids
            .retain(|&shown_pane_id| shown_pane_id != pane_id);
        self.is_view_changed |= self.shown_floating_pane_ids.len() != shown_floating_pane_count;
        if self.focused_floating_pane_id == Some(pane_id) {
            self.focused_floating_pane_id = self.shown_floating_pane_ids.last().copied();
        }
    }

    /// Replace this client's floating view with a saved one, for the floating
    /// panes `floating_pane_ids` lists: the session's floating panes.
    ///
    /// The shown floating panes are the panes of `saved_shown_floating_pane_ids`
    /// that `floating_pane_ids` lists and whose saved view is not minimized, in
    /// that order, and the newest of them is focused; with none, no floating
    /// pane is focused. Each pane of `floating_pane_ids` takes the position of
    /// its saved view, or the default placement when
    /// `saved_floating_pane_views` holds none, and is minimized unless it is
    /// shown. A saved view of a pane that `floating_pane_ids` does not list is
    /// dropped.
    ///
    /// Saved `[a, b, c]` shown with `c` not in `floating_pane_ids` gives
    /// `[a, b]` with `b` focused.
    pub fn replace_floating_view(
        &mut self,
        saved_floating_pane_views: &HashMap<PaneId, FloatingPaneView>,
        saved_shown_floating_pane_ids: &[PaneId],
        floating_pane_ids: &[PaneId],
    ) {
        let shown_floating_pane_ids: Vec<PaneId> = saved_shown_floating_pane_ids
            .iter()
            .copied()
            .filter(|pane_id| {
                floating_pane_ids.contains(pane_id)
                    && saved_floating_pane_views
                        .get(pane_id)
                        .is_some_and(|floating_pane_view| !floating_pane_view.is_minimized)
            })
            .collect();
        let floating_pane_view_by_pane_id: HashMap<PaneId, FloatingPaneView> = floating_pane_ids
            .iter()
            .map(|&pane_id| {
                let position = saved_floating_pane_views
                    .get(&pane_id)
                    .map_or(FloatingPanePosition::Default, |floating_pane_view| {
                        floating_pane_view.position
                    });
                let floating_pane_view = FloatingPaneView {
                    position,
                    is_minimized: !shown_floating_pane_ids.contains(&pane_id),
                };
                (pane_id, floating_pane_view)
            })
            .collect();
        let focused_floating_pane_id = shown_floating_pane_ids.last().copied();
        let has_shown_floating_view_changed = shown_floating_pane_ids
            != self.shown_floating_pane_ids
            || focused_floating_pane_id != self.focused_floating_pane_id
            || shown_floating_pane_ids.iter().any(|pane_id| {
                floating_pane_view_by_pane_id.get(pane_id)
                    != self.floating_pane_view_by_pane_id.get(pane_id)
            });
        self.is_view_changed |= has_shown_floating_view_changed;
        self.floating_pane_view_by_pane_id = floating_pane_view_by_pane_id;
        self.shown_floating_pane_ids = shown_floating_pane_ids;
        self.focused_floating_pane_id = focused_floating_pane_id;
    }

    /// Whether state that this client's own frame shows changed since the
    /// last call. Clears the change.
    fn take_view_change(&mut self) -> bool {
        std::mem::take(&mut self.is_view_changed)
    }
}

/// The clients currently attached to one session, keyed by [`ClientId`]. The
/// session owns exactly one registry and holds no per-client state itself:
/// focus, lock mode and viewport live on each [`Client`]. The map is ordered,
/// so iteration walks clients in id order.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ClientRegistry {
    client_by_id: BTreeMap<ClientId, Client>,
    /// Whether a client was attached or handed out for change since the last
    /// [`ClientRegistry::take_changed_client_ids`].
    #[serde(skip)]
    has_client_write: bool,
    /// The clients detached since the last
    /// [`ClientRegistry::take_changed_client_ids`], in detach order.
    #[serde(skip)]
    detached_client_ids: Vec<ClientId>,
}

impl ClientRegistry {
    /// An empty registry with no clients attached.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The client attached under `client_id`, or `None` if none is.
    #[must_use]
    pub fn get_client_by_id(&self, client_id: ClientId) -> Option<&Client> {
        self.client_by_id.get(&client_id)
    }

    /// Mutable access to one client for in-place edits to its view state —
    /// active tab, per-tab focus, lock mode, viewport. `None` if no client is
    /// attached under `client_id`.
    ///
    /// A client's id is read-only: the entry stays keyed under `client_id` for
    /// as long as it is attached. Changing a client's id means
    /// [`detach_client`](Self::detach_client) then [`attach_client`](Self::attach_client).
    pub fn get_client_mut_by_id(&mut self, client_id: ClientId) -> Option<&mut Client> {
        self.has_client_write = true;
        self.client_by_id.get_mut(&client_id)
    }

    /// Detach the client under `client_id` on disconnect, returning the removed
    /// [`Client`]. `None` if it was not attached.
    pub fn detach_client(&mut self, client_id: ClientId) -> Option<Client> {
        let detached_client = self.client_by_id.remove(&client_id)?;
        self.detached_client_ids.push(client_id);
        Some(detached_client)
    }

    /// Register `client` on attach, keyed by its own id. Returns the previous
    /// record if that id was already attached — a re-attach replaces in place.
    /// The attached client counts as changed at the next
    /// `ClientRegistry::take_changed_client_ids`.
    pub fn attach_client(&mut self, mut client: Client) -> Option<Client> {
        self.has_client_write = true;
        client.is_view_changed = true;
        self.client_by_id.insert(client.client_id, client)
    }

    /// The clients whose own view changed ([`Client`] marks each change in
    /// the function that makes it), that attached, or that detached since the
    /// last call, in id order. Clears every change. Returns an empty set
    /// without reading a client when no client was attached, detached or
    /// handed out for change.
    pub(crate) fn take_changed_client_ids(&mut self) -> BTreeSet<ClientId> {
        let mut changed_client_ids: BTreeSet<ClientId> =
            self.detached_client_ids.drain(..).collect();
        if std::mem::take(&mut self.has_client_write) {
            for client in self.client_by_id.values_mut() {
                if client.take_view_change() {
                    changed_client_ids.insert(client.client_id);
                }
            }
        }
        changed_client_ids
    }

    /// Empty the kept active tab solve ([`Client::get_own_active_tab_solve`])
    /// of every attached client whose active tab is `tab_id`.
    pub(crate) fn clear_own_active_tab_solves(&mut self, tab_id: TabId) {
        for client in self.client_by_id.values_mut() {
            if client.active_tab_id == tab_id {
                client.clear_own_active_tab_solve();
            }
        }
    }

    /// Every attached client, in id order.
    pub fn list_attached_clients(&self) -> impl Iterator<Item = &Client> {
        self.client_by_id.values()
    }

    /// Mutable access to every attached client, in id order.
    pub fn list_attached_clients_mut(&mut self) -> impl Iterator<Item = &mut Client> {
        self.has_client_write = true;
        self.client_by_id.values_mut()
    }

    /// How many clients are attached.
    #[must_use]
    pub fn count_clients(&self) -> usize {
        self.client_by_id.len()
    }

    /// Whether one or more clients are attached.
    #[must_use]
    pub fn has_clients(&self) -> bool {
        !self.client_by_id.is_empty()
    }
}

#[cfg(test)]
mod tests;
