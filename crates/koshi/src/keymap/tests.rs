//! Offline keymap view tests: defaults-only and user-layer folding, the
//! all-or-nothing revert, steal visibility, and the file dry-run.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use koshi_config::key::Leader;
use koshi_config::key_sequence::parse_sequence;
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionReference;
use koshi_core::key::{BindingModifierFlags, KeySequence};

use super::*;

use koshi_config::conflict::LayerOrigin;
use koshi_config::types::build_default_mode_bindings;

/// Parse `key_sequence_text` with the default leader and a depth of 8.
pub(crate) fn parse_test_key_sequence(key_sequence_text: &str) -> KeySequence {
    parse_sequence(key_sequence_text, KeybindingsConfig::default().leader, 8)
        .expect("test sequence parses")
}

/// A user partial holding one `normal`-mode binding of `key_sequence_text` to
/// `action_reference_text`.
pub(crate) fn build_partial_keybindings_config_with_binding(
    key_sequence_text: &str,
    action_reference_text: &str,
) -> PartialKeybindingsConfig {
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        parse_test_key_sequence(key_sequence_text),
        BoundAction {
            action_reference: ActionReference::from_str(action_reference_text)
                .expect("valid action reference"),
        },
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: Default::default(),
        },
    );
    PartialKeybindingsConfig {
        mode_bindings_by_name: Some(mode_bindings_by_name),
        ..PartialKeybindingsConfig::default()
    }
}

#[test]
fn the_offline_default_layer_follows_the_configured_leader() {
    let alt_leader = Leader::Modifiers(BindingModifierFlags::ALT);
    let keymap_layers = build_keymap_layers(None, alt_leader);
    assert_eq!(keymap_layers.len(), 1);
    // The default table is built against `alt_leader`, not against the
    // built-in Ctrl leader.
    assert_eq!(
        keymap_layers[0].mode_bindings_by_name,
        build_default_mode_bindings(alt_leader)
    );
    assert_ne!(
        keymap_layers[0].mode_bindings_by_name,
        build_default_mode_bindings(Leader::default())
    );
}

#[test]
fn a_configured_leader_moves_the_offline_defaults_off_ctrl() {
    // The whole offline view path: a file that sets only `leader "A-"` admits,
    // and its default bindings resolve against Alt. They differ from the
    // built-in Ctrl defaults.
    let alt_keymap_view = build_keymap_view_from_partial(
        Some(PartialKeybindingsConfig {
            leader: Some(Leader::Modifiers(BindingModifierFlags::ALT)),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );
    assert!(
        !alt_keymap_view.is_reverted_to_defaults,
        "a leader-only file has no conflicts to revert"
    );

    let default_keymap_view = build_keymap_view_from_partial(None, None, None);
    let normal_mode_name = ModeName::from_text("normal");
    let alt_default_key_sequences: BTreeSet<_> = alt_keymap_view.merged_keymap.mode_keymap_by_name
        [&normal_mode_name]
        .default_bindings_by_key_sequence
        .keys()
        .collect();
    let built_in_default_key_sequences: BTreeSet<_> =
        default_keymap_view.merged_keymap.mode_keymap_by_name[&normal_mode_name]
            .default_bindings_by_key_sequence
            .keys()
            .collect();
    assert_ne!(alt_default_key_sequences, built_in_default_key_sequences);
}

#[test]
fn defaults_only_view_is_not_reverted_and_lists_the_shipped_bindings() {
    let keymap_view = build_keymap_view_from_partial(None, None, None);
    assert!(!keymap_view.is_reverted_to_defaults);
    assert_eq!(keymap_view.keybindings_config, KeybindingsConfig::default());
    let normal_mode_bindings =
        &keymap_view.merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];
    assert_eq!(
        normal_mode_bindings.default_bindings_by_key_sequence[&parse_test_key_sequence("<Tab>")]
            .action_reference,
        ActionReference::from_core_action_name("next-tab").unwrap()
    );
    assert!(normal_mode_bindings
        .user_bindings_by_key_sequence
        .is_empty());
}

#[test]
fn an_admitted_user_layer_appears_as_user_set() {
    let keymap_view = build_keymap_view_from_partial(
        Some(build_partial_keybindings_config_with_binding(
            "<C-y>",
            "core:new-tab",
        )),
        None,
        None,
    );
    assert!(!keymap_view.is_reverted_to_defaults);
    let normal_mode_bindings =
        &keymap_view.merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];
    let user_binding =
        &normal_mode_bindings.user_bindings_by_key_sequence[&parse_test_key_sequence("<C-y>")];
    assert_eq!(
        user_binding.bound_action.action_reference,
        ActionReference::from_core_action_name("new-tab").unwrap()
    );
    assert_eq!(user_binding.layer_origin, LayerOrigin::User);
}

#[test]
fn a_steal_moves_the_default_to_unbound() {
    let keymap_view = build_keymap_view_from_partial(
        Some(build_partial_keybindings_config_with_binding(
            "<A-f>",
            "core:close-pane",
        )),
        None,
        None,
    );
    let normal_mode_bindings =
        &keymap_view.merged_keymap.mode_keymap_by_name[&ModeName::from_text("normal")];
    assert_eq!(
        normal_mode_bindings.user_bindings_by_key_sequence[&parse_test_key_sequence("<A-f>")]
            .bound_action
            .action_reference,
        ActionReference::from_core_action_name("close-pane").unwrap()
    );
    assert!(!normal_mode_bindings
        .default_bindings_by_key_sequence
        .contains_key(&parse_test_key_sequence("<A-f>")));
    assert_eq!(
        normal_mode_bindings.unbound_default_bindings_by_key_sequence
            [&parse_test_key_sequence("<A-f>")]
            .action_reference,
        ActionReference::from_core_action_name("toggle-pane-fullscreen").unwrap()
    );
}

#[test]
fn a_rejected_user_layer_reverts_its_bindings_and_folded_fields_to_defaults() {
    // The file sets a chord timeout and removes the locked-mode reserved
    // unlock `<C-l>`. Removing the reserved unlock is a fatal finding.
    let mut removed_key_sequences = BTreeSet::new();
    removed_key_sequences.insert(parse_test_key_sequence("<C-l>"));
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences,
        },
    );

    let keymap_view = build_keymap_view_from_partial(
        Some(PartialKeybindingsConfig {
            chord_timeout_ms: Some(750),
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );

    assert!(keymap_view.is_reverted_to_defaults);
    assert_eq!(
        keymap_view.conflict_report.get_verdict(),
        KeymapVerdict::Reject
    );
    assert_eq!(keymap_view.keybindings_config, KeybindingsConfig::default());
    let locked_mode_bindings =
        &keymap_view.merged_keymap.mode_keymap_by_name[&ModeName::from_text("locked")];
    assert_eq!(
        locked_mode_bindings.default_bindings_by_key_sequence[&parse_test_key_sequence("<C-l>")]
            .action_reference,
        ActionReference::from_core_action_name("unlock").unwrap()
    );
}

#[test]
fn a_file_error_reverts_the_view_and_carries_the_reason() {
    let keymap_view = build_keymap_view_from_partial(None, None, Some("boom".to_string()));
    assert!(keymap_view.is_reverted_to_defaults);
    assert_eq!(
        keymap_view.keybinding_file_error_message.as_deref(),
        Some("boom")
    );
    assert_eq!(keymap_view.keybindings_config, KeybindingsConfig::default());
}

#[test]
fn an_admitted_user_layer_folds_its_timeout_and_depth_fields_onto_the_defaults() {
    // A file that sets only the chord timers and depth has no conflicts. It
    // admits, and its values replace the defaults in the effective config.
    let keymap_view = build_keymap_view_from_partial(
        Some(PartialKeybindingsConfig {
            chord_timeout_ms: Some(750),
            which_key_delay_ms: Some(250),
            maximum_chord_depth: Some(6),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );
    assert!(!keymap_view.is_reverted_to_defaults);
    assert_eq!(keymap_view.keybindings_config.chord_timeout_ms, 750);
    assert_eq!(keymap_view.keybindings_config.which_key_delay_ms, 250);
    assert_eq!(keymap_view.keybindings_config.maximum_chord_depth, 6);
}

#[test]
fn a_syntax_error_lists_as_one_line() {
    // An unbalanced brace is a KDL syntax error: the parser returns the
    // `Syntax` variant, and it lists as exactly one line.
    let keybinding_parse_error =
        parse_keybindings(Path::new("keybinding.kdl"), "mode \"normal\" {")
            .expect_err("unbalanced brace is a syntax error");
    match &keybinding_parse_error {
        KeybindingParseError::Syntax(syntax_error) => {
            assert_eq!(
                list_parse_error_lines(&keybinding_parse_error),
                vec![syntax_error.to_string()]
            );
        }
        other_parse_error => panic!("expected a syntax error, got {other_parse_error:?}"),
    }
}

#[test]
fn validate_keybinding_file_reports_parse_failures_and_accepts_clean_files() {
    // Each run writes its two files into a temporary directory of its own.
    let test_directory = tempfile::tempdir().expect("test directory");
    let valid_keybinding_file_path = test_directory.path().join("good.kdl");
    let invalid_keybinding_file_path = test_directory.path().join("bad.kdl");
    std::fs::write(
        &valid_keybinding_file_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    std::fs::write(
        &invalid_keybinding_file_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n",
    )
    .expect("write");

    match validate_keybinding_file(&valid_keybinding_file_path).expect("readable") {
        KeymapValidationOutcome::Checked {
            is_applicable,
            conflict_report,
        } => {
            assert!(is_applicable);
            assert_eq!(conflict_report.get_verdict(), KeymapVerdict::Apply);
        }
        KeymapValidationOutcome::ParseFailed(keybinding_parse_error_messages) => {
            panic!("expected clean check, got {keybinding_parse_error_messages:?}")
        }
    }
    match validate_keybinding_file(&invalid_keybinding_file_path).expect("readable") {
        KeymapValidationOutcome::ParseFailed(keybinding_parse_error_messages) => {
            assert_eq!(
                keybinding_parse_error_messages,
                vec!["invalid key `<C-`: missing closing `>`".to_string()]
            );
        }
        KeymapValidationOutcome::Checked { .. } => panic!("expected a parse failure"),
    }
}

#[test]
fn validate_keybinding_file_returns_not_found_for_a_missing_path() {
    let test_directory = tempfile::tempdir().expect("test directory");
    let missing_keybinding_file_path = test_directory.path().join("absent.kdl");

    match validate_keybinding_file(&missing_keybinding_file_path) {
        Err(keybinding_file_read_error) => {
            assert_eq!(
                keybinding_file_read_error.kind(),
                std::io::ErrorKind::NotFound
            )
        }
        Ok(_) => panic!("expected a read error"),
    }
}

#[test]
fn two_invalid_binds_list_as_two_lines_in_file_order() {
    let keybinding_parse_error = parse_keybindings(
        Path::new("keybinding.kdl"),
        "version 1\nmode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n    bind \"<A-\" \"core:quit\"\n}\n",
    )
    .expect_err("both key strings are invalid");

    assert_eq!(
        list_parse_error_lines(&keybinding_parse_error),
        vec![
            "invalid key `<C-`: missing closing `>`".to_string(),
            "invalid key `<A-`: missing closing `>`".to_string(),
        ]
    );
}
