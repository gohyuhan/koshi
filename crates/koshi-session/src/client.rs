//! Attached clients: the identity and per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport, input modes and
//! the view of each floating pane live on each client; the session holds only
//! this registry. Each client also carries what the server set at attach: its
//! origin, its generated label and its color.

use std::{
    collections::{BTreeMap, HashMap},
    time::SystemTime,
};

pub use koshi_core::client::ClientOrigin;
use koshi_core::{
    command::Selection,
    geometry::{PaneArea, PixelCellSize, Point, Rect, Size},
    ids::{ClientId, PaneId, SessionId, TabId},
    lock::LockMode,
};
use koshi_layout::mode::LayoutMode;
use serde::{Deserialize, Serialize};

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
/// whether this client minimized it.
///
/// A client that stores no view of a floating pane reads this type's
/// `Default`: the default placement, not minimized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingPaneView {
    /// Where this client draws the pane.
    /// [`Client::set_floating_pane_position`] refuses a pinned pane.
    pub position: FloatingPanePosition,
    /// Whether this client minimized the pane.
    /// [`Client::focus_floating_pane`] refuses a minimized pane.
    pub is_minimized: bool,
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
/// `client_viewport` (rounding down), then moved `(2, 1)` per cascade step.
/// The step is `cascade_index % (max_steps + 1)`, where `max_steps` is
/// `min(right_margin / 2, bottom_margin)` and the margins count the cells
/// right of and below the centered rectangle. The rectangle then moves left
/// and up until it lies inside `client_viewport`; on an axis where `size` is
/// larger than `client_viewport`, it starts at `0`. `position` itself is not
/// changed.
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
            let centered_column = free_column_count / 2;
            let centered_row = free_row_count / 2;
            let right_margin = free_column_count - centered_column;
            let bottom_margin = free_row_count - centered_row;
            let max_cascade_steps = usize::from((right_margin / 2).min(bottom_margin));
            let cascade_step = (cascade_index % (max_cascade_steps + 1)) as u16;
            Point {
                column: centered_column + 2 * cascade_step,
                row: centered_row + cascade_step,
            }
        }
    };
    Rect {
        origin: Point {
            column: unclamped_top_left_cell.column.min(free_column_count),
            row: unclamped_top_left_cell.row.min(free_row_count),
        },
        size,
    }
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
    focused_pane_id_by_tab_id: HashMap<TabId, PaneId>,
    lock_mode: LockMode,
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
    /// Generation of this client's committed geometry and view.
    placement_revision: u64,
    /// This client's view of each floating pane, keyed by pane id. The map
    /// holds only views that differ from [`FloatingPaneView::default`]: a pane
    /// with no entry reads as the default placement, not minimized.
    floating_pane_view_by_pane_id: HashMap<PaneId, FloatingPaneView>,
    /// The floating panes this client focused or restored, least recently
    /// first. Focusing or restoring a pane moves it to the end.
    floating_pane_focus_order: Vec<PaneId>,
    /// The floating pane that holds this client's focus, or `None` when no
    /// floating pane does. A set value is the last entry of
    /// `floating_pane_focus_order` and names a pane this client has not
    /// minimized.
    focused_floating_pane_id: Option<PaneId>,
}

impl Client {
    /// A newly attached client viewing `active_tab_id` at `viewport_size`, with no
    /// per-tab focus recorded yet and in [`LockMode::Normal`]. The caller
    /// supplies `attached_at`, `origin`, `label` and `color_index`; this never reads
    /// the clock itself.
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
            focused_pane_id_by_tab_id: HashMap::new(),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
            scroll_offset_by_pane_id: HashMap::new(),
            selection_by_pane_id: HashMap::new(),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            placement_revision: 0,
            floating_pane_view_by_pane_id: HashMap::new(),
            floating_pane_focus_order: Vec::new(),
            focused_floating_pane_id: None,
        }
    }

    /// This client's stable identifier.
    #[must_use]
    pub fn get_client_id(&self) -> ClientId {
        self.client_id
    }

    /// The generation of this client's committed placement view.
    #[must_use]
    pub fn get_placement_revision(&self) -> u64 {
        self.placement_revision
    }

    /// Whether this client revision can advance without wrapping.
    #[must_use]
    pub fn can_advance_placement_revision(&self) -> bool {
        self.placement_revision < u64::MAX
    }

    /// Advance this client's revision once. Returns `false` at the maximum value.
    pub fn advance_placement_revision(&mut self) -> bool {
        let Some(next_revision) = self.placement_revision.checked_add(1) else {
            return false;
        };
        self.placement_revision = next_revision;
        true
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
        self.focused_pane_id_by_tab_id.get(&tab_id).copied()
    }

    /// Every focused pane this client remembers, keyed by tab id.
    #[must_use]
    pub fn list_focused_pane_ids(&self) -> &HashMap<TabId, PaneId> {
        &self.focused_pane_id_by_tab_id
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
        self.zoomed_pane_id_by_tab_id.insert(tab_id, pane_id);
    }

    /// Leave zoom in `tab_id`: this client sees the tab tiled again.
    pub fn clear_zoom(&mut self, tab_id: TabId) {
        self.zoomed_pane_id_by_tab_id.remove(&tab_id);
    }

    /// Leave zoom in every tab where this client was zoomed on `pane_id`. The
    /// client sees those tabs tiled again.
    ///
    /// Called when a pane is removed.
    pub fn clear_zoom_of_pane(&mut self, pane_id: PaneId) {
        self.zoomed_pane_id_by_tab_id
            .retain(|_, zoomed_pane_id| *zoomed_pane_id != pane_id);
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
        self.selection_by_pane_id.insert(pane_id, selection);
    }

    /// Drop this client's highlight in `pane_id`, leaving visual mode for that
    /// pane. Clearing a pane with no highlight changes nothing. Other panes'
    /// highlights are untouched.
    pub fn clear_selection(&mut self, pane_id: PaneId) {
        self.selection_by_pane_id.remove(&pane_id);
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
        self.lock_mode = lock_mode
    }

    /// Whether this client grabs the mouse for text selection.
    #[must_use]
    pub fn is_mouse_selection_enabled(&self) -> bool {
        self.is_mouse_selection_enabled
    }

    /// Flip [`is_mouse_selection_enabled`](Self::is_mouse_selection_enabled)
    /// and return the new value.
    pub fn toggle_mouse_selection(&mut self) -> bool {
        self.is_mouse_selection_enabled = !self.is_mouse_selection_enabled;
        self.is_mouse_selection_enabled
    }

    /// Set the pane this client has focused in `tab_id`, returning the prior pane if one was set.
    ///
    /// **Zoom follows focus.** When this client has `tab_id` zoomed, the zoom
    /// moves to the newly focused pane: the zoomed view swaps its content and
    /// stays on. Every path that moves focus — a keybinding, a `focus-pane`
    /// command, focus repair after a close — runs through here.
    pub fn update_focused_pane(&mut self, tab_id: TabId, pane_id: PaneId) -> Option<PaneId> {
        if let Some(zoomed_pane_id) = self.zoomed_pane_id_by_tab_id.get_mut(&tab_id) {
            *zoomed_pane_id = pane_id;
        }
        self.focused_pane_id_by_tab_id.insert(tab_id, pane_id)
    }

    /// Forget the pane this client focused in `tab_id`, and leave any zoom in
    /// that tab.
    pub fn remove_focused_pane(&mut self, tab_id: TabId) {
        self.focused_pane_id_by_tab_id.remove(&tab_id);
        self.zoomed_pane_id_by_tab_id.remove(&tab_id);
    }

    /// Switch this client to viewing `tab_id`. The highlights it made in the
    /// tab it leaves stay where they are, and it finds them again on switching
    /// back.
    pub fn update_active_tab_id(&mut self, tab_id: TabId) {
        self.active_tab_id = tab_id;
    }

    /// Set this client's viewport size to `viewport_size`.
    pub fn update_viewport_size(&mut self, viewport_size: Size) {
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

    /// Replace this client's reported pane region with `pane_area`, `None`
    /// included.
    pub fn update_pane_area(&mut self, pane_area: Option<PaneArea>) {
        self.pane_area = pane_area
    }

    /// Set where this client's current connection came from.
    pub fn update_origin(&mut self, origin: ClientOrigin) {
        self.origin = origin;
    }

    /// This client's view of `pane_id`: the stored view, or
    /// [`FloatingPaneView::default`] when this client stores none.
    #[must_use]
    pub fn get_floating_pane_view(&self, pane_id: PaneId) -> FloatingPaneView {
        self.floating_pane_view_by_pane_id
            .get(&pane_id)
            .copied()
            .unwrap_or_default()
    }

    /// Every floating pane view this client stores, keyed by pane id. Each one
    /// differs from [`FloatingPaneView::default`].
    pub(crate) fn list_floating_pane_views(&self) -> &HashMap<PaneId, FloatingPaneView> {
        &self.floating_pane_view_by_pane_id
    }

    /// The floating panes this client focused or restored, least recently
    /// first.
    #[must_use]
    pub fn list_floating_pane_focus_order(&self) -> &[PaneId] {
        &self.floating_pane_focus_order
    }

    /// The floating pane that holds this client's focus, or `None` when no
    /// floating pane does.
    #[must_use]
    pub fn get_focused_floating_pane_id(&self) -> Option<PaneId> {
        self.focused_floating_pane_id
    }

    /// Focus `pane_id` for this client and move it to the end of the floating
    /// focus order, appending it when absent. `[a, b, c]` focusing `b` →
    /// `[a, c, b]`. Does not check that `pane_id` is a floating pane.
    ///
    /// Returns `false`, changing nothing, when this client minimized `pane_id`.
    #[must_use]
    pub fn focus_floating_pane(&mut self, pane_id: PaneId) -> bool {
        if self.get_floating_pane_view(pane_id).is_minimized {
            return false;
        }
        self.raise_and_focus_floating_pane(pane_id);
        true
    }

    /// Minimize `pane_id` for this client. The pane keeps its place in the
    /// floating focus order. A floating focus on `pane_id` clears. Does not
    /// check that `pane_id` is a floating pane.
    pub fn minimize_floating_pane(&mut self, pane_id: PaneId) {
        let mut floating_pane_view = self.get_floating_pane_view(pane_id);
        floating_pane_view.is_minimized = true;
        self.set_floating_pane_view(pane_id, floating_pane_view);
        if self.focused_floating_pane_id == Some(pane_id) {
            self.focused_floating_pane_id = None;
        }
    }

    /// Restore `pane_id` for this client: clear its minimized state, focus it
    /// and move it to the end of the floating focus order. A pane this client
    /// never focused is appended. Does not check that `pane_id` is a floating
    /// pane.
    pub fn restore_floating_pane(&mut self, pane_id: PaneId) {
        let mut floating_pane_view = self.get_floating_pane_view(pane_id);
        floating_pane_view.is_minimized = false;
        self.set_floating_pane_view(pane_id, floating_pane_view);
        self.raise_and_focus_floating_pane(pane_id);
    }

    /// Pin `pane_id` for this client at `top_left_cell`, counted from this
    /// client's pane-area origin. A pinned pane refuses
    /// [`set_floating_pane_position`](Self::set_floating_pane_position). The
    /// focus and the floating focus order stay as they are. Does not check that
    /// `pane_id` is a floating pane.
    pub fn pin_floating_pane(&mut self, pane_id: PaneId, top_left_cell: Point) {
        let mut floating_pane_view = self.get_floating_pane_view(pane_id);
        floating_pane_view.position = FloatingPanePosition::Pinned(top_left_cell);
        self.set_floating_pane_view(pane_id, floating_pane_view);
    }

    /// Unpin `pane_id` for this client: a pane pinned at a cell stays at that
    /// cell, unpinned. A pane this client did not pin is left as it is. Does
    /// not check that `pane_id` is a floating pane.
    pub fn unpin_floating_pane(&mut self, pane_id: PaneId) {
        let mut floating_pane_view = self.get_floating_pane_view(pane_id);
        if let FloatingPanePosition::Pinned(top_left_cell) = floating_pane_view.position {
            floating_pane_view.position = FloatingPanePosition::Moved(top_left_cell);
            self.set_floating_pane_view(pane_id, floating_pane_view);
        }
    }

    /// Store `top_left_cell` for `pane_id` in this client's view, counted from
    /// this client's pane-area origin. Does not check that `pane_id` is a
    /// floating pane.
    ///
    /// Returns `false`, changing nothing, when this client pinned `pane_id`.
    #[must_use]
    pub fn set_floating_pane_position(&mut self, pane_id: PaneId, top_left_cell: Point) -> bool {
        let mut floating_pane_view = self.get_floating_pane_view(pane_id);
        if let FloatingPanePosition::Pinned(_) = floating_pane_view.position {
            return false;
        }
        floating_pane_view.position = FloatingPanePosition::Moved(top_left_cell);
        self.set_floating_pane_view(pane_id, floating_pane_view);
        true
    }

    /// Drop `pane_id` from this client's floating view: its stored view, its
    /// floating focus order entry, and the floating focus when it names
    /// `pane_id`.
    pub(crate) fn remove_floating_pane_view(&mut self, pane_id: PaneId) {
        self.floating_pane_view_by_pane_id.remove(&pane_id);
        self.floating_pane_focus_order
            .retain(|&ordered_pane_id| ordered_pane_id != pane_id);
        if self.focused_floating_pane_id == Some(pane_id) {
            self.focused_floating_pane_id = None;
        }
    }

    /// Move `pane_id` to the end of the floating focus order, appending it when
    /// absent, and focus it.
    fn raise_and_focus_floating_pane(&mut self, pane_id: PaneId) {
        self.floating_pane_focus_order
            .retain(|&ordered_pane_id| ordered_pane_id != pane_id);
        self.floating_pane_focus_order.push(pane_id);
        self.focused_floating_pane_id = Some(pane_id);
    }

    /// Store `floating_pane_view` as this client's view of `pane_id`, or drop
    /// the stored view when `floating_pane_view` is the default.
    pub(crate) fn set_floating_pane_view(
        &mut self,
        pane_id: PaneId,
        floating_pane_view: FloatingPaneView,
    ) {
        if floating_pane_view == FloatingPaneView::default() {
            self.floating_pane_view_by_pane_id.remove(&pane_id);
        } else {
            self.floating_pane_view_by_pane_id
                .insert(pane_id, floating_pane_view);
        }
    }
}

/// The clients currently attached to one session, keyed by [`ClientId`]. The
/// session owns exactly one registry and holds no per-client state itself:
/// focus, lock mode and viewport live on each [`Client`]. The map is ordered,
/// so iteration walks clients in id order.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ClientRegistry {
    client_by_id: BTreeMap<ClientId, Client>,
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
        self.client_by_id.get_mut(&client_id)
    }

    /// Detach the client under `client_id` on disconnect, returning the removed
    /// [`Client`]. `None` if it was not attached.
    pub fn detach_client(&mut self, client_id: ClientId) -> Option<Client> {
        self.client_by_id.remove(&client_id)
    }

    /// Register `client` on attach, keyed by its own id. Returns the previous
    /// record if that id was already attached — a re-attach replaces in place.
    pub fn attach_client(&mut self, client: Client) -> Option<Client> {
        self.client_by_id.insert(client.client_id, client)
    }

    /// Every attached client, in id order.
    pub fn list_attached_clients(&self) -> impl Iterator<Item = &Client> {
        self.client_by_id.values()
    }

    /// Mutable access to every attached client, in id order.
    pub fn list_attached_clients_mut(&mut self) -> impl Iterator<Item = &mut Client> {
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
