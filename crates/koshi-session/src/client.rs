//! Attached clients: the per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport, and input
//! modes are per-client so two attached terminals never fight over one
//! global cursor; the session itself holds only this registry.

use std::{
    collections::{BTreeMap, HashMap},
    time::{Instant, SystemTime},
};

use koshi_core::{
    geometry::{Direction, Point, Size},
    ids::{ClientId, PaneId, SessionId, TabId},
    key::KeySequence,
    lock::LockMode,
};
use koshi_layout::mode::LayoutMode;

/// Convert a full client terminal viewport into the middle pane region by
/// reserving one top tabline row and one bottom key-hint row.
#[must_use]
pub const fn pane_viewport(viewport: Size) -> Size {
    Size {
        cols: viewport.cols,
        rows: viewport.rows.saturating_sub(2),
    }
}

/// One attached client: a single terminal connected to a session, holding the
/// view state that is the client's alone. Two clients on the same session — and
/// even viewing the same tab — keep independent focus, lock mode, and viewport,
/// so they never fight over one cursor or mode.
#[derive(Debug)]
pub struct Client {
    id: ClientId,
    session_id: SessionId,
    attached_at: SystemTime,
    viewport: Size,
    active_tab: TabId,
    focus_by_tab: HashMap<TabId, PaneId>,
    lock_mode: LockMode,
    mouse_state: MouseState,
    /// This client's in-flight pane-border resize drag, held only between the
    /// mouse press on a border that begins it and the release that ends it.
    pending_resize_drag: Option<ResizeDragState>,
    /// The pane a forwarded mouse press captured. While a button is held, its
    /// drags and its release go to this pane even as the pointer leaves it, and a
    /// release with no capture is not forwarded. Set on a forwarded press,
    /// cleared on the release.
    mouse_capture: Option<PaneId>,
    /// This client's tabline scroll position: `None` follows the active tab —
    /// the window always reveals it — while `Some(i)` peeks from tab index `i`
    /// without changing focus. Mouse scroll, arrow clicks, and drag set it;
    /// [`update_active_tab`](Self::update_active_tab) resets it to `None`, so a
    /// tab switch always cancels a peek and reveals the new tab.
    tabline_offset: Option<usize>,
    /// This client's in-flight tabline peek-drag, held only between the mouse
    /// press that begins it and the release that ends it.
    tabline_drag: Option<TablineDragState>,
    pending_key_sequence: Option<PendingKeySequence>,
    /// This client's scrollback view offset per pane: lines scrolled up from the
    /// live bottom. A pane absent from the map (the default) follows live output;
    /// only scrolled-back panes have an entry, always with a non-zero offset. It
    /// lives on the client because scrolling is per-view — two clients scroll a
    /// shared pane independently.
    scroll_by_pane: HashMap<PaneId, usize>,
    /// The pane this client has zoomed in each tab: the one pane filling the tab
    /// while the others are hidden. A tab absent from the map (the default) is
    /// tiled for this client.
    ///
    /// Zoom lives on the client, beside focus, because it is a property of one
    /// view rather than of the tab: two clients on the same tab zoom
    /// independently, and one zooming a pane leaves the other's tiled view
    /// untouched. The tab's layout tree is never rewritten either way — a zoom
    /// only changes how that tree is solved for this client.
    zoom_by_tab: HashMap<TabId, PaneId>,
}

impl Client {
    /// A newly attached client viewing `active_tab` at `viewport`, with no
    /// per-tab focus recorded yet, [`LockMode::Normal`], and no resize drag in
    /// progress. `attached_at` is supplied by the caller at the attach
    /// boundary, not read from the clock here, so it stays controllable.
    #[must_use]
    pub fn new(
        id: ClientId,
        session_id: SessionId,
        attached_at: SystemTime,
        viewport: Size,
        active_tab: TabId,
    ) -> Self {
        Client {
            id,
            session_id,
            attached_at,
            viewport,
            active_tab,
            focus_by_tab: HashMap::new(),
            lock_mode: LockMode::Normal,
            mouse_state: MouseState,
            pending_resize_drag: None,
            mouse_capture: None,
            tabline_offset: None,
            tabline_drag: None,
            pending_key_sequence: None,
            scroll_by_pane: HashMap::new(),
            zoom_by_tab: HashMap::new(),
        }
    }

    /// This client's id.
    #[must_use]
    pub fn id(&self) -> ClientId {
        self.id
    }

    /// The session this client is attached to.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// When this client attached.
    #[must_use]
    pub fn attached_at(&self) -> SystemTime {
        self.attached_at
    }

    /// This client's current viewport size.
    #[must_use]
    pub fn viewport(&self) -> Size {
        self.viewport
    }

    /// The tab this client is currently viewing. Once the session's last tab
    /// closes (the session is quitting), this keeps naming the closed tab
    /// until the transport disconnects the client — there is no successor to
    /// point at.
    #[must_use]
    pub fn active_tab(&self) -> TabId {
        self.active_tab
    }

    /// This client's lock mode.
    #[must_use]
    pub fn lock_mode(&self) -> LockMode {
        self.lock_mode
    }

    /// The pane this client has focused in `tab_id`, or `None` if it has not
    /// focused one there.
    #[must_use]
    pub fn focused_pane(&self, tab_id: TabId) -> Option<PaneId> {
        self.focus_by_tab.get(&tab_id).copied()
    }

    /// Every focused pane this client remembers, keyed by tab id.
    #[must_use]
    pub fn focused_panes(&self) -> &HashMap<TabId, PaneId> {
        &self.focus_by_tab
    }

    /// How `tab_id` is laid out **for this client**: zoomed on one pane, or
    /// tiled. The tab's tree is the same either way; this only says how this
    /// client solves it, so another client can be tiled on the same tab at the
    /// same moment.
    #[must_use]
    pub fn layout_mode(&self, tab_id: TabId) -> LayoutMode {
        self.zoom_by_tab
            .get(&tab_id)
            .map_or(LayoutMode::Tiled, |&focused| LayoutMode::Fullscreen {
                focused,
            })
    }

    /// The pane this client has zoomed in `tab_id`, if any.
    #[must_use]
    pub fn zoomed_pane(&self, tab_id: TabId) -> Option<PaneId> {
        self.zoom_by_tab.get(&tab_id).copied()
    }

    /// Every pane this client has zoomed, keyed by tab id. A tab with no entry is
    /// tiled for this client.
    #[must_use]
    pub fn zoomed_panes(&self) -> &HashMap<TabId, PaneId> {
        &self.zoom_by_tab
    }

    /// Zoom `pane_id` for this client in `tab_id`: it fills the tab and the
    /// tab's other panes are hidden, for this client's view alone.
    pub fn zoom_pane(&mut self, tab_id: TabId, pane_id: PaneId) {
        self.zoom_by_tab.insert(tab_id, pane_id);
    }

    /// Leave zoom in `tab_id`: this client sees the tab tiled again.
    pub fn clear_zoom(&mut self, tab_id: TabId) {
        self.zoom_by_tab.remove(&tab_id);
    }

    /// Leave zoom in every tab where this client was zoomed on `pane_id`.
    ///
    /// Called when a pane is removed: a zoom on a pane that no longer exists has
    /// nothing to show, so the client falls back to its tiled view rather than
    /// silently zooming whatever pane inherits the focus.
    pub fn clear_zoom_of_pane(&mut self, pane_id: PaneId) {
        self.zoom_by_tab.retain(|_, zoomed| *zoomed != pane_id);
    }

    /// This client's mouse interaction state.
    #[must_use]
    pub fn mouse_state(&self) -> &MouseState {
        &self.mouse_state
    }

    /// This client's in-flight resize drag, if one is in progress.
    #[must_use]
    pub fn pending_resize_drag(&self) -> Option<&ResizeDragState> {
        self.pending_resize_drag.as_ref()
    }

    /// This client's incomplete multi-chord key sequence.
    #[must_use]
    pub fn pending_key_sequence(&self) -> Option<&PendingKeySequence> {
        self.pending_key_sequence.as_ref()
    }

    /// Replace this client's incomplete multi-chord key sequence.
    pub fn update_pending_key_sequence(&mut self, pending: Option<PendingKeySequence>) {
        self.pending_key_sequence = pending
    }

    /// Take and clear this client's incomplete multi-chord key sequence.
    pub fn take_pending_key_sequence(&mut self) -> Option<PendingKeySequence> {
        self.pending_key_sequence.take()
    }

    /// This client's scrollback view offset for `pane_id`: lines scrolled up from
    /// the live bottom. `0` — the default for any pane not scrolled back — means
    /// the view follows live output.
    #[must_use]
    pub fn scroll_offset(&self, pane_id: PaneId) -> usize {
        self.scroll_by_pane.get(&pane_id).copied().unwrap_or(0)
    }

    /// Set this client's scrollback view offset for `pane_id`. An offset of `0`
    /// removes the entry, restoring live-following, so the map holds only
    /// scrolled-back panes.
    pub fn set_scroll_offset(&mut self, pane_id: PaneId, offset: usize) {
        if offset == 0 {
            self.scroll_by_pane.remove(&pane_id);
        } else {
            self.scroll_by_pane.insert(pane_id, offset);
        }
    }

    /// This client's tabline scroll position: `None` follows the active tab,
    /// `Some(i)` peeks from tab index `i`. See the field docs.
    #[must_use]
    pub fn tabline_offset(&self) -> Option<usize> {
        self.tabline_offset
    }

    /// Set this client's tabline scroll position. `Some(i)` peeks from index
    /// `i` without changing focus; `None` restores following the active tab.
    pub fn set_tabline_offset(&mut self, offset: Option<usize>) {
        self.tabline_offset = offset;
    }

    /// This client's in-flight tabline peek-drag, if one is under way.
    #[must_use]
    pub fn tabline_drag(&self) -> Option<TablineDragState> {
        self.tabline_drag
    }

    /// Begin or update (with `Some`) or end (with `None`) this client's tabline
    /// peek-drag.
    pub fn set_tabline_drag(&mut self, drag: Option<TablineDragState>) {
        self.tabline_drag = drag;
    }

    /// Update this client's lock mode.
    pub fn update_lock_mode(&mut self, lock_mode: LockMode) {
        self.lock_mode = lock_mode
    }

    /// Set the pane this client has focused in `tab_id`, returning the prior pane if one was set.
    ///
    /// **Zoom follows focus.** When this client has `tab_id` zoomed, the zoom
    /// moves to the newly focused pane: the zoomed view swaps its content and
    /// stays on. Doing it here means every path that moves focus — a keybinding,
    /// a `focus-pane` command, focus repair after a close — keeps the two in step
    /// without having to remember to.
    pub fn update_focused_pane(&mut self, tab_id: TabId, pane_id: PaneId) -> Option<PaneId> {
        if let Some(zoomed) = self.zoom_by_tab.get_mut(&tab_id) {
            *zoomed = pane_id;
        }
        self.focus_by_tab.insert(tab_id, pane_id)
    }

    /// Forget the pane this client focused in `tab_id`, and leave any zoom there:
    /// with no focused pane there is no pane for a zoom to show.
    pub fn remove_focused_pane(&mut self, tab_id: TabId) {
        self.focus_by_tab.remove(&tab_id);
        self.zoom_by_tab.remove(&tab_id);
    }

    /// Switch this client to viewing `tab_id`.
    ///
    /// A tab switch always reveals the new tab: it drops any tabline peek so
    /// the strip follows the active tab again, and ends any in-flight tabline
    /// drag. It also ends any in-flight border-resize drag or captured mouse
    /// gesture, whose pane is no longer on the client's frame.
    pub fn update_active_tab(&mut self, tab_id: TabId) {
        self.active_tab = tab_id;
        self.tabline_offset = None;
        self.tabline_drag = None;
        self.pending_resize_drag = None;
        self.mouse_capture = None;
    }

    /// Update this client's viewport size.
    pub fn update_viewport(&mut self, viewport: Size) {
        self.viewport = viewport
    }

    /// Update this client's mouse state.
    pub fn update_mouse_state(&mut self, mouse_state: MouseState) {
        self.mouse_state = mouse_state
    }

    /// Update this client's in-flight resize drag.
    pub fn update_pending_resize_drag(&mut self, pending_resize_drag: Option<ResizeDragState>) {
        self.pending_resize_drag = pending_resize_drag
    }

    /// The pane a forwarded mouse gesture is captured to, if a button is held.
    #[must_use]
    pub fn mouse_capture(&self) -> Option<PaneId> {
        self.mouse_capture
    }

    /// Set (`Some`) or clear (`None`) the pane a forwarded mouse gesture is
    /// captured to.
    pub fn set_mouse_capture(&mut self, pane: Option<PaneId>) {
        self.mouse_capture = pane;
    }
}

/// The clients currently attached to one session, keyed by [`ClientId`]. The
/// session owns exactly one registry and holds no per-client state itself —
/// focus, lock mode, and viewport live on each [`Client`] — so attached
/// terminals stay independent. The map is ordered, so iteration walks
/// clients in id order deterministically.
#[derive(Debug, Default)]
pub struct ClientRegistry {
    records: BTreeMap<ClientId, Client>,
}

impl ClientRegistry {
    /// An empty registry with no clients attached.
    #[must_use]
    pub fn new() -> Self {
        ClientRegistry {
            records: BTreeMap::new(),
        }
    }

    /// The client attached under `client_id`, or `None` if none is.
    #[must_use]
    pub fn get(&self, client_id: ClientId) -> Option<&Client> {
        self.records.get(&client_id)
    }

    /// Mutable access to one client for in-place edits to its view state —
    /// active tab, per-tab focus, lock mode, viewport.
    ///
    /// The client exposes its `id`, but **mutating `id` through this handle does
    /// not move the map entry** — the client would stay keyed under its old id,
    /// desyncing the key from `client.id`. Identity changes happen via detach + attach.
    pub fn get_mut(&mut self, client_id: ClientId) -> Option<&mut Client> {
        self.records.get_mut(&client_id)
    }

    /// Detach the client under `client_id` on disconnect, returning the removed
    /// [`Client`] so the caller can run teardown and re-reconcile tab sizes.
    /// `None` if it was not attached.
    pub fn detach(&mut self, client_id: ClientId) -> Option<Client> {
        self.records.remove(&client_id)
    }

    /// Register `client` on attach, keyed by its own id. Returns the previous
    /// record if that id was already attached — a re-attach replaces in place.
    pub fn attach(&mut self, client: Client) -> Option<Client> {
        self.records.insert(client.id, client)
    }

    /// Every attached client. Used to reconcile a tab's effective size across
    /// all clients viewing it and to fan out per-client work.
    pub fn list_attached(&self) -> impl Iterator<Item = &Client> {
        self.records.values()
    }

    /// Mutable access to every attached client, to fan out per-client view-state
    /// updates — e.g. re-anchoring scrolled-back panes as new output arrives.
    pub fn list_attached_mut(&mut self) -> impl Iterator<Item = &mut Client> {
        self.records.values_mut()
    }

    /// How many clients are attached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether no clients are attached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// One incomplete multi-chord keybinding: the chords typed into it so far, and
/// the instant an ambiguous one resolves.
///
/// The chords are Koshi's, not the pane's. A sequence that is open captures the
/// keyboard until it resolves, so no chord held here is ever written to a pane —
/// it fires a binding, or it is dropped when the sequence is left. That is why
/// the pane a chord was typed into, and the byte form it would have taken there,
/// are not kept: nothing will ever send them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingKeySequence {
    /// Canonical chords pressed so far.
    pub sequence: KeySequence,
    /// Disambiguation instant, set only when the chords so far are BOTH a
    /// complete binding and the prefix of a longer one — reaching it fires the
    /// complete binding. A prefix-only sequence carries `None` and waits for
    /// the next chord indefinitely.
    pub deadline: Option<Instant>,
}

/// Per-client mouse interaction state: what this client's pointer is currently
/// doing within its own view — last position, pressed buttons, and any
/// in-progress hover or selection. It lives on the client because each
/// attached terminal drives its own pointer independently; two clients viewing
/// the same tab keep separate mouse state.
///
/// Placeholder: the mouse-routing layer fills in the concrete fields. This is
/// transient runtime state — re-initialized on each attach, never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MouseState;

/// Per-client state of an in-flight pane-border resize drag: the pane whose
/// border was grabbed, which side it is, and the cell the last *applied* resize
/// tracked to. Dragging moves that border a cell at a time to follow the
/// pointer; `last` advances only when a resize is accepted, so pushing the
/// pointer past a pane's minimum size leaves `last` at the wall and a reverse
/// drag reacts at once. Held only between the border mouse-press that begins the
/// drag and the release that ends it. It lives on the client because the gesture
/// belongs to the one terminal performing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeDragState {
    /// The pane whose border is being dragged.
    pub pane: PaneId,
    /// Which of the pane's borders was grabbed.
    pub side: Direction,
    /// The cell the last accepted resize tracked to; the next drag delta is
    /// measured from here.
    pub last: Point,
}

/// Per-client state of an in-flight tabline peek-drag: the cell the drag
/// anchored on and the first visible tab index at that instant. Dragging
/// horizontally from the anchor scrolls the tab strip without changing which
/// tab is active. Held only between the tabline mouse-press that begins the
/// drag and the release that ends it. It lives on the client because the
/// gesture belongs to the one terminal performing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablineDragState {
    /// The screen column the drag anchored on.
    pub anchor_x: u16,
    /// The first visible tab index when the drag began.
    pub anchor_first_visible: usize,
}

#[cfg(test)]
mod tests;
