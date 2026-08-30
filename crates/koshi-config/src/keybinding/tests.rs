//! Keybinding file parsing tests: the schema, every rejection with its exact
//! message and span, leader resolution order, and the all-or-nothing
//! contract.

use std::path::Path;

use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};

use super::*;
use crate::types::SCHEMA_VERSION;

/// Parses `source` as a keybinding file at a fixed test path.
fn parse(source: &str) -> Result<PartialKeybindingsConfig, KeybindingParseError> {
    let source = if source
        .lines()
        .any(|line| line.trim_start().starts_with("version "))
    {
        source.to_string()
    } else {
        format!("version 1\n{source}")
    };
    parse_keybindings(Path::new("keybinding.kdl"), &source)
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
fn version_only_file_yields_the_empty_partial() {
    let partial = parse("").expect("empty file is a valid empty layer");
    assert_eq!(partial, PartialKeybindingsConfig::default());
}

#[test]
fn missing_version_is_rejected() {
    assert_eq!(
        match parse_keybindings(Path::new("keybinding.kdl"), "") {
            Err(KeybindingParseError::Invalid { diagnostics, .. }) => diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message().to_string())
                .collect::<Vec<_>>(),
            result => panic!("expected missing-version error, got {result:?}"),
        },
        vec!["keybinding file must declare `version`".to_string()]
    );
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
    assert_eq!(locked.removed, BTreeSet::new());
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
    let normal = &modes[&ModeName::new("normal")];
    assert_eq!(normal.keys.len(), 1);
    assert_eq!(
        normal.keys[&expected].action,
        ActionRef::from_str("core:new-pane").unwrap()
    );
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
    let normal = &modes[&ModeName::new("normal")];
    assert_eq!(normal.keys.len(), 1);
    assert_eq!(
        normal.keys[&seq1(ModFlags::CTRL, Key::Char('t'))].action,
        ActionRef::from_str("core:new-tab").unwrap()
    );
}

#[test]
fn a_modifier_run_leader_node_merges_into_the_binding() {
    // `leader "C-"` is a modifier run. `<leader>t` is the single chord
    // `<C-t>`, not two chords.
    let partial = parse(
        r#"
leader "C-"
mode "normal" {
    bind "<leader>t" "core:new-tab"
}
"#,
    )
    .expect("valid file parses");
    assert_eq!(partial.leader, Some(Leader::Mods(ModFlags::CTRL)));
    let modes = partial.modes.expect("mode present");
    let normal = &modes[&ModeName::new("normal")];
    assert_eq!(normal.keys.len(), 1);
    assert_eq!(
        normal.keys[&seq1(ModFlags::CTRL, Key::Char('t'))].action,
        ActionRef::from_str("core:new-tab").unwrap()
    );
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
    assert_eq!(normal.keys.len(), 1);
    assert_eq!(
        normal.keys[&tab].action,
        ActionRef::from_str("core:next-tab").unwrap()
    );
    assert_eq!(normal.removed, [tab].into());
}

#[test]
fn mode_names_are_case_sensitive() {
    // `Normal` and `normal` are two different modes, not a duplicate block.
    let partial = parse(
        r#"
mode "normal" { bind "<C-t>" "core:new-tab" }
mode "Normal" { bind "<C-w>" "core:close-pane" }
"#,
    )
    .expect("two differently cased names are two modes");
    let modes = partial.modes.expect("modes present");
    assert_eq!(
        modes.keys().cloned().collect::<Vec<_>>(),
        vec![ModeName::new("Normal"), ModeName::new("normal")]
    );
    assert_eq!(
        modes[&ModeName::new("normal")].keys[&seq1(ModFlags::CTRL, Key::Char('t'))].action,
        ActionRef::from_str("core:new-tab").unwrap()
    );
    assert_eq!(
        modes[&ModeName::new("Normal")].keys[&seq1(ModFlags::CTRL, Key::Char('w'))].action,
        ActionRef::from_str("core:close-pane").unwrap()
    );
}

#[test]
fn an_empty_mode_name_is_a_mode_of_its_own() {
    let partial = parse(r#"mode "" { bind "<C-t>" "core:new-tab" }"#).expect("valid file parses");
    let modes = partial.modes.expect("mode present");
    assert_eq!(
        modes.keys().cloned().collect::<Vec<_>>(),
        vec![ModeName::new("")]
    );
    assert_eq!(modes[&ModeName::new("")].keys.len(), 1);
}

#[test]
fn mode_with_no_children_is_the_empty_bindings() {
    let partial = parse(r#"mode "normal""#).expect("valid file parses");
    let modes = partial.modes.expect("mode present");
    assert_eq!(modes[&ModeName::new("normal")], ModeBindings::default());
}

#[test]
fn overlong_sequences_parse_without_a_cap() {
    // The file's own `max-chord-depth` is not applied at parse time: an
    // eight-chord bind parses under `max-chord-depth 2`.
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
    assert_eq!(
        *sequence,
        KeySequence::new(
            KeyChord::new(ModFlags::CTRL, Key::Char('a')),
            "bcdefgh"
                .chars()
                .map(|c| KeyChord::new(ModFlags::NONE, Key::Char(c)))
                .collect(),
        )
    );
}

#[test]
fn invalid_kdl_syntax_is_a_syntax_error() {
    let err = parse("mode \"normal\" {").expect_err("unclosed block");
    assert_eq!(err.to_string(), "config parse error in keybinding.kdl");
    assert!(
        matches!(err, KeybindingParseError::Syntax(_)),
        "got: {err:?}"
    );
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
    assert_eq!(msgs, ["invalid key `<C-`: missing closing `>`"]);
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
    assert_eq!(msgs, ["unknown key `keybindings`; did you mean `version`?"]);
}

#[test]
fn unknown_node_inside_mode_is_rejected() {
    let msgs = messages(r#"mode "normal" { unbind "<Tab>" }"#);
    assert_eq!(msgs, ["unknown key `unbind`; did you mean `bind`?"]);
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
fn remove_arity_violations_are_rejected() {
    assert_eq!(
        messages(r#"mode "normal" { remove }"#),
        ["`remove` takes exactly one string argument"]
    );
    assert_eq!(
        messages(r#"mode "normal" { remove "<Tab>" "extra" }"#),
        ["`remove` takes exactly one string argument"]
    );
    assert_eq!(
        messages(r#"mode "normal" { remove "<Tab>" { } }"#),
        ["`remove` takes no children"]
    );
    assert_eq!(
        messages(r#"mode "normal" { remove 1 }"#),
        ["`remove` argument must be a string"]
    );
}

#[test]
fn mode_name_must_be_a_string() {
    assert_eq!(
        messages(r#"mode 1 { }"#),
        ["`mode` argument must be a string"]
    );
}

#[test]
fn bind_of_an_empty_key_is_rejected() {
    assert_eq!(
        messages(r#"mode "normal" { bind "" "core:new-tab" }"#),
        ["invalid key ``: empty key"]
    );
}

#[test]
fn every_problem_in_the_file_is_collected_in_document_order() {
    assert_eq!(
        messages(
            r#"
chord-timeout-ms "x"
mode "normal" {
    bind "<C-" "core:new-tab"
    remove "Ctrl-g"
}
"#
        ),
        [
            "`chord-timeout-ms` must be an integer from 0 to 4294967295",
            "invalid key `<C-`: missing closing `>`",
            "invalid key `Ctrl-g`: a multi-character key must be bracketed, as in `<Tab>`",
        ]
    );
}

#[test]
fn the_parse_time_chord_ceiling_is_255() {
    // The file's own `max-chord-depth` is not applied here. The cap is the
    // widest a `u8` carries: 255 chords parse, 256 do not.
    let at_ceiling = "a".repeat(255);
    let partial = parse(&format!(
        r#"mode "normal" {{ bind "{at_ceiling}" "core:new-tab" }}"#
    ))
    .expect("255 chords parse");
    let modes = partial.modes.expect("mode present");
    let (sequence, _) = modes[&ModeName::new("normal")]
        .keys
        .iter()
        .next()
        .expect("one binding");
    let a = KeyChord::new(ModFlags::NONE, Key::Char('a'));
    assert_eq!(*sequence, KeySequence::new(a, vec![a; 254]));

    let past_ceiling = "a".repeat(256);
    assert_eq!(
        messages(&format!(
            r#"mode "normal" {{ bind "{past_ceiling}" "core:new-tab" }}"#
        )),
        [format!(
            "invalid key `{past_ceiling}`: the sequence has 256 chords; the cap is 255"
        )]
    );
}

#[test]
fn action_without_a_namespace_is_rejected_with_the_full_ref_hint() {
    assert_eq!(
        messages(r#"mode "normal" { bind "<C-t>" "new-tab" }"#),
        [concat!(
            "action ref is missing a 'namespace:' prefix; ",
            "write the full reference, like `core:new-tab`"
        )]
    );
}

#[test]
fn bad_key_sequence_is_rejected_at_its_entry() {
    let source = "version 1\nmode \"normal\" { bind \"Ctrl-g\" \"core:new-tab\" }\n";
    let diagnostics = match parse_keybindings(Path::new("keybinding.kdl"), source) {
        Err(KeybindingParseError::Invalid { diagnostics, .. }) => diagnostics,
        result => panic!("expected a schema error, got {result:?}"),
    };
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].message(),
        "invalid key `Ctrl-g`: a multi-character key must be bracketed, as in `<Tab>`"
    );
    // The caret sits on the key entry, quotes included — not on the whole
    // `bind` node.
    let offset = source
        .find("\"Ctrl-g\"")
        .expect("key entry is in the source");
    assert_eq!(
        diagnostics[0].span(),
        SourceSpan::from(offset..offset + "\"Ctrl-g\"".len())
    );
}

#[test]
fn only_the_first_problem_in_one_bind_node_is_reported() {
    // `<C-` and `new-tab` are both wrong. The key is checked first and the
    // node is abandoned there, so the action is never reached.
    assert_eq!(
        messages(r#"mode "normal" { bind "<C-" "new-tab" }"#),
        ["invalid key `<C-`: missing closing `>`"]
    );
}

#[test]
fn a_bad_action_reference_is_rejected_at_its_own_entry() {
    let source = "version 1\nmode \"normal\" { bind \"<C-t>\" \"new-tab\" }\n";
    let diagnostics = match parse_keybindings(Path::new("keybinding.kdl"), source) {
        Err(KeybindingParseError::Invalid { diagnostics, .. }) => diagnostics,
        result => panic!("expected a schema error, got {result:?}"),
    };
    assert_eq!(diagnostics.len(), 1);
    // The caret sits on the action entry, quotes included.
    let offset = source
        .find("\"new-tab\"")
        .expect("action entry is in the source");
    assert_eq!(
        diagnostics[0].span(),
        SourceSpan::from(offset..offset + "\"new-tab\"".len())
    );
}

#[test]
fn a_rejected_leader_leaves_the_binds_on_the_built_in_leader() {
    // The bad `leader` node is the only error: it writes no leader, so
    // `<leader>t` still resolves against the built-in `C-` run.
    assert_eq!(
        messages(
            r#"
leader "<C-"
mode "normal" {
    bind "<leader>t" "core:new-tab"
}
"#
        ),
        ["invalid key `<C-`: missing closing `>`"]
    );
}

#[test]
fn duplicate_leader_node_is_rejected() {
    assert_eq!(
        messages("leader \"<C-p>\"\nleader \"<A-p>\""),
        ["duplicate `leader` node"]
    );
}

#[test]
fn integer_settings_accept_their_widest_values() {
    let partial = parse("max-chord-depth 255\nchord-timeout-ms 4294967295\nwhich-key-delay-ms 0")
        .expect("boundary values parse");
    assert_eq!(partial.max_chord_depth, Some(u8::MAX));
    assert_eq!(partial.chord_timeout_ms, Some(u32::MAX));
    assert_eq!(partial.which_key_delay_ms, Some(0));
}

#[test]
fn bad_leader_value_is_rejected() {
    assert_eq!(
        messages(r#"leader "<C-""#),
        ["invalid key `<C-`: missing closing `>`"]
    );
}

#[test]
fn bad_unlock_alternative_value_is_rejected() {
    assert_eq!(
        messages(r#"unlock-alternative "not a chord""#),
        ["invalid key `not a chord`: a multi-character key must be bracketed, as in `<Tab>`"]
    );
}

#[test]
fn newer_version_is_rejected() {
    assert_eq!(
        messages("version 999"),
        [format!(
            "config schema version 999 is newer than this koshi supports ({SCHEMA_VERSION})"
        )]
    );
}

#[test]
fn version_zero_is_rejected() {
    assert_eq!(
        messages("version 0"),
        ["config schema version must be at least 1"]
    );
}

#[test]
fn a_duplicate_version_node_is_reported_once_and_the_second_is_not_checked() {
    // The duplicate short-circuits before the version check. The `999` in the
    // second node produces no too-new error of its own.
    assert_eq!(
        messages("version 1\nversion 999"),
        ["duplicate `version` node"]
    );
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
