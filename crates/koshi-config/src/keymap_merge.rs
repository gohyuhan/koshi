//! Per-scope keymap merging: folds the ordered keymap layers into the
//! per-mode lookup tables a keypress consults.
//!
//! Bindings arrive in the same layers conflict detection reads — the
//! built-in defaults, then the user's own surfaces (user file, session,
//! layout), lowest precedence first. [`merge_keymaps`] folds them per key: a later layer's entry on a
//! key replaces a lower layer's on the same key, and every other key is
//! untouched. The result splits each mode into two maps because the two
//! resolve at different tiers of the key-resolution stack — sticky plugin
//! layers sit between them:
//!
//! - **`user_set`** — the winning user-authored entries, each tagged with
//!   the layer that authored it.
//! - **`defaults`** — the surviving built-in entries: shipped defaults
//!   whose key no user surface took or removed.
//!
//! Merging honors the same firing model as detection (one shared predicate
//! inside the conflict module, so the two can never disagree): a binding the resolver
//! refuses, or one a keypress cannot reach, is transparent — it wins no
//! key, and the firing binding beneath it shows through. A `remove` in a
//! higher layer voids lower layers' entries on that key outright.
//!
//! Merge runs only on a keymap detection has already verdicted: every
//! layer on [`KeymapVerdict::Apply`](crate::conflict::KeymapVerdict::Apply),
//! or the defaults alone after
//! [`RevertToDefaults`](crate::conflict::KeymapVerdict::RevertToDefaults).
//! The unlock guarantee and collision policing are detection's concerns;
//! merge trusts the verdict. Like detection, merging is pure and re-runs
//! whenever the layers or the action registry change (config reload, plugin
//! load or unload) — the layers stay the source of truth, so a binding that
//! turns live re-enters the merged map on the next run.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::registry::ActionRegistry;

use crate::conflict::{is_firing, removal_index, removed_above, KeyMapLayer, LayerOrigin};
use crate::types::{BoundAction, KeybindingsConfig, ModeName};

/// One merged binding: what fires on the key plus the layer that authored
/// it, kept for source attribution (`koshi keys describe`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedBinding {
    /// The action and preset arguments the key triggers.
    pub bound: BoundAction,
    /// The user-authored surface the winning entry came from.
    pub source: LayerOrigin,
}

/// One mode's merged lookup tables plus the bookkeeping diagnostics read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedModeMap {
    /// The winning user-authored binding per key. Resolves above sticky
    /// plugin layers in the key-resolution stack.
    pub user_set: BTreeMap<KeySequence, MergedBinding>,
    /// The surviving built-in binding per key: firing shipped defaults no
    /// user surface took or removed. Resolves below sticky plugin layers.
    pub defaults: BTreeMap<KeySequence, BoundAction>,
    /// Every key a user surface removed in this mode, whether or not a
    /// lower layer held it.
    pub removed_keys: BTreeSet<KeySequence>,
    /// Built-in bindings displaced by the user — their key stolen by a
    /// `user_set` entry or cleared by a remove. Kept so `koshi keys list`
    /// can show the default action as unbound.
    pub unbound_defaults: BTreeMap<KeySequence, BoundAction>,
}

/// The merged keymap: one [`MergedModeMap`] per registered mode any layer
/// binds or removes keys in.
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
/// must fit; `known_modes` holds every registered mode name — a layer's
/// bindings in an unregistered mode are skipped, matching detection. The
/// reserved unlock chord is `unlock_alternative` when set, otherwise
/// [`KeybindingsConfig::RESERVED_UNLOCK`].
///
/// Per key, the highest firing entry wins. A firing user-authored entry on
/// a defaulted key takes it and the displaced default moves to
/// [`unbound_defaults`](MergedModeMap::unbound_defaults); a remove above
/// the defaults layer does the same. A dead binding (resolver-refused,
/// swallowed by the locked-mode reserved-chord bypass, or longer than the
/// chord-depth cap) enters no map: a
/// dead user entry leaves the default beneath it live, and a dead default
/// is simply absent — dead by build state, not displaced by the user.
#[must_use]
pub fn merge_keymaps(
    layers: &[KeyMapLayer],
    unlock_alternative: Option<KeyChord>,
    max_chord_depth: u8,
    registry: &ActionRegistry,
    known_modes: &BTreeSet<ModeName>,
) -> MergedKeyMap {
    let reserved = unlock_alternative.unwrap_or(KeybindingsConfig::RESERVED_UNLOCK);
    let locked = ModeName::new("locked");
    let removals = removal_index(layers, known_modes);

    let mut modes: BTreeMap<ModeName, MergedModeMap> = BTreeMap::new();

    for (index, layer) in layers.iter().enumerate() {
        for (mode, bindings) in &layer.modes {
            if !known_modes.contains(mode) {
                continue;
            }
            let merged = modes.entry(mode.clone()).or_default();

            for key in &bindings.removed {
                merged.removed_keys.insert(key.clone());
            }

            for (key, bound) in &bindings.keys {
                if !is_firing(
                    mode,
                    key,
                    bound,
                    registry,
                    reserved,
                    &locked,
                    max_chord_depth,
                ) {
                    continue;
                }
                if removed_above(&removals, mode, key, index) {
                    // A removed default was live until the user cleared it,
                    // so it surfaces as unbound; a removed user entry is
                    // the user's own authored intent and vanishes silently.
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
        let stolen: Vec<KeySequence> = merged
            .defaults
            .keys()
            .filter(|key| merged.user_set.contains_key(*key))
            .cloned()
            .collect();
        for key in stolen {
            if let Some(bound) = merged.defaults.remove(&key) {
                merged.unbound_defaults.insert(key, bound);
            }
        }
    }

    MergedKeyMap { modes }
}

#[cfg(test)]
mod tests;
