//! Offline keymap view tests: defaults-only and user-layer folding, the
//! all-or-nothing revert, steal visibility, and the file dry-run.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use koshi_config::key::Leader;
use koshi_config::key_sequence::parse_sequence;
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionReference;
use koshi_core::key::{KeySequence, ModFlags};
use koshi_core::resolve::ActionArgs;

use super::*;

use koshi_config::conflict::LayerOrigin;
use koshi_config::types::build_default_mode_bindings;

/// Parse a test key sequence with the default leader and depth.
fn parse_test_key_sequence(key_sequence: &str) -> KeySequence {
    parse_sequence(key_sequence, KeybindingsConfig::default().leader, 8)
        .expect("test sequence parses")
}

/// A user partial holding one `normal`-mode binding of `key` to `action`.
fn build_partial_binding(
    key_sequence_text: &str,
    action_reference_text: &str,
) -> PartialKeybindingsConfig {
    let mut bound_action_by_key_sequence = BTreeMap::new();
    bound_action_by_key_sequence.insert(
        parse_test_key_sequence(key_sequence_text),
        BoundAction {
            action_reference: ActionReference::from_str(action_reference_text)
                .expect("valid action reference"),
            action_arguments: ActionArgs::None,
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
    let alt_leader = Leader::Mods(ModFlags::ALT);
    let keymap_layers = build_keymap_layers(None, alt_leader);
    assert_eq!(keymap_layers.len(), 1);
    // The default table is built against the passed leader — the same one a
    // running koshi uses — not the built-in Ctrl table.
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
    // The whole offline view path: a file that only sets `leader "A-"` admits,
    // and its default bindings resolve against Alt, so they differ from the
    // built-in Ctrl defaults. Fails if `build_keymap_view_from_partial` stops threading the
    // effective leader into the default layer.
    let alt_view = build_keymap_view_from_partial(
        Some(PartialKeybindingsConfig {
            leader: Some(Leader::Mods(ModFlags::ALT)),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );
    assert!(
        !alt_view.is_reverted_to_defaults,
        "a leader-only file has no conflicts to revert"
    );

    let default_view = build_keymap_view_from_partial(None, None, None);
    let normal_mode_name = ModeName::from_text("normal");
    let alt_keys: BTreeSet<_> = alt_view.merged_keymap.mode_map_by_name[&normal_mode_name]
        .default_bindings_by_key_sequence
        .keys()
        .collect();
    let default_keys: BTreeSet<_> = default_view.merged_keymap.mode_map_by_name[&normal_mode_name]
        .default_bindings_by_key_sequence
        .keys()
        .collect();
    assert_ne!(alt_keys, default_keys);
}

#[test]
fn defaults_only_view_is_not_reverted_and_lists_the_shipped_bindings() {
    let view = build_keymap_view_from_partial(None, None, None);
    assert!(!view.is_reverted_to_defaults);
    assert_eq!(view.config, KeybindingsConfig::default());
    let normal = &view.merged_keymap.mode_map_by_name[&ModeName::from_text("normal")];
    assert_eq!(
        normal.default_bindings_by_key_sequence[&parse_test_key_sequence("<Tab>")].action_reference,
        ActionReference::from_core_action_name("next-tab").unwrap()
    );
    assert!(normal.user_bindings_by_key_sequence.is_empty());
}

#[test]
fn an_admitted_user_layer_appears_as_user_set() {
    let view = build_keymap_view_from_partial(
        Some(build_partial_binding("<C-y>", "core:new-tab")),
        None,
        None,
    );
    assert!(!view.is_reverted_to_defaults);
    let normal = &view.merged_keymap.mode_map_by_name[&ModeName::from_text("normal")];
    let binding = &normal.user_bindings_by_key_sequence[&parse_test_key_sequence("<C-y>")];
    assert_eq!(
        binding.bound_action.action_reference,
        ActionReference::from_core_action_name("new-tab").unwrap()
    );
    assert_eq!(binding.layer_origin, LayerOrigin::User);
}

#[test]
fn a_steal_moves_the_default_to_unbound() {
    let view = build_keymap_view_from_partial(
        Some(build_partial_binding("<A-f>", "core:close-pane")),
        None,
        None,
    );
    let normal = &view.merged_keymap.mode_map_by_name[&ModeName::from_text("normal")];
    assert_eq!(
        normal.user_bindings_by_key_sequence[&parse_test_key_sequence("<A-f>")]
            .bound_action
            .action_reference,
        ActionReference::from_core_action_name("close-pane").unwrap()
    );
    assert!(!normal
        .default_bindings_by_key_sequence
        .contains_key(&parse_test_key_sequence("<A-f>")));
    assert_eq!(
        normal.unbound_default_bindings_by_key_sequence[&parse_test_key_sequence("<A-f>")]
            .action_reference,
        ActionReference::from_core_action_name("toggle-pane-fullscreen").unwrap()
    );
}

#[test]
fn a_fatal_user_layer_reverts_the_view_to_defaults() {
    // Removing the locked-mode reserved unlock is a fatal finding.
    let mut mode_bindings_by_name = BTreeMap::new();
    let mut removed_key_sequences = std::collections::BTreeSet::new();
    removed_key_sequences.insert(parse_test_key_sequence("<C-l>"));
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences,
        },
    );
    let partial_keybindings_config = PartialKeybindingsConfig {
        mode_bindings_by_name: Some(mode_bindings_by_name),
        ..PartialKeybindingsConfig::default()
    };

    let view = build_keymap_view_from_partial(Some(partial_keybindings_config), None, None);
    assert!(view.is_reverted_to_defaults);
    assert_ne!(view.report.get_verdict(), KeymapVerdict::Apply);
    // The defaults survive: the reserved unlock still fires.
    let locked = &view.merged_keymap.mode_map_by_name[&ModeName::from_text("locked")];
    assert_eq!(
        locked.default_bindings_by_key_sequence[&parse_test_key_sequence("<C-l>")].action_reference,
        ActionReference::from_core_action_name("unlock").unwrap()
    );
}

#[test]
fn a_file_error_reverts_the_view_and_carries_the_reason() {
    let view = build_keymap_view_from_partial(None, None, Some("boom".to_string()));
    assert!(view.is_reverted_to_defaults);
    assert_eq!(view.file_error_message.as_deref(), Some("boom"));
    assert_eq!(view.config, KeybindingsConfig::default());
}

#[test]
fn an_admitted_user_layer_folds_its_timeout_and_depth_fields_onto_the_defaults() {
    // A file that only tweaks the chord timers and depth has no conflicts, so
    // it admits and its values replace the defaults in the effective config.
    let view = build_keymap_view_from_partial(
        Some(PartialKeybindingsConfig {
            chord_timeout_ms: Some(750),
            which_key_delay_ms: Some(250),
            max_chord_depth: Some(6),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );
    assert!(!view.is_reverted_to_defaults);
    assert_eq!(view.config.chord_timeout_ms, 750);
    assert_eq!(view.config.which_key_delay_ms, 250);
    assert_eq!(view.config.max_chord_depth, 6);
}

#[test]
fn a_syntax_error_renders_as_one_line_that_render_joins_unchanged() {
    // An unbalanced brace is a KDL syntax error, so the parser returns the
    // `Syntax` variant. That branch renders as exactly one line, and the
    // single-string render is that same line.
    let keybinding_parse_error =
        parse_keybindings(Path::new("keybinding.kdl"), "mode \"normal\" {")
            .expect_err("unbalanced brace is a syntax error");
    match &keybinding_parse_error {
        KeybindingParseError::Syntax(inner) => {
            assert_eq!(
                parse_error_lines(&keybinding_parse_error),
                vec![inner.to_string()]
            );
            assert_eq!(
                render_parse_error(&keybinding_parse_error),
                inner.to_string()
            );
        }
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

#[test]
fn validate_file_reports_parse_failures_and_clean_files() {
    // A directory of this run's own: this crate builds a library and a binary
    // target, so the whole suite runs this test in two processes at once and a
    // shared file name is written and deleted by both.
    let test_directory = tempfile::tempdir().expect("test directory");
    let valid_keymap_path = test_directory.path().join("good.kdl");
    let invalid_keymap_path = test_directory.path().join("bad.kdl");
    std::fs::write(
        &valid_keymap_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    std::fs::write(
        &invalid_keymap_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n",
    )
    .expect("write");

    match validate_keymap_file(&valid_keymap_path).expect("readable") {
        KeymapValidationOutcome::Checked {
            is_applicable,
            report,
        } => {
            assert!(is_applicable);
            assert_eq!(report.get_verdict(), KeymapVerdict::Apply);
        }
        KeymapValidationOutcome::ParseFailed(parse_error_messages) => {
            panic!("expected clean check, got {parse_error_messages:?}")
        }
    }
    match validate_keymap_file(&invalid_keymap_path).expect("readable") {
        KeymapValidationOutcome::ParseFailed(parse_error_messages) => {
            assert_eq!(parse_error_messages.len(), 1);
            assert!(
                parse_error_messages[0].contains("<C-"),
                "got: {}",
                parse_error_messages[0]
            );
        }
        KeymapValidationOutcome::Checked { .. } => panic!("expected a parse failure"),
    }
}

#[test]
fn validate_file_returns_not_found_for_a_path_that_is_not_there() {
    let test_directory = tempfile::tempdir().expect("test directory");
    let missing = test_directory.path().join("absent.kdl");

    match validate_keymap_file(&missing) {
        Err(keymap_read_error) => {
            assert_eq!(keymap_read_error.kind(), std::io::ErrorKind::NotFound)
        }
        Ok(_) => panic!("expected a read error"),
    }
}

#[test]
fn a_refused_user_layer_drops_its_folded_scalar_fields_too() {
    // The file sets a chord timeout and removes the locked-mode reserved
    // unlock. The removal is fatal, so the whole section reverts and the
    // timeout goes back to the built-in value.
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

    let view = build_keymap_view_from_partial(
        Some(PartialKeybindingsConfig {
            chord_timeout_ms: Some(750),
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );

    assert!(view.is_reverted_to_defaults);
    assert_eq!(view.config, KeybindingsConfig::default());
}

#[test]
fn two_invalid_binds_render_as_two_lines_that_render_joins_with_a_semicolon() {
    let keybinding_parse_error = parse_keybindings(
        Path::new("keybinding.kdl"),
        "version 1\nmode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n    bind \"<A-\" \"core:quit\"\n}\n",
    )
    .expect_err("both key strings are invalid");

    let parse_error_lines = parse_error_lines(&keybinding_parse_error);
    assert_eq!(parse_error_lines.len(), 2);
    assert_eq!(
        render_parse_error(&keybinding_parse_error),
        format!("{}; {}", parse_error_lines[0], parse_error_lines[1])
    );
}
