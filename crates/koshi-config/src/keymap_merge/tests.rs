//! Tests for keymap merging: the higher-precedence layer wins each key, user
//! bindings and surviving defaults sit in separate maps, a default that a user
//! binding takes or removes is recorded as unbound, a binding to an
//! unregistered action changes nothing, a locked-mode sequence that holds the
//! unlock chord enters no map, and only registered modes reach the merged
//! keymap.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::action::ActionReference;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey};
use koshi_core::registry::ActionRegistry;

use super::*;

fn build_character_chord(modifier_flags: BindingModifierFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

fn build_single_chord_sequence(
    modifier_flags: BindingModifierFlags,
    key_character: char,
) -> KeySequence {
    KeySequence::from(build_character_chord(modifier_flags, key_character))
}

fn build_two_chord_sequence(first_chord: KeyChord, second_chord: KeyChord) -> KeySequence {
    KeySequence::from_first_and_rest(first_chord, vec![second_chord])
}

fn build_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("test action name satisfies the grammar"),
    }
}

fn parse_mode_name(mode_name: &str) -> ModeName {
    ModeName::from_text(mode_name)
}

/// A one-mode layer built from `(sequence, bound action)` entries plus the
/// keys it removes.
fn build_keymap_layer_with_removed_key_sequences(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
    removed_key_sequences: Vec<KeySequence>,
) -> KeymapLayer {
    KeymapLayer {
        origin,
        mode_bindings_by_name: BTreeMap::from([(
            parse_mode_name(mode_name),
            crate::types::ModeBindings {
                bound_action_by_key_sequence: binding_entries.into_iter().collect(),
                removed_key_sequences: removed_key_sequences.into_iter().collect(),
            },
        )]),
    }
}

/// A one-mode layer built from `(sequence, bound action)` binding entries.
fn build_keymap_layer(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
) -> KeymapLayer {
    build_keymap_layer_with_removed_key_sequences(origin, mode_name, binding_entries, Vec::new())
}

/// The built-in default bindings as the lowest layer.
fn build_default_keymap_layer() -> KeymapLayer {
    KeymapLayer {
        origin: LayerOrigin::Defaults,
        mode_bindings_by_name: KeybindingsConfig::default().mode_bindings_by_name,
    }
}

/// The chord-depth cap the tests merge under: 4, the shipped default.
const TEST_MAXIMUM_CHORD_DEPTH: u8 = 4;

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
fn no_layers_yield_an_empty_merged_map() {
    assert_eq!(merge_test_keymaps(&[]), MergedKeymap::default());
}

#[test]
fn a_built_in_mode_no_layer_binds_is_absent_from_the_merged_map() {
    // A built-in mode never seeds an entry of its own. The shipped defaults
    // bind `normal`, `locked`, and `pane-placement`; `resize` gets no entry.
    let merged_keymap = merge_keymaps(
        &[build_default_keymap_layer()],
        None,
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        merged_keymap
            .mode_map_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            parse_mode_name("locked"),
            parse_mode_name("normal"),
            parse_mode_name("pane-placement"),
        ]
    );
}

#[test]
fn a_mode_a_layer_names_with_no_entries_still_reaches_the_merged_map() {
    // A `mode "normal" { }` block binds and removes nothing. The mode still
    // gets an entry, and that entry is empty.
    let merged_keymap =
        merge_test_keymaps(&[build_keymap_layer(LayerOrigin::User, "normal", Vec::new())]);

    assert_eq!(
        merged_keymap
            .mode_map_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![parse_mode_name("normal")]
    );
    assert_eq!(
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")],
        MergedModeMap::default()
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
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")],
        MergedModeMap::default()
    );
    assert_eq!(
        merged_keymap.mode_map_by_name[&parse_mode_name("locked")],
        MergedModeMap::default()
    );
}

#[test]
fn defaults_alone_fill_the_defaults_map_and_nothing_else() {
    let merged_keymap = merge_test_keymaps(&[build_default_keymap_layer()]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    // All 24 shipped normal-mode defaults fire in this build.
    assert_eq!(normal_mode_map.default_bindings_by_key_sequence.len(), 24);
    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&build_default_fullscreen_key_sequence()],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(normal_mode_map.removed_key_sequences, BTreeSet::new());
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );

    let locked_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("locked")];
    assert_eq!(
        locked_mode_map.default_bindings_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
    assert_eq!(
        locked_mode_map.default_bindings_by_key_sequence
            [&build_single_chord_sequence(BindingModifierFlags::CTRL, 'q')],
        build_bound_action("quit")
    );
    assert_eq!(
        locked_mode_map.default_bindings_by_key_sequence[&KeySequence::from_first_and_rest(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
            vec![KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('m')
            )],
        )],
        build_bound_action("begin-pane-placement")
    );
    assert_eq!(
        locked_mode_map.default_bindings_by_key_sequence
            [&build_single_chord_sequence(BindingModifierFlags::CTRL, 'g')],
        build_bound_action("mouse-select")
    );
    assert_eq!(locked_mode_map.default_bindings_by_key_sequence.len(), 4);
}

#[test]
fn dead_default_is_absent_not_unbound() {
    // `core:copy-selection` is not a registered action. A defaults-layer
    // binding to it enters neither `default_bindings_by_key_sequence` nor
    // `unbound_default_bindings_by_key_sequence`.
    let unregistered_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'c');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(
                unregistered_key_sequence.clone(),
                build_bound_action("copy-selection"),
            )],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map
            .default_bindings_by_key_sequence
            .get(&unregistered_key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map
            .unbound_default_bindings_by_key_sequence
            .get(&unregistered_key_sequence),
        None
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(normal_mode_map.default_bindings_by_key_sequence.len(), 24);
    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&build_default_fullscreen_key_sequence()],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(normal_mode_map.removed_key_sequences, BTreeSet::new());
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn a_layout_layer_is_user_authored_and_carries_its_own_attribution() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Layout,
        }
    );
    assert_eq!(normal_mode_map.default_bindings_by_key_sequence.len(), 24);
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
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")].user_bindings_by_key_sequence
            [&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        merged_keymap.mode_map_by_name[&parse_mode_name("locked")].user_bindings_by_key_sequence
            [&key_sequence],
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_map
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("toggle-pane-fullscreen")
    );
    // The other defaults stay.
    assert_eq!(normal_mode_map.default_bindings_by_key_sequence.len(), 23);
    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence
            [&build_single_chord_sequence(BindingModifierFlags::CTRL, 'l')],
        build_bound_action("lock")
    );
}

#[test]
fn higher_precedence_user_layer_wins_the_key_and_its_attribution() {
    // Two user-authored layers bind one key to the same action. The
    // higher-precedence layer's entry wins, and `layer_origin` names it.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Session,
        }
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal_mode_map.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn remove_then_rebind_moves_a_key_between_user_layers() {
    // The session layer removes the user layer's key and binds the key
    // itself.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
            vec![key_sequence.clone()],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    // The session layer's bind survives its own remove. The user layer's
    // entry is dropped.
    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Session,
        }
    );
    assert_eq!(
        normal_mode_map.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
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
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Session,
        }
    );
    assert_eq!(
        normal_mode_map.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn remove_and_rebind_of_a_defaulted_key_in_one_layer_records_both_sides() {
    // `<A-f>` is a shipped default. One user layer clears it and takes it:
    // the user entry wins the key, and the displaced default surfaces as
    // unbound.
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_map
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal_mode_map.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
}

#[test]
fn removed_keys_accumulate_across_layers() {
    let user_removed_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'x');
    let session_removed_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'y');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![user_removed_key_sequence.clone()],
        ),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![session_removed_key_sequence.clone()],
        ),
    ]);

    assert_eq!(
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")].removed_key_sequences,
        BTreeSet::from([user_removed_key_sequence, session_removed_key_sequence])
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
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")].removed_key_sequences,
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
        merged_keymap.mode_map_by_name.get(&parse_mode_name("git")),
        None
    );
    assert_eq!(
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")].removed_key_sequences,
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.removed_key_sequences,
        BTreeSet::from([key_sequence])
    );
    assert_eq!(normal_mode_map.default_bindings_by_key_sequence.len(), 24);
    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
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
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(
        normal_mode_map.removed_key_sequences,
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn a_dead_user_binding_above_a_live_one_leaves_the_lower_layer_winning() {
    // The session layer names an unregistered action on a key the user layer
    // already took. The dead entry changes nothing: `layer_origin` stays
    // `User`.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'w');
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("does-not-exist"))],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&key_sequence],
        build_bound_action("lock")
    );
    assert_eq!(normal_mode_map.default_bindings_by_key_sequence.len(), 1);
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
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
                parse_mode_name("normal"),
                crate::types::ModeBindings {
                    bound_action_by_key_sequence: BTreeMap::from([(
                        normal_mode_key_sequence.clone(),
                        build_bound_action("lock"),
                    )]),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
            (
                parse_mode_name("git"),
                crate::types::ModeBindings {
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
            .mode_map_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![parse_mode_name("normal")]
    );
    assert_eq!(
        merged_keymap.mode_map_by_name[&parse_mode_name("normal")].user_bindings_by_key_sequence
            [&normal_mode_key_sequence],
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
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
    );
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let locked_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("locked")];

    assert_eq!(
        locked_mode_map
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        locked_mode_map.default_bindings_by_key_sequence
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
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('x')),
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
    let locked_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("locked")];

    assert_eq!(
        locked_mode_map
            .user_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        locked_mode_map
            .default_bindings_by_key_sequence
            .get(&key_sequence),
        None
    );
    assert_eq!(
        locked_mode_map.default_bindings_by_key_sequence
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
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
    );
    let merged_keymap = merge_test_keymaps(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn unlock_alternative_moves_the_reserved_chord() {
    // A declared alternative `<C-u>` is the reserved chord. In locked mode a
    // sequence it opens enters no map, and a sequence opened by the default
    // `<C-l>` is an ordinary binding.
    let unlock_alternative_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('u'));
    let dead_key_sequence = build_two_chord_sequence(
        unlock_alternative_chord,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
    );
    let live_key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
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
    let locked_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("locked")];

    assert_eq!(
        locked_mode_map
            .user_bindings_by_key_sequence
            .get(&dead_key_sequence),
        None
    );
    assert_eq!(
        locked_mode_map.user_bindings_by_key_sequence[&live_key_sequence],
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
        merged_keymap.mode_map_by_name.get(&parse_mode_name("git")),
        None
    );
    assert_eq!(
        merged_keymap
            .mode_map_by_name
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            parse_mode_name("locked"),
            parse_mode_name("normal"),
            parse_mode_name("pane-placement"),
        ]
    );
}

#[test]
fn sequences_merge_per_key_like_single_chords() {
    // `<C-p> x` is the shipped `core:close-pane-tree` default. The user binds
    // that sequence, and the `<C-p> n` default stays.
    let close_pane_tree_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'p'),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
    );
    let new_pane_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'p'),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('n')),
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&close_pane_tree_key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence[&close_pane_tree_key_sequence],
        build_bound_action("close-pane-tree")
    );
    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&new_pane_key_sequence],
        build_bound_action("new-pane")
    );
}

#[test]
fn named_key_defaults_survive_untouched() {
    let merged_keymap = merge_test_keymaps(&[build_default_keymap_layer()]);
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&KeySequence::from_first_and_rest(
            KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p')),
            vec![KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Named(NamedKey::Left)
            )],
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn binding_past_the_chord_depth_cap_is_transparent() {
    // At a cap of 1 a two-chord entry enters no map, from the user layer or
    // the defaults layer. One-chord entries merge as usual.
    let long_default_key_sequence = build_two_chord_sequence(
        KeyChord::from_parts(BindingModifierFlags::ALT, Key::Char('p')),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('q')),
    );
    let short_default_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 't');
    let long_user_key_sequence = build_two_chord_sequence(
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('y')),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
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
    let normal_mode_map = &merged_keymap.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal_mode_map
            .user_bindings_by_key_sequence
            .get(&long_user_key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.user_bindings_by_key_sequence[&short_user_key_sequence],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal_mode_map
            .default_bindings_by_key_sequence
            .get(&long_default_key_sequence),
        None
    );
    assert_eq!(
        normal_mode_map.default_bindings_by_key_sequence[&short_default_key_sequence],
        build_bound_action("new-tab")
    );
    // The two-chord default is in no map.
    assert_eq!(
        normal_mode_map.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}
