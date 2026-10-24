//! Tests for client lock mode.

use super::*;

#[test]
fn the_default_lock_mode_is_normal() {
    assert_eq!(LockMode::default(), LockMode::Normal);
}

#[test]
fn all_lock_modes_list_each_mode_once_with_its_keymap_name() {
    let keymap_names: Vec<&str> = LockMode::ALL
        .iter()
        .map(|mode| mode.get_keymap_name())
        .collect();
    assert_eq!(
        keymap_names,
        [
            "normal",
            "locked",
            "pane-placement",
            "pane-move-choice",
            "floating-pane-move",
            "dock-undock"
        ]
    );
}

#[test]
fn a_lock_mode_survives_a_serde_round_trip() {
    for mode in LockMode::ALL {
        let serialized_lock_mode = serde_json::to_string(&mode).expect("serialize");
        let deserialized_lock_mode: LockMode =
            serde_json::from_str(&serialized_lock_mode).expect("deserialize");
        assert_eq!(mode, deserialized_lock_mode);
    }
}

#[test]
fn serde_wire_form_is_the_pascal_case_variant_not_the_keymap_name() {
    // The wire form is the variant name, `Locked`. `get_keymap_name()` gives the
    // keymap name, `locked`.
    assert_eq!(
        serde_json::to_string(&LockMode::Locked).expect("serialize"),
        "\"Locked\""
    );
    assert_eq!(
        serde_json::to_string(&LockMode::PaneMoveChoice).expect("serialize"),
        "\"PaneMoveChoice\""
    );
}

#[test]
fn only_normal_and_locked_pass_unbound_input_to_the_pane() {
    let mode_expectations = [
        (LockMode::Normal, true),
        (LockMode::Locked, true),
        (LockMode::PanePlacement, false),
        (LockMode::PaneMoveChoice, false),
        (LockMode::FloatingPaneMove, false),
        (LockMode::DockUndock, false),
    ];
    for (mode, expected_should_pass_unbound_input) in mode_expectations {
        assert_eq!(
            mode.should_pass_unbound_input_to_pane(),
            expected_should_pass_unbound_input,
            "{mode:?} must answer {expected_should_pass_unbound_input}"
        );
    }
}

#[test]
fn decoding_the_keymap_name_is_refused() {
    let lock_mode_parse_error = serde_json::from_str::<LockMode>("\"locked\"")
        .expect_err("the wire form is the variant name, not the keymap name");
    assert_eq!(
        lock_mode_parse_error.to_string(),
        "unknown variant `locked`, expected one of `Normal`, `Locked`, `PanePlacement`, \
         `PaneMoveChoice`, `FloatingPaneMove`, `DockUndock` at line 1 column 8"
    );
}
