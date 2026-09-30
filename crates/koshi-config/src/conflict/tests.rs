//! Tests for keybinding conflict detection: every conflict class, the
//! steal/collision line, the reserved-unlock guarantee with and without an
//! alternative, verdict precedence, and the exact user-facing messages.

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

fn build_core_action(action_name: &str) -> ActionReference {
    ActionReference::from_core_action_name(action_name)
        .expect("test action name satisfies the grammar")
}

fn build_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: build_core_action(action_name),
    }
}

fn parse_mode_name(mode_name: &str) -> ModeName {
    ModeName::from_text(mode_name)
}

/// A one-mode layer built from `(sequence, bound action)` binding entries.
fn build_keymap_layer(
    origin: LayerOrigin,
    mode_name: &str,
    binding_entries: Vec<(KeySequence, BoundAction)>,
) -> KeymapLayer {
    build_keymap_layer_with_removed(origin, mode_name, binding_entries, Vec::new())
}

/// A one-mode layer built from `(sequence, bound action)` binding entries plus the
/// keys it removes.
fn build_keymap_layer_with_removed(
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
fn build_default_keymap_layer() -> KeymapLayer {
    KeymapLayer {
        origin: LayerOrigin::Defaults,
        mode_bindings_by_name: KeybindingsConfig::default().mode_bindings_by_name,
    }
}

/// The chord-depth cap the tests run under, matching the shipped default.
const TEST_MAXIMUM_CHORD_DEPTH: u8 = 4;

/// Runs detection with the default leader, no unlock alternative, and the
/// seeded core registry.
fn detect_test_conflicts(layers: &[KeymapLayer]) -> ConflictReport {
    detect_conflicts(
        layers,
        Leader::default(),
        None,
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    )
}

#[test]
fn build_keymap_layers_appends_the_user_layer_verbatim() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'n');
    let mut user_mode_bindings_by_name = BTreeMap::new();
    user_mode_bindings_by_name.insert(
        parse_mode_name("normal"),
        ModeBindings {
            bound_action_by_key_sequence: [(key_sequence.clone(), build_bound_action("run"))]
                .into_iter()
                .collect(),
            removed_key_sequences: BTreeSet::new(),
        },
    );

    let layers = build_keymap_layers(Some(user_mode_bindings_by_name.clone()), Leader::default());

    assert_eq!(layers.len(), 2);
    assert_eq!(layers[1].origin, LayerOrigin::User);
    assert_eq!(layers[1].mode_bindings_by_name, user_mode_bindings_by_name);
}

#[test]
fn build_keymap_layers_leaves_the_defaults_layer_untouched() {
    let layers = build_keymap_layers(None, Leader::default());

    assert_eq!(layers.len(), 1, "no user modes means the defaults alone");
    assert_eq!(layers[0].origin, LayerOrigin::Defaults);
    assert_eq!(
        layers[0].mode_bindings_by_name,
        build_default_mode_bindings(Leader::default()),
        "the defaults layer is the default table verbatim"
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
        [
            "normal",
            "locked",
            "resize",
            "pane-placement",
            "tab",
            "scroll",
        ]
        .map(parse_mode_name),
    );
    assert_eq!(list_builtin_mode_names(), expected_mode_names);
}

#[test]
fn defaults_alone_report_nothing() {
    let report = detect_test_conflicts(&[build_default_keymap_layer()]);
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let layers = [
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ];
    let report = detect_test_conflicts(&layers);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence,
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("quit"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence,
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence,
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
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(
                build_single_chord_sequence(BindingModifierFlags::ALT, 't'),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn identical_bound_action_in_two_user_layers_passes() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence, build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_actions_on_a_shared_key_do_not_collide() {
    // Both claims name unregistered actions: inactive bindings, warned as
    // orphans.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("ghost-a"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("ghost-b"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: key_sequence.clone(),
                action_reference: build_core_action("ghost-a"),
            },
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::Session,
                mode_name: parse_mode_name("normal"),
                key_sequence,
                action_reference: build_core_action("ghost-b"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn one_orphan_claim_does_not_collide_with_a_live_one() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let orphan_bound_action = build_bound_action("ghost");
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), orphan_bound_action.clone())],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn bindings_in_an_orphan_mode_do_not_collide() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 's');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "git",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "git",
            vec![(key_sequence, build_bound_action("new-tab"))],
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
fn an_action_needing_cli_arguments_warns_and_does_not_collide() {
    // `core:run` needs a program only `koshi run` supplies. The user layer's
    // binding never fires, and the session layer's binding applies with no
    // revert.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("run"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ArgumentsRequired {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence,
            action_reference: build_core_action("run"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn rebinding_the_reserved_unlock_is_fatal() {
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
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
fn a_cli_only_action_on_the_reserved_chord_is_dead_not_a_shadow() {
    // `core:run` never fires from a binding: it is transparent, and the
    // default unlock beneath it wins the reserved chord.
    let key_sequence = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("run"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ArgumentsRequired {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("locked"),
            key_sequence,
            action_reference: build_core_action("run"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_unlock_claims_do_not_collide() {
    // Both layers bind a locked-mode sequence the reserved chord swallows;
    // neither can ever fire. Each is warned dead, with no collision and no
    // revert.
    let key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: key_sequence.clone(),
                action_reference: build_core_action("lock"),
            },
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::Session,
                key_sequence,
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
    let key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            layer_origin: LayerOrigin::User,
            key_sequence,
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
    let report = detect_test_conflicts(&[build_default_keymap_layer()]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_unlock_sequences_do_not_pair_as_prefixes() {
    // `<C-l> x` is a strict prefix of `<C-l> x y`, but both hold the
    // reserved chord: two dead warnings, no ambiguous-prefix pair.
    let short_key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let long_key_sequence = KeySequence::from_first_and_rest(
        KeybindingsConfig::RESERVED_UNLOCK,
        vec![
            build_character_chord(BindingModifierFlags::NONE, 'x'),
            build_character_chord(BindingModifierFlags::NONE, 'y'),
        ],
    );
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![
                (short_key_sequence.clone(), build_bound_action("lock")),
                (long_key_sequence.clone(), build_bound_action("new-tab")),
            ],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: short_key_sequence,
                action_reference: build_core_action("lock"),
            },
            ConflictDiagnostic::DeadUnderReservedUnlock {
                layer_origin: LayerOrigin::User,
                key_sequence: long_key_sequence,
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::NONE, 'g');
    let orphan_bound_action = build_bound_action("ghost");
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), orphan_bound_action.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_bindings_skip_per_binding_warns() {
    // The whole overlay is inactive: one mode warning, no orphan-action or
    // typeable warnings for the bindings inside it.
    let orphan_bound_action = build_bound_action("ghost");
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "git",
            vec![(
                build_single_chord_sequence(BindingModifierFlags::NONE, 'g'),
                orphan_bound_action,
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
fn orphan_on_the_reserved_chord_does_not_shadow() {
    // The higher layer's binding names an unregistered action: inactive,
    // transparent, and the default unlock beneath it still fires. Only the
    // orphan warning is reported; the keymap is not rejected.
    let key_sequence = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let orphan_bound_action = build_bound_action("ghost");
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), orphan_bound_action.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("locked"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn shadow_with_a_bound_alternative_passes() {
    let unlock_alternative_chord = build_character_chord(BindingModifierFlags::CTRL, 'u');
    let layers = [
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![
                (
                    KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                    build_bound_action("lock"),
                ),
                (
                    KeySequence::from(unlock_alternative_chord),
                    build_bound_action("unlock"),
                ),
            ],
        ),
    ];
    let report = detect_conflicts(
        &layers,
        Leader::default(),
        Some(unlock_alternative_chord),
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn declared_but_unbound_alternative_is_fatal() {
    let unlock_alternative_chord = build_character_chord(BindingModifierFlags::CTRL, 'u');
    let report = detect_conflicts(
        &[build_default_keymap_layer()],
        Leader::default(),
        Some(unlock_alternative_chord),
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockMissing {
            reserved_unlock_chord: unlock_alternative_chord,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn typeable_alternative_is_fatal() {
    let unlock_alternative_chord = build_character_chord(BindingModifierFlags::NONE, 'u');
    let report = detect_conflicts(
        &[build_default_keymap_layer()],
        Leader::default(),
        Some(unlock_alternative_chord),
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::UnlockAlternativeTypeable {
                unlock_alternative_chord,
            },
            ConflictDiagnostic::ReservedUnlockMissing {
                reserved_unlock_chord: unlock_alternative_chord,
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
            ConflictDiagnostic::PanePlacementCancelBindingMissing,
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn user_prefix_of_default_sequences_warns_without_revert() {
    // The defaults bind `<C-p> n`, the four `<C-p>` vim-letter splits,
    // `<C-p> x`, and the four `<C-p>` arrow focus sequences; the user binds
    // bare `<C-p>`.
    let prefix_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'p');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix_key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    let build_ambiguous_prefix =
        |longer_key: Key, longer_action_name: &str| ConflictDiagnostic::AmbiguousPrefix {
            mode_name: parse_mode_name("normal"),
            prefix_sequence: prefix_key_sequence.clone(),
            prefix_action_reference: build_core_action("lock"),
            longer_sequence: build_two_chord_sequence(
                build_character_chord(BindingModifierFlags::CTRL, 'p'),
                KeyChord::from_parts(BindingModifierFlags::NONE, longer_key),
            ),
            longer_action_reference: build_core_action(longer_action_name),
        };
    assert_eq!(
        report.diagnostics,
        vec![
            build_ambiguous_prefix(Key::Char('h'), "new-pane-left"),
            build_ambiguous_prefix(Key::Char('j'), "new-pane-down"),
            build_ambiguous_prefix(Key::Char('k'), "new-pane-up"),
            build_ambiguous_prefix(Key::Char('l'), "new-pane-right"),
            build_ambiguous_prefix(Key::Char('m'), "begin-pane-placement"),
            build_ambiguous_prefix(Key::Char('n'), "new-pane"),
            build_ambiguous_prefix(Key::Char('s'), "new-pane-stacked"),
            build_ambiguous_prefix(Key::Char('x'), "close-pane-tree"),
            build_ambiguous_prefix(Key::Named(NamedKey::Left), "focus-pane-left"),
            build_ambiguous_prefix(Key::Named(NamedKey::Right), "focus-pane-right"),
            build_ambiguous_prefix(Key::Named(NamedKey::Up), "focus-pane-up"),
            build_ambiguous_prefix(Key::Named(NamedKey::Down), "focus-pane-down"),
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_three_deep_prefix_chain_reports_every_pair() {
    // `<C-y>`, `<C-y> n`, and `<C-y> n o` are each a prefix of the ones
    // longer than it: three pairs total.
    let short_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let middle_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'y'),
        build_character_chord(BindingModifierFlags::NONE, 'n'),
    );
    let long_key_sequence = KeySequence::from_first_and_rest(
        build_character_chord(BindingModifierFlags::CTRL, 'y'),
        vec![
            build_character_chord(BindingModifierFlags::NONE, 'n'),
            build_character_chord(BindingModifierFlags::NONE, 'o'),
        ],
    );
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![
                (short_key_sequence.clone(), build_bound_action("lock")),
                (middle_key_sequence.clone(), build_bound_action("new-tab")),
                (long_key_sequence.clone(), build_bound_action("quit")),
            ],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: short_key_sequence.clone(),
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: middle_key_sequence.clone(),
                longer_action_reference: build_core_action("new-tab"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: short_key_sequence,
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: long_key_sequence.clone(),
                longer_action_reference: build_core_action("quit"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: middle_key_sequence,
                prefix_action_reference: build_core_action("new-tab"),
                longer_sequence: long_key_sequence,
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
    let short_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let long_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'y'),
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let user_keymap_layer = KeymapLayer {
        origin: LayerOrigin::User,
        mode_bindings_by_name: BTreeMap::from([
            (
                parse_mode_name("normal"),
                ModeBindings {
                    bound_action_by_key_sequence: [(
                        short_key_sequence,
                        build_bound_action("lock"),
                    )]
                    .into_iter()
                    .collect(),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
            (
                parse_mode_name("locked"),
                ModeBindings {
                    bound_action_by_key_sequence: [(
                        long_key_sequence,
                        build_bound_action("new-tab"),
                    )]
                    .into_iter()
                    .collect(),
                    removed_key_sequences: BTreeSet::new(),
                },
            ),
        ]),
    };
    let report = detect_test_conflicts(&[build_default_keymap_layer(), user_keymap_layer]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn the_reserved_chord_opening_a_normal_mode_sequence_is_an_ordinary_prefix_pair() {
    // The reserved chord is only swallowed in LOCKED mode; the identical
    // chord opening a longer sequence in NORMAL mode is an ordinary
    // ambiguous-prefix warning, not a dead binding.
    let short_key_sequence = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let long_key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(long_key_sequence.clone(), build_bound_action("new-tab"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::AmbiguousPrefix {
            mode_name: parse_mode_name("normal"),
            prefix_sequence: short_key_sequence,
            prefix_action_reference: build_core_action("lock"),
            longer_sequence: long_key_sequence,
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Layout,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
        build_keymap_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn locked_sequence_opening_with_the_reserved_chord_is_dead_not_ambiguous() {
    let key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::DeadUnderReservedUnlock {
            layer_origin: LayerOrigin::User,
            key_sequence,
            action_reference: build_core_action("lock"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_action_warns_without_revert() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'o');
    let orphan_bound_action = build_bound_action("my-macro");
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), orphan_bound_action.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_warns_without_revert() {
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "git",
            vec![(
                build_single_chord_sequence(BindingModifierFlags::ALT, 's'),
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::NONE, 'g');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableBinding {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("normal"),
            key_sequence,
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
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(
                build_two_chord_sequence(
                    build_character_chord(BindingModifierFlags::CTRL, 'p'),
                    build_character_chord(BindingModifierFlags::NONE, 'g'),
                ),
                build_bound_action("lock"),
            )],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn shift_only_modifier_leader_warns() {
    let report = detect_conflicts(
        &[build_default_keymap_layer()],
        Leader::Modifiers(BindingModifierFlags::SHIFT),
        None,
        TEST_MAXIMUM_CHORD_DEPTH,
        &ActionRegistry::new(),
    );
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::TypeableLeader {
            leader: Leader::Modifiers(BindingModifierFlags::SHIFT),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn typeable_chord_leader_warns() {
    let leader = Leader::Chord(build_character_chord(BindingModifierFlags::NONE, ','));
    let report = detect_conflicts(
        &[build_default_keymap_layer()],
        leader,
        None,
        TEST_MAXIMUM_CHORD_DEPTH,
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
        Leader::Modifiers(BindingModifierFlags::CTRL),
        Leader::Modifiers(BindingModifierFlags::ALT.union(BindingModifierFlags::SHIFT)),
        Leader::Chord(build_character_chord(BindingModifierFlags::CTRL, 'b')),
    ] {
        let report = detect_conflicts(
            &[build_default_keymap_layer()],
            leader,
            None,
            TEST_MAXIMUM_CHORD_DEPTH,
            &ActionRegistry::new(),
        );
        assert_eq!(report.diagnostics, Vec::new());
    }
}

#[test]
fn a_fatal_finding_outranks_a_collision() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer(
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
                key_sequence,
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
fn every_diagnostic_maps_to_its_exact_severity() {
    let binding_claims = vec![
        (LayerOrigin::User, build_bound_action("new-tab")),
        (LayerOrigin::Session, build_bound_action("lock")),
    ];
    let severity_cases = [
        (
            ConflictDiagnostic::KeyCollision {
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'y'),
                binding_claims,
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
                unlock_alternative_chord: build_character_chord(BindingModifierFlags::NONE, 'u'),
            },
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::PanePlacementCancelBindingMissing,
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: parse_mode_name("normal"),
                prefix_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'p'),
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: build_two_chord_sequence(
                    build_character_chord(BindingModifierFlags::CTRL, 'p'),
                    build_character_chord(BindingModifierFlags::NONE, 'n'),
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
                    build_character_chord(BindingModifierFlags::NONE, 'x'),
                ),
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::ArgumentsRequired {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'y'),
                action_reference: build_core_action("run"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::User,
                mode_name: parse_mode_name("normal"),
                key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'o'),
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
                key_sequence: build_single_chord_sequence(BindingModifierFlags::NONE, 'g'),
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::TypeableLeader {
                leader: Leader::Modifiers(BindingModifierFlags::SHIFT),
            },
            ConflictSeverity::Warning,
        ),
    ];
    for (conflict_diagnostic, expected_severity) in severity_cases {
        assert_eq!(
            conflict_diagnostic.get_severity(),
            expected_severity,
            "{conflict_diagnostic:?}"
        );
    }
}

#[test]
fn display_messages_are_exact() {
    let key_collision = ConflictDiagnostic::KeyCollision {
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'y'),
        binding_claims: vec![
            (LayerOrigin::User, build_bound_action("new-tab")),
            (LayerOrigin::Session, build_bound_action("lock")),
        ],
    };
    assert_eq!(
        key_collision.to_string(),
        "key `<C-y>` in mode `normal` is bound by user to `core:new-tab` and by session \
         to `core:lock`; all user keybindings revert to defaults"
    );

    let ambiguous_prefix = ConflictDiagnostic::AmbiguousPrefix {
        mode_name: parse_mode_name("normal"),
        prefix_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'p'),
        prefix_action_reference: build_core_action("lock"),
        longer_sequence: build_two_chord_sequence(
            build_character_chord(BindingModifierFlags::CTRL, 'p'),
            build_character_chord(BindingModifierFlags::NONE, 'n'),
        ),
        longer_action_reference: build_core_action("new-pane"),
    };
    assert_eq!(
        ambiguous_prefix.to_string(),
        "`<C-p>` (`core:lock`) is a prefix of `<C-p> n` (`core:new-pane`) in mode \
         `normal`; the shorter binding fires only on the chord timeout"
    );

    let reserved_unlock_shadowed = ConflictDiagnostic::ReservedUnlockShadowed {
        layer_origin: LayerOrigin::User,
        action_reference: build_core_action("lock"),
    };
    assert_eq!(
        reserved_unlock_shadowed.to_string(),
        "the reserved unlock key is bound by user to `core:lock` in locked mode; \
         declare `unlock_alternative` before rebinding it"
    );

    let reserved_unlock_missing = ConflictDiagnostic::ReservedUnlockMissing {
        reserved_unlock_chord: KeybindingsConfig::RESERVED_UNLOCK,
    };
    assert_eq!(
        reserved_unlock_missing.to_string(),
        "locked mode has no binding from `<C-l>` to `core:unlock`; the unlock escape \
         would be unreachable"
    );

    let typeable_unlock_alternative = ConflictDiagnostic::UnlockAlternativeTypeable {
        unlock_alternative_chord: build_character_chord(BindingModifierFlags::NONE, 'u'),
    };
    assert_eq!(
        typeable_unlock_alternative.to_string(),
        "`unlock_alternative` `u` is a key plain typing produces; hold Ctrl, Alt, or Super"
    );

    let pane_placement_cancel_binding_missing =
        ConflictDiagnostic::PanePlacementCancelBindingMissing;
    assert_eq!(
        pane_placement_cancel_binding_missing.to_string(),
        "the `pane-placement` mode has no live `core:cancel-pane-placement` binding; bind that action to a key \
         before removing its last cancellation key"
    );

    let dead_under_reserved_unlock = ConflictDiagnostic::DeadUnderReservedUnlock {
        layer_origin: LayerOrigin::User,
        key_sequence: build_two_chord_sequence(
            KeybindingsConfig::RESERVED_UNLOCK,
            build_character_chord(BindingModifierFlags::NONE, 'x'),
        ),
        action_reference: build_core_action("lock"),
    };
    assert_eq!(
        dead_under_reserved_unlock.to_string(),
        "`<C-l> x` (user, `core:lock`) in locked mode can never fire: it holds the \
         reserved unlock chord, which resolves instantly wherever it is pressed"
    );

    let arguments_required = ConflictDiagnostic::ArgumentsRequired {
        layer_origin: LayerOrigin::User,
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'y'),
        action_reference: build_core_action("run"),
    };
    assert_eq!(
        arguments_required.to_string(),
        "`<C-y>` in mode `normal` (user) binds `core:run`, which needs arguments only \
         its CLI verb supplies; the binding can never fire"
    );

    let orphan_action = ConflictDiagnostic::OrphanAction {
        layer_origin: LayerOrigin::User,
        mode_name: parse_mode_name("normal"),
        key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'o'),
        action_reference: build_core_action("my-macro"),
    };
    assert_eq!(
        orphan_action.to_string(),
        "`<C-o>` in mode `normal` (user) names unknown action `core:my-macro`; the \
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
        key_sequence: build_single_chord_sequence(BindingModifierFlags::NONE, 'g'),
        action_reference: build_core_action("lock"),
    };
    assert_eq!(
        typeable_binding.to_string(),
        "`g` in mode `normal` (user, `core:lock`) opens with a key plain typing \
         produces; it steals that key from the pane"
    );

    let typeable_leader = ConflictDiagnostic::TypeableLeader {
        leader: Leader::Modifiers(BindingModifierFlags::SHIFT),
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
            vec![key_sequence],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn remove_without_rebind_voids_the_lower_claim() {
    // The user layer binds the key, session only removes it: one claim,
    // voided — no collision, and the key reaches nothing.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![key_sequence],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn remove_below_both_claims_does_not_stop_their_collision() {
    // A remove voids only LOWER layers' claims: with the remove at the
    // bottom user layer, the two claims above it still collide.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
            LayerOrigin::User,
            "normal",
            Vec::new(),
            vec![key_sequence.clone()],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode_name: parse_mode_name("normal"),
            key_sequence,
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
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Layout,
            "normal",
            Vec::new(),
            vec![key_sequence],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn removing_the_locked_unlock_binding_is_fatal() {
    // Clearing the reserved chord's binding in locked mode leaves no unlock
    // escape: the effective map misses it, and the keymap is refused.
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
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
fn removing_the_pane_placement_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
            LayerOrigin::User,
            "pane-placement",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::PanePlacementCancelBindingMissing]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn rebinding_pane_placement_cancel_action_keeps_the_effective_map_valid() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let custom_cancel_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('c'),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
            LayerOrigin::User,
            "pane-placement",
            vec![(
                custom_cancel_sequence,
                build_bound_action("cancel-pane-placement"),
            )],
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn higher_layer_removing_the_last_pane_placement_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let custom_cancel_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('c'),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
            LayerOrigin::User,
            "pane-placement",
            Vec::new(),
            vec![escape_sequence],
        ),
        build_keymap_layer(
            LayerOrigin::Session,
            "pane-placement",
            vec![(
                custom_cancel_sequence.clone(),
                build_bound_action("cancel-pane-placement"),
            )],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Layout,
            "pane-placement",
            Vec::new(),
            vec![custom_cancel_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::PanePlacementCancelBindingMissing]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn replacing_pane_placement_cancel_action_without_another_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let replacement_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::CTRL,
        Key::Char('c'),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed(
            LayerOrigin::User,
            "pane-placement",
            vec![(replacement_sequence, build_bound_action("lock"))],
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::PanePlacementCancelBindingMissing]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removed_binding_draws_no_per_binding_warns() {
    // The user layer binds an orphan action on a typeable key; session
    // removes the key. The removed binding draws neither the orphan warning
    // nor the typeable warning.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::NONE, 'g');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("does-not-exist"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![key_sequence],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn removed_prefix_binding_does_not_pair_as_a_prefix() {
    // A single-chord `<C-p>` binding pairs with the defaults' `<C-p> n` and
    // `<C-p> x` sequences. A higher layer that removes it voids the pairing.
    let prefix_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'p');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix_key_sequence.clone(), build_bound_action("lock"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Session,
            "normal",
            Vec::new(),
            vec![prefix_key_sequence],
        ),
    ]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn binding_past_the_chord_depth_cap_warns_and_applies() {
    // At a cap of 1, a two-chord user binding is never reached: the input
    // path flushes the pending sequence before lookup. It warns, stays
    // transparent, and the keymap applies.
    let long_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'y'),
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let report = detect_conflicts(
        &[
            build_default_keymap_layer(),
            build_keymap_layer(
                LayerOrigin::User,
                "normal",
                vec![(long_key_sequence.clone(), build_bound_action("new-tab"))],
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
            key_sequence: long_key_sequence,
            action_reference: build_core_action("new-tab"),
            maximum_chord_depth: 1,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
    assert_eq!(
        report.diagnostics[0].to_string(),
        "`<C-y> x` in mode `normal` (user, `core:new-tab`) is 2 chords, over the \
         `maximum_chord_depth` cap of 1; the binding can never fire"
    );
}

#[test]
fn binding_with_exactly_maximum_chord_depth_chords_fires() {
    // At a cap of 1, a one-chord user binding sits exactly at the cap,
    // fires, and draws no warning.
    let report = detect_conflicts(
        &[
            build_default_keymap_layer(),
            build_keymap_layer(
                LayerOrigin::User,
                "normal",
                vec![(
                    build_single_chord_sequence(BindingModifierFlags::CTRL, 'y'),
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
    let key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'x'),
        KeybindingsConfig::RESERVED_UNLOCK,
    );
    let report = detect_conflicts(
        &[
            build_default_keymap_layer(),
            build_keymap_layer(
                LayerOrigin::User,
                "locked",
                vec![(key_sequence.clone(), build_bound_action("new-tab"))],
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
            key_sequence,
            action_reference: build_core_action("new-tab"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn an_orphan_action_on_a_reserved_led_sequence_warns_orphan_not_dead() {
    // A locked sequence holding the reserved chord that also names an
    // unregistered action draws one warning, the resolver's refusal.
    let key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let orphan_bound_action = build_bound_action("ghost");
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(key_sequence.clone(), orphan_bound_action.clone())],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            layer_origin: LayerOrigin::User,
            mode_name: parse_mode_name("locked"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_chord_depth_of_zero_fails_the_unlock_guarantee() {
    // With every sequence at least one chord, a cap of 0 makes the whole
    // keymap unreachable — including the locked-mode unlock and pane placement
    // cancellation bindings, which the guarantee checks report as missing.
    let report = detect_conflicts(
        &[build_default_keymap_layer()],
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
            ConflictDiagnostic::PanePlacementCancelBindingMissing,
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn remove_in_an_unregistered_mode_is_inert() {
    // Removals in an unknown mode are skipped like its bindings; only the
    // orphan-mode warning surfaces.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("new-tab"))],
        ),
        build_keymap_layer_with_removed(
            LayerOrigin::Session,
            "git",
            Vec::new(),
            vec![key_sequence],
        ),
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
        key_sequence: KeySequence::from(KeyChord::from_parts(
            BindingModifierFlags::CTRL,
            Key::Char('y'),
        )),
        binding_claims: vec![(
            LayerOrigin::User,
            BoundAction {
                action_reference: ActionReference::from_core_action_name("lock")
                    .expect("a core action"),
            },
        )],
    };

    assert_eq!(
        one_claim.to_string(),
        "key `<C-y>` in mode `normal` is bound by user to `core:lock`; \
         all user keybindings revert to defaults"
    );
}
