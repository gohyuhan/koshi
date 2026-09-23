//! Tests for keybinding conflict detection: every conflict class, the
//! steal/collision line, the reserved-unlock guarantee with and without an
//! alternative, verdict precedence, and the exact user-facing messages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

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

/// A one-mode layer built from `(sequence, bound action)` binding entries.
fn build_key_map_layer(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
) -> KeymapLayer {
    build_key_map_layer_with_removed(origin, mode_name, binding_entries, Vec::new())
}

/// A one-mode layer built from `(sequence, bound action)` binding entries plus the
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
            ModeBindings {
                bound_action_by_key_sequence: binding_entries.into_iter().collect(),
                removed_key_sequences: removed_key_sequences.into_iter().collect(),
            },
        )]),
    }
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

/// Runs detection with the default leader, no unlock alternative, and the
/// seeded core registry.
fn detect_test_conflicts(layers: &[KeymapLayer]) -> ConflictReport {
    detect_conflicts(
        layers,
        Leader::default(),
        None,
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    )
}

#[test]
fn user_layer_args_are_stripped_to_the_action_mapping() {
    // A user binding carrying arguments ("run, program htop") comes out
    // bare: only the key → action mapping survives, and a bare `run` names
    // no program.
    let key = build_single_chord_sequence(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action_reference: build_core_action("run"),
        action_arguments: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            arguments: vec![],
            direction: None,
            should_stack: false,
        },
    };
    let stripped = build_key_map_layer(LayerOrigin::User, "normal", vec![(key.clone(), smuggled)])
        .strip_user_arguments();
    assert_eq!(
        stripped.mode_bindings_by_name[&parse_mode_name("normal")].bound_action_by_key_sequence
            [&key],
        build_bound_action("run")
    );
}

#[test]
fn session_and_layout_layer_args_are_stripped_too() {
    // Stripping covers every user-authored origin, not the user file alone.
    let key = build_single_chord_sequence(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action_reference: build_core_action("run"),
        action_arguments: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            arguments: vec![],
            direction: None,
            should_stack: false,
        },
    };
    for origin in [LayerOrigin::Session, LayerOrigin::Layout] {
        let stripped = build_key_map_layer(origin, "normal", vec![(key.clone(), smuggled.clone())])
            .strip_user_arguments();
        assert_eq!(
            stripped.mode_bindings_by_name[&parse_mode_name("normal")].bound_action_by_key_sequence
                [&key],
            build_bound_action("run")
        );
    }
}

#[test]
fn build_keymap_layers_strips_arguments_off_the_user_layer() {
    // `build_keymap_layers` applies the stripping to the user layer: a user
    // binding `run, program /usr/bin/htop` comes out as a bare `run`, which
    // names no program.
    let key = build_single_chord_sequence(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action_reference: build_core_action("run"),
        action_arguments: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            arguments: vec![],
            direction: None,
            should_stack: false,
        },
    };
    let mut modes = BTreeMap::new();
    modes.insert(
        parse_mode_name("normal"),
        ModeBindings {
            bound_action_by_key_sequence: [(key.clone(), smuggled)].into_iter().collect(),
            removed_key_sequences: BTreeSet::new(),
        },
    );

    let layers = build_keymap_layers(Some(modes), Leader::default());

    let user = layers
        .iter()
        .find(|layer| layer.origin == LayerOrigin::User)
        .expect("a user layer was supplied, so one comes back");
    assert_eq!(
        user.mode_bindings_by_name[&parse_mode_name("normal")].bound_action_by_key_sequence[&key],
        build_bound_action("run")
    );
}

#[test]
fn build_keymap_layers_leaves_the_defaults_layer_untouched() {
    // The defaults layer keeps its arguments: `resize-pane` keeps the
    // amount it ships with.
    let layers = build_keymap_layers(None, Leader::default());

    assert_eq!(layers.len(), 1, "no user modes means the defaults alone");
    assert_eq!(layers[0].origin, LayerOrigin::Defaults);
    assert_eq!(
        layers[0].mode_bindings_by_name,
        build_default_mode_bindings(Leader::default()),
        "the defaults layer is the default table verbatim, arguments included"
    );
}

#[test]
fn stripping_leaves_the_defaults_layer_alone() {
    // `strip_user_arguments` returns the defaults layer untouched.
    assert_eq!(
        build_default_key_map_layer().strip_user_arguments(),
        build_default_key_map_layer()
    );
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
fn list_builtin_mode_names_returns_every_lock_mode() {
    let expected_mode_names = BTreeSet::from(
        ["normal", "locked", "resize", "move-pane", "tab", "scroll"].map(parse_mode_name),
    );
    assert_eq!(list_builtin_mode_names(), expected_mode_names);
}

#[test]
fn defaults_alone_report_nothing() {
    let report = detect_test_conflicts(&[build_default_key_map_layer()]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn empty_report_applies() {
    assert_eq!(
        ConflictReport::default().get_verdict(),
        KeymapVerdict::Apply
    );
}

#[test]
fn user_vs_session_same_key_different_action_collides() {
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let layers = [
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ];
    let report = detect_test_conflicts(&layers);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            binding_claims: vec![
                (LayerOrigin::User, build_bound_action("new-tab")),
                (LayerOrigin::Session, build_bound_action("lock")),
            ],
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn three_layers_with_three_distinct_actions_all_appear_in_the_collision() {
    // A collision lists every distinct claimant: three layers binding three
    // distinct actions give three claims.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), build_bound_action("quit"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            binding_claims: vec![
                (LayerOrigin::User, build_bound_action("new-tab")),
                (LayerOrigin::Session, build_bound_action("lock")),
                (LayerOrigin::Layout, build_bound_action("quit")),
            ],
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn a_repeated_claim_across_nonadjacent_layers_dedups_against_a_third_distinct_one() {
    // User and Layout bind the identical action; Session's differing claim
    // sits between them. Dedup compares against every earlier distinct
    // claim: the result is exactly two distinct claims.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            binding_claims: vec![
                (LayerOrigin::User, build_bound_action("new-tab")),
                (LayerOrigin::Session, build_bound_action("lock")),
            ],
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn steal_of_a_defaulted_key_is_not_a_collision() {
    // `<A-t>` is the default new-tab key; one user layer takes it.
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(
                build_single_chord_sequence(ModFlags::ALT, 't'),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn identical_bound_action_in_two_user_layers_passes() {
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key, build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn same_action_with_different_args_collides() {
    // Unrepresentable from user files (their args are stripped), but the
    // type still allows it for system-authored layers. The collision is
    // judged on the whole bound value, args included.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'e');
    let run_with = |program: &str| BoundAction {
        action_reference: build_core_action("run"),
        action_arguments: ActionArgs::Run {
            program: PathBuf::from(program),
            arguments: vec![],
            direction: None,
            should_stack: false,
        },
    };
    let layers = [
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), run_with("/usr/bin/htop"))],
        ),
        build_key_map_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), run_with("/usr/bin/btop"))],
        ),
    ];
    let report = detect_test_conflicts(&layers);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            binding_claims: vec![
                (LayerOrigin::User, run_with("/usr/bin/htop")),
                (LayerOrigin::Layout, run_with("/usr/bin/btop")),
            ],
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn orphan_actions_on_a_shared_key_do_not_collide() {
    // Both claims name unregistered actions: inactive bindings, warned as
    // orphans, re-judged when detection re-runs at registration.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let ghost = |action_name: &str| BoundAction {
        action_reference: ActionReference::from_user_action_name(action_name)
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(LayerOrigin::User, "normal", vec![(key.clone(), ghost("a"))]),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), ghost("b"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: key.clone(),
                action_reference: ghost("a").action_reference,
            },
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::Session,
                mode_name: parse_mode_name("normal"),
                key_sequence: key,
                action_reference: ghost("b").action_reference,
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn one_orphan_claim_does_not_collide_with_a_live_one() {
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let ghost = BoundAction {
        action_reference: ActionReference::from_user_action_name("ghost")
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), ghost.clone())],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            action_reference: ghost.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn bindings_in_an_orphan_mode_do_not_collide() {
    let key = build_single_chord_sequence(ModFlags::ALT, 's');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "git",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "git",
            vec![(key, build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::OrphanMode {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("git"),
            },
            ConflictDiagnostic::OrphanMode {
                layer_origin: LayerOrigin::Session,
                mode_name: parse_mode_name("git"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn coming_soon_binding_warns_without_revert() {
    // `core:copy-selection` is seeded but not implemented; the binding cannot fire.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("copy-selection"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ComingSoonAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            action_reference: build_core_action("copy-selection"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn coming_soon_claims_do_not_collide() {
    // Neither binding can fire in this build; the collision surfaces at
    // the first load of a build that implements the actions.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("copy-selection"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("plugin-install"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::ComingSoonAction {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: key.clone(),
                action_reference: build_core_action("copy-selection"),
            },
            ConflictDiagnostic::ComingSoonAction {
                layer_origin: LayerOrigin::Session,
                mode_name: parse_mode_name("normal"),
                key_sequence: key,
                action_reference: build_core_action("plugin-install"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn unresolvable_args_binding_warns_and_does_not_collide() {
    // The user layer's binding carries arguments `core:lock` cannot take
    // and never fires; the session layer's working binding applies with no
    // revert.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let broken = BoundAction {
        action_reference: build_core_action("lock"),
        action_arguments: ActionArgs::Run {
            program: PathBuf::from("/usr/bin/htop"),
            arguments: vec![],
            direction: None,
            should_stack: false,
        },
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(LayerOrigin::User, "normal", vec![(key.clone(), broken)]),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::UnresolvableArgs {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            action_reference: build_core_action("lock"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn rebinding_the_reserved_unlock_is_fatal() {
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockShadowed {
            layer_origin: LayerOrigin::User,
            action_reference: build_core_action("lock"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn unlock_with_wrong_arguments_is_dead_not_a_shadow() {
    // `core:unlock` fires only with no arguments: this binding never
    // fires, it is transparent, and the default unlock beneath it wins the
    // reserved chord.
    let key = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(
                key.clone(),
                BoundAction {
                    action_reference: build_core_action("unlock"),
                    action_arguments: ActionArgs::Run {
                        program: PathBuf::from("/usr/bin/htop"),
                        arguments: vec![],
                        direction: None,
                        should_stack: false,
                    },
                },
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::UnresolvableArgs {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("locked"),
            key_sequence: key,
            action_reference: build_core_action("unlock"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_unlock_claims_do_not_collide() {
    // Both layers bind a locked-mode sequence the reserved chord swallows;
    // neither can ever fire. Each is warned dead, with no collision and no
    // revert.
    let key = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "locked",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: key.clone(),
                action_reference: build_core_action("lock"),
            },
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::Session,
                key_sequence: key,
                action_reference: build_core_action("new-tab"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_locked_sequence_holding_the_reserved_chord_anywhere_is_dead() {
    // `<C-x> <C-l>` does not OPEN with the reserved chord. The input path
    // resolves the unlock the instant it is pressed, open sequence or not:
    // the `<C-l>` unlocks and `core:new-tab` never runs. A locked sequence
    // holding the chord at any position is warned dead.
    let key = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            layer_origin: LayerOrigin::User,
            key_sequence: key,
            action_reference: build_core_action("new-tab"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn the_one_chord_unlock_binding_itself_stays_live() {
    // The dead judgment covers only sequences of two or more chords that
    // hold the reserved chord. Locked mode's own one-chord `<C-l>` →
    // `core:unlock` fires and draws no warning.
    let report = detect_test_conflicts(&[build_default_key_map_layer()]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_unlock_sequences_do_not_pair_as_prefixes() {
    // `<C-l> x` is a strict prefix of `<C-l> x y`, but both hold the
    // reserved chord: two dead warnings, no ambiguous-prefix pair.
    let short = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let long = KeySequence::from_first_and_rest(
        KeybindingsConfig::RESERVED_UNLOCK,
        vec![
            build_character_chord(ModFlags::NONE, 'x'),
            build_character_chord(ModFlags::NONE, 'y'),
        ],
    );
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![
                (short.clone(), build_bound_action("lock")),
                (long.clone(), build_bound_action("new-tab")),
            ],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: short,
                action_reference: build_core_action("lock"),
            },
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: long,
                action_reference: build_core_action("new-tab"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn dead_binding_does_not_warn_typeable() {
    // `g` opens typeable, but the binding is orphaned and steals nothing;
    // it gets exactly the orphan warning, not a stealing warning on top.
    let key = build_single_chord_sequence(ModFlags::NONE, 'g');
    let ghost = BoundAction {
        action_reference: ActionReference::from_user_action_name("ghost")
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), ghost.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            action_reference: ghost.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_bindings_skip_per_binding_warns() {
    // The whole overlay is inactive: one mode warning, no orphan-action or
    // typeable warnings for the bindings inside it.
    let ghost = BoundAction {
        action_reference: ActionReference::from_user_action_name("ghost")
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "git",
            vec![(build_single_chord_sequence(ModFlags::NONE, 'g'), ghost)],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("git"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_on_the_reserved_chord_does_not_shadow() {
    // The higher layer's binding names an unregistered action: inactive,
    // transparent, and the default unlock beneath it still fires. Only the
    // orphan warning is reported; the keymap is not rejected.
    let key = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let ghost = BoundAction {
        action_reference: ActionReference::from_user_action_name("ghost")
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), ghost.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("locked"),
            key_sequence: key,
            action_reference: ghost.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn shadow_with_a_bound_alternative_passes() {
    let alternative = build_character_chord(ModFlags::CTRL, 'u');
    let layers = [
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![
                (
                    KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                    build_bound_action("lock"),
                ),
                (KeySequence::from(alternative), build_bound_action("unlock")),
            ],
        ),
    ];
    let report = detect_conflicts(
        &layers,
        Leader::default(),
        Some(alternative),
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn declared_but_unbound_alternative_is_fatal() {
    let alternative = build_character_chord(ModFlags::CTRL, 'u');
    let report = detect_conflicts(
        &[build_default_key_map_layer()],
        Leader::default(),
        Some(alternative),
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved_unlock_chord: alternative,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn typeable_alternative_is_fatal() {
    let alternative = build_character_chord(ModFlags::NONE, 'u');
    let report = detect_conflicts(
        &[build_default_key_map_layer()],
        Leader::default(),
        Some(alternative),
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::UnlockAlternativeTypeable {
                unlock_alternative_chord: alternative,
            },
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved_unlock_chord: alternative,
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn no_layers_report_missing_required_bindings() {
    let report = detect_test_conflicts(&[]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved_unlock_chord: KeybindingsConfig::RESERVED_UNLOCK,
            },
            ConflictDiagnostic::MovePaneCancelBindingMissing,
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn user_prefix_of_default_sequences_warns_without_revert() {
    // The defaults bind `<C-p> n`, the four `<C-p>` vim-letter splits,
    // `<C-p> x`, and the four `<C-p>` arrow focus sequences; the user binds
    // bare `<C-p>`.
    let prefix = build_single_chord_sequence(ModFlags::CTRL, 'p');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix.clone(), build_bound_action("lock"))],
        ),
    ]);
    let ambiguous = |longer_key: Key, longer_action: &str| ConflictDiagnostic::AmbiguousPrefix {
        mode_name: parse_mode_name("normal"),
        prefix_sequence: prefix.clone(),
        prefix_action_reference: build_core_action("lock"),
        longer_sequence: build_two_chord_sequence(
            build_character_chord(ModFlags::CTRL, 'p'),
            KeyChord::from_parts(ModFlags::NONE, longer_key),
        ),
        longer_action_reference: build_core_action(longer_action),
    };
    assert_eq!(
        report.diagnostics,
        vec![
            ambiguous(Key::Char('h'), "new-pane-left"),
            ambiguous(Key::Char('j'), "new-pane-down"),
            ambiguous(Key::Char('k'), "new-pane-up"),
            ambiguous(Key::Char('l'), "new-pane-right"),
            ambiguous(Key::Char('m'), "move-pane"),
            ambiguous(Key::Char('n'), "new-pane"),
            ambiguous(Key::Char('x'), "close-pane-tree"),
            ambiguous(Key::Named(NamedKey::Left), "focus-pane-left"),
            ambiguous(Key::Named(NamedKey::Right), "focus-pane-right"),
            ambiguous(Key::Named(NamedKey::Up), "focus-pane-up"),
            ambiguous(Key::Named(NamedKey::Down), "focus-pane-down"),
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_three_deep_prefix_chain_reports_every_pair() {
    // `<C-y>`, `<C-y> n`, and `<C-y> n o` are each a prefix of the ones
    // longer than it: three pairs total.
    let short = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let mid = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'y'),
        build_character_chord(ModFlags::NONE, 'n'),
    );
    let long = KeySequence::from_first_and_rest(
        build_character_chord(ModFlags::CTRL, 'y'),
        vec![
            build_character_chord(ModFlags::NONE, 'n'),
            build_character_chord(ModFlags::NONE, 'o'),
        ],
    );
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![
                (short.clone(), build_bound_action("lock")),
                (mid.clone(), build_bound_action("new-tab")),
                (long.clone(), build_bound_action("quit")),
            ],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: short.clone(),
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: mid.clone(),
                longer_action_reference: build_core_action("new-tab"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: short,
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: long.clone(),
                longer_action_reference: build_core_action("quit"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: mid,
                prefix_action_reference: build_core_action("new-tab"),
                longer_sequence: long,
                longer_action_reference: build_core_action("quit"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn prefix_pairs_do_not_cross_modes() {
    // `<C-y>` bound in normal and `<C-y> x` bound in locked do not pair:
    // prefixes are judged within one mode.
    let short = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let long = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'y'),
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let user = KeymapLayer {
        origin: LayerOrigin::User,
        mode_bindings_by_name: BTreeMap::from([
            (
                parse_mode_name("normal"),
                ModeBindings {
                    bound_action_by_key_sequence: [(short, build_bound_action("lock"))]
                        .into_iter()
                        .collect(),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
            (
                parse_mode_name("locked"),
                ModeBindings {
                    bound_action_by_key_sequence: [(long, build_bound_action("new-tab"))]
                        .into_iter()
                        .collect(),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
        ]),
    };
    let report = detect_test_conflicts(&[build_default_key_map_layer(), user]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn the_reserved_chord_opening_a_normal_mode_sequence_is_an_ordinary_prefix_pair() {
    // The reserved chord is only swallowed in LOCKED mode; the identical
    // chord opening a longer sequence in NORMAL mode is an ordinary
    // ambiguous-prefix warning, not a dead binding.
    let short = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let long = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(long.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::AmbiguousPrefix {
            mode_name: parse_mode_name("normal"),
            prefix_sequence: short,
            prefix_action_reference: build_core_action("lock"),
            longer_sequence: long,
            longer_action_reference: build_core_action("new-tab"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_redundant_remove_at_higher_precedence_voids_a_rebind_below_it() {
    // Two layers remove the same key; the LAST (highest-index) remove sets
    // the index a claim must beat. Removal is positional, not per-origin: a
    // stack may hold several layers of one origin. User removes the key
    // (index 1, no bind), Session rebinds it without removing (index 2), a
    // first layout layer removes it again (index 3, no bind), a second
    // layout layer rebinds with a different action (index 4). The remove at
    // index 3 voids Session's rebind at index 2, leaving the top claim
    // alone and nothing to collide with.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
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
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer_with_removed(
            LayerOrigin::Layout,
            "normal",
            Vec::new(),
            vec![key.clone()],
        ),
        build_key_map_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn locked_sequence_opening_with_the_reserved_chord_is_dead_not_ambiguous() {
    let key = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            layer_origin: LayerOrigin::User,
            key_sequence: key,
            action_reference: build_core_action("lock"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_action_warns_without_revert() {
    let key = build_single_chord_sequence(ModFlags::CTRL, 'o');
    let orphan = BoundAction {
        action_reference: ActionReference::from_user_action_name("my-macro")
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), orphan.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            action_reference: orphan.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_warns_without_revert() {
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "git",
            vec![(
                build_single_chord_sequence(ModFlags::ALT, 's'),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("git"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_opening_chord_warns() {
    let key = build_single_chord_sequence(ModFlags::NONE, 'g');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableBinding {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            action_reference: build_core_action("lock"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_subsequent_chord_does_not_warn() {
    // Only the opening chord matters: a plain second chord is read while
    // the pending sequence is live.
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(
                build_two_chord_sequence(
                    build_character_chord(ModFlags::CTRL, 'p'),
                    build_character_chord(ModFlags::NONE, 'g'),
                ),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn shift_only_mods_leader_warns() {
    let report = detect_conflicts(
        &[build_default_key_map_layer()],
        Leader::Mods(ModFlags::SHIFT),
        None,
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableLeader {
            leader: Leader::Mods(ModFlags::SHIFT),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_chord_leader_warns() {
    let leader = Leader::Chord(build_character_chord(ModFlags::NONE, ','));
    let report = detect_conflicts(
        &[build_default_key_map_layer()],
        leader,
        None,
        TEST_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableLeader { leader }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn non_typeable_leaders_do_not_warn() {
    for leader in [
        Leader::Mods(ModFlags::CTRL),
        Leader::Mods(ModFlags::ALT.union(ModFlags::SHIFT)),
        Leader::Chord(build_character_chord(ModFlags::CTRL, 'b')),
    ] {
        let report = detect_conflicts(
            &[build_default_key_map_layer()],
            leader,
            None,
            TEST_CHORD_DEPTH,
            &ActionRegistry::new(),
        );
        assert_eq!(report.diagnostics, Vec::new());
    }
}

#[test]
fn a_fatal_finding_outranks_a_collision() {
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer(
            LayerOrigin::Layout,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::KeyCollision {
                mode_name: parse_mode_name("normal"),
                key_sequence: key,
                binding_claims: vec![
                    (LayerOrigin::User, build_bound_action("new-tab")),
                    (LayerOrigin::Session, build_bound_action("lock")),
                ],
            },
            ConflictDiagnostic::ReservedUnlockShadowed {
                layer_origin: LayerOrigin::Layout,
                action_reference: build_core_action("lock"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn severity_table() {
    let claims = vec![
        (LayerOrigin::User, build_bound_action("new-tab")),
        (LayerOrigin::Session, build_bound_action("lock")),
    ];
    let cases = [
        (
            ConflictDiagnostic::KeyCollision {
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'y'),
                binding_claims: claims,
            },
            ConflictSeverity::Collision,
        ),
        (
            ConflictDiagnostic::ReservedUnlockShadowed {
                layer_origin: LayerOrigin::User,
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved_unlock_chord: KeybindingsConfig::RESERVED_UNLOCK,
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::UnlockAlternativeTypeable {
                unlock_alternative_chord: build_character_chord(ModFlags::NONE, 'u'),
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::MovePaneCancelBindingMissing,
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: build_single_chord_sequence(ModFlags::CTRL, 'p'),
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: build_two_chord_sequence(
                    build_character_chord(ModFlags::CTRL, 'p'),
                    build_character_chord(ModFlags::NONE, 'n'),
                ),
                longer_action_reference: build_core_action("new-pane"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: build_two_chord_sequence(
                    KeybindingsConfig::RESERVED_UNLOCK,
                    build_character_chord(ModFlags::NONE, 'x'),
                ),
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::ComingSoonAction {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'y'),
                action_reference: build_core_action("copy-selection"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::UnresolvableArgs {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'y'),
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'o'),
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanMode {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("git"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::TypeableBinding {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(ModFlags::NONE, 'g'),
                action_reference: build_core_action("lock"),
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
        assert_eq!(diagnostic.get_severity(), severity, "{diagnostic:?}");
    }
}

#[test]
fn display_messages_are_exact() {
    let collision = ConflictDiagnostic::KeyCollision {
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'y'),
        binding_claims: vec![
            (LayerOrigin::User, build_bound_action("new-tab")),
            (LayerOrigin::Session, build_bound_action("lock")),
        ],
    };
    assert_eq!(
        collision.to_string(),
        "key `<C-y>` in mode `normal` is bound by user to `core:new-tab` and by session \
         to `core:lock`; all user keybindings revert to defaults"
    );

    let prefix = ConflictDiagnostic::AmbiguousPrefix {
        mode_name: parse_mode_name("normal"),
        prefix_sequence: build_single_chord_sequence(ModFlags::CTRL, 'p'),
        prefix_action_reference: build_core_action("lock"),
        longer_sequence: build_two_chord_sequence(
            build_character_chord(ModFlags::CTRL, 'p'),
            build_character_chord(ModFlags::NONE, 'n'),
        ),
        longer_action_reference: build_core_action("new-pane"),
    };
    assert_eq!(
        prefix.to_string(),
        "`<C-p>` (`core:lock`) is a prefix of `<C-p> n` (`core:new-pane`) in mode \
         `normal`; the shorter binding fires only on the chord timeout"
    );

    let shadowed = ConflictDiagnostic::ReservedUnlockShadowed {
        layer_origin: LayerOrigin::User,
        action_reference: build_core_action("lock"),
    };
    assert_eq!(
        shadowed.to_string(),
        "the reserved unlock key is bound by user to `core:lock` in locked mode; \
         declare `unlock_alternative` before rebinding it"
    );

    let missing = ConflictDiagnostic::ReservedUnlockMissing {
        reserved_unlock_chord: KeybindingsConfig::RESERVED_UNLOCK,
    };
    assert_eq!(
        missing.to_string(),
        "locked mode has no binding from `<C-l>` to `core:unlock`; the unlock escape \
         would be unreachable"
    );

    let typeable_alt = ConflictDiagnostic::UnlockAlternativeTypeable {
        unlock_alternative_chord: build_character_chord(ModFlags::NONE, 'u'),
    };
    assert_eq!(
        typeable_alt.to_string(),
        "`unlock_alternative` `u` is a key plain typing produces; hold Ctrl, Alt, or Super"
    );

    let move_pane_cancel_binding_missing = ConflictDiagnostic::MovePaneCancelBindingMissing;
    assert_eq!(
        move_pane_cancel_binding_missing.to_string(),
        "the `move-pane` mode has no live `core:cancel-pane-move` binding; bind that action to a key \
         before removing its last cancellation key"
    );

    let dead = ConflictDiagnostic::DeadUnderReservedUnlock {
        layer_origin: LayerOrigin::User,
        key_sequence: build_two_chord_sequence(
            KeybindingsConfig::RESERVED_UNLOCK,
            build_character_chord(ModFlags::NONE, 'x'),
        ),
        action_reference: build_core_action("lock"),
    };
    assert_eq!(
        dead.to_string(),
        "`<C-l> x` (user, `core:lock`) in locked mode can never fire: it holds the \
         reserved unlock chord, which resolves instantly wherever it is pressed"
    );

    let same_action_collision = ConflictDiagnostic::KeyCollision {
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'e'),
        binding_claims: vec![
            (
                LayerOrigin::User,
                BoundAction {
                    action_reference: build_core_action("run"),
                    action_arguments: ActionArgs::Run {
                        program: PathBuf::from("/usr/bin/htop"),
                        arguments: vec![],
                        direction: None,
                        should_stack: false,
                    },
                },
            ),
            (
                LayerOrigin::Layout,
                BoundAction {
                    action_reference: build_core_action("run"),
                    action_arguments: ActionArgs::Run {
                        program: PathBuf::from("/usr/bin/btop"),
                        arguments: vec![],
                        direction: None,
                        should_stack: false,
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
        layer_origin: LayerOrigin::User,
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'y'),
        action_reference: build_core_action("lock"),
    };
    assert_eq!(
        unresolvable.to_string(),
        "`<C-y>` in mode `normal` (user) binds `core:lock` with arguments it cannot \
         take; the binding can never fire as written"
    );

    let coming_soon = ConflictDiagnostic::ComingSoonAction {
        layer_origin: LayerOrigin::User,
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'y'),
        action_reference: build_core_action("copy-selection"),
    };
    assert_eq!(
        coming_soon.to_string(),
        "`<C-y>` in mode `normal` (user) binds `core:copy-selection`, which is not implemented \
         yet; the binding cannot fire until it is"
    );

    let orphan_action = ConflictDiagnostic::OrphanAction {
        layer_origin: LayerOrigin::User,
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(ModFlags::CTRL, 'o'),
        action_reference: ActionReference::from_user_action_name("my-macro")
            .expect("valid user action name"),
    };
    assert_eq!(
        orphan_action.to_string(),
        "`<C-o>` in mode `normal` (user) names unknown action `user:my-macro`; the \
         binding is inactive until the action is registered"
    );

    let orphan_mode = ConflictDiagnostic::OrphanMode {
        layer_origin: LayerOrigin::Session,
        mode_name: parse_mode_name("git"),
    };
    assert_eq!(
        orphan_mode.to_string(),
        "the session keymap binds keys in unregistered mode `git`; those bindings are \
         inactive until the mode is registered"
    );

    let typeable_binding = ConflictDiagnostic::TypeableBinding {
        layer_origin: LayerOrigin::User,
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(ModFlags::NONE, 'g'),
        action_reference: build_core_action("lock"),
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
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
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
            vec![key],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn remove_without_rebind_voids_the_lower_claim() {
    // The user layer binds the key, session only removes it: one claim,
    // voided — no collision, and the key reaches nothing.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer_with_removed(LayerOrigin::Session, "normal", Vec::new(), vec![key]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn remove_below_both_claims_does_not_stop_their_collision() {
    // A remove voids only LOWER layers' claims: with the remove at the
    // bottom user layer, the two claims above it still collide.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
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
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence: key,
            binding_claims: vec![
                (LayerOrigin::Session, build_bound_action("new-tab")),
                (LayerOrigin::Layout, build_bound_action("lock")),
            ],
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn remove_above_both_claims_voids_the_collision() {
    // A remove above both claims voids both: no collision, and no warning
    // fires.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer_with_removed(LayerOrigin::Layout, "normal", Vec::new(), vec![key]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn removing_the_locked_unlock_binding_is_fatal() {
    // Clearing the reserved chord's binding in locked mode leaves no unlock
    // escape: the effective map misses it, and the keymap is refused.
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "locked",
            Vec::new(),
            vec![KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved_unlock_chord: KeybindingsConfig::RESERVED_UNLOCK,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removing_the_move_pane_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        ModFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "move-pane",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::MovePaneCancelBindingMissing]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn rebinding_move_pane_cancel_action_keeps_the_effective_map_valid() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        ModFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let custom_cancel_sequence =
        KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('c')));
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "move-pane",
            vec![(
                custom_cancel_sequence,
                build_bound_action("cancel-pane-move"),
            )],
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn higher_layer_removing_the_last_move_pane_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        ModFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let custom_cancel_sequence =
        KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('c')));
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "move-pane",
            Vec::new(),
            vec![escape_sequence],
        ),
        build_key_map_layer(
            LayerOrigin::Session,
            "move-pane",
            vec![(
                custom_cancel_sequence.clone(),
                build_bound_action("cancel-pane-move"),
            )],
        ),
        build_key_map_layer_with_removed(
            LayerOrigin::Layout,
            "move-pane",
            Vec::new(),
            vec![custom_cancel_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::MovePaneCancelBindingMissing]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn replacing_move_pane_cancel_action_without_another_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        ModFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let replacement_sequence =
        KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('c')));
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer_with_removed(
            LayerOrigin::User,
            "move-pane",
            vec![(replacement_sequence, build_bound_action("lock"))],
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::MovePaneCancelBindingMissing]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removed_binding_draws_no_per_binding_warns() {
    // The user layer binds an orphan action on a typeable key; session
    // removes the key. The removed binding draws neither the orphan warning
    // nor the typeable warning.
    let key = build_single_chord_sequence(ModFlags::NONE, 'g');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("does-not-exist"))],
        ),
        build_key_map_layer_with_removed(LayerOrigin::Session, "normal", Vec::new(), vec![key]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn removed_prefix_binding_does_not_pair_as_a_prefix() {
    // A single-chord `<C-p>` binding would pair with the defaults' `<C-p> n`
    // and `<C-p> x` sequences; removing it above voids the pairing.
    let prefix = build_single_chord_sequence(ModFlags::CTRL, 'p');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix.clone(), build_bound_action("lock"))],
        ),
        build_key_map_layer_with_removed(LayerOrigin::Session, "normal", Vec::new(), vec![prefix]),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn binding_past_the_chord_depth_cap_warns_and_applies() {
    // At a cap of 1, a two-chord user binding is never reached: the input
    // path flushes the pending sequence before lookup. It warns, stays
    // transparent, and the keymap applies.
    let long = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'y'),
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let report = detect_conflicts(
        &[
            build_default_key_map_layer(),
            build_key_map_layer(
                LayerOrigin::User,
                "normal",
                vec![(long.clone(), build_bound_action("new-tab"))],
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
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence: long,
            action_reference: build_core_action("new-tab"),
            max_chord_depth: 1,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
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
            build_default_key_map_layer(),
            build_key_map_layer(
                LayerOrigin::User,
                "normal",
                vec![(
                    build_single_chord_sequence(ModFlags::CTRL, 'y'),
                    build_bound_action("new-tab"),
                )],
            ),
        ],
        Leader::default(),
        None,
        1,
        &ActionRegistry::new(),
    );
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_reserved_unlock_sequence_past_the_cap_warns_dead_not_depth() {
    // A locked two-chord sequence holding the reserved chord while over a
    // cap of 1 draws one warning, the reserved-chord one.
    let key = build_two_chord_sequence(
        build_character_chord(ModFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let report = detect_conflicts(
        &[
            build_default_key_map_layer(),
            build_key_map_layer(
                LayerOrigin::User,
                "locked",
                vec![(key.clone(), build_bound_action("new-tab"))],
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
            layer_origin: LayerOrigin::User,
            key_sequence: key,
            action_reference: build_core_action("new-tab"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn an_orphan_action_on_a_reserved_led_sequence_warns_orphan_not_dead() {
    // A locked sequence holding the reserved chord that also names an
    // unregistered action draws one warning, the resolver's refusal.
    let key = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(ModFlags::NONE, 'x'),
    );
    let ghost = BoundAction {
        action_reference: ActionReference::from_user_action_name("ghost")
            .expect("valid user action name"),
        action_arguments: ActionArgs::None,
    };
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "locked",
            vec![(key.clone(), ghost.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("locked"),
            key_sequence: key,
            action_reference: ghost.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_chord_depth_of_zero_fails_the_unlock_guarantee() {
    // With every sequence at least one chord, a cap of 0 makes the whole
    // keymap unreachable — including the locked-mode unlock and pane-move
    // cancellation bindings, which the guarantee checks report as missing.
    let report = detect_conflicts(
        &[build_default_key_map_layer()],
        Leader::default(),
        None,
        0,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved_unlock_chord: KeybindingsConfig::RESERVED_UNLOCK,
            },
            ConflictDiagnostic::MovePaneCancelBindingMissing,
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn remove_in_an_unregistered_mode_is_inert() {
    // Removals in an unknown mode are skipped like its bindings; only the
    // orphan-mode warning surfaces.
    let key = build_single_chord_sequence(ModFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_key_map_layer(),
        build_key_map_layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), build_bound_action("new-tab"))],
        ),
        build_key_map_layer_with_removed(LayerOrigin::Session, "git", Vec::new(), vec![key]),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            layer_origin: LayerOrigin::Session,
            mode_name: parse_mode_name("git"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_collision_naming_one_claim_does_not_say_the_arguments_differ() {
    // `detect_conflicts` never builds this, but the variant and its fields are
    // public: one claim names no second action to differ from.
    let one_claim = ConflictDiagnostic::KeyCollision {
        mode_name: ModeName::from_text("normal"),
        key_sequence: KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y'))),
        binding_claims: vec![(
            LayerOrigin::User,
            BoundAction {
                action_reference: ActionReference::from_core_action_name("lock")
                    .expect("a core action"),
                action_arguments: ActionArgs::None,
            },
        )],
    };

    assert_eq!(
        one_claim.to_string(),
        "key `<C-y>` in mode `normal` is bound by user to `core:lock`; \
         all user keybindings revert to defaults"
    );
}
