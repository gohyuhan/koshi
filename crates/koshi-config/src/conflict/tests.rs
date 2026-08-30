//! Tests for keybinding conflict detection: every conflict class, the
//! steal/collision line, the reserved-unlock guarantee with and without an
//! alternative, verdict precedence, and the exact user-facing messages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

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

/// A one-mode layer built from `(sequence, bound action)` entries.
fn layer(
    origin: LayerOrigin,
    mode_name: &str,
    entries: Vec<(KeySequence, BoundAction)>,
) -> KeyMapLayer {
    layer_with_removed(origin, mode_name, entries, Vec::new())
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
            ModeBindings {
                keys: entries.into_iter().collect(),
                removed: removed.into_iter().collect(),
            },
        )]),
    }
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

/// Runs detection with the default leader, no unlock alternative, and the
/// seeded core registry.
fn detect(layers: &[KeyMapLayer]) -> ConflictReport {
    detect_conflicts(
        layers,
        Leader::default(),
        None,
        DEPTH,
        &ActionRegistry::new(),
    )
}

#[test]
fn user_layer_args_are_stripped_to_the_action_mapping() {
    // A user binding carrying arguments ("run, program htop") comes out
    // bare: only the key → action mapping survives, and a bare `run` names
    // no program.
    let key = seq(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action: core("run"),
        args: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            args: vec![],
            direction: None,
            stacked: false,
        },
    };
    let stripped =
        layer(LayerOrigin::User, "normal", vec![(key.clone(), smuggled)]).with_user_args_stripped();
    assert_eq!(stripped.modes[&mode("normal")].keys[&key], bound("run"));
}

#[test]
fn session_and_layout_layer_args_are_stripped_too() {
    // Stripping covers every user-authored origin, not the user file alone.
    let key = seq(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action: core("run"),
        args: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            args: vec![],
            direction: None,
            stacked: false,
        },
    };
    for origin in [LayerOrigin::Session, LayerOrigin::Layout] {
        let stripped = layer(origin, "normal", vec![(key.clone(), smuggled.clone())])
            .with_user_args_stripped();
        assert_eq!(stripped.modes[&mode("normal")].keys[&key], bound("run"));
    }
}

#[test]
fn keymap_layers_strips_arguments_off_the_user_layer() {
    // `keymap_layers` applies the stripping to the user layer: a user
    // binding `run, program /usr/bin/htop` comes out as a bare `run`, which
    // names no program.
    let key = seq(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action: core("run"),
        args: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            args: vec![],
            direction: None,
            stacked: false,
        },
    };
    let mut modes = BTreeMap::new();
    modes.insert(
        mode("normal"),
        ModeBindings {
            keys: [(key.clone(), smuggled)].into_iter().collect(),
            removed: BTreeSet::new(),
        },
    );

    let layers = keymap_layers(Some(modes), Leader::default());

    let user = layers
        .iter()
        .find(|layer| layer.origin == LayerOrigin::User)
        .expect("a user layer was supplied, so one comes back");
    assert_eq!(user.modes[&mode("normal")].keys[&key], bound("run"));
}

#[test]
fn keymap_layers_leaves_the_defaults_layer_untouched() {
    // The defaults layer keeps its arguments: `resize-pane` keeps the
    // amount it ships with.
    let layers = keymap_layers(None, Leader::default());

    assert_eq!(layers.len(), 1, "no user modes means the defaults alone");
    assert_eq!(layers[0].origin, LayerOrigin::Defaults);
    assert_eq!(
        layers[0].modes,
        default_mode_bindings(Leader::default()),
        "the defaults layer is the default table verbatim, arguments included"
    );
}

#[test]
fn stripping_leaves_the_defaults_layer_alone() {
    // `with_user_args_stripped` returns the defaults layer untouched.
    assert_eq!(defaults().with_user_args_stripped(), defaults());
}

#[test]
fn only_the_defaults_origin_is_not_user_authored() {
    assert!(!LayerOrigin::Defaults.is_user_authored());
    assert!(LayerOrigin::User.is_user_authored());
    assert!(LayerOrigin::Session.is_user_authored());
    assert!(LayerOrigin::Layout.is_user_authored());
}

#[test]
fn layer_origin_display_is_exact() {
    assert_eq!(LayerOrigin::Defaults.to_string(), "defaults");
    assert_eq!(LayerOrigin::User.to_string(), "user");
    assert_eq!(LayerOrigin::Session.to_string(), "session");
    assert_eq!(LayerOrigin::Layout.to_string(), "layout");
}

#[test]
fn built_in_modes_names_every_lock_mode() {
    let expected =
        BTreeSet::from(["normal", "locked", "resize", "pane", "tab", "scroll"].map(mode));
    assert_eq!(built_in_modes(), expected);
}

#[test]
fn defaults_alone_report_nothing() {
    let report = detect(&[defaults()]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn empty_report_applies() {
    assert_eq!(ConflictReport::default().verdict(), KeymapVerdict::Apply);
}

#[test]
fn user_vs_session_same_key_different_action_collides() {
    let key = seq(ModFlags::CTRL, 'y');
    let layers = [
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ];
    let report = detect(&layers);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode: mode("normal"),
            key,
            claims: vec![
                (LayerOrigin::User, bound("new-tab")),
                (LayerOrigin::Session, bound("lock")),
            ],
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn three_layers_with_three_distinct_actions_all_appear_in_the_collision() {
    // A collision lists every distinct claimant: three layers binding three
    // distinct actions give three claims.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), bound("quit"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode: mode("normal"),
            key,
            claims: vec![
                (LayerOrigin::User, bound("new-tab")),
                (LayerOrigin::Session, bound("lock")),
                (LayerOrigin::Layout, bound("quit")),
            ],
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn a_repeated_claim_across_nonadjacent_layers_dedups_against_a_third_distinct_one() {
    // User and Layout bind the identical action; Session's differing claim
    // sits between them. Dedup compares against every earlier distinct
    // claim: the result is exactly two distinct claims.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode: mode("normal"),
            key,
            claims: vec![
                (LayerOrigin::User, bound("new-tab")),
                (LayerOrigin::Session, bound("lock")),
            ],
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn steal_of_a_defaulted_key_is_not_a_collision() {
    // `<A-t>` is the default new-tab key; one user layer takes it.
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(seq(ModFlags::ALT, 't'), bound("lock"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn identical_bound_action_in_two_user_layers_passes() {
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key, bound("new-tab"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn same_action_with_different_args_collides() {
    // Unrepresentable from user files (their args are stripped), but the
    // type still allows it for system-authored layers. The collision is
    // judged on the whole bound value, args included.
    let key = seq(ModFlags::CTRL, 'e');
    let run_with = |program: &str| BoundAction {
        action: core("run"),
        args: ActionArgs::Run {
            program: PathBuf::from(program),
            args: vec![],
            direction: None,
            stacked: false,
        },
    };
    let layers = [
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), run_with("/usr/bin/htop"))],
        ),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), run_with("/usr/bin/btop"))],
        ),
    ];
    let report = detect(&layers);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode: mode("normal"),
            key,
            claims: vec![
                (LayerOrigin::User, run_with("/usr/bin/htop")),
                (LayerOrigin::Layout, run_with("/usr/bin/btop")),
            ],
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn orphan_actions_on_a_shared_key_do_not_collide() {
    // Both claims name unregistered actions: inactive bindings, warned as
    // orphans, re-judged when detection re-runs at registration.
    let key = seq(ModFlags::CTRL, 'y');
    let ghost = |name: &str| BoundAction {
        action: ActionRef::user(name).expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(LayerOrigin::User, "normal", vec![(key.clone(), ghost("a"))]),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), ghost("b"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::OrphanAction {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: key.clone(),
                action: ghost("a").action,
            },
            ConflictDiagnostic::OrphanAction {
                origin: LayerOrigin::Session,
                mode: mode("normal"),
                key,
                action: ghost("b").action,
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn one_orphan_claim_does_not_collide_with_a_live_one() {
    let key = seq(ModFlags::CTRL, 'y');
    let ghost = BoundAction {
        action: ActionRef::user("ghost").expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), ghost.clone())],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: ghost.action,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn bindings_in_an_orphan_mode_do_not_collide() {
    let key = seq(ModFlags::ALT, 's');
    let report = detect(&[
        defaults(),
        layer(LayerOrigin::User, "git", vec![(key.clone(), bound("lock"))]),
        layer(LayerOrigin::Session, "git", vec![(key, bound("new-tab"))]),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::OrphanMode {
                origin: LayerOrigin::User,
                mode: mode("git"),
            },
            ConflictDiagnostic::OrphanMode {
                origin: LayerOrigin::Session,
                mode: mode("git"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn coming_soon_binding_warns_without_revert() {
    // `core:copy-selection` is seeded but not implemented; the binding cannot fire.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("copy-selection"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ComingSoonAction {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: core("copy-selection"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn coming_soon_claims_do_not_collide() {
    // Neither binding can fire in this build; the collision surfaces at
    // the first load of a build that implements the actions.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("copy-selection"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("plugin-install"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::ComingSoonAction {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: key.clone(),
                action: core("copy-selection"),
            },
            ConflictDiagnostic::ComingSoonAction {
                origin: LayerOrigin::Session,
                mode: mode("normal"),
                key,
                action: core("plugin-install"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn unresolvable_args_binding_warns_and_does_not_collide() {
    // The user layer's binding carries arguments `core:lock` cannot take
    // and never fires; the session layer's working binding applies with no
    // revert.
    let key = seq(ModFlags::CTRL, 'y');
    let broken = BoundAction {
        action: core("lock"),
        args: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            args: vec![],
            direction: None,
            stacked: false,
        },
    };
    let report = detect(&[
        defaults(),
        layer(LayerOrigin::User, "normal", vec![(key.clone(), broken)]),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::UnresolvableArgs {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: core("lock"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn rebinding_the_reserved_unlock_is_fatal() {
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                bound("lock"),
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockShadowed {
            origin: LayerOrigin::User,
            action: core("lock"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn unlock_with_wrong_arguments_is_dead_not_a_shadow() {
    // `core:unlock` fires only with no arguments: this binding never
    // fires, it is transparent, and the default unlock beneath it wins the
    // reserved chord.
    let key = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(
                key.clone(),
                BoundAction {
                    action: core("unlock"),
                    args: ActionArgs::Run {
                        program: PathBuf::from("/usr/bin/htop"),
                        args: vec![],
                        direction: None,
                        stacked: false,
                    },
                },
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::UnresolvableArgs {
            origin: LayerOrigin::User,
            mode: mode("locked"),
            key,
            action: core("unlock"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_led_claims_do_not_collide() {
    // Both layers bind a locked-mode sequence the reserved chord swallows;
    // neither can ever fire. Each is warned dead, with no collision and no
    // revert.
    let key = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        chord(ModFlags::NONE, 'x'),
    );
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::Session,
            "locked",
            vec![(key.clone(), bound("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::DeadUnderReservedUnlock {
                origin: LayerOrigin::User,
                key: key.clone(),
                action: core("lock"),
            },
            ConflictDiagnostic::DeadUnderReservedUnlock {
                origin: LayerOrigin::Session,
                key,
                action: core("new-tab"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_locked_sequence_holding_the_reserved_chord_anywhere_is_dead() {
    // `<C-x> <C-l>` does not OPEN with the reserved chord. The input path
    // resolves the unlock the instant it is pressed, open sequence or not:
    // the `<C-l>` unlocks and `core:new-tab` never runs. A locked sequence
    // holding the chord at any position is warned dead.
    let key = seq2(
        chord(ModFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), bound("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            origin: LayerOrigin::User,
            key,
            action: core("new-tab"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn the_one_chord_unlock_binding_itself_stays_live() {
    // The dead judgment covers only sequences of two or more chords that
    // hold the reserved chord. Locked mode's own one-chord `<C-l>` →
    // `core:unlock` fires and draws no warning.
    let report = detect(&[defaults()]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_led_sequences_do_not_pair_as_prefixes() {
    // `<C-l> x` is a strict prefix of `<C-l> x y`, but both hold the
    // reserved chord: two dead warnings, no ambiguous-prefix pair.
    let short = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        chord(ModFlags::NONE, 'x'),
    );
    let long = KeySequence::new(
        KeybindingsConfig::RESERVED_UNLOCK,
        vec![chord(ModFlags::NONE, 'x'), chord(ModFlags::NONE, 'y')],
    );
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![
                (short.clone(), bound("lock")),
                (long.clone(), bound("new-tab")),
            ],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::DeadUnderReservedUnlock {
                origin: LayerOrigin::User,
                key: short,
                action: core("lock"),
            },
            ConflictDiagnostic::DeadUnderReservedUnlock {
                origin: LayerOrigin::User,
                key: long,
                action: core("new-tab"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn dead_binding_does_not_warn_typeable() {
    // `g` opens typeable, but the binding is orphaned and steals nothing;
    // it gets exactly the orphan warning, not a stealing warning on top.
    let key = seq(ModFlags::NONE, 'g');
    let ghost = BoundAction {
        action: ActionRef::user("ghost").expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), ghost.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: ghost.action,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_bindings_skip_per_binding_warns() {
    // The whole overlay is inactive: one mode warning, no orphan-action or
    // typeable warnings for the bindings inside it.
    let ghost = BoundAction {
        action: ActionRef::user("ghost").expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "git",
            vec![(seq(ModFlags::NONE, 'g'), ghost)],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            origin: LayerOrigin::User,
            mode: mode("git"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_on_the_reserved_chord_does_not_shadow() {
    // The higher layer's binding names an unregistered action: inactive,
    // transparent, and the default unlock beneath it still fires. Only the
    // orphan warning is reported; the keymap is not rejected.
    let key = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let ghost = BoundAction {
        action: ActionRef::user("ghost").expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), ghost.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            origin: LayerOrigin::User,
            mode: mode("locked"),
            key,
            action: ghost.action,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn shadow_with_a_bound_alternative_passes() {
    let alternative = chord(ModFlags::CTRL, 'u');
    let layers = [
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![
                (
                    KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                    bound("lock"),
                ),
                (KeySequence::from(alternative), bound("unlock")),
            ],
        ),
    ];
    let report = detect_conflicts(
        &layers,
        Leader::default(),
        Some(alternative),
        DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn declared_but_unbound_alternative_is_fatal() {
    let alternative = chord(ModFlags::CTRL, 'u');
    let report = detect_conflicts(
        &[defaults()],
        Leader::default(),
        Some(alternative),
        DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved: alternative,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn typeable_alternative_is_fatal() {
    let alternative = chord(ModFlags::NONE, 'u');
    let report = detect_conflicts(
        &[defaults()],
        Leader::default(),
        Some(alternative),
        DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::UnlockAlternativeTypeable { chord: alternative },
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved: alternative,
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn no_layers_report_the_unlock_missing() {
    let report = detect(&[]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved: KeybindingsConfig::RESERVED_UNLOCK,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn user_prefix_of_default_sequences_warns_without_revert() {
    // The defaults bind `<C-p> n`, the four `<C-p>` vim-letter splits,
    // `<C-p> x`, and the four `<C-p>` arrow focus sequences; the user binds
    // bare `<C-p>`.
    let prefix = seq(ModFlags::CTRL, 'p');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix.clone(), bound("lock"))],
        ),
    ]);
    let ambiguous = |longer_key: Key, longer_action: &str| ConflictDiagnostic::AmbiguousPrefix {
        mode: mode("normal"),
        prefix: prefix.clone(),
        prefix_action: core("lock"),
        longer: seq2(
            chord(ModFlags::CTRL, 'p'),
            KeyChord::new(ModFlags::NONE, longer_key),
        ),
        longer_action: core(longer_action),
    };
    assert_eq!(
        report.diagnostics,
        vec![
            ambiguous(Key::Char('h'), "new-pane-left"),
            ambiguous(Key::Char('j'), "new-pane-down"),
            ambiguous(Key::Char('k'), "new-pane-up"),
            ambiguous(Key::Char('l'), "new-pane-right"),
            ambiguous(Key::Char('n'), "new-pane"),
            ambiguous(Key::Char('x'), "close-pane-tree"),
            ambiguous(Key::Named(NamedKey::Left), "focus-pane-left"),
            ambiguous(Key::Named(NamedKey::Right), "focus-pane-right"),
            ambiguous(Key::Named(NamedKey::Up), "focus-pane-up"),
            ambiguous(Key::Named(NamedKey::Down), "focus-pane-down"),
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_three_deep_prefix_chain_reports_every_pair() {
    // `<C-y>`, `<C-y> n`, and `<C-y> n o` are each a prefix of the ones
    // longer than it: three pairs total.
    let short = seq(ModFlags::CTRL, 'y');
    let mid = seq2(chord(ModFlags::CTRL, 'y'), chord(ModFlags::NONE, 'n'));
    let long = KeySequence::new(
        chord(ModFlags::CTRL, 'y'),
        vec![chord(ModFlags::NONE, 'n'), chord(ModFlags::NONE, 'o')],
    );
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![
                (short.clone(), bound("lock")),
                (mid.clone(), bound("new-tab")),
                (long.clone(), bound("quit")),
            ],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::AmbiguousPrefix {
                mode: mode("normal"),
                prefix: short.clone(),
                prefix_action: core("lock"),
                longer: mid.clone(),
                longer_action: core("new-tab"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode: mode("normal"),
                prefix: short,
                prefix_action: core("lock"),
                longer: long.clone(),
                longer_action: core("quit"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode: mode("normal"),
                prefix: mid,
                prefix_action: core("new-tab"),
                longer: long,
                longer_action: core("quit"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn prefix_pairs_do_not_cross_modes() {
    // `<C-y>` bound in normal and `<C-y> x` bound in locked do not pair:
    // prefixes are judged within one mode.
    let short = seq(ModFlags::CTRL, 'y');
    let long = seq2(chord(ModFlags::CTRL, 'y'), chord(ModFlags::NONE, 'x'));
    let user = KeyMapLayer {
        origin: LayerOrigin::User,
        modes: BTreeMap::from([
            (
                mode("normal"),
                ModeBindings {
                    keys: [(short, bound("lock"))].into_iter().collect(),
                    removed: BTreeSet::new(),
                },
            ),
            (
                mode("locked"),
                ModeBindings {
                    keys: [(long, bound("new-tab"))].into_iter().collect(),
                    removed: BTreeSet::new(),
                },
            ),
        ]),
    };
    let report = detect(&[defaults(), user]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn the_reserved_chord_opening_a_normal_mode_sequence_is_an_ordinary_prefix_pair() {
    // The reserved chord is only swallowed in LOCKED mode; the identical
    // chord opening a longer sequence in NORMAL mode is an ordinary
    // ambiguous-prefix warning, not a dead binding.
    let short = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let long = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        chord(ModFlags::NONE, 'x'),
    );
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(long.clone(), bound("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::AmbiguousPrefix {
            mode: mode("normal"),
            prefix: short,
            prefix_action: core("lock"),
            longer: long,
            longer_action: core("new-tab"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_later_redundant_remove_voids_a_rebind_that_an_earlier_remove_would_not() {
    // Two layers remove the same key; the LAST (highest-index) remove sets
    // the index a claim must beat. Removal is positional, not per-origin: a
    // stack may hold several layers of one origin. User removes the key
    // (index 1, no bind), Session rebinds it without removing (index 2), a
    // first layout layer removes it again (index 3, no bind), a second
    // layout layer rebinds with a different action (index 4). The remove at
    // index 3 voids Session's rebind at index 2, leaving the top claim
    // alone and nothing to collide with.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer_with_removed(LayerOrigin::Layout, "normal", Vec::new(), vec![key.clone()]),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn locked_sequence_opening_with_the_reserved_chord_is_dead_not_ambiguous() {
    let key = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        chord(ModFlags::NONE, 'x'),
    );
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            origin: LayerOrigin::User,
            key,
            action: core("lock"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_action_warns_without_revert() {
    let key = seq(ModFlags::CTRL, 'o');
    let orphan = BoundAction {
        action: ActionRef::user("my-macro").expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), orphan.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: orphan.action,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_warns_without_revert() {
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "git",
            vec![(seq(ModFlags::ALT, 's'), bound("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            origin: LayerOrigin::User,
            mode: mode("git"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_opening_chord_warns() {
    let key = seq(ModFlags::NONE, 'g');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableBinding {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: core("lock"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_later_chord_does_not_warn() {
    // Only the opening chord matters: a plain second chord is read while
    // the pending sequence is live.
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(
                seq2(chord(ModFlags::CTRL, 'p'), chord(ModFlags::NONE, 'g')),
                bound("lock"),
            )],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn shift_only_mods_leader_warns() {
    let report = detect_conflicts(
        &[defaults()],
        Leader::Mods(ModFlags::SHIFT),
        None,
        DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableLeader {
            leader: Leader::Mods(ModFlags::SHIFT),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_chord_leader_warns() {
    let leader = Leader::Chord(chord(ModFlags::NONE, ','));
    let report = detect_conflicts(&[defaults()], leader, None, DEPTH, &ActionRegistry::new());
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableLeader { leader }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn non_typeable_leaders_do_not_warn() {
    for leader in [
        Leader::Mods(ModFlags::CTRL),
        Leader::Mods(ModFlags::ALT.union(ModFlags::SHIFT)),
        Leader::Chord(chord(ModFlags::CTRL, 'b')),
    ] {
        let report = detect_conflicts(&[defaults()], leader, None, DEPTH, &ActionRegistry::new());
        assert_eq!(report.diagnostics, Vec::new());
    }
}

#[test]
fn a_fatal_finding_outranks_a_collision() {
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer(
            LayerOrigin::Layout,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                bound("lock"),
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::KeyCollision {
                mode: mode("normal"),
                key,
                claims: vec![
                    (LayerOrigin::User, bound("new-tab")),
                    (LayerOrigin::Session, bound("lock")),
                ],
            },
            ConflictDiagnostic::ReservedUnlockShadowed {
                origin: LayerOrigin::Layout,
                action: core("lock"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn severity_table() {
    let claims = vec![
        (LayerOrigin::User, bound("new-tab")),
        (LayerOrigin::Session, bound("lock")),
    ];
    let cases = [
        (
            ConflictDiagnostic::KeyCollision {
                mode: mode("normal"),
                key: seq(ModFlags::CTRL, 'y'),
                claims,
            },
            ConflictSeverity::Collision,
        ),
        (
            ConflictDiagnostic::ReservedUnlockShadowed {
                origin: LayerOrigin::User,
                action: core("lock"),
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved: KeybindingsConfig::RESERVED_UNLOCK,
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::UnlockAlternativeTypeable {
                chord: chord(ModFlags::NONE, 'u'),
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::AmbiguousPrefix {
                mode: mode("normal"),
                prefix: seq(ModFlags::CTRL, 'p'),
                prefix_action: core("lock"),
                longer: seq2(chord(ModFlags::CTRL, 'p'), chord(ModFlags::NONE, 'n')),
                longer_action: core("new-pane"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::DeadUnderReservedUnlock {
                origin: LayerOrigin::User,
                key: seq2(
                    KeybindingsConfig::RESERVED_UNLOCK,
                    chord(ModFlags::NONE, 'x'),
                ),
                action: core("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::ComingSoonAction {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: seq(ModFlags::CTRL, 'y'),
                action: core("copy-selection"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::UnresolvableArgs {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: seq(ModFlags::CTRL, 'y'),
                action: core("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanAction {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: seq(ModFlags::CTRL, 'o'),
                action: core("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanMode {
                origin: LayerOrigin::User,
                mode: mode("git"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::TypeableBinding {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: seq(ModFlags::NONE, 'g'),
                action: core("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::TypeableLeader {
                leader: Leader::Mods(ModFlags::SHIFT),
            },
            ConflictSeverity::Warning,
        ),
    ];
    for (diagnostic, severity) in cases {
        assert_eq!(diagnostic.severity(), severity, "{diagnostic:?}");
    }
}

#[test]
fn display_messages_are_exact() {
    let collision = ConflictDiagnostic::KeyCollision {
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 'y'),
        claims: vec![
            (LayerOrigin::User, bound("new-tab")),
            (LayerOrigin::Session, bound("lock")),
        ],
    };
    assert_eq!(
        collision.to_string(),
        "key `<C-y>` in mode `normal` is bound by user to `core:new-tab` and by session \
         to `core:lock`; all user keybindings revert to defaults"
    );

    let prefix = ConflictDiagnostic::AmbiguousPrefix {
        mode: mode("normal"),
        prefix: seq(ModFlags::CTRL, 'p'),
        prefix_action: core("lock"),
        longer: seq2(chord(ModFlags::CTRL, 'p'), chord(ModFlags::NONE, 'n')),
        longer_action: core("new-pane"),
    };
    assert_eq!(
        prefix.to_string(),
        "`<C-p>` (`core:lock`) is a prefix of `<C-p> n` (`core:new-pane`) in mode \
         `normal`; the shorter binding fires only on the chord timeout"
    );

    let shadowed = ConflictDiagnostic::ReservedUnlockShadowed {
        origin: LayerOrigin::User,
        action: core("lock"),
    };
    assert_eq!(
        shadowed.to_string(),
        "the reserved unlock key is bound by user to `core:lock` in locked mode; \
         declare `unlock_alternative` before rebinding it"
    );

    let missing = ConflictDiagnostic::ReservedUnlockMissing {
        reserved: KeybindingsConfig::RESERVED_UNLOCK,
    };
    assert_eq!(
        missing.to_string(),
        "locked mode has no binding from `<C-l>` to `core:unlock`; the unlock escape \
         would be unreachable"
    );

    let typeable_alt = ConflictDiagnostic::UnlockAlternativeTypeable {
        chord: chord(ModFlags::NONE, 'u'),
    };
    assert_eq!(
        typeable_alt.to_string(),
        "`unlock_alternative` `u` is a key plain typing produces; hold Ctrl, Alt, or Super"
    );

    let dead = ConflictDiagnostic::DeadUnderReservedUnlock {
        origin: LayerOrigin::User,
        key: seq2(
            KeybindingsConfig::RESERVED_UNLOCK,
            chord(ModFlags::NONE, 'x'),
        ),
        action: core("lock"),
    };
    assert_eq!(
        dead.to_string(),
        "`<C-l> x` (user, `core:lock`) in locked mode can never fire: it holds the \
         reserved unlock chord, which resolves instantly wherever it is pressed"
    );

    let same_action_collision = ConflictDiagnostic::KeyCollision {
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 'e'),
        claims: vec![
            (
                LayerOrigin::User,
                BoundAction {
                    action: core("run"),
                    args: ActionArgs::Run {
                        program: PathBuf::from("/usr/bin/htop"),
                        args: vec![],
                        direction: None,
                        stacked: false,
                    },
                },
            ),
            (
                LayerOrigin::Layout,
                BoundAction {
                    action: core("run"),
                    args: ActionArgs::Run {
                        program: PathBuf::from("/usr/bin/btop"),
                        args: vec![],
                        direction: None,
                        stacked: false,
                    },
                },
            ),
        ],
    };
    assert_eq!(
        same_action_collision.to_string(),
        "key `<C-e>` in mode `normal` is bound by user to `core:run` and by \
         layout to `core:run` with different arguments; all user keybindings \
         revert to defaults"
    );

    let unresolvable = ConflictDiagnostic::UnresolvableArgs {
        origin: LayerOrigin::User,
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 'y'),
        action: core("lock"),
    };
    assert_eq!(
        unresolvable.to_string(),
        "`<C-y>` in mode `normal` (user) binds `core:lock` with arguments it cannot \
         take; the binding can never fire as written"
    );

    let coming_soon = ConflictDiagnostic::ComingSoonAction {
        origin: LayerOrigin::User,
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 'y'),
        action: core("copy-selection"),
    };
    assert_eq!(
        coming_soon.to_string(),
        "`<C-y>` in mode `normal` (user) binds `core:copy-selection`, which is not implemented \
         yet; the binding cannot fire until it is"
    );

    let orphan_action = ConflictDiagnostic::OrphanAction {
        origin: LayerOrigin::User,
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 'o'),
        action: ActionRef::user("my-macro").expect("valid user action name"),
    };
    assert_eq!(
        orphan_action.to_string(),
        "`<C-o>` in mode `normal` (user) names unknown action `user:my-macro`; the \
         binding is inactive until the action is registered"
    );

    let orphan_mode = ConflictDiagnostic::OrphanMode {
        origin: LayerOrigin::Session,
        mode: mode("git"),
    };
    assert_eq!(
        orphan_mode.to_string(),
        "the session keymap binds keys in unregistered mode `git`; those bindings are \
         inactive until the mode is registered"
    );

    let typeable_binding = ConflictDiagnostic::TypeableBinding {
        origin: LayerOrigin::User,
        mode: mode("normal"),
        key: seq(ModFlags::NONE, 'g'),
        action: core("lock"),
    };
    assert_eq!(
        typeable_binding.to_string(),
        "`g` in mode `normal` (user, `core:lock`) opens with a key plain typing \
         produces; it steals that key from the pane"
    );

    let typeable_leader = ConflictDiagnostic::TypeableLeader {
        leader: Leader::Mods(ModFlags::SHIFT),
    };
    assert_eq!(
        typeable_leader.to_string(),
        "leader `S-` is reachable by plain typing; bindings that start with it steal \
         those keys from panes"
    );
}

#[test]
fn remove_then_rebind_across_user_layers_is_not_a_collision() {
    // The supported way to re-key: the session layer removes the user
    // layer's key, voiding its claim, and rebinds the key itself.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
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
            vec![key],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn remove_without_rebind_voids_the_lower_claim() {
    // The user layer binds the key, session only removes it: one claim,
    // voided — no collision, and the key reaches nothing.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer_with_removed(LayerOrigin::Session, "normal", Vec::new(), vec![key]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn remove_below_both_claims_does_not_stop_their_collision() {
    // A remove voids only LOWER layers' claims: with the remove at the
    // bottom user layer, the two claims above it still collide.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer_with_removed(LayerOrigin::User, "normal", Vec::new(), vec![key.clone()]),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode: mode("normal"),
            key,
            claims: vec![
                (LayerOrigin::Session, bound("new-tab")),
                (LayerOrigin::Layout, bound("lock")),
            ],
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn remove_above_both_claims_voids_the_collision() {
    // A remove above both claims voids both: no collision, and no warning
    // fires.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("lock"))],
        ),
        layer_with_removed(LayerOrigin::Layout, "normal", Vec::new(), vec![key]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn removing_the_locked_unlock_binding_is_fatal() {
    // Clearing the reserved chord's binding in locked mode leaves no unlock
    // escape: the effective map misses it, and the keymap is refused.
    let report = detect(&[
        defaults(),
        layer_with_removed(
            LayerOrigin::User,
            "locked",
            Vec::new(),
            vec![KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved: KeybindingsConfig::RESERVED_UNLOCK,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn removed_binding_draws_no_per_binding_warns() {
    // The user layer binds an orphan action on a typeable key; session
    // removes the key. The removed binding draws neither the orphan warning
    // nor the typeable warning.
    let key = seq(ModFlags::NONE, 'g');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("does-not-exist"))],
        ),
        layer_with_removed(LayerOrigin::Session, "normal", Vec::new(), vec![key]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn removed_prefix_binding_does_not_pair_as_a_prefix() {
    // A single-chord `<C-p>` binding would pair with the defaults' `<C-p> n`
    // and `<C-p> x` sequences; removing it above voids the pairing.
    let prefix = seq(ModFlags::CTRL, 'p');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix.clone(), bound("lock"))],
        ),
        layer_with_removed(LayerOrigin::Session, "normal", Vec::new(), vec![prefix]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn binding_past_the_chord_depth_cap_warns_and_applies() {
    // At a cap of 1, a two-chord user binding is never reached: the input
    // path flushes the pending sequence before lookup. It warns, stays
    // transparent, and the keymap applies.
    let long = seq2(chord(ModFlags::CTRL, 'y'), chord(ModFlags::NONE, 'x'));
    let report = detect_conflicts(
        &[
            defaults(),
            layer(
                LayerOrigin::User,
                "normal",
                vec![(long.clone(), bound("new-tab"))],
            ),
        ],
        Leader::default(),
        None,
        1,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ExceedsChordDepth {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key: long,
            action: core("new-tab"),
            max_chord_depth: 1,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
    assert_eq!(
        report.diagnostics[0].to_string(),
        "`<C-y> x` in mode `normal` (user, `core:new-tab`) is 2 chords, over the \
         `max_chord_depth` cap of 1; the binding can never fire"
    );
}

#[test]
fn binding_with_exactly_max_chord_depth_chords_fires() {
    // At a cap of 1, a one-chord user binding sits exactly at the cap,
    // fires, and draws no warning.
    let report = detect_conflicts(
        &[
            defaults(),
            layer(
                LayerOrigin::User,
                "normal",
                vec![(seq(ModFlags::CTRL, 'y'), bound("new-tab"))],
            ),
        ],
        Leader::default(),
        None,
        1,
        &ActionRegistry::new(),
    );
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_reserved_led_sequence_past_the_cap_warns_dead_not_depth() {
    // A locked two-chord sequence holding the reserved chord while over a
    // cap of 1 draws one warning, the reserved-chord one.
    let key = seq2(
        chord(ModFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let report = detect_conflicts(
        &[
            defaults(),
            layer(
                LayerOrigin::User,
                "locked",
                vec![(key.clone(), bound("new-tab"))],
            ),
        ],
        Leader::default(),
        None,
        1,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            origin: LayerOrigin::User,
            key,
            action: core("new-tab"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn an_orphan_action_on_a_reserved_led_sequence_warns_orphan_not_dead() {
    // A locked sequence holding the reserved chord that also names an
    // unregistered action draws one warning, the resolver's refusal.
    let key = seq2(
        KeybindingsConfig::RESERVED_UNLOCK,
        chord(ModFlags::NONE, 'x'),
    );
    let ghost = BoundAction {
        action: ActionRef::user("ghost").expect("valid user action name"),
        args: ActionArgs::None,
    };
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), ghost.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            origin: LayerOrigin::User,
            mode: mode("locked"),
            key,
            action: ghost.action,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_chord_depth_of_zero_fails_the_unlock_guarantee() {
    // With every sequence at least one chord, a cap of 0 makes the whole
    // keymap unreachable — including the locked-mode unlock, which the
    // guarantee check reports as missing.
    let report = detect_conflicts(
        &[defaults()],
        Leader::default(),
        None,
        0,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved: KeybindingsConfig::RESERVED_UNLOCK,
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
}

#[test]
fn remove_in_an_unregistered_mode_is_inert() {
    // Removals in an unknown mode are skipped like its bindings; only the
    // orphan-mode warning surfaces.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer_with_removed(LayerOrigin::Session, "git", Vec::new(), vec![key]),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            origin: LayerOrigin::Session,
            mode: mode("git"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_collision_naming_one_claim_does_not_say_the_arguments_differ() {
    // `detect_conflicts` never builds this, but the variant and its fields are
    // public: one claim names no second action to differ from.
    let one_claim = ConflictDiagnostic::KeyCollision {
        mode: ModeName::new("normal"),
        key: KeySequence::from(KeyChord::new(ModFlags::CTRL, Key::Char('y'))),
        claims: vec![(
            LayerOrigin::User,
            BoundAction {
                action: ActionRef::core("lock").expect("a core action"),
                args: ActionArgs::None,
            },
        )],
    };

    assert_eq!(
        one_claim.to_string(),
        "key `<C-y>` in mode `normal` is bound by user to `core:lock`; \
         all user keybindings revert to defaults"
    );
}
