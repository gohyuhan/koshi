//! Offline keymap view tests: defaults-only and user-layer folding, the
//! all-or-nothing revert, steal visibility, and the file dry-run.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use koshi_config::key::Leader;
use koshi_config::key_sequence::parse_sequence;
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionRef;
use koshi_core::key::{KeySequence, ModFlags};
use koshi_core::resolve::ActionArgs;

use super::*;

/// Parse a test key sequence with the default leader and depth.
fn seq(s: &str) -> KeySequence {
    parse_sequence(s, KeybindingsConfig::default().leader, 8).expect("test sequence parses")
}

/// A user partial holding one `normal`-mode binding of `key` to `action`.
fn partial_binding(key: &str, action: &str) -> PartialKeybindingsConfig {
    let mut keys = BTreeMap::new();
    keys.insert(
        seq(key),
        BoundAction {
            action: ActionRef::from_str(action).expect("valid ref"),
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("normal"),
        ModeBindings {
            keys,
            removed: Default::default(),
        },
    );
    PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    }
}

#[test]
fn the_offline_default_layer_follows_the_configured_leader() {
    let alt = Leader::Mods(ModFlags::ALT);
    let layers = keymap_layers(None, alt);
    assert_eq!(layers.len(), 1);
    // The default table is built against the passed leader — the same one a
    // running koshi uses — not the built-in Ctrl table.
    assert_eq!(layers[0].modes, default_mode_bindings(alt));
    assert_ne!(layers[0].modes, default_mode_bindings(Leader::default()));
}

#[test]
fn a_configured_leader_moves_the_offline_defaults_off_ctrl() {
    // The whole offline view path: a file that only sets `leader "A-"` admits,
    // and its default bindings resolve against Alt, so they differ from the
    // built-in Ctrl defaults. Fails if `view_from_partial` stops threading the
    // effective leader into the default layer.
    let alt_view = view_from_partial(
        Some(PartialKeybindingsConfig {
            leader: Some(Leader::Mods(ModFlags::ALT)),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );
    assert!(
        !alt_view.reverted,
        "a leader-only file has no conflicts to revert"
    );

    let default_view = view_from_partial(None, None, None);
    let mode = ModeName::new("normal");
    let alt_keys: BTreeSet<_> = alt_view.merged.modes[&mode].defaults.keys().collect();
    let default_keys: BTreeSet<_> = default_view.merged.modes[&mode].defaults.keys().collect();
    assert_ne!(alt_keys, default_keys);
}

#[test]
fn defaults_only_view_is_not_reverted_and_lists_the_shipped_bindings() {
    let view = view_from_partial(None, None, None);
    assert!(!view.reverted);
    assert_eq!(view.config, KeybindingsConfig::default());
    let normal = &view.merged.modes[&ModeName::new("normal")];
    assert_eq!(
        normal.defaults[&seq("<Tab>")].action,
        ActionRef::core("next-tab").unwrap()
    );
    assert!(normal.user_set.is_empty());
}

#[test]
fn an_admitted_user_layer_appears_as_user_set() {
    let view = view_from_partial(Some(partial_binding("<C-y>", "core:new-tab")), None, None);
    assert!(!view.reverted);
    let normal = &view.merged.modes[&ModeName::new("normal")];
    let binding = &normal.user_set[&seq("<C-y>")];
    assert_eq!(binding.bound.action, ActionRef::core("new-tab").unwrap());
    assert_eq!(binding.source, LayerOrigin::User);
}

#[test]
fn a_steal_moves_the_default_to_unbound() {
    let view = view_from_partial(
        Some(partial_binding("<A-f>", "core:close-pane")),
        None,
        None,
    );
    let normal = &view.merged.modes[&ModeName::new("normal")];
    assert_eq!(
        normal.user_set[&seq("<A-f>")].bound.action,
        ActionRef::core("close-pane").unwrap()
    );
    assert!(!normal.defaults.contains_key(&seq("<A-f>")));
    assert_eq!(
        normal.unbound_defaults[&seq("<A-f>")].action,
        ActionRef::core("toggle-pane-fullscreen").unwrap()
    );
}

#[test]
fn a_fatal_user_layer_reverts_the_view_to_defaults() {
    // Removing the locked-mode reserved unlock is a fatal finding.
    let mut modes = BTreeMap::new();
    let mut removed = std::collections::BTreeSet::new();
    removed.insert(seq("<C-l>"));
    modes.insert(
        ModeName::new("locked"),
        ModeBindings {
            keys: BTreeMap::new(),
            removed,
        },
    );
    let partial = PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    };

    let view = view_from_partial(Some(partial), None, None);
    assert!(view.reverted);
    assert_ne!(view.report.verdict(), KeymapVerdict::Apply);
    // The defaults survive: the reserved unlock still fires.
    let locked = &view.merged.modes[&ModeName::new("locked")];
    assert_eq!(
        locked.defaults[&seq("<C-l>")].action,
        ActionRef::core("unlock").unwrap()
    );
}

#[test]
fn a_file_error_reverts_the_view_and_carries_the_reason() {
    let view = view_from_partial(None, None, Some("boom".to_string()));
    assert!(view.reverted);
    assert_eq!(view.file_error.as_deref(), Some("boom"));
    assert_eq!(view.config, KeybindingsConfig::default());
}

#[test]
fn an_admitted_user_layer_folds_its_timeout_and_depth_fields_onto_the_defaults() {
    // A file that only tweaks the chord timers and depth has no conflicts, so
    // it admits and its values replace the defaults in the effective config.
    let view = view_from_partial(
        Some(PartialKeybindingsConfig {
            chord_timeout_ms: Some(750),
            which_key_delay_ms: Some(250),
            max_chord_depth: Some(6),
            ..PartialKeybindingsConfig::default()
        }),
        None,
        None,
    );
    assert!(!view.reverted);
    assert_eq!(view.config.chord_timeout_ms, 750);
    assert_eq!(view.config.which_key_delay_ms, 250);
    assert_eq!(view.config.max_chord_depth, 6);
}

#[test]
fn a_syntax_error_renders_as_one_line_that_render_joins_unchanged() {
    // An unbalanced brace is a KDL syntax error, so the parser returns the
    // `Syntax` variant. That branch renders as exactly one line, and the
    // single-string render is that same line.
    let err = parse_keybindings(Path::new("keybinding.kdl"), "mode \"normal\" {")
        .expect_err("unbalanced brace is a syntax error");
    match &err {
        KeybindingParseError::Syntax(inner) => {
            assert_eq!(parse_error_lines(&err), vec![inner.to_string()]);
            assert_eq!(render_parse_error(&err), inner.to_string());
        }
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

#[test]
fn validate_file_reports_parse_failures_and_clean_files() {
    let dir = std::env::temp_dir();
    let good = dir.join("koshi-keymap-test-good.kdl");
    let bad = dir.join("koshi-keymap-test-bad.kdl");
    std::fs::write(
        &good,
        "mode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    std::fs::write(
        &bad,
        "mode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n",
    )
    .expect("write");

    match validate_file(&good).expect("readable") {
        ValidationOutcome::Checked { applies, report } => {
            assert!(applies);
            assert_eq!(report.verdict(), KeymapVerdict::Apply);
        }
        ValidationOutcome::ParseFailed(errors) => panic!("expected clean check, got {errors:?}"),
    }
    match validate_file(&bad).expect("readable") {
        ValidationOutcome::ParseFailed(errors) => {
            assert_eq!(errors.len(), 1);
            assert!(errors[0].contains("<C-"), "got: {}", errors[0]);
        }
        ValidationOutcome::Checked { .. } => panic!("expected a parse failure"),
    }

    std::fs::remove_file(&good).expect("cleanup");
    std::fs::remove_file(&bad).expect("cleanup");
}
