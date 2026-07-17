//! Tests for client lock mode.

use super::LockMode;

#[test]
fn the_default_lock_mode_is_normal() {
    assert_eq!(LockMode::default(), LockMode::Normal);
}

#[test]
fn all_lists_every_mode_once_with_its_keymap_name() {
    let names: Vec<&str> = LockMode::ALL.iter().map(|mode| mode.name()).collect();
    assert_eq!(
        names,
        ["normal", "locked", "resize", "pane", "tab", "scroll"]
    );
}

#[test]
fn a_lock_mode_survives_a_serde_round_trip() {
    for mode in [
        LockMode::Normal,
        LockMode::Locked,
        LockMode::Resize,
        LockMode::PaneMode,
        LockMode::TabMode,
        LockMode::ScrollMode,
    ] {
        let json = serde_json::to_string(&mode).expect("serialize");
        let restored: LockMode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(mode, restored);
    }
}

#[test]
fn serde_wire_form_is_the_pascal_case_variant_not_the_keymap_name() {
    // The wire form comes from the derive (variant name, e.g. `Locked`), which
    // is distinct from `name()` (the keymap grouping key, e.g. `locked`). A
    // caller must not conflate the two.
    assert_eq!(
        serde_json::to_string(&LockMode::Locked).expect("serialize"),
        "\"Locked\""
    );
    assert_eq!(
        serde_json::to_string(&LockMode::ScrollMode).expect("serialize"),
        "\"ScrollMode\""
    );
}
