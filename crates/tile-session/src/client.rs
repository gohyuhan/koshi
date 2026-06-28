//! Attached clients: the per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport, and input
//! modes are per-client so two attached terminals never fight over one
//! global cursor; the session itself holds only this registry.

use std::{collections::HashMap, time::SystemTime};

use tile_core::{
    geometry::Size,
    ids::{ClientId, PaneId, SessionId, TabId},
    lock::LockMode,
};

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
    pending_resize_drag: Option<ResizeDragState>,
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

    /// The tab this client is currently viewing.
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

    /// Update this client's lock mode.
    pub fn update_lock_mode(&mut self, lock_mode: LockMode) {
        self.lock_mode = lock_mode
    }

    /// Set the pane this client has focused in `tab_id`, returning the prior pane if one was set.
    pub fn update_focused_pane(&mut self, tab_id: TabId, pane_id: PaneId) -> Option<PaneId> {
        self.focus_by_tab.insert(tab_id, pane_id)
    }

    /// Forget the pane this client focused in `tab_id`.
    pub fn remove_focused_pane(&mut self, tab_id: TabId) {
        self.focus_by_tab.remove(&tab_id);
    }

    /// Switch this client to viewing `tab_id`.
    pub fn update_active_tab(&mut self, tab_id: TabId) {
        self.active_tab = tab_id
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
}

/// The clients currently attached to one session, keyed by [`ClientId`]. The
/// session owns exactly one registry and holds no per-client state itself —
/// focus, lock mode, and viewport live on each [`Client`] — so attached
/// terminals stay independent.
#[derive(Debug, Default)]
pub struct ClientRegistry {
    records: HashMap<ClientId, Client>,
}

impl ClientRegistry {
    /// An empty registry with no clients attached.
    #[must_use]
    pub fn new() -> Self {
        ClientRegistry {
            records: HashMap::new(),
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

/// Per-client state of an in-flight border resize drag: which pane border is
/// being dragged, the cell the drag anchored on, and the current delta. It is
/// held only between the border mouse-press that begins the drag and the
/// release that commits it as a pane resize; while present, drag motion feeds
/// the resize and is not forwarded to the pane's process. A locked client has
/// no resize drag — the mouse-routing layer clears any pending drag when a
/// client enters a lock mode, so locked input is never treated as a resize. It
/// lives on the client because the gesture belongs to the one terminal
/// performing it.
///
/// Placeholder: the mouse-routing layer fills in the concrete fields. This is
/// transient runtime state — never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResizeDragState;

#[cfg(test)]
mod tests;
