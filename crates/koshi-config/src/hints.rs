//! The keymap hint catalog: one resolved lookup table serving both the hint
//! bar and keyboard resolution.
//!
//! [`KeymapHintCatalog::from_parts`] builds the catalog at startup from the
//! keybinding layers and the action table: it folds the layers with
//! [`merge_keymaps`], joins every surviving binding to its action's display
//! name from the [`ActionRegistry`], and files the result per mode behind
//! [`Arc`]s. [`KeymapHintCatalog::hints_for`] then hands one mode's data out
//! as `Arc` clones, and [`KeymapHintCatalog::match_sequence`] answers one
//! pending key sequence from the same folded map.
//!
//! [`HintBinding`] and [`KeymapHints`] describe the keymap; the renderer
//! re-exports both.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::conflict::{built_in_modes, keymap_layers, KeyMapLayer};
use crate::key::Leader;
use crate::keymap_merge::{merge_keymaps, MergedKeyMap, MergedModeMap};
use crate::types::{default_prefix_labels, BoundAction, KeybindingsConfig, ModeName};
use koshi_core::action::ActionRef;
use koshi_core::key::{KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;

/// The keybinding data behind the hint bar, projected for one client's
/// current input mode.
///
/// Everything is plain data: the merged keymap's bindings for the mode, each
/// already joined to its action's display name. The per-mode collections
/// travel behind [`Arc`]s — [`KeymapHintCatalog`] computes them once per
/// keymap change, and every frame shares them by reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapHints {
    /// Every binding in the client's current mode, sorted by key sequence.
    pub entries: Arc<Vec<HintBinding>>,
    /// Display labels for prefix chords whose sequence group is untouched
    /// defaults (`<C-p>` → `PANE`). A group with any user-authored entry, or
    /// a user removal under it, ignores this and shows a `+N` marker instead.
    pub prefix_labels: Arc<BTreeMap<KeyChord, String>>,
    /// Every key a user surface removed in the current mode. A removal under
    /// a labeled prefix voids the label: the shipped name no longer describes
    /// the group.
    pub removed: Arc<BTreeSet<KeySequence>>,
    /// True when the user keymap was reverted to defaults over a key
    /// collision: the bar shows a conflict marker, and the hints listed are
    /// the reverted-to defaults.
    pub reverted: bool,
}

/// One binding the hint bar can show: a key sequence, the display name of the
/// action it fires, and the flags the bar's grouping and ordering read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintBinding {
    /// The chords pressed to fire the binding.
    pub sequence: KeySequence,
    /// The bound action's human-facing name, from its registry metadata.
    pub label: String,
    /// Whether a user surface authored the winning entry (a default shows
    /// `false`). Any `true` entry under a prefix voids the prefix's label.
    pub user_set: bool,
    /// Whether the hint sorts ahead of the unpinned hints in its own modifier
    /// group — set on every locked-mode entry firing `core:unlock`.
    pub pinned: bool,
}

/// Per-mode hint-bar data: every mode's bindings joined to display names,
/// shared by reference with each frame's snapshot.
///
/// Cloning is cheap — every collection travels behind an [`Arc`].
#[derive(Clone)]
pub struct KeymapHintCatalog {
    /// Liveness-filtered lookup table shared by hints and keyboard resolution.
    merged: Arc<MergedKeyMap>,
    /// Multi-chord wait before an incomplete prefix falls through.
    chord_timeout: Duration,
    /// The chord that unlocks a locked client, ahead of every other lookup.
    unlock_chord: KeyChord,
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
    pub fn from_registry(registry: &ActionRegistry) -> Self {
        Self::from_parts(
            &keymap_layers(None, Leader::default()),
            &KeybindingsConfig::default(),
            registry,
        )
    }

    /// Resolve the hint catalog from `layers` and the effective keybinding
    /// config, whose timing fields and unlock alternative carry into lookups.
    ///
    /// Folds the layers with [`merge_keymaps`]: a binding whose action the
    /// resolver refuses (unregistered, or not yet implemented) yields no
    /// hint. In locked mode every entry firing `core:unlock` is flagged
    /// pinned; the hint bar sorts pinned hints before unpinned ones in the
    /// same modifier group.
    pub fn from_parts(
        layers: &[KeyMapLayer],
        config: &KeybindingsConfig,
        registry: &ActionRegistry,
    ) -> Self {
        let chord_timeout = Duration::from_millis(u64::from(config.chord_timeout_ms));
        let unlock_chord = config
            .unlock_alternative
            .unwrap_or(KeybindingsConfig::RESERVED_UNLOCK);
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
            unlock_chord,
            entries,
            removed,
            prefix_labels: Arc::new(default_prefix_labels(config.leader)),
            reverted: false,
        }
    }

    /// Resolve one pending sequence in a built-in mode.
    pub fn match_sequence(&self, mode: LockMode, sequence: &KeySequence) -> KeyMatch {
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

    pub fn chord_timeout(&self) -> Duration {
        self.chord_timeout
    }

    /// The chord that unlocks a locked client: the configured
    /// `unlock_alternative` when the user named one, else the reserved
    /// `<C-l>`. Conflict detection refuses a config whose locked mode does
    /// not fire `core:unlock` from it, so this chord always escapes.
    pub fn unlock_chord(&self) -> KeyChord {
        self.unlock_chord
    }

    /// The hint-bar data for one client's current mode: the mode's bindings
    /// and removals shared by reference, plus the labels and the revert flag.
    pub fn hints_for(&self, mode: LockMode) -> KeymapHints {
        let name = ModeName::new(mode.name());
        KeymapHints {
            entries: self.entries.get(&name).map(Arc::clone).unwrap_or_default(),
            prefix_labels: Arc::clone(&self.prefix_labels),
            removed: self.removed.get(&name).map(Arc::clone).unwrap_or_default(),
            reverted: self.reverted,
        }
    }
}

/// Exact and longer-prefix results for one sequence lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyMatch {
    pub exact: Option<BoundAction>,
    pub prefix: bool,
}

/// One mode's merged bindings joined to display names, sorted by sequence.
///
/// Walks the mode's user-set entries and surviving defaults (steal already
/// resolved by the merge, so the two never hold the same key), reads each
/// action's display name from the registry, and flags every locked-mode
/// binding firing `unlock` pinned.
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
