//! Tests for the keymap hint catalog: the defaults-only merge joined to
//! display names, the firing filter (a `ComingSoon` action yields no hint),
//! the pinned locked-mode unlock, empty modes, and the per-frame `Arc`
//! sharing.

use super::*;

use koshi_core::key::{Key, ModFlags};

/// The catalog resolved from a fresh registry — the built-in defaults over
/// the built-in action table, exactly what a stock runtime holds.
fn catalog() -> KeymapHintCatalog {
    KeymapHintCatalog::from_registry(&ActionRegistry::new())
}

/// A `Ctrl`-modified character chord.
fn ctrl(key: char) -> KeyChord {
    KeyChord::new(ModFlags::CTRL, Key::Char(key))
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
