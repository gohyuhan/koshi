//! Tests for keybinding conflict detection: every conflict class, the
//! steal/collision line, the reserved-unlock guarantee with and without an
//! alternative, verdict precedence, and the exact user-facing messages.

use std::collections::{BTreeMap, BTreeSet};

use koshi_core::action::ActionRef;
use koshi_core::geometry::Direction;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
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
    KeyMapLayer {
        origin,
        modes: BTreeMap::from([(
            mode(mode_name),
            ModeBindings {
                keys: entries.into_iter().collect(),
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

/// Runs detection with the default leader, no unlock alternative, and the
/// seeded core registry.
fn detect(layers: &[KeyMapLayer]) -> ConflictReport {
    detect_conflicts(
        layers,
        Leader::default(),
        None,
        &ActionRegistry::new(),
        &known(),
    )
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
fn user_vs_project_same_key_different_action_collides() {
    let key = seq(ModFlags::CTRL, 't');
    let layers = [
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(key.clone(), bound("new-tab"))],
        ),
        layer(
            LayerOrigin::Project,
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
                (LayerOrigin::Project, bound("lock")),
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
fn unlock_bound_with_arguments_is_fatal() {
    // `core:unlock` fires only with no arguments; a reserved-chord binding
    // carrying any is a shadow — the escape would refuse to resolve.
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
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
        vec![ConflictDiagnostic::ReservedUnlockShadowed {
            origin: LayerOrigin::User,
            action: core("unlock"),
        }]
    );
    assert_eq!(report.verdict(), KeymapVerdict::Reject);
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
    // The defaults bind `<C-p> n` and `<C-p> x`; the user binds bare `<C-p>`.
    let prefix = seq(ModFlags::CTRL, 'p');
    let report = detect(&[
        defaults(),
        layer(
            LayerOrigin::User,
            "normal",
            vec![(prefix.clone(), bound("lock"))],
        ),
    ]);
    assert_eq!(
        report.diagnostics,
        vec![
            ConflictDiagnostic::AmbiguousPrefix {
                mode: mode("normal"),
                prefix: prefix.clone(),
                prefix_action: core("lock"),
                longer: seq2(chord(ModFlags::CTRL, 'p'), chord(ModFlags::NONE, 'n')),
                longer_action: core("new-pane"),
            },
            ConflictDiagnostic::AmbiguousPrefix {
                mode: mode("normal"),
                prefix,
                prefix_action: core("lock"),
                longer: seq2(chord(ModFlags::CTRL, 'p'), chord(ModFlags::NONE, 'x')),
                longer_action: core("close-pane"),
            },
        ]
    );
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
        layer(LayerOrigin::Project, "normal", vec![(key, bound("lock"))]),
        layer(
            LayerOrigin::Session,
            "locked",
            vec![(
                KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK),
                bound("quit"),
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
        (LayerOrigin::Project, bound("lock")),
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
            (LayerOrigin::Project, bound("lock")),
        ],
    };
    assert_eq!(
        collision.to_string(),
        "key `<C-t>` in mode `normal` is bound by user to `core:new-tab` and by project \
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
        "locked mode has no binding from `<C-g>` to `core:unlock`; the unlock escape \
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
        "`<C-g> x` (user, `core:lock`) in locked mode can never fire: its first chord \
         is the reserved unlock, which resolves instantly"
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
        origin: LayerOrigin::Project,
        mode: mode("git"),
    };
    assert_eq!(
        orphan_mode.to_string(),
        "the project keymap binds keys in unregistered mode `git`; those bindings are \
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
