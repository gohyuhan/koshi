//! The keymap hint catalog: one resolved lookup table serving both the hint
//! bar and keyboard resolution.
//!
//! [`KeymapHintCatalog::from_keymap_layers_config_and_registry`] builds the
//! catalog at startup from the keybinding layers and the action table: it
//! folds the layers with [`merge_keymaps`], joins every surviving binding to
//! its action's display name from the [`ActionRegistry`], and files the
//! result per mode behind [`Arc`]s, in each label form a frame shows.
//! [`KeymapHintCatalog::build_frame_hints`] then hands one frame's data out as
//! `Arc` clones, and
//! [`KeymapHintCatalog::match_sequence`] answers one pending key sequence from
//! the same folded map.
//!
//! [`HintBinding`] and [`KeymapHints`] describe the keymap; the renderer
//! re-exports both.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

use crate::conflict::{build_keymap_layers, KeymapLayer};
use crate::key::Leader;
use crate::keymap_merge::{merge_keymaps, MergedKeymap, MergedModeKeymap};
use crate::types::{build_default_prefix_labels, BoundAction, KeybindingsConfig, ModeName};
use koshi_core::action::{
    ActionReference, MOUSE_SELECT_HINT, MOUSE_UNSELECT_HINT, MOVE_FLOATING_PANE_HINT,
    MOVE_FOCUSED_PANE_HINT, MOVE_TILED_PANE_HINT,
};
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
    pub hint_bindings: Arc<Vec<HintBinding>>,
    /// Display labels for prefix chords whose sequence group is untouched
    /// defaults (`<C-p>` → `PANE`). A group with any user-authored entry, or
    /// a user removal under it, ignores this and shows a `+N` marker instead.
    pub prefix_labels: Arc<BTreeMap<KeyChord, String>>,
    /// Every key a user surface removed in the current mode. A removal under
    /// a labeled prefix voids that label.
    pub removed_key_sequences: Arc<BTreeSet<KeySequence>>,
}

/// One binding the hint bar can show: a key sequence, the display name of the
/// action it fires, and the flags the bar's grouping and ordering read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintBinding {
    /// The chords pressed to fire the binding.
    pub key_sequence: KeySequence,
    /// The bound action's human-facing name, from its registry metadata.
    pub action_display_name: String,
    /// Whether a user surface authored the winning entry (a default shows
    /// `false`). Any `true` entry under a prefix voids the prefix's label.
    pub is_user_authored: bool,
    /// Whether the hint sorts ahead of the unpinned hints in its own modifier
    /// group — set on every locked-mode entry firing `core:unlock`.
    pub is_pinned: bool,
}

/// Per-mode hint-bar data: every mode's bindings joined to display names,
/// shared by reference with each frame's snapshot.
///
/// A clone copies the two per-mode maps. The merged keymap, each binding
/// list, each removal set, and the prefix labels travel behind [`Arc`]s.
#[derive(Clone)]
pub struct KeymapHintCatalog {
    /// Liveness-filtered lookup table shared by hints and keyboard resolution.
    merged_keymap: Arc<MergedKeymap>,
    /// Multi-chord wait before an incomplete prefix falls through.
    chord_timeout_duration: Duration,
    /// The chord that unlocks a locked client, ahead of every other lookup.
    unlock_chord: KeyChord,
    /// One sorted binding list per built-in mode in each label form; a mode
    /// nothing binds in holds empty lists.
    hint_bindings_by_mode_name: BTreeMap<ModeName, ModeHintBindings>,
    /// Per-mode keys a user surface removed; empty until user layers load.
    removed_key_sequences_by_mode_name: BTreeMap<ModeName, Arc<BTreeSet<KeySequence>>>,
    /// Display labels for the default table's prefix chords.
    prefix_labels: Arc<BTreeMap<KeyChord, String>>,
}

impl KeymapHintCatalog {
    /// Resolve the hint catalog from the built-in default bindings and the
    /// live action table.
    pub fn from_registry(registry: &ActionRegistry) -> Self {
        Self::from_keymap_layers_config_and_registry(
            &build_keymap_layers(None, Leader::default()),
            &KeybindingsConfig::default(),
            registry,
        )
    }

    /// Resolve the hint catalog from `keymap_layers` and the effective
    /// keybinding configuration. Reads `chord_timeout_ms`,
    /// `unlock_alternative`, `maximum_chord_depth` and `leader`;
    /// `mode_bindings_by_name` and `which_key_delay_ms` are not read, and
    /// `keymap_layers` carries the bindings.
    ///
    /// Folds the layers with [`merge_keymaps`]: a binding that does not fire
    /// yields no hint — its action unregistered, its arguments unresolvable, a
    /// locked-mode sequence of two or more chords holding the unlock chord, or
    /// a sequence longer than `maximum_chord_depth`. In locked mode every entry
    /// firing `core:unlock` is flagged pinned; the hint bar sorts pinned hints
    /// before unpinned ones in the same modifier group.
    pub fn from_keymap_layers_config_and_registry(
        keymap_layers: &[KeymapLayer],
        keybindings_config: &KeybindingsConfig,
        registry: &ActionRegistry,
    ) -> Self {
        let chord_timeout_duration =
            Duration::from_millis(u64::from(keybindings_config.chord_timeout_ms));
        let unlock_chord =
            KeybindingsConfig::resolve_unlock_chord(keybindings_config.unlock_alternative);
        let merged_keymap = merge_keymaps(
            keymap_layers,
            keybindings_config.unlock_alternative,
            keybindings_config.maximum_chord_depth,
            registry,
        );

        let unlock_action_reference = ActionReference::from_core_action_name("unlock")
            .expect("the reserved unlock action name satisfies the action-name grammar");
        let empty_merged_mode_keymap = MergedModeKeymap::default();

        let mut hint_bindings_by_mode_name = BTreeMap::new();
        let mut removed_key_sequences_by_mode_name = BTreeMap::new();
        for lock_mode in LockMode::ALL {
            let mode_name = ModeName::from_text(lock_mode.get_keymap_name());
            let merged_mode_keymap = merged_keymap
                .mode_keymap_by_name
                .get(&mode_name)
                .unwrap_or(&empty_merged_mode_keymap);
            let hint_bindings = Arc::new(build_mode_hint_bindings(
                merged_mode_keymap,
                registry,
                lock_mode,
                &unlock_action_reference,
            ));
            let tiled_pane_hint_bindings =
                relabel_hint_bindings(&hint_bindings, MOVE_FOCUSED_PANE_HINT, MOVE_TILED_PANE_HINT);
            let floating_pane_hint_bindings = relabel_hint_bindings(
                &hint_bindings,
                MOVE_FOCUSED_PANE_HINT,
                MOVE_FLOATING_PANE_HINT,
            );
            hint_bindings_by_mode_name.insert(
                mode_name.clone(),
                ModeHintBindings {
                    mouse_selection_tiled_pane_hint_bindings: relabel_hint_bindings(
                        &tiled_pane_hint_bindings,
                        MOUSE_SELECT_HINT,
                        MOUSE_UNSELECT_HINT,
                    ),
                    mouse_selection_floating_pane_hint_bindings: relabel_hint_bindings(
                        &floating_pane_hint_bindings,
                        MOUSE_SELECT_HINT,
                        MOUSE_UNSELECT_HINT,
                    ),
                    tiled_pane_hint_bindings,
                    floating_pane_hint_bindings,
                },
            );
            removed_key_sequences_by_mode_name.insert(
                mode_name,
                Arc::new(merged_mode_keymap.removed_key_sequences.clone()),
            );
        }

        KeymapHintCatalog {
            merged_keymap: Arc::new(merged_keymap),
            chord_timeout_duration,
            unlock_chord,
            hint_bindings_by_mode_name,
            removed_key_sequences_by_mode_name,
            prefix_labels: Arc::new(build_default_prefix_labels(keybindings_config.leader)),
        }
    }

    /// Resolve one pending sequence in a built-in mode.
    ///
    /// [`KeyMatch::exact_bound_action`] holds the binding `key_sequence` fires,
    /// the user-authored entry ahead of the surviving default.
    /// [`KeyMatch::has_longer_key_sequence`] is true when some binding in the
    /// mode is longer than `key_sequence` and opens with it. A mode with no
    /// bindings answers `KeyMatch::default()`: `exact_bound_action` is `None`
    /// and `has_longer_key_sequence` is false.
    pub fn match_sequence(&self, lock_mode: LockMode, key_sequence: &KeySequence) -> KeyMatch {
        let Some(merged_mode_keymap) = self
            .merged_keymap
            .mode_keymap_by_name
            .get(lock_mode.get_keymap_name())
        else {
            return KeyMatch::default();
        };
        let exact_bound_action = merged_mode_keymap
            .user_bindings_by_key_sequence
            .get(key_sequence)
            .map(|merged_binding| merged_binding.bound_action.clone())
            .or_else(|| {
                merged_mode_keymap
                    .default_bindings_by_key_sequence
                    .get(key_sequence)
                    .cloned()
            });
        let has_longer_key_sequence = has_longer_key_sequence_starting_with(
            &merged_mode_keymap.user_bindings_by_key_sequence,
            key_sequence,
        ) || has_longer_key_sequence_starting_with(
            &merged_mode_keymap.default_bindings_by_key_sequence,
            key_sequence,
        );
        KeyMatch {
            exact_bound_action,
            has_longer_key_sequence,
        }
    }

    /// How long an ambiguous sequence — one that both fires and opens a
    /// longer binding — waits for its next chord, from `chord_timeout_ms`.
    pub fn get_chord_timeout(&self) -> Duration {
        self.chord_timeout_duration
    }

    /// The chord that unlocks a locked client: the configured
    /// `unlock_alternative` when the user named one, else the reserved
    /// `<C-l>`. Conflict detection refuses a config whose locked mode does
    /// not fire `core:unlock` from this chord.
    pub fn get_unlock_chord(&self) -> KeyChord {
        self.unlock_chord
    }

    /// The hint-bar data one frame is painted from: the bindings and removals
    /// of `lock_mode` shared by reference, plus the prefix labels.
    ///
    /// Every entry labelled [`MOUSE_SELECT_HINT`] reads [`MOUSE_UNSELECT_HINT`]
    /// while `is_mouse_selection_enabled` is `true`. Every entry labelled
    /// [`MOVE_FOCUSED_PANE_HINT`] reads [`MOVE_FLOATING_PANE_HINT`] while
    /// `is_focused_pane_floating` is `true`, and [`MOVE_TILED_PANE_HINT`]
    /// otherwise. Matching is on the label, so a rebound or duplicated binding
    /// is relabelled too: `Mouse Select` becomes `Mouse Unselect` on every key
    /// bound to `core:mouse-select`.
    pub fn build_frame_hints(
        &self,
        lock_mode: LockMode,
        is_mouse_selection_enabled: bool,
        is_focused_pane_floating: bool,
    ) -> KeymapHints {
        let mode_name = lock_mode.get_keymap_name();
        KeymapHints {
            hint_bindings: self
                .hint_bindings_by_mode_name
                .get(mode_name)
                .map(|mode_hint_bindings| {
                    let hint_bindings = match (is_mouse_selection_enabled, is_focused_pane_floating)
                    {
                        (false, false) => &mode_hint_bindings.tiled_pane_hint_bindings,
                        (false, true) => &mode_hint_bindings.floating_pane_hint_bindings,
                        (true, false) => {
                            &mode_hint_bindings.mouse_selection_tiled_pane_hint_bindings
                        }
                        (true, true) => {
                            &mode_hint_bindings.mouse_selection_floating_pane_hint_bindings
                        }
                    };
                    Arc::clone(hint_bindings)
                })
                .unwrap_or_default(),
            prefix_labels: Arc::clone(&self.prefix_labels),
            removed_key_sequences: self
                .removed_key_sequences_by_mode_name
                .get(mode_name)
                .map(Arc::clone)
                .unwrap_or_default(),
        }
    }
}

/// One built-in mode's sorted binding list in each label form that
/// [`KeymapHintCatalog::build_frame_hints`] hands out. Two forms with the same
/// labels share one list.
#[derive(Clone)]
struct ModeHintBindings {
    /// `Mouse Select` and `Move Tiled Pane` labels.
    tiled_pane_hint_bindings: Arc<Vec<HintBinding>>,
    /// `Mouse Select` and `Move Floating Pane` labels.
    floating_pane_hint_bindings: Arc<Vec<HintBinding>>,
    /// `Mouse Unselect` and `Move Tiled Pane` labels.
    mouse_selection_tiled_pane_hint_bindings: Arc<Vec<HintBinding>>,
    /// `Mouse Unselect` and `Move Floating Pane` labels.
    mouse_selection_floating_pane_hint_bindings: Arc<Vec<HintBinding>>,
}

/// `hint_bindings` with every entry labelled `current_label` relabelled
/// `replacement_label`, and nothing else changed. With no entry labelled
/// `current_label`, it returns `hint_bindings` itself, the same [`Arc`].
fn relabel_hint_bindings(
    hint_bindings: &Arc<Vec<HintBinding>>,
    current_label: &str,
    replacement_label: &str,
) -> Arc<Vec<HintBinding>> {
    let has_current_label = hint_bindings
        .iter()
        .any(|hint_binding| hint_binding.action_display_name == current_label);
    if !has_current_label {
        return Arc::clone(hint_bindings);
    }
    let relabelled_hint_bindings: Vec<HintBinding> = hint_bindings
        .iter()
        .map(|hint_binding| {
            let mut hint_binding = hint_binding.clone();
            if hint_binding.action_display_name == current_label {
                replacement_label.clone_into(&mut hint_binding.action_display_name);
            }
            hint_binding
        })
        .collect();
    Arc::new(relabelled_hint_bindings)
}

/// Exact and longer-prefix results for one sequence lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyMatch {
    /// The binding the sequence fires, or `None` when nothing binds it.
    pub exact_bound_action: Option<BoundAction>,
    /// True when a longer binding in the same mode opens with the sequence.
    pub has_longer_key_sequence: bool,
}

/// True when `bindings_by_key_sequence` holds a key longer than `key_sequence`
/// that opens with it.
///
/// Reads only the first key after `key_sequence` in sort order: keys sort
/// lexicographically by chord, and every longer key opening with
/// `key_sequence` sorts directly after it.
fn has_longer_key_sequence_starting_with<Binding>(
    bindings_by_key_sequence: &BTreeMap<KeySequence, Binding>,
    key_sequence: &KeySequence,
) -> bool {
    bindings_by_key_sequence
        .range((Bound::Excluded(key_sequence), Bound::Unbounded))
        .next()
        .is_some_and(|(candidate_key_sequence, _)| {
            candidate_key_sequence
                .list_chords()
                .starts_with(key_sequence.list_chords())
        })
}

/// One mode's merged bindings joined to display names, sorted by sequence.
///
/// Walks the mode's user-authored entries and surviving defaults — the merge
/// leaves no key in both — reads each action's display name from the
/// registry, and flags every locked-mode binding firing `unlock` pinned.
fn build_mode_hint_bindings(
    merged_mode_keymap: &MergedModeKeymap,
    registry: &ActionRegistry,
    lock_mode: LockMode,
    unlock_action_reference: &ActionReference,
) -> Vec<HintBinding> {
    let user_bindings = merged_mode_keymap
        .user_bindings_by_key_sequence
        .iter()
        .map(|(key_sequence, merged_binding)| (key_sequence, &merged_binding.bound_action, true));
    let default_bindings = merged_mode_keymap
        .default_bindings_by_key_sequence
        .iter()
        .map(|(key_sequence, bound_action)| (key_sequence, bound_action, false));

    let mut hint_bindings: Vec<HintBinding> = user_bindings
        .chain(default_bindings)
        .map(|(key_sequence, bound_action, is_user_authored)| {
            let action_display_name = registry
                .find_action_metadata(&bound_action.action_reference)
                // `merge_keymaps` admits only bindings whose action resolves
                // in this same registry.
                .expect("a merged binding's action is registered")
                .display_name
                .clone();
            HintBinding {
                key_sequence: key_sequence.clone(),
                action_display_name,
                is_user_authored,
                is_pinned: lock_mode == LockMode::Locked
                    && bound_action.action_reference == *unlock_action_reference,
            }
        })
        .collect();
    hint_bindings
        .sort_by(|left_hint, right_hint| left_hint.key_sequence.cmp(&right_hint.key_sequence));
    hint_bindings
}

#[cfg(test)]
mod tests;
