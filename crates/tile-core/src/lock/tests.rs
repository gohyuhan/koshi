use super::LockMode;

#[test]
fn the_default_lock_mode_is_normal() {
    assert_eq!(LockMode::default(), LockMode::Normal);
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
