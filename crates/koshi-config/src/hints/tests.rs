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
fn catalog() -> KeymapHintCatalog {
    KeymapHintCatalog::from_registry(&ActionRegistry::new())
}

/// A `Ctrl`-modified character chord.
fn ctrl(key: char) -> KeyChord {
    KeyChord::new(ModFlags::CTRL, Key::Char(key))
}

/// An `Alt`-modified character chord.
fn alt(key: char) -> KeyChord {
    KeyChord::new(ModFlags::ALT, Key::Char(key))
}

/// A core action bound with no preset arguments.
fn bound(name: &str) -> BoundAction {
    BoundAction {
        action: ActionRef::core(name).expect("a core action name satisfies the grammar"),
        args: koshi_core::resolve::ActionArgs::None,
    }
}

/// The catalog for a user layer holding `keys` and `removed` in `mode`, over
/// the built-in defaults, the default keybinding config and a fresh registry.
fn catalog_with_user(
    mode: &str,
    keys: BTreeMap<KeySequence, BoundAction>,
    removed: BTreeSet<KeySequence>,
) -> KeymapHintCatalog {
    let modes = BTreeMap::from([(ModeName::new(mode), ModeBindings { keys, removed })]);
    KeymapHintCatalog::from_parts(
        &keymap_layers(Some(modes), Leader::default()),
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    )
}

/// The catalog for the built-in defaults under `config`, with the defaults
/// layer built against the config's own leader.
fn catalog_with_config(config: &KeybindingsConfig) -> KeymapHintCatalog {
    KeymapHintCatalog::from_parts(
        &keymap_layers(None, config.leader),
        config,
        &ActionRegistry::new(),
    )
}

#[test]
fn normal_mode_joins_defaults_to_display_names() {
    let hints = catalog().hints_for(LockMode::Normal);

    // All 22 shipped normal-mode bindings fire in this build.
    assert_eq!(hints.entries.len(), 22);

    let new_pane = KeySequence::new(
        ctrl('p'),
        vec![KeyChord::new(ModFlags::NONE, Key::Char('n'))],
    );
    let entry = hints
        .entries
        .iter()
        .find(|entry| entry.sequence == new_pane)
        .expect("the default <C-p> n binding yields a hint");
    assert_eq!(entry.label, "New Pane");
    assert!(!entry.user_set);
    assert!(!entry.pinned);
}

#[test]
fn quit_binding_surfaces_in_both_modes() {
    let catalog = catalog();
    let quit = KeySequence::from(ctrl('q'));
    for mode in [LockMode::Normal, LockMode::Locked] {
        let hints = catalog.hints_for(mode);
        let entry = hints
            .entries
            .iter()
            .find(|entry| entry.sequence == quit)
            .unwrap_or_else(|| panic!("{mode:?} binds the quit chord"));
        assert_eq!(entry.label, "Quit");
    }
}

#[test]
fn locked_mode_pins_the_reserved_unlock() {
    let hints = catalog().hints_for(LockMode::Locked);
    // The reserved unlock (the same `<C-l>` that locks in normal mode) plus
    // the quit and mouse-select chords, which fire in either mode.
    assert_eq!(hints.entries.len(), 3);
    let entry = hints
        .entries
        .iter()
        .find(|entry| entry.sequence == KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK))
        .expect("locked mode binds the reserved unlock");
    assert_eq!(entry.label, "Unlock");
    assert!(entry.pinned);
}

#[test]
fn modes_without_defaults_are_empty() {
    let catalog = catalog();
    for mode in [
        LockMode::Resize,
        LockMode::PaneMode,
        LockMode::TabMode,
        LockMode::ScrollMode,
    ] {
        let hints = catalog.hints_for(mode);
        assert!(hints.entries.is_empty(), "{mode:?} ships no bindings");
        assert!(hints.removed.is_empty());
    }
}

#[test]
fn prefix_labels_carry_the_shipped_names() {
    let hints = catalog().hints_for(LockMode::Normal);
    assert_eq!(
        hints.prefix_labels.get(&ctrl('p')).map(String::as_str),
        Some("PANE")
    );
    assert_eq!(
        hints.prefix_labels.get(&ctrl('s')).map(String::as_str),
        Some("RESIZE")
    );
    assert_eq!(
        hints.prefix_labels.get(&ctrl('t')).map(String::as_str),
        Some("TAB")
    );
    assert_eq!(hints.prefix_labels.len(), 3);
}

#[test]
fn a_complete_binding_reports_an_exact_match_and_no_longer_sequence() {
    let matched = catalog().match_sequence(LockMode::Normal, &KeySequence::from(ctrl('q')));
    assert_eq!(
        matched.exact,
        Some(BoundAction {
            action: ActionRef::core("quit").expect("`core:quit` is a valid action name"),
            args: koshi_core::resolve::ActionArgs::None,
        })
    );
    assert!(
        !matched.prefix,
        "nothing shipped continues past `<C-q>`, so it is not a prefix of a longer binding"
    );
}

#[test]
fn a_prefix_chord_reports_no_exact_match() {
    let matched = catalog().match_sequence(LockMode::Normal, &KeySequence::from(ctrl('p')));
    assert_eq!(matched.exact, None);
    assert!(matched.prefix);
}

#[test]
fn an_unbound_sequence_matches_nothing() {
    let catalog = catalog();
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &KeySequence::from(ctrl('y'))),
        KeyMatch::default()
    );
    // A mode nothing binds in holds no map at all.
    assert_eq!(
        catalog.match_sequence(LockMode::Resize, &KeySequence::from(ctrl('q'))),
        KeyMatch::default()
    );
}

#[test]
fn the_configured_unlock_alternative_becomes_the_escape_chord() {
    let alt_u = KeyChord::new(ModFlags::ALT, Key::Char('u'));
    let config = KeybindingsConfig {
        chord_timeout_ms: 1234,
        unlock_alternative: Some(alt_u),
        ..KeybindingsConfig::default()
    };

    let catalog = KeymapHintCatalog::from_parts(
        &keymap_layers(None, Leader::default()),
        &config,
        &ActionRegistry::new(),
    );

    assert_eq!(catalog.unlock_chord(), alt_u);
    assert_eq!(catalog.chord_timeout(), Duration::from_millis(1234));
}

#[test]
fn the_unlock_chord_is_the_reserved_one_when_the_config_names_no_alternative() {
    assert_eq!(catalog().unlock_chord(), KeybindingsConfig::RESERVED_UNLOCK);
    assert_eq!(catalog().chord_timeout(), Duration::from_millis(500));
}

#[test]
fn a_rebound_leader_moves_the_prefix_labels() {
    let config = KeybindingsConfig {
        leader: Leader::Mods(ModFlags::ALT),
        ..KeybindingsConfig::default()
    };

    let hints = KeymapHintCatalog::from_parts(
        &keymap_layers(None, Leader::Mods(ModFlags::ALT)),
        &config,
        &ActionRegistry::new(),
    )
    .hints_for(LockMode::Normal);

    assert_eq!(
        hints
            .prefix_labels
            .get(&KeyChord::new(ModFlags::ALT, Key::Char('p')))
            .map(String::as_str),
        Some("PANE")
    );
    assert_eq!(hints.prefix_labels.get(&ctrl('p')), None);
}

#[test]
fn reverted_defaults_to_false() {
    assert!(!catalog().hints_for(LockMode::Normal).reverted);
}

#[test]
fn with_reverted_marks_every_modes_hints() {
    let catalog = catalog().with_reverted();
    assert!(catalog.hints_for(LockMode::Normal).reverted);
    assert!(catalog.hints_for(LockMode::Locked).reverted);
}

#[test]
fn frames_share_the_per_mode_data_by_reference() {
    let catalog = catalog();
    let first = catalog.hints_for(LockMode::Normal);
    let second = catalog.hints_for(LockMode::Normal);
    assert!(Arc::ptr_eq(&first.entries, &second.entries));
    assert!(Arc::ptr_eq(&first.prefix_labels, &second.prefix_labels));
    assert!(Arc::ptr_eq(&first.removed, &second.removed));
}

#[test]
fn a_user_binding_takes_the_default_key_and_shows_as_user_set() {
    let fullscreen = KeySequence::from(alt('f'));
    let catalog = catalog_with_user(
        "normal",
        BTreeMap::from([(fullscreen.clone(), bound("quit"))]),
        BTreeSet::new(),
    );
    let hints = catalog.hints_for(LockMode::Normal);

    // The user entry replaces the default on that key rather than adding one.
    assert_eq!(hints.entries.len(), 22);
    let entry = hints
        .entries
        .iter()
        .find(|entry| entry.sequence == fullscreen)
        .expect("the user binding yields a hint");
    assert_eq!(entry.label, "Quit");
    assert!(entry.user_set);
    assert!(!entry.pinned);
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &fullscreen).exact,
        Some(bound("quit"))
    );
}

#[test]
fn a_user_removal_drops_the_hint_and_matches_nothing() {
    let fullscreen = KeySequence::from(alt('f'));
    let catalog = catalog_with_user(
        "normal",
        BTreeMap::new(),
        BTreeSet::from([fullscreen.clone()]),
    );
    let hints = catalog.hints_for(LockMode::Normal);

    assert_eq!(hints.entries.len(), 21);
    assert_eq!(
        hints
            .entries
            .iter()
            .find(|entry| entry.sequence == fullscreen),
        None
    );
    assert_eq!(*hints.removed, BTreeSet::from([fullscreen.clone()]));
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &fullscreen),
        KeyMatch::default()
    );
    // The removal belongs to the mode that authored it.
    assert_eq!(
        *catalog.hints_for(LockMode::Locked).removed,
        BTreeSet::new()
    );
}

#[test]
fn a_binding_the_resolver_refuses_yields_no_hint() {
    // `core:copy-selection` is registered without an implementation in this
    // build, so the merge drops the binding and no hint carries it.
    let key = KeySequence::from(ctrl('y'));
    let catalog = catalog_with_user(
        "normal",
        BTreeMap::from([(key.clone(), bound("copy-selection"))]),
        BTreeSet::new(),
    );
    let hints = catalog.hints_for(LockMode::Normal);

    assert_eq!(hints.entries.len(), 22);
    assert_eq!(
        hints.entries.iter().find(|entry| entry.sequence == key),
        None
    );
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &key),
        KeyMatch::default()
    );
}

#[test]
fn a_sequence_that_both_fires_and_opens_a_longer_one_reports_both() {
    let open = KeySequence::from(ctrl('y'));
    let longer = KeySequence::new(
        ctrl('y'),
        vec![KeyChord::new(ModFlags::NONE, Key::Char('a'))],
    );
    let catalog = catalog_with_user(
        "normal",
        BTreeMap::from([
            (open.clone(), bound("quit")),
            (longer.clone(), bound("lock")),
        ]),
        BTreeSet::new(),
    );

    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &open),
        KeyMatch {
            exact: Some(bound("quit")),
            prefix: true,
        }
    );
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &longer),
        KeyMatch {
            exact: Some(bound("lock")),
            prefix: false,
        }
    );
}

#[test]
fn every_locked_entry_firing_unlock_is_pinned() {
    let catalog = catalog_with_user(
        "locked",
        BTreeMap::from([(KeySequence::from(alt('u')), bound("unlock"))]),
        BTreeSet::new(),
    );
    let hints = catalog.hints_for(LockMode::Locked);

    assert_eq!(hints.entries.len(), 4);
    let pinned: Vec<String> = hints
        .entries
        .iter()
        .filter(|entry| entry.pinned)
        .map(|entry| entry.sequence.to_string())
        .collect();
    assert_eq!(pinned, vec!["<C-l>", "<A-u>"]);
}

#[test]
fn a_chord_depth_cap_of_one_drops_every_multi_chord_default() {
    let catalog = catalog_with_config(&KeybindingsConfig {
        max_chord_depth: 1,
        ..KeybindingsConfig::default()
    });
    let hints = catalog.hints_for(LockMode::Normal);

    let sequences: Vec<String> = hints
        .entries
        .iter()
        .map(|entry| entry.sequence.to_string())
        .collect();
    assert_eq!(
        sequences,
        vec!["<Tab>", "<C-g>", "<C-l>", "<C-q>", "<A-f>", "<S-Tab>"]
    );
    // `<C-p>` opened the pane group, whose entries are all two chords long.
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &KeySequence::from(ctrl('p'))),
        KeyMatch::default()
    );
}

#[test]
fn a_chord_leader_collapses_the_groups_and_drops_every_prefix_label() {
    // All three groups open at the leader chord itself, so no label names one
    // group and none is offered.
    let space = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Space));
    let hints = catalog_with_config(&KeybindingsConfig {
        leader: Leader::Chord(space),
        ..KeybindingsConfig::default()
    })
    .hints_for(LockMode::Normal);

    assert_eq!(*hints.prefix_labels, BTreeMap::new());
}

#[test]
fn the_chord_timeout_carries_the_configured_milliseconds_at_both_bounds() {
    let zero = catalog_with_config(&KeybindingsConfig {
        chord_timeout_ms: 0,
        ..KeybindingsConfig::default()
    });
    assert_eq!(zero.chord_timeout(), Duration::ZERO);

    let longest = catalog_with_config(&KeybindingsConfig {
        chord_timeout_ms: u32::MAX,
        ..KeybindingsConfig::default()
    });
    assert_eq!(
        longest.chord_timeout(),
        Duration::from_millis(4_294_967_295)
    );
}

#[test]
fn with_reverted_changes_only_the_flag() {
    let base = catalog();
    let reverted = base.clone().with_reverted();
    let before = base.hints_for(LockMode::Normal);
    let after = reverted.hints_for(LockMode::Normal);

    assert!(!before.reverted);
    assert!(after.reverted);
    assert!(Arc::ptr_eq(&before.entries, &after.entries));
    assert!(Arc::ptr_eq(&before.removed, &after.removed));
    assert!(Arc::ptr_eq(&before.prefix_labels, &after.prefix_labels));
    assert_eq!(reverted.unlock_chord(), base.unlock_chord());
    assert_eq!(reverted.chord_timeout(), base.chord_timeout());
    let quit = KeySequence::from(ctrl('q'));
    assert_eq!(
        reverted.match_sequence(LockMode::Normal, &quit),
        base.match_sequence(LockMode::Normal, &quit)
    );
}

#[test]
fn a_binding_in_a_mode_the_build_does_not_register_yields_no_hint() {
    let key = KeySequence::from(alt('u'));
    let catalog = catalog_with_user(
        "vim",
        BTreeMap::from([(key.clone(), bound("quit"))]),
        BTreeSet::new(),
    );

    for mode in LockMode::ALL {
        let hints = catalog.hints_for(mode);
        assert_eq!(
            hints.entries.iter().find(|entry| entry.sequence == key),
            None,
            "{mode:?} carries the binding from the unregistered mode"
        );
        assert_eq!(catalog.match_sequence(mode, &key), KeyMatch::default());
    }
    // The shipped defaults are untouched by the skipped mode.
    assert_eq!(catalog.hints_for(LockMode::Normal).entries.len(), 22);
}

#[test]
fn a_chord_depth_cap_of_zero_drops_every_binding_and_keeps_the_escape_chord() {
    let catalog = catalog_with_config(&KeybindingsConfig {
        max_chord_depth: 0,
        ..KeybindingsConfig::default()
    });

    for mode in LockMode::ALL {
        assert_eq!(
            *catalog.hints_for(mode).entries,
            Vec::<HintBinding>::new(),
            "{mode:?}"
        );
    }
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &KeySequence::from(ctrl('q'))),
        KeyMatch::default()
    );
    // The unlock chord resolves ahead of the keymap, so an empty keymap still
    // reports it.
    assert_eq!(catalog.unlock_chord(), KeybindingsConfig::RESERVED_UNLOCK);
}

#[test]
fn a_removal_of_a_key_nothing_binds_is_still_listed_as_removed() {
    let unbound = KeySequence::from(alt('z'));
    let catalog = catalog_with_user("normal", BTreeMap::new(), BTreeSet::from([unbound.clone()]));
    let hints = catalog.hints_for(LockMode::Normal);

    assert_eq!(hints.entries.len(), 22);
    assert_eq!(*hints.removed, BTreeSet::from([unbound.clone()]));
    assert_eq!(
        catalog.match_sequence(LockMode::Normal, &unbound),
        KeyMatch::default()
    );
}

#[test]
fn a_locked_sequence_holding_the_unlock_chord_yields_no_hint() {
    let key = KeySequence::new(
        KeybindingsConfig::RESERVED_UNLOCK,
        vec![KeyChord::new(ModFlags::NONE, Key::Char('x'))],
    );
    let catalog = catalog_with_user(
        "locked",
        BTreeMap::from([(key.clone(), bound("quit"))]),
        BTreeSet::new(),
    );
    let hints = catalog.hints_for(LockMode::Locked);

    assert_eq!(hints.entries.len(), 3);
    assert_eq!(
        hints.entries.iter().find(|entry| entry.sequence == key),
        None
    );
    assert_eq!(
        catalog.match_sequence(LockMode::Locked, &key),
        KeyMatch::default()
    );
    // The one-chord unlock stays live, and the dropped sequence opens nothing.
    assert_eq!(
        catalog.match_sequence(
            LockMode::Locked,
            &KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
        ),
        KeyMatch {
            exact: Some(bound("unlock")),
            prefix: false,
        }
    );
}

#[test]
fn entries_are_sorted_by_key_sequence_with_no_key_twice() {
    let hints = catalog().hints_for(LockMode::Normal);
    let sequences: Vec<KeySequence> = hints
        .entries
        .iter()
        .map(|entry| entry.sequence.clone())
        .collect();

    let mut sorted_unique = sequences.clone();
    sorted_unique.sort();
    sorted_unique.dedup();
    assert_eq!(sequences, sorted_unique);
}
