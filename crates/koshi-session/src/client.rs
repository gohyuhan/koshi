//! Attached clients: the identity and per-client view state of one session.
//!
//! A session accepts several clients at once. Focus, viewport and input modes
//! live on each client; the session holds only this registry. Each client also
//! carries what the server set at attach: its origin, its generated label and
//! its colour.

use std::{
    collections::{BTreeMap, HashMap},
    time::SystemTime,
};

pub use koshi_core::client::ClientOrigin;
use koshi_core::{
    command::Selection,
    geometry::Size,
    ids::{ClientId, PaneId, SessionId, TabId},
    lock::LockMode,
};
use koshi_layout::mode::LayoutMode;
use serde::{Deserialize, Serialize};

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
/// identity the server gave it at attach and the view state that is the
/// client's alone. Two clients on the same session — and even viewing the same
/// tab — keep independent focus, lock mode and viewport.
#[derive(Debug, Serialize, Deserialize)]
pub struct Client {
    id: ClientId,
    session_id: SessionId,
    attached_at: SystemTime,
    viewport: Size,
    active_tab: TabId,
    /// Where this client connected from, set by the server at attach.
    origin: ClientOrigin,
    /// This client's display name, `C-<adjective>-<noun>`, generated at
    /// attach and never changed.
    label: String,
    /// Which palette entry paints this client's identity in the UI, chosen by
    /// the caller at attach.
    colour: u8,
    focus_by_tab: HashMap<TabId, PaneId>,
    lock_mode: LockMode,
    /// Whether this client grabs the mouse for text selection: while on, a drag
    /// highlights in koshi even over a program that asked for the mouse. Toggled
    /// by `core:mouse-select`; independent of [`lock_mode`](Self::lock_mode).
    mouse_select: bool,
    /// This client's scrollback view position per pane: lines scrolled up from
    /// the live bottom. A pane absent from the map (the default) sits at the live
    /// bottom, offset `0`; only scrolled-up panes have an entry, always with a
    /// non-zero offset. Scrolling is per client: two clients scroll a shared pane
    /// independently.
    ///
    /// This is the position alone. Whether the view is *held* there — showing the
    /// same text as output arrives, rather than following the newest line — is
    /// derived by [`is_view_held`](Self::is_view_held).
    scroll_by_pane: HashMap<PaneId, usize>,
    /// This client's highlighted text, keyed by the pane it is in — the whole of
    /// visual mode, since a highlight existing *is* being in visual mode for that
    /// pane and it clearing *is* leaving. A pane absent from the map has no
    /// highlight.
    ///
    /// **A highlight belongs to one pane, and panes keep their own.** Highlighting
    /// in a second pane leaves the first pane's highlight where it is, so several
    /// can be up at once. Only input that reaches a pane's own child clears that
    /// pane's highlight.
    ///
    /// Highlighting is per client: two clients viewing one pane select in it
    /// independently, and neither sees the other's highlight.
    selection_by_pane: HashMap<PaneId, Selection>,
    /// The pane this client has zoomed in each tab: the one pane filling the tab
    /// while the others are hidden. A tab absent from the map (the default) is
    /// tiled for this client.
    ///
    /// Zoom is per client: one client zooming a pane leaves another's tiled view
    /// as it is. A zoom changes how this client solves the tab's tree; the tree
    /// itself stays unchanged.
    zoom_by_tab: HashMap<TabId, PaneId>,
}

impl Client {
    /// A newly attached client viewing `active_tab` at `viewport`, with no
    /// per-tab focus recorded yet and in [`LockMode::Normal`]. The caller
    /// supplies `attached_at`, `origin`, `label` and `colour`; this never reads
    /// the clock itself.
    // Carries the whole of one attach: the client's identity (`id`,
    // `session_id`, `origin`, `label`, `colour`) and its first view
    // (`attached_at`, `viewport`, `active_tab`).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        id: ClientId,
        session_id: SessionId,
        attached_at: SystemTime,
        viewport: Size,
        active_tab: TabId,
        origin: ClientOrigin,
        label: String,
        colour: u8,
    ) -> Self {
        Client {
            id,
            session_id,
            attached_at,
            viewport,
            active_tab,
            origin,
            label,
            colour,
            focus_by_tab: HashMap::new(),
            lock_mode: LockMode::Normal,
            mouse_select: false,
            scroll_by_pane: HashMap::new(),
            selection_by_pane: HashMap::new(),
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

    /// Where this client connected from.
    #[must_use]
    pub fn origin(&self) -> ClientOrigin {
        self.origin
    }

    /// This client's generated display name.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Which palette entry paints this client's identity.
    #[must_use]
    pub fn colour(&self) -> u8 {
        self.colour
    }

    /// This client's current viewport size.
    #[must_use]
    pub fn viewport(&self) -> Size {
        self.viewport
    }

    /// The tab this client is currently viewing. Once the session's last tab
    /// closes (the session is quitting), this keeps naming the closed tab until
    /// the transport disconnects the client.
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

    /// Leave zoom in every tab where this client was zoomed on `pane_id`, so the
    /// client sees those tabs tiled again.
    ///
    /// Called when a pane is removed.
    pub fn clear_zoom_of_pane(&mut self, pane_id: PaneId) {
        self.zoom_by_tab.retain(|_, zoomed| *zoomed != pane_id);
    }

    /// Where this client's view of `pane_id` sits: lines scrolled up from the
    /// live bottom. `0` — the default for any pane not scrolled up — is the
    /// newest line.
    #[must_use]
    pub fn scroll_offset(&self, pane_id: PaneId) -> usize {
        self.scroll_by_pane.get(&pane_id).copied().unwrap_or(0)
    }

    /// Set where this client's view of `pane_id` sits. An offset of `0` removes
    /// the entry, so the map holds only scrolled-up panes.
    pub fn set_scroll_offset(&mut self, pane_id: PaneId, offset: usize) {
        if offset == 0 {
            self.scroll_by_pane.remove(&pane_id);
        } else {
            self.scroll_by_pane.insert(pane_id, offset);
        }
    }

    /// Every scrolled-up pane this client remembers, keyed by pane id, each
    /// value the lines scrolled up from the live bottom. A pane with no entry
    /// sits at the live bottom.
    #[must_use]
    pub fn scroll_offsets(&self) -> &HashMap<PaneId, usize> {
        &self.scroll_by_pane
    }

    /// This client's highlight in `pane_id`, or `None` if it has none there.
    #[must_use]
    pub fn selection(&self, pane_id: PaneId) -> Option<Selection> {
        self.selection_by_pane.get(&pane_id).copied()
    }

    /// Highlight `selection` in `pane_id` for this client, replacing any highlight
    /// it already had there. Other panes' highlights are untouched — each pane
    /// keeps its own.
    pub fn set_selection(&mut self, pane_id: PaneId, selection: Selection) {
        self.selection_by_pane.insert(pane_id, selection);
    }

    /// Drop this client's highlight in `pane_id`, leaving visual mode for that
    /// pane. Clearing a pane with no highlight changes nothing.
    ///
    /// Called when input reaches the pane's child, and when the pane is removed.
    pub fn clear_selection(&mut self, pane_id: PaneId) {
        self.selection_by_pane.remove(&pane_id);
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
        self.scroll_offset(pane_id) > 0 || self.selection_by_pane.contains_key(&pane_id)
    }

    /// Update this client's lock mode.
    pub fn update_lock_mode(&mut self, lock_mode: LockMode) {
        self.lock_mode = lock_mode
    }

    /// Whether this client grabs the mouse for text selection.
    #[must_use]
    pub fn mouse_select(&self) -> bool {
        self.mouse_select
    }

    /// Flip [`mouse_select`](Self::mouse_select) and return the new value.
    pub fn toggle_mouse_select(&mut self) -> bool {
        self.mouse_select = !self.mouse_select;
        self.mouse_select
    }

    /// Set the pane this client has focused in `tab_id`, returning the prior pane if one was set.
    ///
    /// **Zoom follows focus.** When this client has `tab_id` zoomed, the zoom
    /// moves to the newly focused pane: the zoomed view swaps its content and
    /// stays on. Every path that moves focus — a keybinding, a `focus-pane`
    /// command, focus repair after a close — runs through here.
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

    /// Switch this client to viewing `tab_id`. The highlights it made in the
    /// tab it leaves stay where they are, and it finds them again on switching
    /// back.
    pub fn update_active_tab(&mut self, tab_id: TabId) {
        self.active_tab = tab_id;
    }

    /// Update this client's viewport size.
    pub fn update_viewport(&mut self, viewport: Size) {
        self.viewport = viewport
    }

    /// Set where this client's current connection came from.
    pub fn update_origin(&mut self, origin: ClientOrigin) {
        self.origin = origin;
    }
}

/// The clients currently attached to one session, keyed by [`ClientId`]. The
/// session owns exactly one registry and holds no per-client state itself:
/// focus, lock mode and viewport live on each [`Client`]. The map is ordered,
/// so iteration walks clients in id order.
#[derive(Debug, Default, Serialize, Deserialize)]
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
    /// not move the map entry**: the client stays keyed under its old id.
    /// Identity changes happen via detach + attach.
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
    /// updates — e.g. re-anchoring pinned views as new output arrives.
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

#[cfg(test)]
mod tests;
