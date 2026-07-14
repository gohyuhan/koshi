//! Tests for keybinding conflict detection: every conflict class, the
//! steal/collision line, the reserved-unlock guarantee with and without an
//! alternative, verdict precedence, and the exact user-facing messages.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::action::ActionRef;
use koshi_core::geometry::Direction;
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

fn known() -> BTreeSet<ModeName> {
    BTreeSet::from([mode("normal"), mode("locked")])
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
        &known(),
    )
}

#[test]
fn user_layer_args_are_stripped_to_the_action_mapping() {
    // Even if a user file somehow smuggles arguments into a binding
    // ("new-pane, direction left"), only the key → action mapping survives:
    // the binding runs bare and the action falls back to its system preset
    // (the configured default split direction).
    let key = seq(ModFlags::ALT, 'n');
    let smuggled = BoundAction {
        action: core("new-pane"),
        args: ActionArgs::NewPane {
            direction: Some(Direction::Left),
            stacked: false,
        },
    };
    let stripped =
        layer(LayerOrigin::User, "normal", vec![(key.clone(), smuggled)]).with_user_args_stripped();
    assert_eq!(
        stripped.modes[&mode("normal")].keys[&key],
        bound("new-pane")
    );
}

#[test]
fn stripping_leaves_the_defaults_layer_presets_alone() {
    // The defaults' arguments ARE the system presets (focus directions, the
    // tree-scoped close); stripping is a user-surface guard only.
    assert_eq!(defaults().with_user_args_stripped(), defaults());
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
    let key = seq(ModFlags::CTRL, 't');
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
    // Two claimants is the minimum for a collision; a third distinct
    // claimant must still be listed, not silently dropped after the pair.
    let key = seq(ModFlags::CTRL, 't');
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
    // User and Layout bind the identical action (restating one intent);
    // Session's differing claim sits between them. Dedup must compare
    // against every earlier distinct claim, not just the immediately
    // preceding one, so the result is exactly two distinct claims.
    let key = seq(ModFlags::CTRL, 't');
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
    let key = seq(ModFlags::CTRL, 't');
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
    let key = seq(ModFlags::CTRL, 'e');
    let resize = |direction: Direction| BoundAction {
        action: core("resize-pane"),
        args: ActionArgs::ResizePane { direction, size: 1 },
    };
    let layers = [
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), resize(Direction::Left))],
        ),
        layer(
            LayerOrigin::Layout,
            "normal",
            vec![(key.clone(), resize(Direction::Right))],
        ),
    ];
    let report = detect(&layers);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::KeyCollision {
            mode: mode("normal"),
            key,
            claims: vec![
                (LayerOrigin::User, resize(Direction::Left)),
                (LayerOrigin::Layout, resize(Direction::Right)),
            ],
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::RevertToDefaults);
}

#[test]
fn orphan_actions_on_a_shared_key_do_not_collide() {
    // Both claims name unregistered actions: inactive bindings, warned as
    // orphans, re-judged when detection re-runs at registration.
    let key = seq(ModFlags::CTRL, 't');
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
    let key = seq(ModFlags::CTRL, 't');
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
    // `core:copy-mode-enter` is seeded but not implemented; the binding cannot fire.
    let key = seq(ModFlags::CTRL, 'y');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("copy-mode-enter"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ComingSoonAction {
            origin: LayerOrigin::User,
            mode: mode("normal"),
            key,
            action: core("copy-mode-enter"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn coming_soon_claims_do_not_collide() {
    // Neither binding can fire in this build, so nothing is unreachable;
    // the collision surfaces at the first load of the build that
    // implements the actions.
    let key = seq(ModFlags::CTRL, 't');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("copy-mode-enter"))],
        ),
        layer(
            LayerOrigin::Session,
            "normal",
            vec![(key.clone(), bound("copy-mode-exit"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::ComingSoonAction {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: key.clone(),
                action: core("copy-mode-enter"),
            },
            ConflictDiagnostic::ComingSoonAction {
                origin: LayerOrigin::Session,
                mode: mode("normal"),
                key,
                action: core("copy-mode-exit"),
            },
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn unresolvable_args_binding_warns_and_does_not_collide() {
    // The user layer's binding carries arguments `core:lock` cannot take,
    // so it can never fire; it must not escalate the session layer's
    // working binding into the all-or-nothing revert.
    let key = seq(ModFlags::CTRL, 't');
    let broken = BoundAction {
        action: core("lock"),
        args: ActionArgs::FocusPane {
            target: koshi_core::command::FocusTarget::Direction(Direction::Left),
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
    // `core:unlock` fires only with no arguments, so this binding can never
    // fire — it is transparent, the default unlock beneath it still wins
    // the reserved chord, and the escape stays intact.
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
                    args: ActionArgs::FocusPane {
                        target: koshi_core::command::FocusTarget::Direction(Direction::Left),
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
    // neither can ever fire, so they must not trigger the revert. Each is
    // warned dead instead.
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
    // `<C-x> <C-l>` does not OPEN with the reserved chord, so it looks live —
    // but the input path resolves the unlock the instant it is pressed, open
    // sequence or not, so the `<C-l>` unlocks and `core:new-tab` never runs.
    // Position is irrelevant: a locked sequence holding the chord at all is
    // dead, and must be warned rather than admitted as a firing binding that
    // steals its key and offers a hint-bar continuation that silently unlocks.
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
    // The rule kills sequences that HOLD the reserved chord — never the
    // one-chord binding that IS the unlock. Locked mode's own `<C-l>` →
    // `core:unlock` must keep firing, or the escape guarantee dies with it.
    let report = detect(&[defaults()]);
    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn reserved_led_sequences_do_not_pair_as_prefixes() {
    // `<C-g> x` is a strict prefix of `<C-g> x y`, but both are swallowed
    // by the reserved chord: two dead warnings, no ambiguous-prefix pair.
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
        &known(),
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
        &known(),
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
        &known(),
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
    // The defaults bind `<C-p> n`, `<C-p> x`, and the four `<C-p>` arrow
    // focus sequences; the user binds bare `<C-p>`.
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
            ambiguous(Key::Char('n'), "new-pane"),
            ambiguous(Key::Char('x'), "close-pane"),
            ambiguous(Key::Named(NamedKey::Left), "focus-pane"),
            ambiguous(Key::Named(NamedKey::Right), "focus-pane"),
            ambiguous(Key::Named(NamedKey::Up), "focus-pane"),
            ambiguous(Key::Named(NamedKey::Down), "focus-pane"),
        ]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Apply);
}

#[test]
fn a_three_deep_prefix_chain_reports_every_pair() {
    // `<C-y>`, `<C-y> n`, and `<C-y> n o` are each a prefix of the ones
    // longer than it: three pairs total, not just the two adjacent ones.
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
    // Two layers remove the same key; only the LAST (highest-index) remove
    // determines what index a claim must beat. Removal is positional, not
    // per-origin, so a stack may hold several layers of one origin. User
    // removes the key (index 1, no bind), Session rebinds it without
    // removing (index 2), a first layout layer redundantly removes it again
    // (index 3, no bind), a second layout layer rebinds with a different
    // action (index 4). If `removal_index` recorded the first remove (index
    // 1) instead of the last (index 3), Session's rebind at index 2 would
    // wrongly survive (1 is not > 2) and collide with the top claim;
    // recording the last remove correctly voids it, leaving the top claim
    // alone.
    let key = seq(ModFlags::CTRL, 't');
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
        &known(),
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
    let report = detect_conflicts(
        &[defaults()],
        leader,
        None,
        DEPTH,
        &ActionRegistry::new(),
        &known(),
    );
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
        let report = detect_conflicts(
            &[defaults()],
            leader,
            None,
            DEPTH,
            &ActionRegistry::new(),
            &known(),
        );
        assert_eq!(report.diagnostics, Vec::new());
    }
}

#[test]
fn a_fatal_finding_outranks_a_collision() {
    let key = seq(ModFlags::CTRL, 't');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(LayerOrigin::Session, "normal", vec![(key, bound("lock"))]),
        layer(
            LayerOrigin::Layout,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                bound("lock"),
            )],
        ),
    ]);
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.severity() == ConflictSeverity::Collision));
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.severity() == ConflictSeverity::Fatal));
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
                key: seq(ModFlags::CTRL, 't'),
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
                action: core("copy-mode-enter"),
            },
            ConflictSeverity::Warning,
        ),
        (
            ConflictDiagnostic::UnresolvableArgs {
                origin: LayerOrigin::User,
                mode: mode("normal"),
                key: seq(ModFlags::CTRL, 't'),
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
        key: seq(ModFlags::CTRL, 't'),
        claims: vec![
            (LayerOrigin::User, bound("new-tab")),
            (LayerOrigin::Session, bound("lock")),
        ],
    };
    assert_eq!(
        collision.to_string(),
        "key `<C-t>` in mode `normal` is bound by user to `core:new-tab` and by session \
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
                    action: core("resize-pane"),
                    args: ActionArgs::ResizePane {
                        direction: Direction::Left,
                        size: 1,
                    },
                },
            ),
            (
                LayerOrigin::Layout,
                BoundAction {
                    action: core("resize-pane"),
                    args: ActionArgs::ResizePane {
                        direction: Direction::Right,
                        size: 1,
                    },
                },
            ),
        ],
    };
    assert_eq!(
        same_action_collision.to_string(),
        "key `<C-e>` in mode `normal` is bound by user to `core:resize-pane` and by \
         layout to `core:resize-pane` with different arguments; all user keybindings \
         revert to defaults"
    );

    let unresolvable = ConflictDiagnostic::UnresolvableArgs {
        origin: LayerOrigin::User,
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 't'),
        action: core("lock"),
    };
    assert_eq!(
        unresolvable.to_string(),
        "`<C-t>` in mode `normal` (user) binds `core:lock` with arguments it cannot \
         take; the binding can never fire as written"
    );

    let coming_soon = ConflictDiagnostic::ComingSoonAction {
        origin: LayerOrigin::User,
        mode: mode("normal"),
        key: seq(ModFlags::CTRL, 'y'),
        action: core("copy-mode-enter"),
    };
    assert_eq!(
        coming_soon.to_string(),
        "`<C-y>` in mode `normal` (user) binds `core:copy-mode-enter`, which is not implemented \
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
    let key = seq(ModFlags::CTRL, 't');
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
    let key = seq(ModFlags::CTRL, 't');
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
    let key = seq(ModFlags::CTRL, 't');
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
    // The user disabled the key wholesale in a higher layer; two voided
    // claims cannot collide, and no warning fires — the removal is the
    // user's own authored intent.
    let key = seq(ModFlags::CTRL, 't');
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
    // removes the key. Removal silences both warns the binding would
    // otherwise draw: disabling it is the user's own authored intent, not a
    // surprise.
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
    // At a cap of 1, a two-chord user binding can never be reached — the
    // input path flushes the pending sequence before lookup — so it warns
    // and stays transparent; the keymap still applies.
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
        &known(),
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
        &known(),
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
    let key = seq(ModFlags::CTRL, 't');
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
