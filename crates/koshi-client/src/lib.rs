//! The viewer half of koshi: one attached terminal's own side of a session.
//!
//! A session is authoritative over tabs, panes, and the processes inside them.
//! A viewer owns the terminal in front of the user: its size, the settings it
//! reads from its own config, the colors it paints koshi's chrome with, and the
//! keymap it resolves its own keys against. It also decides what every key and
//! mouse event over its frame means. The two talk only through the session's
//! command door and its event feed.
//!
//! Colors live with the viewer: the frame a session hands out says *which pane
//! is focused*, and each viewer looks up what "focused" looks like in its own
//! theme. Two viewers of one session can paint it two different ways at once.

/// The bare `koshi` launch.
pub mod app;

/// The attached client: join a running session over its control socket and
/// read its event stream. A switch re-attaches the same terminal to the named
/// session; a broken link to a session on a server dials that server again for
/// up to 120 seconds while `remote-reconnect` is on; a detach, the session
/// ending, and a broken link to a session on this machine end the client.
pub mod attach;

pub mod input;

pub mod mouse;

/// The outer terminal an attached client owns: its viewer, its input thread,
/// and painting frames into it.
pub(crate) mod terminal;

pub mod theme;

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use koshi_config::conflict::{detect_conflicts, keymap_layers, ConflictReport, KeymapVerdict};
use koshi_config::hints::{HintBinding, KeymapHintCatalog, KeymapHints};
use koshi_config::layer::{
    ConfigLayers, PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig,
};
use koshi_config::types::ClientConfig;
use koshi_core::action::{MOUSE_SELECT_HINT, MOUSE_UNSELECT_HINT};
use koshi_core::key::PendingKeySequence;
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseButton;
use koshi_core::registry::ActionRegistry;
use koshi_core::{
    event::Event,
    geometry::{PaneArea, Size},
    ids::{ClientId, PaneId, TabId},
};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::region::core_region_solve;
use koshi_renderer::snapshot::{Delivery, Reconnecting};
use koshi_renderer::theme::Theme;

use crate::mouse::{LastPress, ResizeDrag, SelectionDrag, TablineDrag};

#[cfg(test)]
mod tests;

/// Compute the pane area left by the compiled-in navigator and hint regions.
///
/// An `80x24` viewport reports `Reported(80x22)`. A viewport shorter than the
/// two rows reports zero rows instead of an invalid negative size.
#[must_use]
pub(crate) fn core_pane_area(viewport: Size) -> PaneArea {
    PaneArea::Reported(core_region_solve(viewport).pane_rect.size)
}

/// One attached terminal's view side: its id, its own terminal size, its event
/// feed from the session, the settings it read from its own config, the chrome
/// colors and keymap resolved from them, and the outer-terminal restore guard.
///
/// The binary's event loop drives it; it can never mutate session or pane
/// data.
pub struct Client {
    /// This client's id, the one its input events and commands carry.
    id: ClientId,
    /// The client's own outer-terminal size in cells. Updated from resize
    /// events and reported to the session, which reconciles tab sizes from
    /// every viewer's report; this copy is the client's alone.
    viewport: Size,
    /// Receiving end of this client's event subscription, read by
    /// [`apply_events`](Self::apply_events). A viewer subscribed to a session in
    /// this process is fed by that session's bounded fan-out: live events, and
    /// the fresh frame the session sends after this subscriber's queue
    /// overflowed. A viewer attached over a connection is handed a receiver with
    /// no sender — its frames arrive on the connection — so nothing is ever
    /// delivered here.
    events: Receiver<Delivery>,
    /// This viewer's stored config overrides, one layer per config file, as
    /// [`load_startup_config`](Self::load_startup_config) last left them. A
    /// refused `keybinding.kdl` leaves its layer empty.
    layers: ConfigLayers,
    /// The settings this viewer owns, folded from [`layers`](Self::layers).
    config: ClientConfig,
    /// The chrome colors [`config`](Self::config)'s theme resolves to. Held
    /// resolved, so a frame reads them by borrow.
    theme: Theme,
    /// The keymap this viewer resolves its own keys against, built from
    /// [`config`](Self::config)'s keybindings and the action table.
    keymap: KeymapHintCatalog,
    /// The action table a bound name is checked against — for the hint bar's
    /// labels and the `continuous` flag a repeat-capable binding re-arms on.
    /// Dispatch itself happens on the session, against its own table.
    registry: ActionRegistry,
    /// This viewer's input mode. It decides what a key means before anything is
    /// sent. The session keeps its own copy, which `koshi lock --client`
    /// reaches and `koshi list-clients` reports.
    lock_mode: LockMode,
    /// Whether this viewer grabs the mouse for text selection. It decides what
    /// a press means before anything is sent. The session keeps its own copy,
    /// which the frame carries for the mode indicator and the hint bar's label.
    mouse_select: bool,
    /// The multi-chord binding being typed, if any. Held chords belong to
    /// koshi and never reach a pane.
    pending: Option<PendingKeySequence>,
    /// The most recent mouse press, which is what tells a double click from two
    /// separate clicks. `None` before this viewer has pressed anything.
    last_press: Option<LastPress>,
    /// The pane a forwarded press captured, and the button that pressed it.
    /// While a button is held, its drags and its release go to this pane even as
    /// the pointer leaves it, and a drag or release with no capture is not
    /// forwarded. Set when this viewer forwards the press; cleared on the next
    /// release.
    ///
    /// The stored button is the reliable one — a press always names its button,
    /// while some terminals report every drag and release as the left button.
    mouse_capture: Option<(PaneId, MouseButton)>,
    /// The pane-border drag under way, held only between the press on a border
    /// that begins it and the release that ends it.
    resize_drag: Option<ResizeDrag>,
    /// The tab-strip peek-drag under way, held only between the press on the
    /// bare strip that begins it and the release that ends it.
    tabline_drag: Option<TablineDrag>,
    /// Where this viewer's tab strip is scrolled, and the tab it was scrolled
    /// on: `None` follows the active tab, `Some((tab, i))` peeks from tab index
    /// `i` while `tab` is the active one. The peek belongs to the tab it was
    /// made on, and [`Client::note_active_tab`] throws it away as soon as the
    /// viewer sees a frame on another tab.
    tabline_peek: Option<(TabId, usize)>,
    /// The text-selection drag under way, held only between the press on a
    /// pane's content that begins it and the release that ends it. The highlight
    /// it produces lives on the session and outlives it.
    selection_drag: Option<SelectionDrag>,
    /// The line the pane's view showed on its top row when the last edge-scroll
    /// step was asked for, awaiting the session's report of where the view
    /// landed. Set only for a scroll the selection drag's timer asked for.
    scroll_from_top: Option<u64>,
    /// The pane this viewer's pointer is over, or `None` when it is over chrome
    /// or off every pane. The renderer draws an unfocused hovered pane in the
    /// hover color, so the wheel's target is visible before the wheel turns.
    hovered_pane: Option<PaneId>,
    /// Where this viewer's dialing stands while it has no link to the session,
    /// and `None` while it has one. The tabline draws
    /// `RECONNECTING (attempt 4, retry in 8s)` while it holds a
    /// `Reconnecting { attempt: 4, retry_in_seconds: 8 }`.
    reconnecting: Option<Reconnecting>,
    /// Restores the outer terminal when the client ends or the process
    /// panics. Held to be dropped with the client; nothing reads it.
    _cleanup_guard: TerminalCleanupGuard,
}

impl Client {
    /// Build a client from its id, its terminal's current size, the receiver
    /// the session handed out for it, and the outer-terminal cleanup guard.
    ///
    /// It starts on the built-in defaults: no stored config layers, the stock
    /// palette, and the shipped keymap. The files the user wrote arrive
    /// through [`load_startup_config`](Self::load_startup_config).
    #[must_use]
    pub fn new(
        id: ClientId,
        viewport: Size,
        events: Receiver<Delivery>,
        cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
        let layers = ConfigLayers::default();
        let config = layers.effective_client();
        let theme = theme::resolve(&config.theme);
        let registry = ActionRegistry::new();
        let keymap = KeymapHintCatalog::from_registry(&registry);
        Client {
            id,
            viewport,
            events,
            layers,
            config,
            theme,
            keymap,
            registry,
            lock_mode: LockMode::Normal,
            mouse_select: false,
            pending: None,
            last_press: None,
            mouse_capture: None,
            resize_drag: None,
            tabline_drag: None,
            tabline_peek: None,
            selection_drag: None,
            scroll_from_top: None,
            hovered_pane: None,
            reconnecting: None,
            _cleanup_guard: cleanup_guard,
        }
    }

    /// Apply the config files this viewer read at startup: `koshi.kdl`'s
    /// viewer-owned sections, the color theme, and `keybinding.kdl`. Each is
    /// `None` when its file is absent or failed to load, and its defaults then
    /// stand — a `None` `keybindings` puts the keymap back on the built-ins
    /// and drops any sequence being typed.
    ///
    /// App settings and colors are typed values and always apply. Keybindings
    /// are all-or-nothing: conflict detection runs over the candidate against
    /// this viewer's action table, and it commits only on a
    /// [`KeymapVerdict::Apply`] — storing the layer, rebuilding the keymap,
    /// and dropping any sequence being typed. A collision puts the built-in
    /// defaults in place with the hint bar's revert marker set; a fatal
    /// finding keeps the running keymap unmarked. Both put the stored layer
    /// and the folded keybinding settings back on the built-ins.
    ///
    /// Returns the conflict report when `keybindings` is `Some`, and `None`
    /// when it is `None`.
    pub fn load_startup_config(
        &mut self,
        app: Option<PartialKoshiConfig>,
        theme: Option<PartialThemeConfig>,
        keybindings: Option<PartialKeybindingsConfig>,
    ) -> Option<ConflictReport> {
        self.layers = ConfigLayers::from_files(app.clone(), theme.clone(), None);
        self.config = self.layers.effective_client();
        self.theme = theme::resolve(&self.config.theme);

        let Some(candidate) = keybindings else {
            self.keymap = KeymapHintCatalog::from_registry(&self.registry);
            self.pending = None;
            return None;
        };
        let user_modes = candidate.modes.clone();
        let tentative_layers = ConfigLayers::from_files(app, theme, Some(candidate));
        let tentative = tentative_layers.effective_client();
        let key_layers = keymap_layers(user_modes, tentative.keybindings.leader);
        let report = detect_conflicts(
            &key_layers,
            tentative.keybindings.leader,
            tentative.keybindings.unlock_alternative,
            tentative.keybindings.max_chord_depth,
            &self.registry,
        );
        if report.verdict() != KeymapVerdict::Apply {
            // A collision reverts to the built-in defaults with the revert
            // marker in the hint bar; a fatal finding keeps the running
            // keymap unmarked.
            if report.verdict() == KeymapVerdict::RevertToDefaults {
                self.keymap = KeymapHintCatalog::from_registry(&self.registry).with_reverted();
                self.pending = None;
            }
            return Some(report);
        }
        self.layers = tentative_layers;
        self.config = tentative;
        self.keymap =
            KeymapHintCatalog::from_parts(&key_layers, &self.config.keybindings, &self.registry);
        // The chords held so far were reaching for bindings the new keymap may
        // not hold, so the sequence is dropped and resolves to nothing.
        self.pending = None;
        Some(report)
    }

    /// This client's id.
    #[must_use]
    pub fn id(&self) -> ClientId {
        self.id
    }

    /// Record `id`, the id the session minted for this viewer's current attach.
    /// Every command this viewer submits afterwards carries it.
    pub fn set_id(&mut self, id: ClientId) {
        self.id = id;
    }

    /// Record where this viewer's dialing stands, or `None` once it has a link
    /// again. The tabline draws `RECONNECTING (attempt 4, retry in 8s)` for a
    /// `Some(Reconnecting { attempt: 4, retry_in_seconds: 8 })`.
    pub fn set_reconnecting(&mut self, reconnecting: Option<Reconnecting>) {
        self.reconnecting = reconnecting;
    }

    /// The client's own outer-terminal size in cells.
    #[must_use]
    pub fn viewport(&self) -> Size {
        self.viewport
    }

    /// Record the outer terminal's new size. The caller also reports the
    /// resize to the session, which owns the reconciled tab sizes.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
    }

    /// The settings this viewer owns.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The chrome colors every koshi-owned surface in this client's frames is
    /// painted with.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Whether this viewer grabs the mouse for text selection, as the session
    /// last reported it.
    #[must_use]
    pub fn mouse_select(&self) -> bool {
        self.mouse_select
    }

    /// Set whether this viewer grabs the mouse for text selection.
    ///
    /// Called when the session reports the mode for this viewer, either as an
    /// event or in the frame an attached viewer reads. The session owns the
    /// mode; this only moves the viewer's copy of it, which mouse routing
    /// reads.
    pub fn set_mouse_select(&mut self, on: bool) {
        self.mouse_select = on;
    }

    /// The hint-bar data one frame is painted from, using `mode` and the
    /// acting client's `mouse_select` state.
    ///
    /// The entry labelled [`MOUSE_SELECT_HINT`] reads [`MOUSE_UNSELECT_HINT`]
    /// while `mouse_select` is true.
    #[must_use]
    pub(crate) fn frame_hints_for(&self, mode: LockMode, mouse_select: bool) -> KeymapHints {
        mouse_select_hints(self.keymap.hints_for(mode), mouse_select)
    }

    /// Take everything the subscription has delivered and apply what the
    /// viewer must react to, returning how many deliveries were seen.
    ///
    /// The events that matter are the session's reports that this
    /// viewer's input mode changed — which happens when `koshi lock --client`
    /// names it, or when its own lock binding fires — and that its mouse-select
    /// mode changed, which happens when its own `core:mouse-select` binding
    /// fires. Both decide what an input means before anything is sent, and
    /// applying them here keeps the viewer's copies agreeing with the
    /// session's. An event naming another client is skipped.
    ///
    /// A fresh frame arrives when this subscriber's queue overflowed and the
    /// session dropped events it cannot replay. It carries the session's own
    /// copies of both, and the tab this viewer is on. The viewer takes all
    /// three from the frame — a tab-strip peek made on another tab is thrown
    /// away with the last of them — and logs how many events were dropped. The
    /// frame names this viewer, checked by a debug assertion.
    ///
    /// A [`Delivery::Frame`] is the picture composed for a client in another
    /// process. It is counted, and nothing is taken from it. So is a
    /// [`Delivery::MouseAnswer`], which answers that client's mouse round, a
    /// [`Delivery::HostWrite`], which that client writes to its own terminal,
    /// and a [`Delivery::SwitchTo`], which moves that client to another
    /// session.
    pub fn apply_events(&mut self) -> usize {
        let mut seen = 0;
        while let Ok(delivery) = self.events.try_recv() {
            seen += 1;
            match delivery {
                Delivery::Event(event) => match &event {
                    Event::InputModeChanged(changed) if changed.client_id == self.id => {
                        self.set_lock_mode(changed.mode);
                    }
                    Event::MouseSelectChanged(changed) if changed.client_id == self.id => {
                        self.mouse_select = changed.on;
                    }
                    _ => {}
                },
                // A frame, a mouse round's answers, terminal bytes, and a
                // session move all belong to a client in another process, which
                // reads them off its own connection.
                Delivery::Frame(_)
                | Delivery::MouseAnswer { .. }
                | Delivery::HostWrite(_)
                | Delivery::SwitchTo(_) => {}
                Delivery::Snapshot { snapshot, lagged } => {
                    debug_assert_eq!(
                        snapshot.client.id, self.id,
                        "a frame names the client its subscriber views"
                    );
                    tracing::warn!(
                        dropped = lagged.dropped_count,
                        "events were dropped; resuming from a fresh frame"
                    );
                    self.set_lock_mode(snapshot.client.lock_mode);
                    self.note_active_tab(snapshot.client.active_tab);
                    self.mouse_select = snapshot.client.mouse_select;
                }
            }
        }
        seen
    }
}

/// `hints` with the `core:mouse-select` entry wearing its "on" label, so the
/// hint bar reads `Mouse Unselect` while mouse-select mode is active.
///
/// `on` false returns `hints` untouched. `on` true returns a copy in which
/// every entry labelled [`MOUSE_SELECT_HINT`] is relabelled
/// [`MOUSE_UNSELECT_HINT`]; nothing else changes. Matching is on the label, so
/// a rebound or duplicated binding flips too.
fn mouse_select_hints(hints: KeymapHints, on: bool) -> KeymapHints {
    if !on {
        return hints;
    }
    let entries: Vec<HintBinding> = hints
        .entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            if entry.label == MOUSE_SELECT_HINT {
                MOUSE_UNSELECT_HINT.clone_into(&mut entry.label);
            }
            entry
        })
        .collect();
    KeymapHints {
        entries: Arc::new(entries),
        ..hints
    }
}
