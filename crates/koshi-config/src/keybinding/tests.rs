//! Keybinding file parsing tests: the ratified schema, every rejection with
//! its exact message target, leader resolution order, and the all-or-nothing
//! contract.

use std::path::Path;

use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};

use super::*;

/// Parses `source` as a keybinding file at a fixed test path.
fn parse(source: &str) -> Result<PartialKeybindingsConfig, KeybindingParseError> {
    parse_keybindings(Path::new("keybinding.kdl"), source)
}

/// Parses `source`, expecting schema violations, and returns their messages.
fn messages(source: &str) -> Vec<String> {
    match parse(source) {
        Err(KeybindingParseError::Invalid { diagnostics, .. }) => diagnostics
            .iter()
            .map(|d| d.message().to_string())
            .collect(),
        Err(KeybindingParseError::Syntax(err)) => {
            panic!("expected schema errors, got syntax: {err}")
        }
        Ok(_) => panic!("expected schema errors, file parsed"),
    }
}

/// A one-chord sequence.
fn seq1(mods: ModFlags, key: Key) -> KeySequence {
    KeySequence::new(KeyChord::new(mods, key), Vec::new())
}

/// A two-chord sequence.
fn seq2(first: KeyChord, second: KeyChord) -> KeySequence {
    KeySequence::new(first, vec![second])
}

#[test]
fn empty_file_yields_the_empty_partial() {
    let partial = parse("").expect("empty file is a valid empty layer");
    assert_eq!(partial, PartialKeybindingsConfig::default());
}

#[test]
fn full_file_round_trips_every_field() {
    let partial = parse(
        r#"
version 1
chord-timeout-ms 750
which-key-delay-ms 300
max-chord-depth 5
leader "<C-p>"
unlock-alternative "<A-u>"

mode "normal" {
    bind "<C-t>" "core:new-tab"
    bind "<leader> w" "core:close-pane"
    remove "<Tab>"
}

mode "locked" {
    bind "<A-q>" "core:quit"
}
"#,
    )
    .expect("valid file parses");

    assert_eq!(partial.chord_timeout_ms, Some(750));
    assert_eq!(partial.which_key_delay_ms, Some(300));
    assert_eq!(partial.max_chord_depth, Some(5));
    assert_eq!(
        partial.leader,
        Some(Leader::Chord(KeyChord::new(ModFlags::CTRL, Key::Char('p'))))
    );
    assert_eq!(
        partial.unlock_alternative,
        Some(Some(KeyChord::new(ModFlags::ALT, Key::Char('u'))))
    );

    let modes = partial.modes.expect("mode blocks present");
    assert_eq!(modes.len(), 2);

    let normal = &modes[&ModeName::new("normal")];
    assert_eq!(normal.keys.len(), 2);
    let new_tab = &normal.keys[&seq1(ModFlags::CTRL, Key::Char('t'))];
    assert_eq!(new_tab.action, ActionRef::from_str("core:new-tab").unwrap());
    assert_eq!(new_tab.args, ActionArgs::None);
    // `<leader> w` under a chord leader is the leader chord then `w`.
    let close = &normal.keys[&seq2(
        KeyChord::new(ModFlags::CTRL, Key::Char('p')),
        KeyChord::new(ModFlags::NONE, Key::Char('w')),
    )];
    assert_eq!(
        close.action,
        ActionRef::from_str("core:close-pane").unwrap()
    );
    assert_eq!(close.args, ActionArgs::None);
    assert_eq!(
        normal.removed,
        [seq1(ModFlags::NONE, Key::Named(NamedKey::Tab))].into()
    );

    let locked = &modes[&ModeName::new("locked")];
    assert_eq!(locked.keys.len(), 1);
    assert_eq!(
        locked.keys[&seq1(ModFlags::ALT, Key::Char('q'))].action,
        ActionRef::from_str("core:quit").unwrap()
    );
    assert!(locked.removed.is_empty());
}

#[test]
fn leader_node_after_the_mode_block_still_applies() {
    let partial = parse(
        r#"
mode "normal" {
    bind "<leader> n" "core:new-pane"
}
leader "<C-p>"
"#,
    )
    .expect("valid file parses");
    let modes = partial.modes.expect("mode present");
    let expected = seq2(
        KeyChord::new(ModFlags::CTRL, Key::Char('p')),
        KeyChord::new(ModFlags::NONE, Key::Char('n')),
    );
    assert!(modes[&ModeName::new("normal")].keys.contains_key(&expected));
}

#[test]
fn absent_leader_falls_back_to_the_built_in_mods_leader() {
    // The built-in leader is the Ctrl modifier run: `<leader>t` = `<C-t>`.
    let partial = parse(
        r#"
mode "normal" {
    bind "<leader>t" "core:new-tab"
}
"#,
    )
    .expect("valid file parses");
    let modes = partial.modes.expect("mode present");
    assert!(modes[&ModeName::new("normal")]
        .keys
        .contains_key(&seq1(ModFlags::CTRL, Key::Char('t'))));
}

#[test]
fn bind_and_remove_of_the_same_key_in_one_mode_both_hold() {
    // Own-layer remove + rebind: the remove voids lower layers, the bind is
    // this layer's claim.
    let partial = parse(
        r#"
mode "normal" {
    remove "<Tab>"
    bind "<Tab>" "core:next-tab"
}
"#,
    )
    .expect("valid file parses");
    let modes = partial.modes.expect("mode present");
    let normal = &modes[&ModeName::new("normal")];
    let tab = seq1(ModFlags::NONE, Key::Named(NamedKey::Tab));
    assert!(normal.keys.contains_key(&tab));
    assert!(normal.removed.contains(&tab));
}

#[test]
fn mode_with_no_children_is_the_empty_bindings() {
    let partial = parse(r#"mode "normal""#).expect("valid file parses");
    let modes = partial.modes.expect("mode present");
    assert_eq!(modes[&ModeName::new("normal")], ModeBindings::default());
}

#[test]
fn overlong_sequences_parse_without_a_cap() {
    // Chord depth is a liveness question for conflict detection, not a parse
    // error — even with the file's own depth set low.
    let partial = parse(
        r#"
max-chord-depth 2
mode "normal" {
    bind "<C-a> b c d e f g h" "core:new-tab"
}
"#,
    )
    .expect("overlong bind still parses");
    let modes = partial.modes.expect("mode present");
    let (sequence, _) = modes[&ModeName::new("normal")]
        .keys
        .iter()
        .next()
        .expect("one binding");
    assert_eq!(sequence.chords().len(), 8);
}

#[test]
fn invalid_kdl_syntax_is_a_syntax_error() {
    let err = parse("mode \"normal\" {").expect_err("unclosed block");
    assert!(matches!(err, KeybindingParseError::Syntax(_)));
}

#[test]
fn one_bad_bind_rejects_the_whole_file() {
    // All-or-nothing: the good bind does not survive its neighbor's typo.
    let msgs = messages(
        r#"
mode "normal" {
    bind "<C-t>" "core:new-tab"
    bind "<C-" "core:close-pane"
}
"#,
    );
    assert_eq!(msgs.len(), 1);
}

#[test]
fn duplicate_bind_of_one_key_is_rejected() {
    let msgs = messages(
        r#"
mode "normal" {
    bind "<C-t>" "core:new-tab"
    bind "<C-t>" "core:close-pane"
}
"#,
    );
    assert_eq!(
        msgs,
        ["`<C-t>` is already bound in this mode; one action per key"]
    );
}

#[test]
fn duplicate_remove_is_rejected() {
    let msgs = messages(
        r#"
mode "normal" {
    remove "<Tab>"
    remove "<Tab>"
}
"#,
    );
    assert_eq!(msgs, ["duplicate `remove \"<Tab>\"`"]);
}

#[test]
fn duplicate_mode_block_is_rejected() {
    let msgs = messages(
        r#"
mode "normal" { bind "<C-t>" "core:new-tab" }
mode "normal" { bind "<C-w>" "core:close-pane" }
"#,
    );
    assert_eq!(
        msgs,
        ["duplicate `mode \"normal\"` block; one block per mode"]
    );
}

#[test]
fn duplicate_setting_node_is_rejected() {
    let msgs = messages("chord-timeout-ms 500\nchord-timeout-ms 600");
    assert_eq!(msgs, ["duplicate `chord-timeout-ms` node"]);
}

#[test]
fn unknown_top_level_node_is_rejected() {
    let msgs = messages("keybindings { }");
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].starts_with("unknown node `keybindings`"));
}

#[test]
fn unknown_node_inside_mode_is_rejected() {
    let msgs = messages(r#"mode "normal" { unbind "<Tab>" }"#);
    assert_eq!(
        msgs,
        ["unknown node `unbind` in `mode`; expected `bind` or `remove`"]
    );
}

#[test]
fn bind_arity_violations_are_rejected() {
    let expected =
        "`bind` takes exactly two string arguments: a key sequence and an action reference";
    assert_eq!(messages(r#"mode "normal" { bind "<C-t>" }"#), [expected]);
    assert_eq!(
        messages(r#"mode "normal" { bind "<C-t>" "core:new-tab" "extra" }"#),
        [expected]
    );
    assert_eq!(
        messages(r#"mode "normal" { bind key="<C-t>" action="core:new-tab" }"#),
        [expected]
    );
    assert_eq!(
        messages(r#"mode "normal" { bind "<C-t>" "core:new-tab" { } }"#),
        ["`bind` takes no children"]
    );
    assert_eq!(
        messages(r#"mode "normal" { bind 1 2 }"#),
        ["`bind` arguments must be strings"]
    );
}

#[test]
fn action_without_a_namespace_is_rejected_with_the_full_ref_hint() {
    let msgs = messages(r#"mode "normal" { bind "<C-t>" "new-tab" }"#);
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].ends_with("write the full reference, like `core:new-tab`"),
        "got: {}",
        msgs[0]
    );
}

#[test]
fn bad_key_sequence_is_rejected_at_its_entry() {
    let msgs = messages(r#"mode "normal" { bind "Ctrl-g" "core:new-tab" }"#);
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].contains("Ctrl-g"), "got: {}", msgs[0]);
}

#[test]
fn bad_leader_value_is_rejected() {
    let msgs = messages(r#"leader "<C-""#);
    assert_eq!(msgs.len(), 1);
}

#[test]
fn bad_unlock_alternative_value_is_rejected() {
    let msgs = messages(r#"unlock-alternative "not a chord""#);
    assert_eq!(msgs.len(), 1);
}

#[test]
fn newer_version_is_rejected() {
    let msgs = messages("version 999");
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].contains("999"), "got: {}", msgs[0]);
}

#[test]
fn current_version_is_accepted() {
    parse("version 1").expect("current version parses");
}

#[test]
fn out_of_range_integer_is_rejected() {
    assert_eq!(
        messages("max-chord-depth 300"),
        ["`max-chord-depth` must be an integer from 0 to 255"]
    );
    assert_eq!(
        messages("chord-timeout-ms -1"),
        ["`chord-timeout-ms` must be an integer from 0 to 4294967295"]
    );
}

#[test]
fn setting_arity_violations_are_rejected() {
    assert_eq!(
        messages("chord-timeout-ms"),
        ["`chord-timeout-ms` takes exactly one integer argument"]
    );
    assert_eq!(
        messages("chord-timeout-ms 1 2"),
        ["`chord-timeout-ms` takes exactly one integer argument"]
    );
    assert_eq!(
        messages("leader"),
        ["`leader` takes exactly one string argument"]
    );
    assert_eq!(
        messages(r#"leader "<C-p>" { }"#),
        ["`leader` takes no children"]
    );
    assert_eq!(
        messages(r#"unlock-alternative "<A-u>" { }"#),
        ["`unlock-alternative` takes no children"]
    );
    assert_eq!(
        messages("chord-timeout-ms 500 { }"),
        ["`chord-timeout-ms` takes no children"]
    );
    assert_eq!(
        messages("mode { }"),
        ["`mode` takes exactly one string argument"]
    );
}
