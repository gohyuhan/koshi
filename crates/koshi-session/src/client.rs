//! Attached clients: the per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport, and input
//! modes are per-client so two attached terminals never fight over one
//! global cursor; the session itself holds only this registry.

use std::{
    collections::HashMap,
    time::{Instant, SystemTime},
};

use koshi_core::{
    geometry::Size,
    ids::{ClientId, PaneId, SessionId, TabId},
    key::KeySequence,
    lock::LockMode,
};

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
    pending_resize_drag: Option<ResizeDragState>,
    pending_key_sequence: Option<PendingKeySequence>,
    /// This client's scrollback view offset per pane: lines scrolled up from the
    /// live bottom. A pane absent from the map (the default) follows live output;
    /// only scrolled-back panes have an entry, always with a non-zero offset. It
    /// lives on the client because scrolling is per-view — two clients scroll a
    /// shared pane independently.
    scroll_by_pane: HashMap<PaneId, usize>,
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
            pending_key_sequence: None,
            scroll_by_pane: HashMap::new(),
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

/// One incomplete multi-chord keybinding plus its passthrough bytes and expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingKeySequence {
    /// Canonical chords pressed so far.
    pub sequence: KeySequence,
    /// Terminal bytes for each chord, retained for transparent-mode fallback.
    pub raw_bytes: Vec<Vec<u8>>,
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
