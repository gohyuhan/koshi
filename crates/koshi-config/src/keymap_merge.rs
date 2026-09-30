//! Per-scope keymap merging: folds the ordered keymap layers into the
//! per-mode lookup tables a keypress consults.
//!
//! Bindings arrive in the same layers conflict detection reads — the
//! built-in defaults, then the user's own surfaces (user file, session,
//! layout), lowest precedence first. [`merge_keymaps`] folds them per key:
//! a higher-precedence layer's entry on a key replaces a lower layer's on the same key,
//! and every other key is untouched. The result splits each mode into two
//! maps, and a keypress checks the user map before the default map:
//!
//! - **`user_bindings_by_key_sequence`** — the winning user-authored entries, each tagged with
//!   the layer that authored it.
//! - **`default_bindings_by_key_sequence`** — the surviving built-in entries: shipped defaults
//!   whose key no user surface took or removed.
//!
//! Merging and detection read one shared firing predicate in the conflict
//! module: a binding the resolver refuses, or one a keypress cannot reach,
//! is transparent — it wins no key, and the firing binding beneath it shows
//! through. A `remove` in a higher layer voids lower layers' entries on that
//! key outright.
//!
//! Merge runs only on a keymap detection has already approved: every
//! layer on [`KeymapVerdict::Apply`](crate::conflict::KeymapVerdict::Apply),
//! or the defaults alone after
//! [`RevertToDefaults`](crate::conflict::KeymapVerdict::RevertToDefaults).
//! Merge checks neither the unlock guarantee nor cross-layer collisions;
//! detection does both. Merging is pure and re-runs on every config load or
//! reload.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::registry::ActionRegistry;

use crate::conflict::{
    build_removal_layer_index, is_bound_action_firing, is_removed_by_higher_layer,
    list_builtin_mode_names, FiringRules, KeymapLayer, LayerOrigin,
};
use crate::types::{BoundAction, KeybindingsConfig, ModeName};

/// One merged binding: what fires on the key, plus the layer that authored
/// it. `koshi keys describe` reports that layer as the binding's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedBinding {
    /// The action and preset arguments the key triggers.
    pub bound_action: BoundAction,
    /// The user-authored surface the winning entry came from.
    pub layer_origin: LayerOrigin,
}

/// One mode's merged lookup tables plus its removal and displacement
/// records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedModeMap {
    /// The winning user-authored binding per key.
    pub user_bindings_by_key_sequence: BTreeMap<KeySequence, MergedBinding>,
    /// The surviving built-in binding per key: firing shipped defaults no
    /// user surface took or removed.
    pub default_bindings_by_key_sequence: BTreeMap<KeySequence, BoundAction>,
    /// Every key any layer removes in this mode, whether or not a lower
    /// layer held it.
    pub removed_key_sequences: BTreeSet<KeySequence>,
    /// Built-in bindings the user displaced: their key is taken by a
    /// `user_bindings_by_key_sequence` entry or cleared by a remove. `koshi
    /// keys list` shows each one with its default action, marked unbound.
    pub unbound_default_bindings_by_key_sequence: BTreeMap<KeySequence, BoundAction>,
}

/// The merged keymap: one [`MergedModeMap`] per registered mode any layer
/// names, whether or not that mode's block holds an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedKeymap {
    /// Per-mode merged tables.
    pub mode_map_by_name: BTreeMap<ModeName, MergedModeMap>,
}

/// Folds keybinding layers (ordered lowest precedence first) into the
/// per-mode lookup tables.
///
/// `registry` is the action table each binding resolves against: a binding
/// the registry refuses does not fire. `maximum_chord_depth` is the longest
/// sequence, in chords, that fires. A binding whose mode is not a
/// [`LockMode`](koshi_core::lock::LockMode) name is skipped. The reserved
/// unlock chord is `unlock_alternative` when set, otherwise
/// [`KeybindingsConfig::RESERVED_UNLOCK`].
///
/// Per key, the highest firing entry wins. A firing user-authored entry on a
/// defaulted key takes it, and the displaced default moves to
/// [`unbound_default_bindings_by_key_sequence`](MergedModeMap::unbound_default_bindings_by_key_sequence);
/// a remove above the defaults layer does the same. A dead binding enters no
/// map. A binding is dead when the registry refuses it, when it is a
/// locked-mode sequence that holds the reserved unlock chord, or when it is
/// longer than `maximum_chord_depth`. A dead user entry leaves the default
/// beneath it in `default_bindings_by_key_sequence`. A dead default is in
/// neither `default_bindings_by_key_sequence` nor
/// [`unbound_default_bindings_by_key_sequence`](MergedModeMap::unbound_default_bindings_by_key_sequence).
#[must_use]
pub fn merge_keymaps(
    layers: &[KeymapLayer],
    unlock_alternative: Option<KeyChord>,
    maximum_chord_depth: u8,
    registry: &ActionRegistry,
) -> MergedKeymap {
    let known_mode_names = &list_builtin_mode_names();
    let reserved_unlock_chord = unlock_alternative.unwrap_or(KeybindingsConfig::RESERVED_UNLOCK);
    let locked_mode_name = ModeName::from_text("locked");
    let removal_layer_index_by_mode_and_key = build_removal_layer_index(layers, known_mode_names);
    let firing_rules = FiringRules {
        registry,
        reserved_unlock_chord,
        locked_mode_name: &locked_mode_name,
        maximum_chord_depth,
    };

    let mut merged_mode_map_by_name: BTreeMap<ModeName, MergedModeMap> = BTreeMap::new();

    for (layer_index, layer) in layers.iter().enumerate() {
        for (mode_name, mode_bindings) in &layer.mode_bindings_by_name {
            if !known_mode_names.contains(mode_name) {
                continue;
            }
            let merged_mode_map = merged_mode_map_by_name
                .entry(mode_name.clone())
                .or_default();

            merged_mode_map
                .removed_key_sequences
                .extend(mode_bindings.removed_key_sequences.iter().cloned());

            for (key_sequence, bound_action) in &mode_bindings.bound_action_by_key_sequence {
                if !is_bound_action_firing(mode_name, key_sequence, bound_action, &firing_rules) {
                    continue;
                }
                if is_removed_by_higher_layer(
                    &removal_layer_index_by_mode_and_key,
                    mode_name,
                    key_sequence,
                    layer_index,
                ) {
                    // A removed default lands in
                    // `unbound_default_bindings_by_key_sequence`; a removed
                    // user entry enters no map at all.
                    if !layer.origin.is_user_authored() {
                        merged_mode_map
                            .unbound_default_bindings_by_key_sequence
                            .insert(key_sequence.clone(), bound_action.clone());
                    }
                    continue;
                }
                if layer.origin.is_user_authored() {
                    merged_mode_map.user_bindings_by_key_sequence.insert(
                        key_sequence.clone(),
                        MergedBinding {
                            bound_action: bound_action.clone(),
                            layer_origin: layer.origin,
                        },
                    );
                } else {
                    merged_mode_map
                        .default_bindings_by_key_sequence
                        .insert(key_sequence.clone(), bound_action.clone());
                }
            }
        }
    }

    for merged_mode_map in merged_mode_map_by_name.values_mut() {
        for key_sequence in merged_mode_map.user_bindings_by_key_sequence.keys() {
            if let Some(bound_action) = merged_mode_map
                .default_bindings_by_key_sequence
                .remove(key_sequence)
            {
                merged_mode_map
                    .unbound_default_bindings_by_key_sequence
                    .insert(key_sequence.clone(), bound_action);
            }
        }
    }

    MergedKeymap {
        mode_map_by_name: merged_mode_map_by_name,
    }
}

#[cfg(test)]
mod tests;
