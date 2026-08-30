//! Tests for keymap merging: per-key later-wins folding, the user-set vs
//! surviving-defaults split, default steal and removal bookkeeping, dead
//! bindings staying transparent, the reserved-chord reachability rule, and
//! which modes reach the merged map.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::action::ActionRef;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::ActionArgs;

use super::*;

fn chord(mods: ModFlags, key: char) -> KeyChord {
    KeyChord::new(mods, Key::Char(key))
}

fn seq(mods: ModFlags, key: char) -> KeySequence {
    KeySequence::from(chord(mods, key))
}

fn seq2(first: KeyChord, second: KeyChord) -> KeySequence {
    KeySequence::new(first, vec![second])
}

fn core(name: &str) -> ActionRef {
    ActionRef::core(name).expect("test action name satisfies the grammar")
}

fn bound(name: &str) -> BoundAction {
    BoundAction {
        action: core(name),
        args: ActionArgs::None,
    }
}

fn mode(name: &str) -> ModeName {
    ModeName::new(name)
}

/// A one-mode layer built from `(sequence, bound action)` entries plus the
/// keys it removes.
fn layer_with_removed(
    origin: LayerOrigin,
    mode_name: &str,
    entries: Vec<(KeySequence, BoundAction)>,
    removed: Vec<KeySequence>,
) -> KeyMapLayer {
    KeyMapLayer {
        origin,
        modes: BTreeMap::from([(
            mode(mode_name),
            crate::types::ModeBindings {
                keys: entries.into_iter().collect(),
                removed: removed.into_iter().collect(),
            },
        )]),
    }
}

/// A one-mode layer built from `(sequence, bound action)` entries.
fn layer(
    origin: LayerOrigin,
    mode_name: &str,
    entries: Vec<(KeySequence, BoundAction)>,
) -> KeyMapLayer {
    layer_with_removed(origin, mode_name, entries, Vec::new())
}

/// The built-in default bindings as the lowest layer.
fn defaults() -> KeyMapLayer {
    KeyMapLayer {
        origin: LayerOrigin::Defaults,
        modes: KeybindingsConfig::default().modes,
    }
}

/// The chord-depth cap the tests run under, matching the shipped default.
const DEPTH: u8 = 4;

/// Merges with no unlock alternative and the seeded core registry.
fn merge(layers: &[KeyMapLayer]) -> MergedKeyMap {
    merge_keymaps(layers, None, DEPTH, &ActionRegistry::new())
}

/// The `<A-f>` → `core:toggle-pane-fullscreen` shipped default, a
/// single-chord default a user layer can steal or remove whole.
fn default_fullscreen_key() -> KeySequence {
    seq(ModFlags::ALT, 'f')
}

#[test]
fn no_layers_yield_an_empty_merged_map() {
    assert_eq!(merge(&[]), MergedKeyMap::default());
}

#[test]
fn a_built_in_mode_no_layer_binds_is_absent_from_the_merged_map() {
    // A built-in mode never seeds an entry of its own. The shipped defaults
    // bind `normal` and `locked` only, so `resize` gets no entry.
    let merged = merge_keymaps(&[defaults()], None, DEPTH, &ActionRegistry::new());
    assert_eq!(
        merged.modes.keys().cloned().collect::<Vec<_>>(),
        vec![mode("locked"), mode("normal")]
    );
}

#[test]
fn a_mode_a_layer_names_with_no_entries_still_reaches_the_merged_map() {
    // A `mode "normal" { }` block binds and removes nothing. The mode still
    // gets an entry, and that entry is empty.
    let merged = merge(&[layer(LayerOrigin::User, "normal", Vec::new())]);

    assert_eq!(
        merged.modes.keys().cloned().collect::<Vec<_>>(),
        vec![mode("normal")]
    );
    assert_eq!(merged.modes[&mode("normal")], MergedModeMap::default());
}

#[test]
fn a_zero_chord_depth_cap_leaves_every_map_empty() {
    // Every sequence holds at least one chord. A cap of zero admits none:
    // the mode entries exist and hold nothing.
    let merged = merge_keymaps(&[defaults()], None, 0, &ActionRegistry::new());
    assert_eq!(merged.modes[&mode("normal")], MergedModeMap::default());
    assert_eq!(merged.modes[&mode("locked")], MergedModeMap::default());
}

#[test]
fn defaults_alone_fill_the_defaults_map_and_nothing_else() {
    let merged = merge(&[defaults()]);
    let normal = &merged.modes[&mode("normal")];

    // All 22 shipped normal-mode defaults fire in this build.
    assert_eq!(normal.defaults.len(), 22);
    assert_eq!(
        normal.defaults[&default_fullscreen_key()],
        bound("toggle-pane-fullscreen")
    );
    assert_eq!(normal.user_set, BTreeMap::new());
    assert_eq!(normal.removed_keys, BTreeSet::new());
    assert_eq!(normal.unbound_defaults, BTreeMap::new());

    let locked = &merged.modes[&mode("locked")];
    assert_eq!(
        locked.defaults[&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        bound("unlock")
    );
    assert_eq!(locked.defaults[&seq(ModFlags::CTRL, 'q')], bound("quit"));
    assert_eq!(
        locked.defaults[&seq(ModFlags::CTRL, 'g')],
        bound("mouse-select")
    );
    assert_eq!(locked.defaults.len(), 3);
}

#[test]
fn dead_default_is_absent_not_unbound() {
    // `core:copy-selection` is ComingSoon: the resolver refuses it. A
    // defaults-layer binding to it enters neither `defaults` nor
    // `unbound_defaults`.
    let dead_key = seq(ModFlags::ALT, 'c');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(dead_key.clone(), bound("copy-selection"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.defaults.get(&dead_key), None);
    assert_eq!(normal.unbound_defaults.get(&dead_key), None);
}

#[test]
fn user_binding_on_a_fresh_key_adds_without_touching_defaults() {
    let key = seq(ModFlags::ALT, 'w');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.defaults.len(), 22);
    assert_eq!(
        normal.defaults[&default_fullscreen_key()],
        bound("toggle-pane-fullscreen")
    );
    assert_eq!(normal.removed_keys, BTreeSet::new());
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn a_layout_layer_is_user_authored_and_carries_its_own_attribution() {
    let key = seq(ModFlags::ALT, 'w');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::Layout,
        }
    );
    assert_eq!(normal.defaults.len(), 22);
}

#[test]
fn one_key_bound_in_two_modes_merges_independently() {
    let key = seq(ModFlags::ALT, 'w');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), bound("quit"))],
        ),
    ]);

    assert_eq!(
        merged.modes[&mode("normal")].user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(
        merged.modes[&mode("locked")].user_set[&key],
        MergedBinding {
            bound: bound("quit"),
            source: LayerOrigin::User,
        }
    );
}

#[test]
fn user_binding_steals_a_defaulted_key() {
    let key = default_fullscreen_key();
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.defaults.get(&key), None);
    assert_eq!(
        normal.unbound_defaults[&key],
        bound("toggle-pane-fullscreen")
    );
    // Sibling defaults untouched.
    assert_eq!(normal.defaults.len(), 21);
    assert_eq!(normal.defaults[&seq(ModFlags::CTRL, 'l')], bound("lock"));
}

#[test]
fn later_user_layer_wins_the_key_and_its_attribution() {
    // Post-verdict, two user-authored claims on one key hold the identical
    // bound action; the later layer's entry wins, so attribution names it.
    let key = seq(ModFlags::ALT, 'w');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::Session,
        }
    );
}

#[test]
fn remove_clears_a_default_and_records_both_sides() {
    let key = default_fullscreen_key();
    let merged = merge(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.defaults.get(&key), None);
    assert_eq!(
        normal.unbound_defaults[&key],
        bound("toggle-pane-fullscreen")
    );
    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
    assert_eq!(normal.user_set, BTreeMap::new());
}

#[test]
fn remove_then_rebind_moves_a_key_between_user_layers() {
    // The supported way to re-key: the session layer removes the user
    // layer's key and rebinds it itself.
    let key = seq(ModFlags::CTRL, 'y');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer_with_removed(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    // The same-layer rebind survives its own remove; the user entry is
    // voided.
    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::Session,
        }
    );
    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn remove_below_does_not_void_a_higher_binding() {
    let key = seq(ModFlags::CTRL, 'y');
    let merged = merge(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::Session,
        }
    );
    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn remove_and_rebind_of_a_defaulted_key_in_one_layer_records_both_sides() {
    // `<A-f>` is a shipped default. One user layer clears it and takes it:
    // the user entry wins the key, and the displaced default surfaces as
    // unbound.
    let key = default_fullscreen_key();
    let merged = merge(&[
        defaults(),
        layer_with_removed(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.defaults.get(&key), None);
    assert_eq!(
        normal.unbound_defaults[&key],
        bound("toggle-pane-fullscreen")
    );
    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
}

#[test]
fn removed_keys_accumulate_across_layers() {
    let from_user = seq(ModFlags::ALT, 'x');
    let from_session = seq(ModFlags::ALT, 'y');
    let merged = merge(&[
        defaults(),
        layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![from_user.clone()],
        ),
        layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![from_session.clone()],
        ),
    ]);

    assert_eq!(
        merged.modes[&mode("normal")].removed_keys,
        BTreeSet::from([from_user, from_session])
    );
}

#[test]
fn a_removal_from_the_defaults_layer_is_recorded_too() {
    // `removed_keys` collects from every layer, not just the user-authored
    // ones.
    let key = seq(ModFlags::ALT, 'x');
    let merged = merge(&[layer_with_removed(
        LayerOrigin::Defaults,
        "normal",
        Vec::new(),
        vec![key.clone()],
    )]);

    assert_eq!(
        merged.modes[&mode("normal")].removed_keys,
        BTreeSet::from([key])
    );
}

#[test]
fn a_removal_in_an_unregistered_mode_is_skipped() {
    let key = seq(ModFlags::ALT, 'x');
    let merged = merge(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "git", Vec::new(), vec![key]),
    ]);

    assert_eq!(merged.modes.get(&mode("git")), None);
    assert_eq!(merged.modes[&mode("normal")].removed_keys, BTreeSet::new());
}

#[test]
fn remove_of_an_unheld_key_is_recorded_and_nothing_more() {
    let key = seq(ModFlags::ALT, 'x');
    let merged = merge(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
    assert_eq!(normal.defaults.len(), 22);
    assert_eq!(normal.user_set, BTreeMap::new());
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn removed_user_binding_vanishes_silently() {
    // A user entry a higher layer removes is absent from every map, and
    // nothing lands in `unbound_defaults`.
    let key = seq(ModFlags::CTRL, 'y');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![key.clone()],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.user_set.get(&key), None);
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
}

#[test]
fn dead_user_binding_leaves_the_default_beneath_live() {
    // An orphan user binding (unregistered action) is transparent: it
    // steals nothing, and the shipped default keeps firing.
    let key = default_fullscreen_key();
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("does-not-exist"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.user_set.get(&key), None);
    assert_eq!(normal.defaults[&key], bound("toggle-pane-fullscreen"));
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn a_dead_user_binding_above_a_live_one_leaves_the_lower_layer_winning() {
    // The session layer names an unregistered action on a key the user layer
    // already took. The dead entry claims nothing, so attribution stays with
    // the user layer.
    let key = seq(ModFlags::ALT, 'w');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("does-not-exist"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
}

#[test]
fn a_later_defaults_layer_replaces_an_earlier_defaults_entry() {
    let key = seq(ModFlags::ALT, 'w');
    let merged = merge(&[
        layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.defaults[&key], bound("lock"));
    assert_eq!(normal.defaults.len(), 1);
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn one_layer_binding_two_modes_keeps_only_the_registered_one() {
    let registered = seq(ModFlags::ALT, 'w');
    let unregistered = seq(ModFlags::ALT, 'g');
    let mixed = KeyMapLayer {
        origin: LayerOrigin::User,
        modes: BTreeMap::from([
            (
                mode("normal"),
                crate::types::ModeBindings {
                    keys: BTreeMap::from([(registered.clone(), bound("lock"))]),
                    removed: BTreeSet::new(),
                },
            ),
            (
                mode("git"),
                crate::types::ModeBindings {
                    keys: BTreeMap::from([(unregistered, bound("lock"))]),
                    removed: BTreeSet::new(),
                },
            ),
        ]),
    };
    let merged = merge(&[mixed]);

    assert_eq!(
        merged.modes.keys().cloned().collect::<Vec<_>>(),
        vec![mode("normal")]
    );
    assert_eq!(
        merged.modes[&mode("normal")].user_set[&registered],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
}

#[test]
fn reserved_led_locked_sequence_is_transparent() {
    // In locked mode the reserved chord resolves instantly, so a longer
    // sequence opening with it can never fire and wins no key.
    let key = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
    );
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let locked = &merged.modes[&mode("locked")];

    assert_eq!(locked.user_set.get(&key), None);
    assert_eq!(
        locked.defaults[&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        bound("unlock")
    );
}

#[test]
fn a_locked_sequence_holding_the_reserved_chord_later_is_transparent_too() {
    // `<C-x> <C-l>` does not open with the reserved chord, but the unlock
    // resolves wherever in the sequence it is pressed. The sequence never
    // fires, so it wins no key.
    let key = seq2(
        KeyChord::new(ModFlags::CTRL, Key::Char('x')),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), bound("new-tab"))],
        ),
    ]);
    let locked = &merged.modes[&mode("locked")];

    assert_eq!(locked.user_set.get(&key), None);
    assert_eq!(locked.defaults.get(&key), None);
    assert_eq!(
        locked.defaults[&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        bound("unlock")
    );
}

#[test]
fn a_reserved_led_sequence_outside_locked_mode_fires() {
    // The reserved-chord rule is locked mode only. In `normal` the same
    // two-chord sequence is an ordinary binding.
    let key = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
    );
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn unlock_alternative_moves_the_reserved_chord() {
    // With an alternative declared, IT is the reserved chord: sequences it
    // opens are dead in locked mode, and the default `<C-g>` chord is an
    // ordinary key again.
    let alternative = KeyChord::new(ModFlags::CTRL, Key::Char('u'));
    let dead = seq2(alternative, KeyChord::new(ModFlags::NONE, Key::Char('x')));
    let live = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
    );
    let merged = merge_keymaps(
        &[
            defaults(),
            layer(
                LayerOrigin::User,
                "locked",
                vec![(dead.clone(), bound("lock")), (live.clone(), bound("lock"))],
            ),
        ],
        Some(alternative),
        DEPTH,
        &ActionRegistry::new(),
    );
    let locked = &merged.modes[&mode("locked")];

    assert_eq!(locked.user_set.get(&dead), None);
    assert_eq!(
        locked.user_set[&live],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
}

#[test]
fn unregistered_mode_is_skipped() {
    let key = seq(ModFlags::ALT, 'g');
    let merged = merge(&[
        defaults(),
        layer(LayerOrigin::User, "git", vec![(key, bound("lock"))]),
    ]);

    assert_eq!(merged.modes.get(&mode("git")), None);
    assert_eq!(
        merged.modes.keys().cloned().collect::<Vec<_>>(),
        vec![mode("locked"), mode("normal")]
    );
}

#[test]
fn sequences_merge_per_key_like_single_chords() {
    // `<C-p> x` is the shipped tree-close; the user takes exactly that
    // sequence, and the sibling `<C-p> n` default survives.
    let close = seq2(
        chord(ModFlags::CTRL, 'p'),
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
    );
    let new_pane = seq2(
        chord(ModFlags::CTRL, 'p'),
        KeyChord::new(ModFlags::NONE, Key::Char('n')),
    );
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(close.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&close],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.unbound_defaults[&close], bound("close-pane-tree"));
    assert_eq!(normal.defaults[&new_pane], bound("new-pane"));
}

#[test]
fn named_key_defaults_survive_untouched() {
    let merged = merge(&[defaults()]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.defaults[&KeySequence::new(
            KeyChord::new(ModFlags::CTRL, Key::Char('p')),
            vec![KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Left))],
        )],
        bound("focus-pane-left")
    );
}

#[test]
fn stealing_a_dead_defaults_key_unbinds_nothing() {
    // A defaults-layer key bound to the dead `core:copy-selection`; a user
    // binding takes the key. The dead default was never firing, so nothing
    // was displaced: `unbound_defaults` stays empty.
    let key = seq(ModFlags::ALT, 'c');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), bound("copy-selection"))],
        ),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn binding_past_the_chord_depth_cap_is_transparent() {
    // At a cap of 1 a two-chord entry enters no map — a dead user entry
    // leaves the defaulted key untouched, and a dead default is absent, not
    // displaced — while one-chord entries merge as usual.
    let long_default = seq2(
        KeyChord::new(ModFlags::ALT, Key::Char('p')),
        KeyChord::new(ModFlags::NONE, Key::Char('q')),
    );
    let short_default = seq(ModFlags::ALT, 't');
    let long_user = seq2(
        KeyChord::new(ModFlags::CTRL, Key::Char('y')),
        KeyChord::new(ModFlags::NONE, Key::Char('x')),
    );
    let short_user = seq(ModFlags::CTRL, 'y');
    let merged = merge_keymaps(
        &[
            layer(
                LayerOrigin::Defaults,
                "normal",
                vec![
                    (long_default.clone(), bound("new-pane")),
                    (short_default.clone(), bound("new-tab")),
                ],
            ),
            layer(
                LayerOrigin::User,
                "normal",
                vec![
                    (long_user.clone(), bound("lock")),
                    (short_user.clone(), bound("lock")),
                ],
            ),
        ],
        None,
        1,
        &ActionRegistry::new(),
    );
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.user_set.get(&long_user), None);
    assert_eq!(
        normal.user_set[&short_user],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::User,
        }
    );
    assert_eq!(normal.defaults.get(&long_default), None);
    assert_eq!(normal.defaults[&short_default], bound("new-tab"));
    // The dead default is absent by build state, never displaced.
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}
