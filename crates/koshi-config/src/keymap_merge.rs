//! Per-scope keymap merging: folds the ordered keymap layers into the
//! per-mode lookup tables a keypress consults.
//!
//! Bindings arrive in the same layers conflict detection reads — the
//! built-in defaults, then the user's own surfaces (user file, session,
//! layout), lowest precedence first. [`merge_keymaps`] folds them per key:
//! a later layer's entry on a key replaces a lower layer's on the same key,
//! and every other key is untouched. The result splits each mode into two
//! maps. The two resolve at different tiers of the key-resolution stack, with
//! sticky plugin layers between them:
//!
//! - **`user_set`** — the winning user-authored entries, each tagged with
//!   the layer that authored it.
//! - **`defaults`** — the surviving built-in entries: shipped defaults
//!   whose key no user surface took or removed.
//!
//! Merging and detection read one shared firing predicate in the conflict
//! module: a binding the resolver refuses, or one a keypress cannot reach,
//! is transparent — it wins no key, and the firing binding beneath it shows
//! through. A `remove` in a higher layer voids lower layers' entries on that
//! key outright.
//!
//! Merge runs only on a keymap detection has already verdicted: every
//! layer on [`KeymapVerdict::Apply`](crate::conflict::KeymapVerdict::Apply),
//! or the defaults alone after
//! [`RevertToDefaults`](crate::conflict::KeymapVerdict::RevertToDefaults).
//! Merge checks neither the unlock guarantee nor cross-layer collisions;
//! detection does both. Merging is pure and re-runs whenever the layers or
//! the action registry change (config reload, plugin load or unload); a
//! binding that turns live re-enters the merged map on that run.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::registry::ActionRegistry;

use crate::conflict::{
    built_in_modes, is_firing, removal_index, removed_above, FiringRules, KeyMapLayer, LayerOrigin,
};
use crate::types::{BoundAction, KeybindingsConfig, ModeName};

/// One merged binding: what fires on the key, plus the layer that authored
/// it. `koshi keys describe` reports that layer as the binding's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedBinding {
    /// The action and preset arguments the key triggers.
    pub bound: BoundAction,
    /// The user-authored surface the winning entry came from.
    pub source: LayerOrigin,
}

/// One mode's merged lookup tables plus its removal and displacement
/// records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedModeMap {
    /// The winning user-authored binding per key. Resolves above sticky
    /// plugin layers in the key-resolution stack.
    pub user_set: BTreeMap<KeySequence, MergedBinding>,
    /// The surviving built-in binding per key: firing shipped defaults no
    /// user surface took or removed. Resolves below sticky plugin layers.
    pub defaults: BTreeMap<KeySequence, BoundAction>,
    /// Every key any layer removes in this mode, whether or not a lower
    /// layer held it.
    pub removed_keys: BTreeSet<KeySequence>,
    /// Built-in bindings displaced by the user — their key stolen by a
    /// `user_set` entry or cleared by a remove. `koshi keys list` shows each
    /// one with its default action, marked unbound.
    pub unbound_defaults: BTreeMap<KeySequence, BoundAction>,
}

/// The merged keymap: one [`MergedModeMap`] per registered mode any layer
/// names, whether or not that mode's block holds an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedKeyMap {
    /// Per-mode merged tables.
    pub modes: BTreeMap<ModeName, MergedModeMap>,
}

/// Folds keybinding layers (ordered lowest precedence first) into the
/// per-mode lookup tables.
///
/// `registry` is the live action table each binding is resolved against
/// for the firing judgment; `max_chord_depth` is the cap a firing sequence
/// must fit. A layer's binding whose mode is not one of the
/// [`LockMode`](koshi_core::lock::LockMode) names is skipped, matching
/// detection. The reserved unlock chord is `unlock_alternative` when set,
/// otherwise [`KeybindingsConfig::RESERVED_UNLOCK`].
///
/// Per key, the highest firing entry wins. A firing user-authored entry on
/// a defaulted key takes it and the displaced default moves to
/// [`unbound_defaults`](MergedModeMap::unbound_defaults); a remove above
/// the defaults layer does the same. A dead binding (resolver-refused,
/// swallowed by the locked-mode reserved-chord bypass, or longer than the
/// chord-depth cap) enters no map: a dead user entry leaves the default
/// beneath it live, and a dead default is absent from `defaults` and from
/// [`unbound_defaults`](MergedModeMap::unbound_defaults) both.
#[must_use]
pub fn merge_keymaps(
    layers: &[KeyMapLayer],
    unlock_alternative: Option<KeyChord>,
    max_chord_depth: u8,
    registry: &ActionRegistry,
) -> MergedKeyMap {
    let known_modes = &built_in_modes();
    let reserved = unlock_alternative.unwrap_or(KeybindingsConfig::RESERVED_UNLOCK);
    let locked = ModeName::new("locked");
    let removals = removal_index(layers, known_modes);
    let rules = FiringRules {
        registry,
        reserved,
        locked: &locked,
        max_chord_depth,
    };

    let mut modes: BTreeMap<ModeName, MergedModeMap> = BTreeMap::new();

    for (index, layer) in layers.iter().enumerate() {
        for (mode, bindings) in &layer.modes {
            if !known_modes.contains(mode) {
                continue;
            }
            let merged = modes.entry(mode.clone()).or_default();

            merged.removed_keys.extend(bindings.removed.iter().cloned());

            for (key, bound) in &bindings.keys {
                if !is_firing(mode, key, bound, &rules) {
                    continue;
                }
                if removed_above(&removals, mode, key, index) {
                    // A removed default lands in `unbound_defaults`; a removed
                    // user entry enters no map at all.
                    if !layer.origin.is_user_authored() {
                        merged.unbound_defaults.insert(key.clone(), bound.clone());
                    }
                    continue;
                }
                if layer.origin.is_user_authored() {
                    merged.user_set.insert(
                        key.clone(),
                        MergedBinding {
                            bound: bound.clone(),
                            source: layer.origin,
                        },
                    );
                } else {
                    merged.defaults.insert(key.clone(), bound.clone());
                }
            }
        }
    }

    for merged in modes.values_mut() {
        for key in merged.user_set.keys() {
            if let Some(bound) = merged.defaults.remove(key) {
                merged.unbound_defaults.insert(key.clone(), bound);
            }
        }
    }

    MergedKeyMap { modes }
}

#[cfg(test)]
mod tests;
