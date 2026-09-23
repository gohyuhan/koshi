//! Tests for keymap merging: per-key higher-precedence-wins folding, the user-set vs
//! surviving-defaults split, default steal and removal bookkeeping, dead
//! bindings staying transparent, the reserved-chord reachability rule, and
//! which modes reach the merged map.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::action::ActionReference;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::ActionArgs;

use super::*;

fn build_character_chord(modifier_flags: ModFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

fn build_single_chord_sequence(modifier_flags: ModFlags, key_character: char) -> KeySequence {
    KeySequence::from(build_character_chord(modifier_flags, key_character))
}

fn build_two_chord_sequence(first_chord: KeyChord, second_chord: KeyChord) -> KeySequence {
    KeySequence::from_first_and_rest(first_chord, vec![second_chord])
}

fn build_core_action(action_name: &str) -> ActionReference {
    ActionReference::from_core_action_name(action_name)
        .expect("test action name satisfies the grammar")
}

fn build_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: build_core_action(action_name),
        action_arguments: ActionArgs::None,
    }
}

fn parse_mode_name(mode_name: &str) -> ModeName {
    ModeName::from_text(mode_name)
}

/// A one-mode layer built from `(sequence, bound action)` entries plus the
/// keys it removes.
fn build_key_map_layer_with_removed(
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
fn build_key_map_layer(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
) -> KeymapLayer {
    build_key_map_layer_with_removed(origin, mode_name, binding_entries, Vec::new())
}

/// The built-in default bindings as the lowest layer.
fn build_default_key_map_layer() -> KeymapLayer {
    KeymapLayer {
        origin: LayerOrigin::Defaults,
        mode_bindings_by_name: KeybindingsConfig::default().mode_bindings_by_name,
    }
}

/// The chord-depth cap the tests run under, matching the shipped default.
const TEST_CHORD_DEPTH: u8 = 4;

/// Merges with no unlock alternative and the seeded core registry.
fn merge_test_keymaps(layers: &[KeymapLayer]) -> MergedKeyMap {
    merge_keymaps(layers, None, TEST_CHORD_DEPTH, &ActionRegistry::new())
}

/// The `<A-f>` → `core:toggle-pane-fullscreen` shipped default, a
/// single-chord default a user layer can steal or remove whole.
fn build_default_fullscreen_key_sequence() -> KeySequence {
    build_single_chord_sequence(ModFlags::ALT, 'f')
}

#[test]
fn no_layers_yield_an_empty_merged_map() {
    assert_eq!(merge_test_keymaps(&[]), MergedKeyMap::default());
}

#[test]
fn a_built_in_mode_no_layer_binds_is_absent_from_the_merged_map() {
    // A built-in mode never seeds an entry of its own. The shipped defaults
    // bind `normal`, `locked`, and `move-pane`; `resize` gets no entry.
    let merged = merge_keymaps(
        &[build_default_key_map_layer()],
        None,
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        merged.mode_map_by_name.keys().cloned().collect::<Vec<_>>(),
        vec![
            parse_mode_name("locked"),
            parse_mode_name("move-pane"),
            parse_mode_name("normal"),
        ]
    );
}

#[test]
fn a_mode_a_layer_names_with_no_entries_still_reaches_the_merged_map() {
    // A `mode "normal" { }` block binds and removes nothing. The mode still
    // gets an entry, and that entry is empty.
    let merged =
        merge_test_keymaps(&[build_key_map_layer(LayerOrigin::User, "normal", Vec::new())]);

    assert_eq!(
        merged.mode_map_by_name.keys().cloned().collect::<Vec<_>>(),
        vec![parse_mode_name("normal")]
    );
    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")],
        MergedModeMap::default()
    );
}

#[test]
fn a_zero_chord_depth_cap_leaves_every_map_empty() {
    // Every sequence holds at least one chord. A cap of zero admits none:
    // the mode entries exist and hold nothing.
    let merged = merge_keymaps(
        &[build_default_key_map_layer()],
        None,
        0,
        &ActionRegistry::new(),
    );
    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")],
        MergedModeMap::default()
    );
    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("locked")],
        MergedModeMap::default()
    );
}

#[test]
fn defaults_alone_fill_the_defaults_map_and_nothing_else() {
    let merged = merge_test_keymaps(&[build_default_key_map_layer()]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    // All 23 shipped normal-mode defaults fire in this build.
    assert_eq!(normal.default_bindings_by_key_sequence.len(), 23);
    assert_eq!(
        normal.default_bindings_by_key_sequence[&build_default_fullscreen_key_sequence()],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(normal.user_bindings_by_key_sequence, BTreeMap::new());
    assert_eq!(normal.removed_key_sequences, BTreeSet::new());
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );

    let locked = &merged.mode_map_by_name[&parse_mode_name("locked")];
    assert_eq!(
        locked.default_bindings_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
    assert_eq!(
        locked.default_bindings_by_key_sequence[&build_single_chord_sequence(ModFlags::CTRL, 'q')],
        build_bound_action("quit")
    );
    assert_eq!(
        locked.default_bindings_by_key_sequence[&KeySequence::from_first_and_rest(
            KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')),
            vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('m'))],
        )],
        build_bound_action("move-pane")
    );
    assert_eq!(
        locked.default_bindings_by_key_sequence[&build_single_chord_sequence(ModFlags::CTRL, 'g')],
        build_bound_action("mouse-select")
    );
    assert_eq!(locked.default_bindings_by_key_sequence.len(), 4);
}

#[test]
fn dead_default_is_absent_not_unbound() {
    // `core:copy-selection` is ComingSoon: the resolver refuses it. A
    // defaults-layer binding to it enters neither `defaults` nor
    // `unbound_default_bindings_by_key_sequence`.
    let dead_key = build_single_chord_sequence(ModFlags::ALT, 'c');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(dead_key.clone(), build_bound_action("copy-selection"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(normal.default_bindings_by_key_sequence.get(&dead_key), None);
    assert_eq!(
        normal
            .unbound_default_bindings_by_key_sequence
            .get(&dead_key),
        None
    );
}

#[test]
fn user_binding_on_a_fresh_key_adds_without_touching_defaults() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'w');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(normal.default_bindings_by_key_sequence.len(), 23);
    assert_eq!(
        normal.default_bindings_by_key_sequence[&build_default_fullscreen_key_sequence()],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(normal.removed_key_sequences, BTreeSet::new());
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn a_layout_layer_is_user_authored_and_carries_its_own_attribution() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'w');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Layout,
        }
    );
    assert_eq!(normal.default_bindings_by_key_sequence.len(), 23);
}

#[test]
fn one_key_bound_in_two_modes_merges_independently() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'w');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), build_bound_action("quit"))],
        ),
    ]);

    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")].user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("locked")].user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("quit"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn user_binding_steals_a_defaulted_key() {
    let key = build_default_fullscreen_key_sequence();
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(normal.default_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence[&key],
        build_bound_action("toggle-pane-fullscreen")
    );
    // Sibling defaults untouched.
    assert_eq!(normal.default_bindings_by_key_sequence.len(), 22);
    assert_eq!(
        normal.default_bindings_by_key_sequence[&build_single_chord_sequence(ModFlags::CTRL, 'l')],
        build_bound_action("lock")
    );
}

#[test]
fn higher_precedence_user_layer_wins_the_key_and_its_attribution() {
    // Post-verdict, two user-authored claims on one key hold the identical
    // bound action; the higher-precedence layer's entry wins, so attribution names it.
    let key = build_single_chord_sequence(ModFlags::ALT, 'w');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Session,
        }
    );
}

#[test]
fn remove_clears_a_default_and_records_both_sides() {
    let key = build_default_fullscreen_key_sequence();
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(normal.default_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence[&key],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(normal.removed_key_sequences, BTreeSet::from([key]));
    assert_eq!(normal.user_bindings_by_key_sequence, BTreeMap::new());
}

#[test]
fn remove_then_rebind_moves_a_key_between_user_layers() {
    // The supported way to re-key: the session layer removes the user
    // layer's key and rebinds it itself.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    // The same-layer rebind survives its own remove; the user entry is
    // voided.
    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Session,
        }
    );
    assert_eq!(normal.removed_key_sequences, BTreeSet::from([key]));
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn remove_below_does_not_void_a_higher_binding() {
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key.clone()],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::Session,
        }
    );
    assert_eq!(normal.removed_key_sequences, BTreeSet::from([key]));
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn remove_and_rebind_of_a_defaulted_key_in_one_layer_records_both_sides() {
    // `<A-f>` is a shipped default. One user layer clears it and takes it:
    // the user entry wins the key, and the displaced default surfaces as
    // unbound.
    let key = build_default_fullscreen_key_sequence();
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(normal.default_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence[&key],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(normal.removed_key_sequences, BTreeSet::from([key]));
}

#[test]
fn removed_keys_accumulate_across_layers() {
    let from_user = build_single_chord_sequence(ModFlags::ALT, 'x');
    let from_session = build_single_chord_sequence(ModFlags::ALT, 'y');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![from_user.clone()],
        ),
        build_key_map_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![from_session.clone()],
        ),
    ]);

    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")].removed_key_sequences,
        BTreeSet::from([from_user, from_session])
    );
}

#[test]
fn a_removal_from_the_defaults_layer_is_recorded_too() {
    // `removed_key_sequences` collects from every layer, not just the user-authored
    // ones.
    let key = build_single_chord_sequence(ModFlags::ALT, 'x');
    let merged = merge_test_keymaps(&[build_key_map_layer_with_removed(
        LayerOrigin::Defaults,
        "normal",
        Vec::new(),
        vec![key.clone()],
    )]);

    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")].removed_key_sequences,
        BTreeSet::from([key])
    );
}

#[test]
fn a_removal_in_an_unregistered_mode_is_skipped() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'x');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(LayerOrigin::User, "git", Vec::new(), vec![key]),
    ]);

    assert_eq!(merged.mode_map_by_name.get(&parse_mode_name("git")), None);
    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")].removed_key_sequences,
        BTreeSet::new()
    );
}

#[test]
fn remove_of_an_unheld_key_is_recorded_and_nothing_more() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'x');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(normal.removed_key_sequences, BTreeSet::from([key]));
    assert_eq!(normal.default_bindings_by_key_sequence.len(), 23);
    assert_eq!(normal.user_bindings_by_key_sequence, BTreeMap::new());
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn removed_user_binding_vanishes_silently() {
    // A user entry a higher layer removes is absent from every map, and
    // nothing lands in `unbound_default_bindings_by_key_sequence`.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(normal.user_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(normal.removed_key_sequences, BTreeSet::from([key]));
}

#[test]
fn dead_user_binding_leaves_the_default_beneath_live() {
    // An orphan user binding (unregistered action) is transparent: it
    // steals nothing, and the shipped default keeps firing.
    let key = build_default_fullscreen_key_sequence();
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("does-not-exist"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(normal.user_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        normal.default_bindings_by_key_sequence[&key],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn a_dead_user_binding_above_a_live_one_leaves_the_lower_layer_winning() {
    // The session layer names an unregistered action on a key the user layer
    // already took. The dead entry claims nothing, so attribution stays with
    // the user layer.
    let key = build_single_chord_sequence(ModFlags::ALT, 'w');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("does-not-exist"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn a_higher_precedence_defaults_layer_replaces_a_lower_defaults_entry() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'w');
    let merged = merge_test_keymaps(&[
        build_key_map_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.default_bindings_by_key_sequence[&key],
        build_bound_action("lock")
    );
    assert_eq!(normal.default_bindings_by_key_sequence.len(), 1);
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn one_layer_binding_two_modes_keeps_only_the_registered_one() {
    let registered = build_single_chord_sequence(ModFlags::ALT, 'w');
    let unregistered = build_single_chord_sequence(ModFlags::ALT, 'g');
    let mixed = KeymapLayer {
        origin: LayerOrigin::User,
        mode_bindings_by_name: BTreeMap::from([
            (
                parse_mode_name("normal"),
                crate::types::ModeBindings {
                    bound_action_by_key_sequence: BTreeMap::from([(
                        registered.clone(),
                        build_bound_action("lock"),
                    )]),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
            (
                parse_mode_name("git"),
                crate::types::ModeBindings {
                    bound_action_by_key_sequence: BTreeMap::from([(
                        unregistered,
                        build_bound_action("lock"),
                    )]),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
        ]),
    };
    let merged = merge_test_keymaps(&[mixed]);

    assert_eq!(
        merged.mode_map_by_name.keys().cloned().collect::<Vec<_>>(),
        vec![parse_mode_name("normal")]
    );
    assert_eq!(
        merged.mode_map_by_name[&parse_mode_name("normal")].user_bindings_by_key_sequence
            [&registered],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn reserved_unlock_locked_sequence_is_transparent() {
    // In locked mode the reserved chord resolves instantly, so a longer
    // sequence opening with it can never fire and wins no key.
    let key = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::from_parts(ModFlags::NONE, Key::Char('x')),
    );
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let locked = &merged.mode_map_by_name[&parse_mode_name("locked")];

    assert_eq!(locked.user_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        locked.default_bindings_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
}

#[test]
fn a_locked_sequence_holding_the_reserved_unlock_subsequently_is_transparent_too() {
    // `<C-x> <C-l>` does not open with the reserved chord, but the unlock
    // resolves wherever in the sequence it is pressed. The sequence never
    // fires, so it wins no key.
    let key = build_two_chord_sequence(
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('x')),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    let locked = &merged.mode_map_by_name[&parse_mode_name("locked")];

    assert_eq!(locked.user_bindings_by_key_sequence.get(&key), None);
    assert_eq!(locked.default_bindings_by_key_sequence.get(&key), None);
    assert_eq!(
        locked.default_bindings_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
}

#[test]
fn a_reserved_unlock_sequence_outside_locked_mode_fires() {
    // The reserved-chord rule is locked mode only. In `normal` the same
    // two-chord sequence is an ordinary binding.
    let key = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::from_parts(ModFlags::NONE, Key::Char('x')),
    );
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn unlock_alternative_moves_the_reserved_chord() {
    // With an alternative declared, IT is the reserved chord: sequences it
    // opens are dead in locked mode, and the default `<C-g>` chord is an
    // ordinary key again.
    let alternative = KeyChord::from_parts(ModFlags::CTRL, Key::Char('u'));
    let dead = build_two_chord_sequence(
        alternative,
        KeyChord::from_parts(ModFlags::NONE, Key::Char('x')),
    );
    let live = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::from_parts(ModFlags::NONE, Key::Char('x')),
    );
    let merged = merge_keymaps(
        &[
            build_default_key_map_layer(),
            build_key_map_layer(
                LayerOrigin::User,
                "locked",
                vec![
                    (dead.clone(), build_bound_action("lock")),
                    (live.clone(), build_bound_action("lock")),
                ],
            ),
        ],
        Some(alternative),
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    let locked = &merged.mode_map_by_name[&parse_mode_name("locked")];

    assert_eq!(locked.user_bindings_by_key_sequence.get(&dead), None);
    assert_eq!(
        locked.user_bindings_by_key_sequence[&live],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
}

#[test]
fn unregistered_mode_is_skipped() {
    let key = build_single_chord_sequence(ModFlags::ALT, 'g');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "git",
            vec![(key, build_bound_action("lock"))],
        ),
    ]);

    assert_eq!(merged.mode_map_by_name.get(&parse_mode_name("git")), None);
    assert_eq!(
        merged.mode_map_by_name.keys().cloned().collect::<Vec<_>>(),
        vec![
            parse_mode_name("locked"),
            parse_mode_name("move-pane"),
            parse_mode_name("normal"),
        ]
    );
}

#[test]
fn sequences_merge_per_key_like_single_chords() {
    // `<C-p> x` is the shipped tree-close; the user takes exactly that
    // sequence, and the sibling `<C-p> n` default survives.
    let close = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'p'),
        KeyChord::from_parts(ModFlags::NONE, Key::Char('x')),
    );
    let new_pane = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'p'),
        KeyChord::from_parts(ModFlags::NONE, Key::Char('n')),
    );
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(close.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&close],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence[&close],
        build_bound_action("close-pane-tree")
    );
    assert_eq!(
        normal.default_bindings_by_key_sequence[&new_pane],
        build_bound_action("new-pane")
    );
}

#[test]
fn named_key_defaults_survive_untouched() {
    let merged = merge_test_keymaps(&[build_default_key_map_layer()]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.default_bindings_by_key_sequence[&KeySequence::from_first_and_rest(
            KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')),
            vec![KeyChord::from_parts(
                ModFlags::NONE,
                Key::Named(NamedKey::Left)
            )],
        )],
        build_bound_action("focus-pane-left")
    );
}

#[test]
fn stealing_a_dead_defaults_key_unbinds_nothing() {
    // A defaults-layer key bound to the dead `core:copy-selection`; a user
    // binding takes the key. The dead default was never firing, so nothing
    // was displaced: `unbound_default_bindings_by_key_sequence` stays empty.
    let key = build_single_chord_sequence(ModFlags::ALT, 'c');
    let merged = merge_test_keymaps(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), build_bound_action("copy-selection"))],
        ),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(
        normal.user_bindings_by_key_sequence[&key],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}

#[test]
fn binding_past_the_chord_depth_cap_is_transparent() {
    // At a cap of 1 a two-chord entry enters no map — a dead user entry
    // leaves the defaulted key untouched, and a dead default is absent, not
    // displaced — while one-chord entries merge as usual.
    let long_default = build_two_chord_sequence(
        KeyChord::from_parts(ModFlags::ALT, Key::Char('p')),
        KeyChord::from_parts(ModFlags::NONE, Key::Char('q')),
    );
    let short_default = build_single_chord_sequence(ModFlags::ALT, 't');
    let long_user = build_two_chord_sequence(
        KeyChord::from_parts(ModFlags::CTRL, Key::Char('y')),
        KeyChord::from_parts(ModFlags::NONE, Key::Char('x')),
    );
    let short_user = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let merged = merge_keymaps(
        &[
            build_key_map_layer(
                LayerOrigin::Defaults,
                "normal",
                vec![
                    (long_default.clone(), build_bound_action("new-pane")),
                    (short_default.clone(), build_bound_action("new-tab")),
                ],
            ),
            build_key_map_layer(
                LayerOrigin::User,
                "normal",
                vec![
                    (long_user.clone(), build_bound_action("lock")),
                    (short_user.clone(), build_bound_action("lock")),
                ],
            ),
        ],
        None,
        1,
        &ActionRegistry::new(),
    );
    let normal = &merged.mode_map_by_name[&parse_mode_name("normal")];

    assert_eq!(normal.user_bindings_by_key_sequence.get(&long_user), None);
    assert_eq!(
        normal.user_bindings_by_key_sequence[&short_user],
        MergedBinding {
            bound_action: build_bound_action("lock"),
            layer_origin: LayerOrigin::User,
        }
    );
    assert_eq!(
        normal.default_bindings_by_key_sequence.get(&long_default),
        None
    );
    assert_eq!(
        normal.default_bindings_by_key_sequence[&short_default],
        build_bound_action("new-tab")
    );
    // The dead default is absent by build state, never displaced.
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence,
        BTreeMap::new()
    );
}
