//! The viewer half of koshi: one attached terminal's own side of a session.
//!
//! A session is authoritative over tabs, panes, and the processes inside them.
//! A viewer owns what belongs to the terminal in front of the user — its size,
//! the settings it reads from its own config, the colors it paints koshi's own
//! chrome with, and the keymap it resolves its own keys against. The two talk
//! only through the session's command door and its event feed.
//!
//! Colors live with the viewer: the frame a session hands out says *which pane
//! is focused*, and each viewer looks up what "focused" looks like in its own
//! theme. Two viewers of one session can paint it two different ways at once.

pub mod input;
pub mod theme;

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use koshi_config::conflict::{
    built_in_modes, detect_conflicts, keymap_layers, ConflictReport, KeymapVerdict,
};
use koshi_config::hints::{HintBinding, KeymapHintCatalog, KeymapHints};
use koshi_config::layer::{
    ConfigLayers, PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig,
};
use koshi_config::types::ClientConfig;
use koshi_core::action::{MOUSE_SELECT_HINT, MOUSE_UNSELECT_HINT};
use koshi_core::event::InputMode;
use koshi_core::key::PendingKeySequence;
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_core::{event::Event, geometry::Size, ids::ClientId};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_renderer::theme::Theme;

#[cfg(test)]
mod tests;

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
    /// Receiving end of this client's event subscription, fed by the session's
    /// bounded fan-out.
    events: Receiver<Event>,
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
    /// This viewer's input mode. It owns this because it decides what a key
    /// means before anything is sent; the session keeps its own copy so
    /// `koshi lock --client` can reach a viewer and `koshi list-clients` can
    /// report one.
    lock_mode: LockMode,
    /// The multi-chord binding being typed, if any. Held chords belong to
    /// koshi and never reach a pane.
    pending: Option<PendingKeySequence>,
    /// Restores the outer terminal when the client ends or the process
    /// panics.
    cleanup_guard: TerminalCleanupGuard,
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
        events: Receiver<Event>,
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
            pending: None,
            cleanup_guard,
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
    /// and dropping any sequence being typed. A collision or a fatal finding
    /// puts the stored layer and the folded keybinding settings back on the
    /// built-ins, and the keymap already running keeps running.
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
            &built_in_modes(),
        );
        if report.verdict() != KeymapVerdict::Apply {
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

    /// The hint-bar data for the viewer's current mode.
    #[must_use]
    pub fn keymap_hints(&self) -> KeymapHints {
        self.keymap.hints_for(self.lock_mode)
    }

    /// The hint-bar data one frame is painted from: this viewer's current
    /// mode, with the mouse-select entry's label following `mouse_select` —
    /// the acting client's mouse-select state, which the session reports in the
    /// frame's snapshot.
    #[must_use]
    pub fn frame_hints(&self, mouse_select: bool) -> KeymapHints {
        mouse_select_hints(self.keymap_hints(), mouse_select)
    }

    /// Take everything the subscription has delivered and apply what the
    /// viewer must react to, returning how many events were seen.
    ///
    /// Today that is the session's report that this viewer's input mode
    /// changed — which happens when `koshi lock --client` names it, or when
    /// its own lock binding fires and the session records the change. Applying
    /// it here is what keeps the two copies of the mode agreeing.
    pub fn apply_events(&mut self) -> usize {
        let mut seen = 0;
        while let Ok(event) = self.events.try_recv() {
            seen += 1;
            if let Event::InputModeChanged(changed) = &event {
                if changed.client_id == self.id {
                    self.set_lock_mode(match changed.mode {
                        InputMode::Locked => LockMode::Locked,
                        InputMode::Normal => LockMode::Normal,
                    });
                }
            }
        }
        seen
    }

    /// Drop every event the subscription has delivered since the last call,
    /// returning how many were dropped. Keeps the bounded queue from filling
    /// while nothing consumes the feed; drops the events one by one without
    /// collecting them.
    pub fn discard_events(&mut self) -> usize {
        self.events.try_iter().count()
    }

    /// Borrow the outer-terminal cleanup guard.
    #[must_use]
    pub fn cleanup_guard(&self) -> &TerminalCleanupGuard {
        &self.cleanup_guard
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
