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
//! The catalog is rebuilt whenever the keymap inputs change — today that is
//! construction only (the built-in defaults are the sole layer until the
//! config loader and reload land); a config reload or plugin lifecycle
//! change re-runs [`KeymapHintCatalog::from_registry`] the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_config::conflict::{KeyMapLayer, LayerOrigin};
use koshi_config::keymap_merge::{merge_keymaps, MergedModeMap};
use koshi_config::types::{default_prefix_labels, KeybindingsConfig, ModeName};
use koshi_core::action::ActionRef;
use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use koshi_renderer::snapshot::{HintBinding, KeymapHints};

/// Per-mode hint-bar data: every mode's bindings joined to display names,
/// shared by reference with each frame's snapshot.
pub(crate) struct KeymapHintCatalog {
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
    ///
    /// Folds the defaults as the single keymap layer — the exact merge the
    /// config loader will later run over the full layer list — so the hint
    /// bar honors the firing model: a binding whose action the resolver
    /// refuses (unregistered, or not yet implemented) never yields a hint.
    /// In locked mode the entry firing `core:unlock` is pinned, so
    /// truncation keeps the escape hint visible.
    pub(crate) fn from_registry(registry: &ActionRegistry) -> Self {
        let layers = [KeyMapLayer {
            origin: LayerOrigin::Defaults,
            modes: KeybindingsConfig::default().modes,
        }];
        let known_modes: BTreeSet<ModeName> = LockMode::ALL
            .iter()
            .map(|mode| ModeName::new(mode.name()))
            .collect();
        let merged = merge_keymaps(&layers, None, registry, &known_modes);

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
            entries,
            removed,
            prefix_labels: Arc::new(default_prefix_labels()),
            reverted: false,
        }
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
