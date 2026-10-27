//! Tests for keybinding conflict detection: every conflict class, the steal
//! of a defaulted key, the reserved-unlock guarantee with and without an
//! alternative, verdict precedence, and the exact user-facing messages.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::action::ActionReference;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey};
use koshi_core::registry::ActionRegistry;

use super::*;
use crate::keymap_merge::tests::{
    build_character_chord, build_default_keymap_layer, build_keymap_layer,
    build_keymap_layer_with_removed_key_sequences, build_single_chord_sequence,
    build_two_chord_sequence, TEST_MAXIMUM_CHORD_DEPTH,
};
use crate::types::tests::build_bound_action;

fn build_core_action(action_name: &str) -> ActionReference {
    ActionReference::from_core_action_name(action_name)
        .expect("test action name satisfies the grammar")
}

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

/// The fatal finding for `mode_name` binding no key to `core:<cancel_action_name>`.
fn build_mode_cancel_binding_missing(
    mode_name: &str,
    cancel_action_name: &str,
) -> ConflictDiagnostic {
    ConflictDiagnostic::ModeCancelBindingMissing {
        mode_name: ModeName::from_text(mode_name),
        cancel_action_reference: build_core_action(cancel_action_name),
    }
}

#[test]
fn build_keymap_layers_appends_the_user_layer_verbatim() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'n');
    let mut user_mode_bindings_by_name = BTreeMap::new();
    user_mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
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
}

#[test]
fn layer_origin_display_is_exact() {
    assert_eq!(LayerOrigin::Defaults.to_string(), "defaults");
    assert_eq!(LayerOrigin::User.to_string(), "user");
}

#[test]
fn list_builtin_mode_names_returns_every_lock_mode() {
    let expected_mode_names = BTreeSet::from(
        [
            "normal",
            "locked",
            "pane-placement",
            "pane-move-choice",
            "floating-pane-move",
            "dock-undock",
            "floating-pane-tray",
            "notification-center",
        ]
        .map(ModeName::from_text),
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
fn stealing_a_defaulted_key_applies_with_no_finding() {
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
            mode_name: ModeName::from_text("locked"),
            key_sequence,
            action_reference: build_core_action("run"),
        }]
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
            mode_name: ModeName::from_text("normal"),
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
            mode_name: ModeName::from_text("git"),
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
            mode_name: ModeName::from_text("locked"),
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
            build_mode_cancel_binding_missing("pane-placement", "cancel-pane-placement"),
            build_mode_cancel_binding_missing("pane-move-choice", "cancel-pane-move-choice"),
            build_mode_cancel_binding_missing("floating-pane-move", "cancel-floating-pane-move"),
            build_mode_cancel_binding_missing("dock-undock", "cancel-dock-undock"),
            build_mode_cancel_binding_missing("floating-pane-tray", "close-floating-pane-tray"),
            build_mode_cancel_binding_missing("notification-center", "close-notification-center"),
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn user_prefix_of_default_sequences_warns_and_applies() {
    // The defaults bind sixteen `<C-p>` sequences in normal mode: the four
    // vim-letter splits, `m`, `n`, `s`, `u`, `w`, `x`, `<Tab>`, `<S-Tab>`, and
    // the four arrow focus keys. The user binds bare `<C-p>`.
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
            mode_name: ModeName::from_text("normal"),
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
            build_ambiguous_prefix(Key::Char('u'), "begin-dock-undock"),
            build_ambiguous_prefix(Key::Char('w'), "toggle-floating-pane-tray"),
            build_ambiguous_prefix(Key::Char('x'), "close-pane-tree"),
            build_ambiguous_prefix(Key::Named(NamedKey::Tab), "focus-next-floating-pane"),
            build_ambiguous_prefix(Key::Named(NamedKey::Left), "focus-pane-left"),
            build_ambiguous_prefix(Key::Named(NamedKey::Right), "focus-pane-right"),
            build_ambiguous_prefix(Key::Named(NamedKey::Up), "focus-pane-up"),
            build_ambiguous_prefix(Key::Named(NamedKey::Down), "focus-pane-down"),
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: ModeName::from_text("normal"),
                prefix_sequence: prefix_key_sequence.clone(),
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: build_two_chord_sequence(
                    build_character_chord(BindingModifierFlags::CTRL, 'p'),
                    KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Tab)),
                ),
                longer_action_reference: build_core_action("focus-previous-floating-pane"),
            },
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
                mode_name: ModeName::from_text("normal"),
                prefix_sequence: short_key_sequence.clone(),
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: middle_key_sequence.clone(),
                longer_action_reference: build_core_action("new-tab"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: ModeName::from_text("normal"),
                prefix_sequence: short_key_sequence,
                prefix_action_reference: build_core_action("lock"),
                longer_sequence: long_key_sequence.clone(),
                longer_action_reference: build_core_action("quit"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: ModeName::from_text("normal"),
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
                ModeName::from_text("normal"),
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
                ModeName::from_text("locked"),
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
            mode_name: ModeName::from_text("normal"),
            prefix_sequence: short_key_sequence,
            prefix_action_reference: build_core_action("lock"),
            longer_sequence: long_key_sequence,
            longer_action_reference: build_core_action("new-tab"),
        }]
    );
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
fn orphan_action_warns_and_applies() {
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
            mode_name: ModeName::from_text("normal"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn resize_tab_and_scroll_mode_blocks_warn_as_unregistered_modes() {
    // No built-in mode is named `resize`, `tab` or `scroll`: a block for one
    // binds nothing and warns once.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 's');
    let mut user_layer = build_keymap_layer(
        LayerOrigin::User,
        "resize",
        vec![(key_sequence.clone(), build_bound_action("lock"))],
    );
    for mode_name_text in ["tab", "scroll"] {
        user_layer.mode_bindings_by_name.extend(
            build_keymap_layer(
                LayerOrigin::User,
                mode_name_text,
                vec![(key_sequence.clone(), build_bound_action("lock"))],
            )
            .mode_bindings_by_name,
        );
    }
    let report = detect_test_conflicts(&[build_default_keymap_layer(), user_layer]);
    assert_eq!(
        report.diagnostics,
        ["resize", "scroll", "tab"]
            .map(|mode_name_text| ConflictDiagnostic::OrphanMode {
                layer_origin: LayerOrigin::User,
                mode_name: ModeName::from_text(mode_name_text),
            })
            .to_vec()
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn orphan_mode_warns_and_applies() {
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
            mode_name: ModeName::from_text("git"),
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
            mode_name: ModeName::from_text("normal"),
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
fn a_fatal_finding_outranks_a_warning() {
    // One user layer binds an unknown action in normal mode, a warning, and
    // rebinds the reserved unlock chord in locked mode, a fatal finding.
    let orphan_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let mut user_layer = build_keymap_layer(
        LayerOrigin::User,
        "normal",
        vec![(orphan_key_sequence.clone(), build_bound_action("ghost"))],
    );
    user_layer.mode_bindings_by_name.extend(
        build_keymap_layer(
            LayerOrigin::User,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                build_bound_action("lock"),
            )],
        )
        .mode_bindings_by_name,
    );
    let report = detect_test_conflicts(&[build_default_keymap_layer(), user_layer]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::User,
                mode_name: ModeName::from_text("normal"),
                key_sequence: orphan_key_sequence,
                action_reference: build_core_action("ghost"),
            },
            ConflictDiagnostic::ReservedUnlockShadowed {
                layer_origin: LayerOrigin::User,
                action_reference: build_core_action("lock"),
            },
        ]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn every_diagnostic_maps_to_its_exact_severity() {
    let severity_cases = [
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
            build_mode_cancel_binding_missing("pane-placement", "cancel-pane-placement"),
            ConflictSeverity::Fatal,
        ),
        (
            ConflictDiagnostic::AmbiguousPrefix {
                mode_name: ModeName::from_text("normal"),
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
                mode_name: ModeName::from_text("normal"),
                key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'y'),
                action_reference: build_core_action("run"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanAction {
                layer_origin: LayerOrigin::User,
                mode_name: ModeName::from_text("normal"),
                key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'o'),
                action_reference: build_core_action("lock"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::OrphanMode {
                layer_origin: LayerOrigin::User,
                mode_name: ModeName::from_text("git"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::TypeableBinding {
                layer_origin: LayerOrigin::User,
                mode_name: ModeName::from_text("normal"),
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
    let ambiguous_prefix = ConflictDiagnostic::AmbiguousPrefix {
        mode_name: ModeName::from_text("normal"),
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
         declare `unlock-alternative` before rebinding it"
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
        "`unlock-alternative` `u` is a key plain typing produces; hold Ctrl, Alt, or Super"
    );

    assert_eq!(
        build_mode_cancel_binding_missing("pane-placement", "cancel-pane-placement").to_string(),
        "the `pane-placement` mode has no live `core:cancel-pane-placement` binding; bind that action to a key \
         before removing its last cancellation key"
    );
    assert_eq!(
        build_mode_cancel_binding_missing("pane-move-choice", "cancel-pane-move-choice").to_string(),
        "the `pane-move-choice` mode has no live `core:cancel-pane-move-choice` binding; bind that action to a \
         key before removing its last cancellation key"
    );
    assert_eq!(
        build_mode_cancel_binding_missing("floating-pane-move", "cancel-floating-pane-move")
            .to_string(),
        "the `floating-pane-move` mode has no live `core:cancel-floating-pane-move` binding; bind that action \
         to a key before removing its last cancellation key"
    );
    assert_eq!(
        build_mode_cancel_binding_missing("dock-undock", "cancel-dock-undock").to_string(),
        "the `dock-undock` mode has no live `core:cancel-dock-undock` binding; bind that action to a key \
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
        mode_name: ModeName::from_text("normal"),
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
        mode_name: ModeName::from_text("normal"),
        key_sequence: build_single_chord_sequence(BindingModifierFlags::CTRL, 'o'),
        action_reference: build_core_action("my-macro"),
    };
    assert_eq!(
        orphan_action.to_string(),
        "`<C-o>` in mode `normal` (user) names unknown action `core:my-macro`; the \
         binding is inactive until the action is registered"
    );

    let orphan_mode = ConflictDiagnostic::OrphanMode {
        layer_origin: LayerOrigin::User,
        mode_name: ModeName::from_text("git"),
    };
    assert_eq!(
        orphan_mode.to_string(),
        "the user keymap binds keys in unregistered mode `git`; those bindings are \
         inactive until the mode is registered"
    );

    let typeable_binding = ConflictDiagnostic::TypeableBinding {
        layer_origin: LayerOrigin::User,
        mode_name: ModeName::from_text("normal"),
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
fn removing_the_locked_unlock_binding_is_fatal() {
    // Clearing the reserved chord's binding in locked mode leaves no unlock
    // escape: the effective map misses it, and the keymap is refused.
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
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
fn removing_the_notification_center_close_binding_is_fatal_while_its_toggle_stays() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "notification-center",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "notification-center",
            "close-notification-center"
        )]
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
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "pane-placement",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "pane-placement",
            "cancel-pane-placement"
        )]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removing_the_pane_move_choice_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "pane-move-choice",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "pane-move-choice",
            "cancel-pane-move-choice"
        )]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removing_the_floating_pane_move_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "floating-pane-move",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "floating-pane-move",
            "cancel-floating-pane-move"
        )]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removing_the_dock_undock_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "dock-undock",
            Vec::new(),
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "dock-undock",
            "cancel-dock-undock"
        )]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn rebinding_pane_placement_cancel_action_keeps_the_effective_map_valid() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let custom_cancel_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'c');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
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
    let custom_cancel_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'c');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "pane-placement",
            Vec::new(),
            vec![escape_sequence],
        ),
        build_keymap_layer(
            LayerOrigin::User,
            "pane-placement",
            vec![(
                custom_cancel_sequence.clone(),
                build_bound_action("cancel-pane-placement"),
            )],
        ),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "pane-placement",
            Vec::new(),
            vec![custom_cancel_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "pane-placement",
            "cancel-pane-placement"
        )]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn replacing_pane_placement_cancel_action_without_another_cancel_binding_is_fatal() {
    let escape_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Esc),
    ));
    let replacement_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'c');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "pane-placement",
            vec![(replacement_sequence, build_bound_action("lock"))],
            vec![escape_sequence],
        ),
    ]);

    assert_eq!(
        report.diagnostics,
        vec![build_mode_cancel_binding_missing(
            "pane-placement",
            "cancel-pane-placement"
        )]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Reject);
}

#[test]
fn removed_binding_draws_no_per_binding_warns() {
    // The lower user layer binds an orphan action on a typeable key; the
    // higher user layer removes the key. The removed binding draws neither the
    // orphan warning nor the typeable warning.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::NONE, 'g');
    let report = detect_test_conflicts(&[
        build_default_keymap_layer(),
        build_keymap_layer(
            LayerOrigin::User,
            "normal",
            vec![(key_sequence.clone(), build_bound_action("does-not-exist"))],
        ),
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
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
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
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
            mode_name: ModeName::from_text("normal"),
            key_sequence: long_key_sequence,
            action_reference: build_core_action("new-tab"),
            maximum_chord_depth: 1,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
    assert_eq!(
        report.diagnostics[0].to_string(),
        "`<C-y> x` in mode `normal` (user, `core:new-tab`) is 2 chords, over the \
         `max-chord-depth` cap of 1; the binding can never fire"
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
            mode_name: ModeName::from_text("locked"),
            key_sequence,
            action_reference: orphan_bound_action.action_reference,
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_chord_depth_of_zero_fails_the_unlock_guarantee() {
    // With every sequence at least one chord, a cap of 0 makes the whole
    // keymap unreachable — including the locked-mode unlock binding and the
    // cancel binding of `pane-placement`, `pane-move-choice`,
    // `floating-pane-move`, `dock-undock`, `floating-pane-tray`, and
    // `notification-center`, which the guarantee checks report as missing.
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
            build_mode_cancel_binding_missing("pane-placement", "cancel-pane-placement"),
            build_mode_cancel_binding_missing("pane-move-choice", "cancel-pane-move-choice"),
            build_mode_cancel_binding_missing("floating-pane-move", "cancel-floating-pane-move"),
            build_mode_cancel_binding_missing("dock-undock", "cancel-dock-undock"),
            build_mode_cancel_binding_missing("floating-pane-tray", "close-floating-pane-tray"),
            build_mode_cancel_binding_missing("notification-center", "close-notification-center"),
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
        build_keymap_layer_with_removed_key_sequences(
            LayerOrigin::User,
            "git",
            Vec::new(),
            vec![key_sequence],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::OrphanMode {
            layer_origin: LayerOrigin::User,
            mode_name: ModeName::from_text("git"),
        }]
    );
    assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
}
