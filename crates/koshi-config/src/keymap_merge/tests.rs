//! Tests for keymap merging: the higher-precedence layer wins each key, user
//! bindings and surviving defaults sit in separate maps, a default that a user
//! binding takes or removes is recorded as unbound, a binding to an
//! unregistered action changes nothing, a locked-mode sequence that holds the
//! unlock chord enters no map, and only registered modes reach the merged
//! keymap.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey};
use koshi_core::registry::ActionRegistry;

use super::*;
use crate::types::tests::build_bound_action;
use crate::types::ModeBindings;

/// The chord of `key_character` with `modifier_flags` held.
pub(crate) fn build_character_chord(
    modifier_flags: BindingModifierFlags,
    key_character: char,
) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

/// The one-chord sequence of `key_character` with `modifier_flags` held.
pub(crate) fn build_single_chord_sequence(
    modifier_flags: BindingModifierFlags,
    key_character: char,
) -> KeySequence {
    KeySequence::from(build_character_chord(modifier_flags, key_character))
}

/// The sequence `first_chord` then `second_chord`.
pub(crate) fn build_two_chord_sequence(
    first_chord: KeyChord,
    second_chord: KeyChord,
) -> KeySequence {
    KeySequence::from_first_and_rest(first_chord, vec![second_chord])
}

/// A one-mode layer built from `(sequence, bound action)` entries plus the
/// keys it removes.
pub(crate) fn build_keymap_layer_with_removed_key_sequences(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
    removed_key_sequences: Vec<KeySequence>,
) -> KeymapLayer {
    KeymapLayer {
        origin,
        mode_bindings_by_name: BTreeMap::from([(
            ModeName::from_text(mode_name),
            ModeBindings {
                bound_action_by_key_sequence: binding_entries.into_iter().collect(),
                removed_key_sequences: removed_key_sequences.into_iter().collect(),
            },
        )]),
    }
}

/// A one-mode layer built from `(sequence, bound action)` binding entries.
pub(crate) fn build_keymap_layer(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
) -> KeymapLayer {
    build_keymap_layer_with_removed_key_sequences(origin, mode_name, binding_entries, Vec::new())
}

/// The built-in default bindings as the lowest layer.
pub(crate) fn build_default_keymap_layer() -> KeymapLayer {
    KeymapLayer {
        origin: LayerOrigin::Defaults,
        mode_bindings_by_name: KeybindingsConfig::default().mode_bindings_by_name,
    }
}

/// The shipped default bindings of `mode_name`, keyed by sequence.
fn list_default_bound_actions(mode_name: &str) -> BTreeMap<KeySequence, BoundAction> {
    KeybindingsConfig::default().mode_bindings_by_name[&ModeName::from_text(mode_name)]
        .bound_action_by_key_sequence
        .clone()
}

/// The chord-depth cap the tests run under: 4, the shipped default.
pub(crate) const TEST_MAXIMUM_CHORD_DEPTH: u8 = 4;

/// Merges `keymap_layers` with no unlock alternative, the cap
/// `TEST_MAXIMUM_CHORD_DEPTH`, and the core action registry.
fn merge_test_keymaps(keymap_layers: &[KeymapLayer]) -> MergedKeymap {
    merge_keymaps(
        keymap_layers,
        None,
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    )
}

/// The one-chord shipped default `<A-f>` → `core:toggle-pane-fullscreen`.
fn build_default_fullscreen_key_sequence() -> KeySequence {
    build_single_chord_sequence(BindingModifierFlags::ALT, 'f')
}

#[test]
fn no_layers_yield_an_empty_merged_keymap() {
    assert_eq!(merge_test_keymaps(&[]), MergedKeymap::default());
}

#[test]
fn a_built_in_mode_no_layer_binds_is_absent_from_the_merged_keymap() {
    // A built-in mode never seeds an entry of its own. The shipped defaults
    // bind `normal`, `locked`, `pane-placement`, `pane-move-choice`,
    // `floating-pane-move`, `floating-pane-tray` and `notification-center`;
    // `resize` gets no entry.
    let merged_keymap = merge_test_keymaps(&[build_default_keymap_layer()]);
    assert_eq!(
        merged_keymap
            .mode_keymap_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            ModeName::from_text("floating-pane-move"),
            ModeName::from_text("floating-pane-tray"),
            ModeName::from_text("locked"),
            ModeName::from_text("normal"),
            ModeName::from_text("notification-center"),
            ModeName::from_text("pane-move-choice"),
            ModeName::from_text("pane-placement"),
        ]
    );
}

#[test]
fn a_mode_a_layer_names_with_no_entries_still_reaches_the_merged_keymap() {
    // A `mode "normal" { }` block binds and removes nothing. The mode still
    // gets an entry, and that entry is empty.
    let merged_keymap =
        merge_test_keymaps(&[build_keymap_layer(LayerOrigin::User, "normal", Vec::new())]);

    assert_eq!(
        merged_keymap
            .mode_keymap_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![ModeName::from_text("normal")]
    );
    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")],
        MergedModeKeymap::default()
    );
}

#[test]
fn a_zero_chord_depth_cap_leaves_every_map_empty() {
    // Every sequence holds at least one chord. A cap of zero admits none:
    // the mode entries exist and hold nothing.
    let merged_keymap = merge_keymaps(
        &[build_default_keymap_layer()],
        None,
        0,
        &ActionRegistry::new(),
    );
    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")],
        MergedModeKeymap::default()
    );
    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("locked")],
        MergedModeKeymap::default()
    );
}

#[test]
fn defaults_alone_fill_the_defaults_map_and_nothing_else() {
    let merged_keymap = merge_test_keymaps(&[build_default_keymap_layer()]);

    // Every shipped default of every mode fires in this build.
    for mode_name in KeybindingsConfig::default().mode_bindings_by_name.keys() {
        assert_eq!(
            merged_keymap.mode_keymap_by_name[mode_name],
            MergedModeKeymap {
                default_bindings_by_key_sequence: list_default_bound_actions(mode_name.get_name()),
                ..MergedModeKeymap::default()
            },
            "{}",
            mode_name.get_name()
        );
    }
}

#[test]
fn dead_default_is_absent_not_unbound() {
    // `core:copy-selection` is not a registered action. A defaults-layer
    // binding to it enters neither `default_bindings_by_key_sequence` nor
    // `unbound_default_bindings_by_key_sequence`.
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(
                build_single_chord_sequence(BindingModifierFlags::ALT, 'c'),
                build_bound_action("copy-selection"),
            )],
        ),
    ]);

    assert_eq!(
        merged_keymap,
        merge_test_keymaps(&[build_default_keymap_layer()])
    );
}

#[test]
fn user_binding_on_a_fresh_key_adds_without_touching_defaults() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence,
        list_default_bound_actions("normal")
    );
    assert_eq!(normal_mode_keymap.removed_key_sequences, BTreeSet::new());
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn one_key_bound_in_two_modes_merges_independently() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("quit"))],
        ),
    ]);

    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")]
            .user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("locked")]
            .user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("quit"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn user_binding_steals_a_defaulted_key() {
    let key_sequence = build_default_fullscreen_key_sequence();
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence[&key_sequence],
        UnboundDefaultBinding {
            bound_action: build_bound_action("toggle-pane-fullscreen"),
            default_displacement: DefaultDisplacement::Rebound {
                layer_origin: LayerOrigin::User,
            },
        }
    );
    // The other defaults stay.
    let mut surviving_default_bound_actions = list_default_bound_actions("normal");
    surviving_default_bound_actions.remove(&key_sequence);
    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence,
        surviving_default_bound_actions
    );
}

#[test]
fn remove_clears_a_default_and_records_both_sides() {
    let key_sequence = build_default_fullscreen_key_sequence();
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence[&key_sequence],
        UnboundDefaultBinding {
            bound_action: build_bound_action("toggle-pane-fullscreen"),
            default_displacement: DefaultDisplacement::Removed {
                layer_origin: LayerOrigin::User,
            },
        }
    );
    assert_eq!(
        normal_mode_keymap.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn remove_below_does_not_void_a_higher_binding() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn remove_and_rebind_of_a_defaulted_key_in_one_layer_records_both_sides() {
    // `<A-f>` is a shipped default. One user layer clears it and takes it:
    // the user entry wins the key, and the displaced default surfaces as
    // unbound, displaced by the `remove`.
    let key_sequence = build_default_fullscreen_key_sequence();
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
            vec![key_sequence.clone()],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence[&key_sequence],
        UnboundDefaultBinding {
            bound_action: build_bound_action("toggle-pane-fullscreen"),
            default_displacement: DefaultDisplacement::Removed {
                layer_origin: LayerOrigin::User,
            },
        }
    );
    assert_eq!(
        normal_mode_keymap.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
}

#[test]
fn removed_keys_accumulate_across_layers() {
    let lower_removed_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'x');
    let higher_removed_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'y');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![lower_removed_key_sequence.clone()],
        ),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![higher_removed_key_sequence.clone()],
        ),
    ]);

    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")].removed_key_sequences,
        BTreeSet::from([lower_removed_key_sequence, higher_removed_key_sequence])
    );
}

#[test]
fn a_removal_from_the_defaults_layer_is_recorded_too() {
    // `removed_key_sequences` collects from every layer, not just the user-authored
    // ones.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'x');
    let merged_keymap = merge_test_keymaps(&[build_keymap_layer_with_removed_key_sequences(
        LayerOrigin::Defaults,
        "normal",
        Vec::new(),
        vec![key_sequence.clone()],
    )]);

    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")].removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
}

#[test]
fn a_removal_in_an_unregistered_mode_is_skipped() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'x');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "git",
            Vec::new(),
            vec![key_sequence],
        ),
    ]);

    assert_eq!(
        merged_keymap
            .mode_keymap_by_name
            .get(&ModeName::from_text("git")),
        None
    );
    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")].removed_key_sequences,
        BTreeSet::new()
    );
}

#[test]
fn remove_of_an_unheld_key_is_recorded_and_nothing_more() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'x');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence,
        list_default_bound_actions("normal")
    );
    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn removed_user_binding_vanishes_silently() {
    // A user entry a higher layer removes is absent from every map, and
    // nothing lands in `unbound_default_bindings_by_key_sequence`.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(
        normal_mode_keymap.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
}

#[test]
fn dead_user_binding_leaves_the_default_beneath_live() {
    // A user binding to an unregistered action changes nothing: the shipped
    // default stays in `default_bindings_by_key_sequence`.
    let key_sequence = build_default_fullscreen_key_sequence();
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("does-not-exist"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn a_dead_user_binding_above_a_live_one_leaves_the_lower_layer_winning() {
    // The higher user layer names an unregistered action on a key the lower
    // user layer already took. The dead entry changes nothing: the lower
    // layer's `core:lock` keeps the key.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("does-not-exist"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn a_higher_precedence_defaults_layer_replaces_a_lower_defaults_entry() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_keymap_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("lock")
    );
    assert_eq!(normal_mode_keymap.default_bindings_by_key_sequence.len(), 1);
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn one_layer_binding_two_modes_keeps_only_the_registered_one() {
    let normal_mode_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let git_mode_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'g');
    let two_mode_keymap_layer = KeymapLayer {
        origin: LayerOrigin::User,
        mode_bindings_by_name: BTreeMap::from([
            (
                ModeName::from_text("normal"),
                ModeBindings {
                    bound_action_by_key_sequence: BTreeMap::from([(
                        normal_mode_key_sequence.clone(),
                        build_bound_action("lock"),
                    )]),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
            (
                ModeName::from_text("git"),
                ModeBindings {
                    bound_action_by_key_sequence: BTreeMap::from([(
                        git_mode_key_sequence,
                        build_bound_action("lock"),
                    )]),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
        ]),
    };
    let merged_keymap = merge_test_keymaps(&[two_mode_keymap_layer]);

    assert_eq!(
        merged_keymap
            .mode_keymap_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![ModeName::from_text("normal")]
    );
    assert_eq!(
        merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")]
            .user_bindings_by_key_sequence[&normal_mode_key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn reserved_unlock_locked_sequence_is_transparent() {
    // In locked mode the reserved chord resolves at once. A longer sequence
    // that opens with it does not fire and enters no map.
    let key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let locked_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("locked")];

    assert_eq!(
        locked_mode_keymap
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        locked_mode_keymap.default_bindings_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
}

#[test]
fn a_locked_sequence_holding_the_reserved_unlock_subsequently_is_transparent_too() {
    // `<C-x> <C-l>` does not open with the reserved chord. The unlock chord
    // resolves at any position in the sequence: the sequence does not fire and
    // enters no map.
    let key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    let locked_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("locked")];

    assert_eq!(
        locked_mode_keymap
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        locked_mode_keymap
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        locked_mode_keymap.default_bindings_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
}

#[test]
fn a_reserved_unlock_sequence_outside_locked_mode_fires() {
    // The reserved-chord rule is locked mode only. In `normal` the same
    // two-chord sequence is an ordinary binding.
    let key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn unlock_alternative_moves_the_reserved_chord() {
    // A declared alternative `<C-u>` is the reserved chord. In locked mode a
    // sequence it opens enters no map, and a sequence opened by the default
    // `<C-l>` is an ordinary binding.
    let unlock_alternative_chord = build_character_chord(BindingModifierFlags::CTRL, 'u');
    let dead_key_sequence = build_two_chord_sequence(
        unlock_alternative_chord,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let live_key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let merged_keymap = merge_keymaps(
        &[
            build_default_keymap_layer(),
            build_keymap_layer(
                LayerOrigin::User,
                "locked",
                vec![
                    (dead_key_sequence.clone(), build_bound_action("lock")),
                    (live_key_sequence.clone(), build_bound_action("lock")),
                ],
            ),
        ],
        Some(unlock_alternative_chord),
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    let locked_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("locked")];

    assert_eq!(
        locked_mode_keymap
            .user_bindings_by_key_sequence
            .get(&dead_key_sequence),
        None
    );
    assert_eq!(
        locked_mode_keymap.user_bindings_by_key_sequence[&live_key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn unregistered_mode_is_skipped() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'g');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "git",
            vec![(key_sequence, build_bound_action("lock"))],
        ),
    ]);

    assert_eq!(
        merged_keymap
            .mode_keymap_by_name
            .get(&ModeName::from_text("git")),
        None
    );
    assert_eq!(
        merged_keymap
            .mode_keymap_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            ModeName::from_text("floating-pane-move"),
            ModeName::from_text("floating-pane-tray"),
            ModeName::from_text("locked"),
            ModeName::from_text("normal"),
            ModeName::from_text("notification-center"),
            ModeName::from_text("pane-move-choice"),
            ModeName::from_text("pane-placement"),
        ]
    );
}

#[test]
fn sequences_merge_per_key_like_single_chords() {
    // `<C-p> x` is the shipped `core:close-pane-tree` default. The user binds
    // that sequence, and the `<C-p> n` default stays.
    let close_pane_tree_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'p'),
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let new_pane_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'p'),
        build_character_chord(BindingModifierFlags::NONE, 'n'),
    );
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(
                close_pane_tree_key_sequence.clone(),
                build_bound_action("lock"),
            )],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&close_pane_tree_key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence[&close_pane_tree_key_sequence],
        UnboundDefaultBinding {
            bound_action: build_bound_action("close-pane-tree"),
            default_displacement: DefaultDisplacement::Rebound {
                layer_origin: LayerOrigin::User,
            },
        }
    );
    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence[&new_pane_key_sequence],
        build_bound_action("new-pane")
    );
}

#[test]
fn named_key_defaults_survive_untouched() {
    let merged_keymap = merge_test_keymaps(&[build_default_keymap_layer()]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence[&build_two_chord_sequence(
            build_character_chord(BindingModifierFlags::CTRL, 'p'),
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Left))
        )],
        build_bound_action("focus-pane-left")
    );
}

#[test]
fn stealing_a_dead_defaults_key_unbinds_nothing() {
    // A defaults-layer key is bound to the unregistered `core:copy-selection`,
    // and a user binding takes the key. The dead default is in no map:
    // `unbound_default_bindings_by_key_sequence` stays empty.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'c');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("copy-selection"))],
        ),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn binding_past_the_chord_depth_cap_is_transparent() {
    // At a cap of 1 a two-chord entry enters no map, from the user layer or
    // the defaults layer. One-chord entries merge as usual.
    let long_default_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::ALT, 'p'),
        build_character_chord(BindingModifierFlags::NONE, 'q'),
    );
    let short_default_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 't');
    let long_user_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'y'),
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let short_user_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let merged_keymap = merge_keymaps(
        &[
            build_keymap_layer(
                LayerOrigin::Defaults,
                "normal",
                vec![
                    (
                        long_default_key_sequence.clone(),
                        build_bound_action("new-pane"),
                    ),
                    (
                        short_default_key_sequence.clone(),
                        build_bound_action("new-tab"),
                    ),
                ],
            ),
            build_keymap_layer(
                LayerOrigin::User,
                "normal",
                vec![
                    (long_user_key_sequence.clone(), build_bound_action("lock")),
                    (short_user_key_sequence.clone(), build_bound_action("lock")),
                ],
            ),
        ],
        None,
        1,
        &ActionRegistry::new(),
    );
    let normal_mode_keymap = &merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];

    assert_eq!(
        normal_mode_keymap
            .user_bindings_by_key_sequence
            .get(&long_user_key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.user_bindings_by_key_sequence[&short_user_key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_keymap
            .default_bindings_by_key_sequence
            .get(&long_default_key_sequence),
        None
    );
    assert_eq!(
        normal_mode_keymap.default_bindings_by_key_sequence[&short_default_key_sequence],
        build_bound_action("new-tab")
    );
    // The two-chord default is in no map.
    assert_eq!(
        normal_mode_keymap.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}
