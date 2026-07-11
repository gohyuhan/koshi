//! Tests for keymap merging: per-key later-wins folding, the user-set vs
//! surviving-defaults split, default steal and removal bookkeeping, dead
//! bindings staying transparent, and the reserved-chord reachability rule.

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

fn known() -> BTreeSet<ModeName> {
    BTreeSet::from([mode("normal"), mode("locked")])
}

/// Merges with no unlock alternative and the seeded core registry.
fn merge(layers: &[KeyMapLayer]) -> MergedKeyMap {
    merge_keymaps(layers, None, &ActionRegistry::new(), &known())
}

/// The `<A-t>` → `core:new-tab` shipped default.
fn default_new_tab_key() -> KeySequence {
    seq(ModFlags::ALT, 't')
}

#[test]
fn defaults_alone_fill_the_defaults_map_and_nothing_else() {
    let merged = merge(&[defaults()]);
    let normal = &merged.modes[&mode("normal")];

    // All 20 shipped normal-mode defaults fire in this build.
    assert_eq!(normal.defaults.len(), 20);
    assert_eq!(normal.defaults[&default_new_tab_key()], bound("new-tab"));
    assert_eq!(normal.user_set, BTreeMap::new());
    assert_eq!(normal.removed_keys, BTreeSet::new());
    assert_eq!(normal.unbound_defaults, BTreeMap::new());

    let locked = &merged.modes[&mode("locked")];
    assert_eq!(
        locked.defaults[&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        bound("unlock")
    );
    assert_eq!(locked.defaults[&seq(ModFlags::CTRL, 'q')], bound("quit"));
    assert_eq!(locked.defaults.len(), 2);
}

#[test]
fn dead_default_is_absent_not_unbound() {
    // `core:copy-mode-enter` is ComingSoon: the resolver refuses it, so a
    // defaults-layer binding to it enters no map — the key falls through to
    // the pane, and it is not "unbound" since the user displaced nothing.
    let dead_key = seq(ModFlags::ALT, 'c');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(dead_key.clone(), bound("copy-mode-enter"))],
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
    assert_eq!(normal.defaults.len(), 20);
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn user_binding_steals_a_defaulted_key() {
    let key = default_new_tab_key();
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
    assert_eq!(normal.unbound_defaults[&key], bound("new-tab"));
    // Sibling defaults untouched.
    assert_eq!(normal.defaults.len(), 19);
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
            LayerOrigin::Project,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(
        normal.user_set[&key],
        MergedBinding {
            bound: bound("lock"),
            source: LayerOrigin::Project,
        }
    );
}

#[test]
fn remove_clears_a_default_and_records_both_sides() {
    let key = default_new_tab_key();
    let merged = merge(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.defaults.get(&key), None);
    assert_eq!(normal.unbound_defaults[&key], bound("new-tab"));
    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
    assert_eq!(normal.user_set, BTreeMap::new());
}

#[test]
fn remove_then_rebind_moves_a_key_between_user_layers() {
    // The supported way to re-key: the session layer removes the project
    // layer's key and rebinds it itself.
    let key = seq(ModFlags::CTRL, 't');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::Project,
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

    // The same-layer rebind survives its own remove; the project entry is
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
    let key = seq(ModFlags::CTRL, 't');
    let merged = merge(&[
        defaults(),
        layer_with_removed(
            LayerOrigin::Project,
            "normal",
            Vec::new(),
            vec![key.clone()],
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
fn remove_of_an_unheld_key_is_recorded_and_nothing_more() {
    let key = seq(ModFlags::ALT, 'x');
    let merged = merge(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
    ]);
    let normal = &merged.modes[&mode("normal")];

    assert_eq!(normal.removed_keys, BTreeSet::from([key]));
    assert_eq!(normal.defaults.len(), 20);
    assert_eq!(normal.user_set, BTreeMap::new());
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
}

#[test]
fn removed_user_binding_vanishes_silently() {
    // A user entry voided by a higher layer's remove is the user's own
    // authored intent: absent from the merged map, nothing unbound.
    let key = seq(ModFlags::CTRL, 't');
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
    let key = default_new_tab_key();
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
    assert_eq!(normal.defaults[&key], bound("new-tab"));
    assert_eq!(normal.unbound_defaults, BTreeMap::new());
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
        &ActionRegistry::new(),
        &known(),
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
    assert_eq!(
        normal.unbound_defaults[&close],
        BoundAction {
            action: core("close-pane"),
            args: ActionArgs::ClosePane {
                force: false,
                tree: true,
            },
        }
    );
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
        BoundAction {
            action: core("focus-pane"),
            args: ActionArgs::FocusPane {
                target: koshi_core::command::FocusTarget::Direction(
                    koshi_core::geometry::Direction::Left
                ),
            },
        }
    );
}

#[test]
fn stealing_a_dead_defaults_key_unbinds_nothing() {
    // A defaults-layer key bound to the dead `core:copy-mode-enter`; a user
    // binding takes the key. The dead default was never firing, so nothing
    // was displaced: `unbound_defaults` stays empty.
    let key = seq(ModFlags::ALT, 'c');
    let merged = merge(&[
        defaults(),
        layer(
            LayerOrigin::Defaults,
            "normal",
            vec![(key.clone(), bound("copy-mode-enter"))],
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
