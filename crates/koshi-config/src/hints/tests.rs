//! Tests for the keymap hint catalog: the defaults-only merge joined to
//! display names, user bindings and removals over the defaults, the firing
//! filter (a refused action, a locked-mode sequence holding the unlock chord,
//! and a mode the build does not register each yield no hint), the
//! chord-depth cap at 0 and 1, the pinned locked-mode unlock, prefix labels
//! under either leader shape, sequence matching, empty modes, and the
//! per-frame `Arc` sharing.

use super::*;

use koshi_core::key::{Key, ModFlags, NamedKey};

use crate::types::ModeBindings;

/// The catalog resolved from a fresh registry — the built-in defaults over
/// the built-in action table, exactly what a stock runtime holds.
fn build_default_hint_catalog() -> KeymapHintCatalog {
    KeymapHintCatalog::from_registry(&ActionRegistry::new())
}

/// A `Ctrl`-modified character chord.
fn build_control_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(ModFlags::CTRL, Key::Char(key_character))
}

/// An `Alt`-modified character chord.
fn build_alt_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(ModFlags::ALT, Key::Char(key_character))
}

/// A core action bound with no preset arguments.
fn build_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("a core action name satisfies the grammar"),
        action_arguments: koshi_core::resolve::ActionArgs::None,
    }
}

/// The catalog for a user layer holding `bound_action_by_key_sequence` and
/// `removed_key_sequences` in `mode_name`, over
/// the built-in defaults, the default keybinding config and a fresh registry.
fn build_hint_catalog_with_user(
    mode_name: &str,
    bound_action_by_key_sequence: BTreeMap<KeySequence, BoundAction>,
    removed_key_sequences: BTreeSet<KeySequence>,
) -> KeymapHintCatalog {
    let modes = BTreeMap::from([(
        ModeName::from_text(mode_name),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences,
        },
    )]);
    KeymapHintCatalog::from_parts(
        &build_keymap_layers(Some(modes), Leader::default()),
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    )
}

/// The catalog for the built-in defaults under `config`, with the defaults
/// layer built against the config's own leader.
fn build_hint_catalog_with_config(config: &KeybindingsConfig) -> KeymapHintCatalog {
    KeymapHintCatalog::from_parts(
        &build_keymap_layers(None, config.leader),
        config,
        &ActionRegistry::new(),
    )
}

#[test]
fn normal_mode_joins_defaults_to_display_names() {
    let hints = build_default_hint_catalog().build_hints_for_mode(LockMode::Normal);

    // All 22 shipped normal-mode bindings fire in this build.
    assert_eq!(hints.hint_bindings.len(), 22);

    let new_pane_key_sequence = KeySequence::from_first_and_rest(
        build_control_chord('p'),
        vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('n'))],
    );
    let hint_binding = hints
        .hint_bindings
        .iter()
        .find(|hint_binding| hint_binding.key_sequence == new_pane_key_sequence)
        .expect("the default <C-p> n binding yields a hint");
    assert_eq!(hint_binding.action_display_name, "New Pane");
    assert!(!hint_binding.is_user_authored);
    assert!(!hint_binding.is_pinned);
}

#[test]
fn quit_binding_surfaces_in_both_modes() {
    let hint_catalog = build_default_hint_catalog();
    let quit = KeySequence::from(build_control_chord('q'));
    for lock_mode in [LockMode::Normal, LockMode::Locked] {
        let hints = hint_catalog.build_hints_for_mode(lock_mode);
        let hint_binding = hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == quit)
            .unwrap_or_else(|| panic!("{lock_mode:?} binds the quit chord"));
        assert_eq!(hint_binding.action_display_name, "Quit");
    }
}

#[test]
fn locked_mode_pins_the_reserved_unlock() {
    let hints = build_default_hint_catalog().build_hints_for_mode(LockMode::Locked);
    // The reserved unlock (the same `<C-l>` that locks in normal mode) plus
    // the quit and mouse-select chords, which fire in either mode.
    assert_eq!(hints.hint_bindings.len(), 3);
    let hint_binding = hints
        .hint_bindings
        .iter()
        .find(|hint_binding| {
            hint_binding.key_sequence == KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
        })
        .expect("locked mode binds the reserved unlock");
    assert_eq!(hint_binding.action_display_name, "Unlock");
    assert!(hint_binding.is_pinned);
}

#[test]
fn modes_without_defaults_are_empty() {
    let hint_catalog = build_default_hint_catalog();
    for lock_mode in [
        LockMode::Resize,
        LockMode::PaneMode,
        LockMode::TabMode,
        LockMode::ScrollMode,
    ] {
        let hints = hint_catalog.build_hints_for_mode(lock_mode);
        assert!(
            hints.hint_bindings.is_empty(),
            "{lock_mode:?} ships no bindings"
        );
        assert!(hints.removed_key_sequences.is_empty());
    }
}

#[test]
fn prefix_labels_carry_the_shipped_names() {
    let hints = build_default_hint_catalog().build_hints_for_mode(LockMode::Normal);
    assert_eq!(
        hints
            .prefix_labels
            .get(&build_control_chord('p'))
            .map(String::as_str),
        Some("PANE")
    );
    assert_eq!(
        hints
            .prefix_labels
            .get(&build_control_chord('s'))
            .map(String::as_str),
        Some("RESIZE")
    );
    assert_eq!(
        hints
            .prefix_labels
            .get(&build_control_chord('t'))
            .map(String::as_str),
        Some("TAB")
    );
    assert_eq!(hints.prefix_labels.len(), 3);
}

#[test]
fn a_complete_binding_reports_an_exact_match_and_no_longer_sequence() {
    let sequence_match = build_default_hint_catalog().match_sequence(
        LockMode::Normal,
        &KeySequence::from(build_control_chord('q')),
    );
    assert_eq!(
        sequence_match.exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("quit")
                .expect("`core:quit` is a valid action name"),
            action_arguments: koshi_core::resolve::ActionArgs::None,
        })
    );
    assert!(
        !sequence_match.has_longer_key_sequence,
        "nothing shipped continues past `<C-q>`, so it is not a prefix of a longer binding"
    );
}

#[test]
fn a_prefix_chord_reports_no_exact_match() {
    let sequence_match = build_default_hint_catalog().match_sequence(
        LockMode::Normal,
        &KeySequence::from(build_control_chord('p')),
    );
    assert_eq!(sequence_match.exact_bound_action, None);
    assert!(sequence_match.has_longer_key_sequence);
}

#[test]
fn an_unbound_sequence_matches_nothing() {
    let hint_catalog = build_default_hint_catalog();
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Normal,
            &KeySequence::from(build_control_chord('y'))
        ),
        KeyMatch::default()
    );
    // A mode nothing binds in holds no map at all.
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Resize,
            &KeySequence::from(build_control_chord('q'))
        ),
        KeyMatch::default()
    );
}

#[test]
fn the_configured_unlock_alternative_becomes_the_escape_chord() {
    let alternative_unlock_chord = KeyChord::from_parts(ModFlags::ALT, Key::Char('u'));
    let config = KeybindingsConfig {
        chord_timeout_ms: 1234,
        unlock_alternative: Some(alternative_unlock_chord),
        ..KeybindingsConfig::default()
    };

    let hint_catalog = KeymapHintCatalog::from_parts(
        &build_keymap_layers(None, Leader::default()),
        &config,
        &ActionRegistry::new(),
    );

    assert_eq!(hint_catalog.get_unlock_chord(), alternative_unlock_chord);
    assert_eq!(
        hint_catalog.get_chord_timeout(),
        Duration::from_millis(1234)
    );
}

#[test]
fn the_unlock_chord_is_the_reserved_one_when_the_config_names_no_alternative() {
    assert_eq!(
        build_default_hint_catalog().get_unlock_chord(),
        KeybindingsConfig::RESERVED_UNLOCK
    );
    assert_eq!(
        build_default_hint_catalog().get_chord_timeout(),
        Duration::from_millis(500)
    );
}

#[test]
fn a_rebound_leader_moves_the_prefix_labels() {
    let config = KeybindingsConfig {
        leader: Leader::Mods(ModFlags::ALT),
        ..KeybindingsConfig::default()
    };

    let hints = KeymapHintCatalog::from_parts(
        &build_keymap_layers(None, Leader::Mods(ModFlags::ALT)),
        &config,
        &ActionRegistry::new(),
    )
    .build_hints_for_mode(LockMode::Normal);

    assert_eq!(
        hints
            .prefix_labels
            .get(&KeyChord::from_parts(ModFlags::ALT, Key::Char('p')))
            .map(String::as_str),
        Some("PANE")
    );
    assert_eq!(hints.prefix_labels.get(&build_control_chord('p')), None);
}

#[test]
fn reverted_defaults_to_false() {
    assert!(
        !build_default_hint_catalog()
            .build_hints_for_mode(LockMode::Normal)
            .is_reverted_to_defaults
    );
}

#[test]
fn mark_reverted_to_defaults_marks_every_mode_hint() {
    let hint_catalog = build_default_hint_catalog().mark_reverted_to_defaults();
    assert!(
        hint_catalog
            .build_hints_for_mode(LockMode::Normal)
            .is_reverted_to_defaults
    );
    assert!(
        hint_catalog
            .build_hints_for_mode(LockMode::Locked)
            .is_reverted_to_defaults
    );
}

#[test]
fn frames_share_the_per_mode_data_by_reference() {
    let hint_catalog = build_default_hint_catalog();
    let first_hints = hint_catalog.build_hints_for_mode(LockMode::Normal);
    let second_hints = hint_catalog.build_hints_for_mode(LockMode::Normal);
    assert!(Arc::ptr_eq(
        &first_hints.hint_bindings,
        &second_hints.hint_bindings
    ));
    assert!(Arc::ptr_eq(
        &first_hints.prefix_labels,
        &second_hints.prefix_labels
    ));
    assert!(Arc::ptr_eq(
        &first_hints.removed_key_sequences,
        &second_hints.removed_key_sequences
    ));
}

#[test]
fn a_user_binding_takes_the_default_key_and_shows_as_user_set() {
    let fullscreen_key_sequence = KeySequence::from(build_alt_chord('f'));
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::from([(fullscreen_key_sequence.clone(), build_bound_action("quit"))]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_hints_for_mode(LockMode::Normal);

    // The user entry replaces the default on that key rather than adding one.
    assert_eq!(hints.hint_bindings.len(), 22);
    let hint_binding = hints
        .hint_bindings
        .iter()
        .find(|hint_binding| hint_binding.key_sequence == fullscreen_key_sequence)
        .expect("the user binding yields a hint");
    assert_eq!(hint_binding.action_display_name, "Quit");
    assert!(hint_binding.is_user_authored);
    assert!(!hint_binding.is_pinned);
    assert_eq!(
        hint_catalog
            .match_sequence(LockMode::Normal, &fullscreen_key_sequence)
            .exact_bound_action,
        Some(build_bound_action("quit"))
    );
}

#[test]
fn a_user_removal_drops_the_hint_and_matches_nothing() {
    let fullscreen_key_sequence = KeySequence::from(build_alt_chord('f'));
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::new(),
        BTreeSet::from([fullscreen_key_sequence.clone()]),
    );
    let hints = hint_catalog.build_hints_for_mode(LockMode::Normal);

    assert_eq!(hints.hint_bindings.len(), 21);
    assert_eq!(
        hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == fullscreen_key_sequence),
        None
    );
    assert_eq!(
        *hints.removed_key_sequences,
        BTreeSet::from([fullscreen_key_sequence.clone()])
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &fullscreen_key_sequence),
        KeyMatch::default()
    );
    // The removal belongs to the mode that authored it.
    assert_eq!(
        *hint_catalog
            .build_hints_for_mode(LockMode::Locked)
            .removed_key_sequences,
        BTreeSet::new()
    );
}

#[test]
fn a_binding_the_resolver_refuses_yields_no_hint() {
    // `core:copy-selection` is registered without an implementation in this
    // build, so the merge drops the binding and no hint carries it.
    let key = KeySequence::from(build_control_chord('y'));
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::from([(key.clone(), build_bound_action("copy-selection"))]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_hints_for_mode(LockMode::Normal);

    assert_eq!(hints.hint_bindings.len(), 22);
    assert_eq!(
        hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == key),
        None
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &key),
        KeyMatch::default()
    );
}

#[test]
fn a_sequence_that_both_fires_and_opens_a_longer_one_reports_both() {
    let opening_sequence = KeySequence::from(build_control_chord('y'));
    let longer_sequence = KeySequence::from_first_and_rest(
        build_control_chord('y'),
        vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('a'))],
    );
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::from([
            (opening_sequence.clone(), build_bound_action("quit")),
            (longer_sequence.clone(), build_bound_action("lock")),
        ]),
        BTreeSet::new(),
    );

    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &opening_sequence),
        KeyMatch {
            exact_bound_action: Some(build_bound_action("quit")),
            has_longer_key_sequence: true,
        }
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &longer_sequence),
        KeyMatch {
            exact_bound_action: Some(build_bound_action("lock")),
            has_longer_key_sequence: false,
        }
    );
}

#[test]
fn every_locked_entry_firing_unlock_is_pinned() {
    let hint_catalog = build_hint_catalog_with_user(
        "locked",
        BTreeMap::from([(
            KeySequence::from(build_alt_chord('u')),
            build_bound_action("unlock"),
        )]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_hints_for_mode(LockMode::Locked);

    assert_eq!(hints.hint_bindings.len(), 4);
    let pinned_key_sequences: Vec<String> = hints
        .hint_bindings
        .iter()
        .filter(|hint_binding| hint_binding.is_pinned)
        .map(|hint_binding| hint_binding.key_sequence.to_string())
        .collect();
    assert_eq!(pinned_key_sequences, vec!["<C-l>", "<A-u>"]);
}

#[test]
fn a_chord_depth_cap_of_one_drops_every_multi_chord_default() {
    let hint_catalog = build_hint_catalog_with_config(&KeybindingsConfig {
        max_chord_depth: 1,
        ..KeybindingsConfig::default()
    });
    let hints = hint_catalog.build_hints_for_mode(LockMode::Normal);

    let key_sequence_strings: Vec<String> = hints
        .hint_bindings
        .iter()
        .map(|hint_binding| hint_binding.key_sequence.to_string())
        .collect();
    assert_eq!(
        key_sequence_strings,
        vec!["<Tab>", "<C-g>", "<C-l>", "<C-q>", "<A-f>", "<S-Tab>"]
    );
    // `<C-p>` opened the pane group, whose entries are all two chords long.
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Normal,
            &KeySequence::from(build_control_chord('p'))
        ),
        KeyMatch::default()
    );
}

#[test]
fn a_chord_leader_collapses_the_groups_and_drops_every_prefix_label() {
    // All three groups open at the leader chord itself, so no label names one
    // group and none is offered.
    let space = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Space));
    let hints = build_hint_catalog_with_config(&KeybindingsConfig {
        leader: Leader::Chord(space),
        ..KeybindingsConfig::default()
    })
    .build_hints_for_mode(LockMode::Normal);

    assert_eq!(*hints.prefix_labels, BTreeMap::new());
}

#[test]
fn the_chord_timeout_carries_the_configured_milliseconds_at_both_bounds() {
    let minimum_timeout_hints = build_hint_catalog_with_config(&KeybindingsConfig {
        chord_timeout_ms: 0,
        ..KeybindingsConfig::default()
    });
    assert_eq!(minimum_timeout_hints.get_chord_timeout(), Duration::ZERO);

    let maximum_timeout_hints = build_hint_catalog_with_config(&KeybindingsConfig {
        chord_timeout_ms: u32::MAX,
        ..KeybindingsConfig::default()
    });
    assert_eq!(
        maximum_timeout_hints.get_chord_timeout(),
        Duration::from_millis(4_294_967_295)
    );
}

#[test]
fn mark_reverted_to_defaults_changes_only_the_flag() {
    let base_hint_catalog = build_default_hint_catalog();
    let reverted_hint_catalog = base_hint_catalog.clone().mark_reverted_to_defaults();
    let base_hints = base_hint_catalog.build_hints_for_mode(LockMode::Normal);
    let reverted_hints = reverted_hint_catalog.build_hints_for_mode(LockMode::Normal);

    assert!(!base_hints.is_reverted_to_defaults);
    assert!(reverted_hints.is_reverted_to_defaults);
    assert!(Arc::ptr_eq(
        &base_hints.hint_bindings,
        &reverted_hints.hint_bindings
    ));
    assert!(Arc::ptr_eq(
        &base_hints.removed_key_sequences,
        &reverted_hints.removed_key_sequences
    ));
    assert!(Arc::ptr_eq(
        &base_hints.prefix_labels,
        &reverted_hints.prefix_labels
    ));
    assert_eq!(
        reverted_hint_catalog.get_unlock_chord(),
        base_hint_catalog.get_unlock_chord()
    );
    assert_eq!(
        reverted_hint_catalog.get_chord_timeout(),
        base_hint_catalog.get_chord_timeout()
    );
    let quit = KeySequence::from(build_control_chord('q'));
    assert_eq!(
        reverted_hint_catalog.match_sequence(LockMode::Normal, &quit),
        base_hint_catalog.match_sequence(LockMode::Normal, &quit)
    );
}

#[test]
fn a_binding_in_a_mode_the_build_does_not_register_yields_no_hint() {
    let key = KeySequence::from(build_alt_chord('u'));
    let hint_catalog = build_hint_catalog_with_user(
        "vim",
        BTreeMap::from([(key.clone(), build_bound_action("quit"))]),
        BTreeSet::new(),
    );

    for lock_mode in LockMode::ALL {
        let hints = hint_catalog.build_hints_for_mode(lock_mode);
        assert_eq!(
            hints
                .hint_bindings
                .iter()
                .find(|hint_binding| hint_binding.key_sequence == key),
            None,
            "{lock_mode:?} carries the binding from the unregistered mode"
        );
        assert_eq!(
            hint_catalog.match_sequence(lock_mode, &key),
            KeyMatch::default()
        );
    }
    // The shipped defaults are untouched by the skipped mode.
    assert_eq!(
        hint_catalog
            .build_hints_for_mode(LockMode::Normal)
            .hint_bindings
            .len(),
        22
    );
}

#[test]
fn a_chord_depth_cap_of_zero_drops_every_binding_and_keeps_the_escape_chord() {
    let hint_catalog = build_hint_catalog_with_config(&KeybindingsConfig {
        max_chord_depth: 0,
        ..KeybindingsConfig::default()
    });

    for lock_mode in LockMode::ALL {
        assert_eq!(
            *hint_catalog.build_hints_for_mode(lock_mode).hint_bindings,
            Vec::<HintBinding>::new(),
            "{lock_mode:?}"
        );
    }
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Normal,
            &KeySequence::from(build_control_chord('q'))
        ),
        KeyMatch::default()
    );
    // The unlock chord resolves ahead of the keymap, so an empty keymap still
    // reports it.
    assert_eq!(
        hint_catalog.get_unlock_chord(),
        KeybindingsConfig::RESERVED_UNLOCK
    );
}

#[test]
fn a_removal_of_a_key_nothing_binds_is_still_listed_as_removed() {
    let unbound = KeySequence::from(build_alt_chord('z'));
    let hint_catalog =
        build_hint_catalog_with_user("normal", BTreeMap::new(), BTreeSet::from([unbound.clone()]));
    let hints = hint_catalog.build_hints_for_mode(LockMode::Normal);

    assert_eq!(hints.hint_bindings.len(), 22);
    assert_eq!(
        *hints.removed_key_sequences,
        BTreeSet::from([unbound.clone()])
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &unbound),
        KeyMatch::default()
    );
}

#[test]
fn a_locked_sequence_holding_the_unlock_chord_yields_no_hint() {
    let key = KeySequence::from_first_and_rest(
        KeybindingsConfig::RESERVED_UNLOCK,
        vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('x'))],
    );
    let hint_catalog = build_hint_catalog_with_user(
        "locked",
        BTreeMap::from([(key.clone(), build_bound_action("quit"))]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_hints_for_mode(LockMode::Locked);

    assert_eq!(hints.hint_bindings.len(), 3);
    assert_eq!(
        hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == key),
        None
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Locked, &key),
        KeyMatch::default()
    );
    // The one-chord unlock stays live, and the dropped sequence opens nothing.
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Locked,
            &KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
        ),
        KeyMatch {
            exact_bound_action: Some(build_bound_action("unlock")),
            has_longer_key_sequence: false,
        }
    );
}

#[test]
fn entries_are_sorted_by_key_sequence_with_no_key_twice() {
    let hints = build_default_hint_catalog().build_hints_for_mode(LockMode::Normal);
    let key_sequences: Vec<KeySequence> = hints
        .hint_bindings
        .iter()
        .map(|hint_binding| hint_binding.key_sequence.clone())
        .collect();

    let mut sorted_unique_key_sequences = key_sequences.clone();
    sorted_unique_key_sequences.sort();
    sorted_unique_key_sequences.dedup();
    assert_eq!(key_sequences, sorted_unique_key_sequences);
}
