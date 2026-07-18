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

    // All 25 shipped normal-mode bindings fire in this build.
    assert_eq!(hints.entries.len(), 25);

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
