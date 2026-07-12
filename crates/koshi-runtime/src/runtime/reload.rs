//! Config reload transactions: swapping a changed config file's settings
//! into the running process, keeping the running settings when the change
//! is invalid.
//!
//! Config lives in separate files in the koshi config directory —
//! `koshi.kdl` (app settings), `theme.kdl` (colors), `keybinding.kdl` (key
//! bindings) — and each file reloads on its own, so one file's reload never
//! touches another file's settings. A file arrives here already
//! deserialized into its partial config layer; discovering, reading, and
//! deserializing the files is the config loader's job.
//!
//! Theme and app settings are typed values and always apply. Keybindings
//! additionally run conflict detection against the live action registry and
//! apply all-or-nothing: any collision or fatal finding keeps the running
//! keymap unchanged and reports the reasons. Every transaction yields one
//! event per live session — [`Event::ConfigReloaded`] when the file's
//! settings applied, [`Event::ConfigReloadFailed`] when the keymap was kept.

use koshi_config::conflict::{detect_conflicts, ConflictReport, ConflictSeverity, KeymapVerdict};
use koshi_config::layer::{
    merge, PartialKeybindingsConfig, PartialKoshiConfig, PartialLayoutDefaults, PartialThemeConfig,
};
use koshi_config::types::KoshiConfig;
use koshi_core::event::{ConfigReloadFailed, ConfigReloaded, Event};
use koshi_core::geometry::Direction;
use koshi_core::ids::SessionId;

use crate::runtime::{
    hints::{built_in_modes, keymap_layers, KeymapHintCatalog},
    render_schedule::InvalidationReason,
    snapshot::resolve_theme,
    state::Runtime,
};

/// The user's stored config overrides, one layer per config file, folded
/// onto the built-in defaults to produce the effective config. Each file's
/// reload transaction replaces its own layer and leaves the others as they
/// are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfigLayers {
    /// The `koshi.kdl` app-settings layer.
    app: PartialKoshiConfig,
    /// The `theme.kdl` layer; only its theme section is set.
    theme: PartialKoshiConfig,
    /// The `keybinding.kdl` layer; only its keybindings section is set.
    keybindings: PartialKoshiConfig,
}

impl ConfigLayers {
    /// Layers holding only the given startup split direction, standing in
    /// for the app config file until the config loader hands over real
    /// layers.
    pub(crate) fn with_default_new_pane_direction(direction: Direction) -> Self {
        ConfigLayers {
            app: PartialKoshiConfig {
                layout: Some(PartialLayoutDefaults {
                    new_pane_direction: Some(direction),
                    default_layout: None,
                }),
                ..PartialKoshiConfig::default()
            },
            ..ConfigLayers::default()
        }
    }

    /// Fold the stored layers onto the built-in defaults. The dedicated
    /// theme and keybinding layers fold after the app layer, so their
    /// sections win over a same-named section in the app file.
    pub(crate) fn effective(&self) -> KoshiConfig {
        merge(
            KoshiConfig::default(),
            vec![
                self.app.clone(),
                self.theme.clone(),
                self.keybindings.clone(),
            ],
        )
    }
}

/// What a keybinding reload produced: the per-session events to publish and
/// the detection report backing them.
#[derive(Debug)]
pub struct KeymapReloadOutcome {
    /// One event per live session: [`Event::ConfigReloaded`] when the keymap
    /// applied, [`Event::ConfigReloadFailed`] when it was kept.
    pub events: Vec<Event>,
    /// Every finding from conflict detection, warnings included, for the
    /// caller to surface to the user.
    pub report: ConflictReport,
}

impl Runtime {
    /// Swap in a reloaded `theme.kdl`: store the candidate as the theme
    /// layer, recompute the effective config, resolve the chrome theme from
    /// it, and schedule a repaint. Returns one [`Event::ConfigReloaded`] per
    /// live session.
    pub fn reload_theme(&mut self, candidate: PartialThemeConfig) -> Vec<Event> {
        self.config_layers.theme = PartialKoshiConfig {
            theme: Some(candidate),
            ..PartialKoshiConfig::default()
        };
        self.config = self.config_layers.effective();
        self.theme = resolve_theme(&self.config.theme);
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
        self.config_reloaded_events()
    }

    /// Swap in a reloaded `koshi.kdl`: replace the app-settings layer,
    /// recompute the effective config, and hand the new values to their
    /// consumers — the default split direction takes effect for the next
    /// `new-pane`. The candidate's theme and keybinding sections are
    /// dropped: those belong to `theme.kdl` and `keybinding.kdl`, so one
    /// file's reload never reaches another file's state. Returns one
    /// [`Event::ConfigReloaded`] per live session.
    pub fn reload_app_config(&mut self, mut candidate: PartialKoshiConfig) -> Vec<Event> {
        candidate.theme = None;
        candidate.keybindings = None;
        self.config_layers.app = candidate;
        self.config = self.config_layers.effective();
        self.config_reloaded_events()
    }

    /// Swap in a reloaded `keybinding.kdl`, all-or-nothing: run conflict
    /// detection over the candidate layers against the live action registry,
    /// and only a clean [`KeymapVerdict::Apply`] with valid timing fields
    /// commits — storing the layer, rebuilding the hint catalog, clearing
    /// every client's pending key sequence, and emitting
    /// [`Event::ConfigReloaded`] per session. Any collision or fatal
    /// finding, or a `max_chord_depth` of 0 (it would stop every binding
    /// from resolving, the locked-mode unlock included), keeps the running
    /// keymap and config exactly as they are and emits
    /// [`Event::ConfigReloadFailed`] carrying every reason.
    pub fn reload_keybindings(
        &mut self,
        candidate: PartialKeybindingsConfig,
    ) -> KeymapReloadOutcome {
        let user_modes = candidate.modes.clone();
        let tentative_layers = ConfigLayers {
            keybindings: PartialKoshiConfig {
                keybindings: Some(candidate),
                ..PartialKoshiConfig::default()
            },
            ..self.config_layers.clone()
        };
        let tentative = tentative_layers.effective();
        let layers = keymap_layers(user_modes);
        let report = detect_conflicts(
            &layers,
            tentative.keybindings.leader,
            tentative.keybindings.unlock_alternative,
            &self.action_registry,
            &built_in_modes(),
        );
        let mut rejections: Vec<String> = Vec::new();
        if tentative.keybindings.max_chord_depth == 0 {
            rejections.push(
                "`max_chord_depth` 0 would disable every keybinding including the \
                 locked-mode unlock; the minimum is 1"
                    .to_owned(),
            );
        }
        if report.verdict() != KeymapVerdict::Apply {
            rejections.push(rejection_reason(&report));
        }
        if !rejections.is_empty() {
            let events = self.config_reload_failed_events(&rejections.join("; "));
            return KeymapReloadOutcome { events, report };
        }
        self.config_layers = tentative_layers;
        self.config = tentative;
        self.keymap_hints =
            KeymapHintCatalog::from_parts(&layers, &self.config.keybindings, &self.action_registry);
        self.clear_pending_key_sequences();
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
        KeymapReloadOutcome {
            events: self.config_reloaded_events(),
            report,
        }
    }

    /// Re-resolve the keymap against the live action registry: re-run
    /// conflict detection over the stored layers and rebuild the hint
    /// catalog, so a binding whose action just registered starts firing and
    /// one whose action vanished falls transparent. Runs after a plugin
    /// registers or unregisters actions; the stored layers stay as they are.
    /// Returns the detection report so the caller can surface findings the
    /// registry change uncovered.
    pub fn refresh_keymap_for_registry(&mut self) -> ConflictReport {
        let user_modes = self
            .config_layers
            .keybindings
            .keybindings
            .as_ref()
            .and_then(|keybindings| keybindings.modes.clone());
        let layers = keymap_layers(user_modes);
        let report = detect_conflicts(
            &layers,
            self.config.keybindings.leader,
            self.config.keybindings.unlock_alternative,
            &self.action_registry,
            &built_in_modes(),
        );
        self.keymap_hints =
            KeymapHintCatalog::from_parts(&layers, &self.config.keybindings, &self.action_registry);
        self.clear_pending_key_sequences();
        self.render_scheduler
            .invalidate(InvalidationReason::StatusChanged);
        report
    }

    /// Drop every attached client's pending multi-chord sequence; a prefix
    /// buffered against the old keymap no longer means anything under the
    /// new one.
    fn clear_pending_key_sequences(&mut self) {
        for session in self.sessions.values_mut() {
            for client in session.clients.list_attached_mut() {
                client.update_pending_key_sequence(None);
            }
        }
    }

    /// One [`Event::ConfigReloaded`] per live session, in session-id order.
    fn config_reloaded_events(&self) -> Vec<Event> {
        self.session_ids_sorted()
            .into_iter()
            .map(|session_id| Event::ConfigReloaded(ConfigReloaded { session_id }))
            .collect()
    }

    /// One [`Event::ConfigReloadFailed`] carrying `reason` per live session,
    /// in session-id order.
    fn config_reload_failed_events(&self, reason: &str) -> Vec<Event> {
        self.session_ids_sorted()
            .into_iter()
            .map(|session_id| {
                Event::ConfigReloadFailed(ConfigReloadFailed {
                    session_id,
                    reason: reason.to_owned(),
                })
            })
            .collect()
    }

    /// Every live session's id, sorted so event order is deterministic.
    fn session_ids_sorted(&self) -> Vec<SessionId> {
        let mut ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

/// The user-facing reason a keymap was kept: every collision and fatal
/// finding's message, joined with `; `.
fn rejection_reason(report: &ConflictReport) -> String {
    report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity() > ConflictSeverity::Warning)
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join("; ")
}

#[cfg(test)]
mod tests;
