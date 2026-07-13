//! The keymap hint catalog: per-mode hint-bar data resolved from the merged
//! keymap.
//!
//! The hint bar draws from plain snapshot data, so the runtime resolves the
//! keybinding side once here: fold the keymap layers with
//! [`merge_keymaps`], join every surviving binding to its action's display
//! name from the [`ActionRegistry`], and file the results per mode behind
//! [`Arc`]s. Building a frame's snapshot then costs two `Arc` clones per
//! field, not a re-merge.
//!
//! The catalog is rebuilt whenever the keymap inputs change: construction
//! and a keybinding config reload run [`KeymapHintCatalog::from_parts`] over
//! the current layers, and a registry refresh after a plugin registers or
//! unregisters actions re-runs it against the live action table.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use koshi_config::conflict::{KeyMapLayer, LayerOrigin};
use koshi_config::keymap_merge::{merge_keymaps, MergedKeyMap, MergedModeMap};
use koshi_config::types::{
    default_prefix_labels, BoundAction, KeybindingsConfig, ModeBindings, ModeName,
};
use koshi_core::action::ActionRef;
use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_renderer::snapshot::{HintBinding, KeymapHints};

/// Per-mode hint-bar data: every mode's bindings joined to display names,
/// shared by reference with each frame's snapshot.
pub(crate) struct KeymapHintCatalog {
    /// Liveness-filtered lookup table shared by hints and keyboard resolution.
    merged: Arc<MergedKeyMap>,
    /// Multi-chord wait before an incomplete prefix falls through.
    chord_timeout: Duration,
    /// Hard cap on one pending key sequence.
    max_chord_depth: usize,
    /// One sorted binding list per built-in mode; a mode nothing binds in
    /// holds an empty list.
    entries: BTreeMap<ModeName, Arc<Vec<HintBinding>>>,
    /// Per-mode keys a user surface removed; empty until user layers load.
    removed: BTreeMap<ModeName, Arc<BTreeSet<KeySequence>>>,
    /// Display labels for the default table's prefix chords.
    prefix_labels: Arc<BTreeMap<KeyChord, String>>,
    /// True when the user keymap was reverted to defaults over a key
    /// collision. Stays `false` until the config loader runs conflict
    /// detection and reports its verdict.
    reverted: bool,
}

impl KeymapHintCatalog {
    /// Resolve the hint catalog from the built-in default bindings and the
    /// live action table.
    pub(crate) fn from_registry(registry: &ActionRegistry) -> Self {
        Self::from_parts(
            &keymap_layers(None, &BTreeMap::new()),
            &KeybindingsConfig::default(),
            registry,
        )
    }

    /// Resolve the hint catalog from `layers` and the effective keybinding
    /// config, whose timing fields and unlock alternative carry into lookups.
    ///
    /// Folds the layers with [`merge_keymaps`], so the hint bar honors the
    /// firing model: a binding whose action the resolver refuses
    /// (unregistered, or not yet implemented) never yields a hint. In locked
    /// mode the entry firing `core:unlock` is pinned, so truncation keeps
    /// the escape hint visible.
    pub(crate) fn from_parts(
        layers: &[KeyMapLayer],
        config: &KeybindingsConfig,
        registry: &ActionRegistry,
    ) -> Self {
        let chord_timeout = Duration::from_millis(u64::from(config.chord_timeout_ms));
        let max_chord_depth = usize::from(config.max_chord_depth);
        let merged = merge_keymaps(
            layers,
            config.unlock_alternative,
            config.max_chord_depth,
            registry,
            &built_in_modes(),
        );

        let unlock = ActionRef::core("unlock")
            .expect("the reserved unlock action name satisfies the action-name grammar");
        let empty = MergedModeMap::default();

        let mut entries = BTreeMap::new();
        let mut removed = BTreeMap::new();
        for mode in LockMode::ALL {
            let name = ModeName::new(mode.name());
            let merged_mode = merged.modes.get(&name).unwrap_or(&empty);
            entries.insert(
                name.clone(),
                Arc::new(mode_entries(merged_mode, registry, mode, &unlock)),
            );
            removed.insert(name, Arc::new(merged_mode.removed_keys.clone()));
        }

        KeymapHintCatalog {
            merged: Arc::new(merged),
            chord_timeout,
            max_chord_depth,
            entries,
            removed,
            prefix_labels: Arc::new(default_prefix_labels()),
            reverted: false,
        }
    }

    /// Resolve one pending sequence in a built-in mode.
    pub(crate) fn match_sequence(&self, mode: LockMode, sequence: &KeySequence) -> KeyMatch {
        let name = ModeName::new(mode.name());
        let Some(mode_map) = self.merged.modes.get(&name) else {
            return KeyMatch::default();
        };
        let exact = mode_map
            .user_set
            .get(sequence)
            .map(|binding| binding.bound.clone())
            .or_else(|| mode_map.defaults.get(sequence).cloned());
        let prefix = mode_map
            .user_set
            .keys()
            .chain(mode_map.defaults.keys())
            .any(|candidate| {
                candidate.chords().len() > sequence.chords().len()
                    && candidate.chords().starts_with(sequence.chords())
            });
        KeyMatch { exact, prefix }
    }

    pub(crate) fn chord_timeout(&self) -> Duration {
        self.chord_timeout
    }

    pub(crate) fn max_chord_depth(&self) -> usize {
        self.max_chord_depth
    }

    /// The hint-bar data for one client's current mode: the mode's bindings
    /// and removals shared by reference, plus the labels and the revert flag.
    pub(crate) fn hints_for(&self, mode: LockMode) -> KeymapHints {
        let name = ModeName::new(mode.name());
        KeymapHints {
            entries: self.entries.get(&name).map(Arc::clone).unwrap_or_default(),
            prefix_labels: Arc::clone(&self.prefix_labels),
            removed: self.removed.get(&name).map(Arc::clone).unwrap_or_default(),
            reverted: self.reverted,
        }
    }
}

/// The ordered keymap layers: the built-in default binding table, the user's
/// `keybinding.kdl` modes when present, and the manual layer's runtime edits
/// when any exist — highest precedence last. Every user-authored layer
/// passes through [`KeyMapLayer::with_user_args_stripped`], so binding
/// arguments smuggled into a user surface are dropped rather than honored.
pub(crate) fn keymap_layers(
    user_modes: Option<BTreeMap<ModeName, ModeBindings>>,
    manual_modes: &BTreeMap<ModeName, ModeBindings>,
) -> Vec<KeyMapLayer> {
    let mut layers = vec![KeyMapLayer {
        origin: LayerOrigin::Defaults,
        modes: KeybindingsConfig::default().modes,
    }];
    if let Some(modes) = user_modes {
        layers.push(
            KeyMapLayer {
                origin: LayerOrigin::User,
                modes,
            }
            .with_user_args_stripped(),
        );
    }
    if !manual_modes.is_empty() {
        layers.push(
            KeyMapLayer {
                origin: LayerOrigin::Manual,
                modes: manual_modes.clone(),
            }
            .with_user_args_stripped(),
        );
    }
    layers
}

/// Every built-in input mode's name.
pub(crate) fn built_in_modes() -> BTreeSet<ModeName> {
    LockMode::ALL
        .iter()
        .map(|mode| ModeName::new(mode.name()))
        .collect()
}

/// Exact and longer-prefix results for one sequence lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyMatch {
    pub(crate) exact: Option<BoundAction>,
    pub(crate) prefix: bool,
}

/// One mode's merged bindings joined to display names, sorted by sequence.
///
/// Walks the mode's user-set entries and surviving defaults (steal already
/// resolved by the merge, so the two never hold the same key), reads each
/// action's display name from the registry, and flags the locked-mode
/// unlock binding pinned.
fn mode_entries(
    merged: &MergedModeMap,
    registry: &ActionRegistry,
    mode: LockMode,
    unlock: &ActionRef,
) -> Vec<HintBinding> {
    let user = merged
        .user_set
        .iter()
        .map(|(sequence, binding)| (sequence, &binding.bound, true));
    let defaults = merged
        .defaults
        .iter()
        .map(|(sequence, bound)| (sequence, bound, false));

    let mut entries: Vec<HintBinding> = user
        .chain(defaults)
        .map(|(sequence, bound, user_set)| {
            let label = registry
                .lookup(&bound.action)
                // The merge admits firing bindings only, and firing requires
                // a registry entry, so the same registry resolves every one.
                .expect("a merged binding's action is registered")
                .display_name
                .clone();
            HintBinding {
                sequence: sequence.clone(),
                label,
                user_set,
                pinned: mode == LockMode::Locked && bound.action == *unlock,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.sequence.cmp(&b.sequence));
    entries
}

#[cfg(test)]
mod tests;
