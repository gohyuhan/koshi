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

    // 22 shipped normal-mode bindings minus the dead `<C-q>` quit.
    assert_eq!(hints.entries.len(), 21);

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
fn coming_soon_action_yields_no_hint() {
    let hints = catalog().hints_for(LockMode::Normal);
    let quit = KeySequence::from(ctrl('q'));
    assert!(
        !hints.entries.iter().any(|entry| entry.sequence == quit),
        "the ComingSoon quit binding must not surface as a hint"
    );
}

#[test]
fn locked_mode_pins_the_reserved_unlock() {
    let hints = catalog().hints_for(LockMode::Locked);
    assert_eq!(hints.entries.len(), 1);
    let entry = &hints.entries[0];
    assert_eq!(
        entry.sequence,
        KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
    );
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
        LockMode::SearchMode,
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
    assert_eq!(hints.prefix_labels.len(), 2);
}

#[test]
fn reverted_defaults_to_false() {
    assert!(!catalog().hints_for(LockMode::Normal).reverted);
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
