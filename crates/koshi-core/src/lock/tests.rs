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
        ["normal", "locked", "resize", "pane", "tab", "scroll", "search"]
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
        LockMode::SearchMode,
    ] {
        let json = serde_json::to_string(&mode).expect("serialize");
        let restored: LockMode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(mode, restored);
    }
}
