//! Tests for [`parse_kdl`], its [`ConfigParseDiagnostic`] error, the shared
//! field-value readers, and the unknown-key suggestion.

use std::path::Path;

use kdl::{KdlDocument, KdlNode};
use miette::{Diagnostic, SourceSpan};

use super::{
    find_first_brace_past_depth_limit, find_string_end_offset, format_unknown_key,
    parse_boolean_kdl_value, parse_integer_kdl_value, parse_kdl, parse_nonempty_string_kdl_value,
    parse_single_kdl_value, parse_string_kdl_value, parse_u16_kdl_value, parse_u32_kdl_value,
    parse_version_argument, set_parsed_field, MAX_BLOCK_DEPTH,
};
use crate::error::ConfigError;

/// Parse a single-node `kdl_text` and hand back that one node, so a reader can be
/// exercised against a real `key value` field.
fn parse_test_kdl_node(kdl_text: &str) -> KdlNode {
    let config_document = parse_kdl(Path::new("t.kdl"), kdl_text).unwrap();
    config_document.nodes()[0].clone()
}

#[test]
fn valid_kdl_parses_to_document() {
    let config_document = parse_kdl(Path::new("cfg.kdl"), "pane width=80\n").unwrap();
    let node_names: Vec<&str> = config_document
        .nodes()
        .iter()
        .map(|node| node.name().value())
        .collect();
    assert_eq!(node_names, vec!["pane"]);
}

#[test]
fn empty_source_is_ok() {
    let config_document = parse_kdl(Path::new("cfg.kdl"), "").unwrap();
    assert_eq!(config_document.nodes().len(), 0);
}

#[test]
fn whitespace_only_is_ok() {
    let config_document = parse_kdl(Path::new("cfg.kdl"), "   \n\t\n").unwrap();
    assert_eq!(config_document.nodes().len(), 0);
}

#[test]
fn nested_children_survive_the_parse() {
    let config_document = parse_kdl(
        Path::new("cfg.kdl"),
        "pane {\n  min-cols 20\n}\ntheme \"midnight\"\n",
    )
    .unwrap();
    let node_names: Vec<&str> = config_document
        .nodes()
        .iter()
        .map(|node| node.name().value())
        .collect();
    assert_eq!(node_names, vec!["pane", "theme"]);
    let child_node_names: Vec<&str> = config_document.nodes()[0]
        .children()
        .expect("`pane` keeps its child block")
        .nodes()
        .iter()
        .map(|node| node.name().value())
        .collect();
    assert_eq!(child_node_names, vec!["min-cols"]);
}

#[test]
fn invalid_syntax_returns_diagnostic_with_path() {
    let parse_diagnostic = parse_kdl(Path::new("bad.kdl"), "pane { width").unwrap_err();
    assert_eq!(
        parse_diagnostic.to_string(),
        "config parse error in bad.kdl"
    );
}

#[test]
fn diagnostic_preserves_spans_from_kdl() {
    let malformed_config_text = "pane { width";
    // The KDL crate carries each span as a `related` sub-diagnostic; the raw
    // error for the same input is the source of truth for their count.
    let raw_kdl_error = malformed_config_text.parse::<KdlDocument>().unwrap_err();
    let raw_related = raw_kdl_error.related().map_or(0, Iterator::count);

    let parse_diagnostic = parse_kdl(Path::new("bad.kdl"), malformed_config_text).unwrap_err();
    let diagnostic_count = parse_diagnostic.related().map_or(0, Iterator::count);

    assert!(
        raw_related > 0,
        "kdl should report at least one sub-diagnostic"
    );
    assert_eq!(diagnostic_count, raw_related);

    let diagnostic_source_text = parse_diagnostic
        .source_code()
        .expect("parse diagnostic carries the source text");
    let source_span_contents = diagnostic_source_text
        .read_span(&SourceSpan::from(0..malformed_config_text.len()), 0, 0)
        .expect("the span covers the whole source");
    assert_eq!(
        std::str::from_utf8(source_span_contents.data()).unwrap(),
        "pane { width"
    );
}

#[test]
fn diagnostic_flattens_to_config_error() {
    let malformed_config_text = "pane { width";
    // The flattened detail is the first sub-diagnostic's specific message, not
    // kdl's generic top-level Display.
    let raw_kdl_error = malformed_config_text.parse::<KdlDocument>().unwrap_err();
    let expected_parse_error_detail = raw_kdl_error.diagnostics.first().unwrap().to_string();
    let parse_diagnostic = parse_kdl(Path::new("bad.kdl"), malformed_config_text).unwrap_err();

    match ConfigError::from(parse_diagnostic) {
        ConfigError::Parse {
            config_path,
            parse_error_detail,
        } => {
            assert_eq!(config_path, "bad.kdl");
            assert_eq!(parse_error_detail, expected_parse_error_detail);
            assert_ne!(parse_error_detail, "Failed to parse KDL document");
        }
        other_error => panic!("expected ConfigError::Parse, got {other_error:?}"),
    }
}

#[test]
fn diagnostic_code_is_stable() {
    let parse_diagnostic = parse_kdl(Path::new("bad.kdl"), "pane { width").unwrap_err();
    let diagnostic_code = parse_diagnostic
        .code()
        .expect("parse diagnostic has a code")
        .to_string();
    assert_eq!(diagnostic_code, "koshi::config::parse");
}

#[test]
fn parse_single_kdl_value_returns_the_lone_argument() {
    assert_eq!(
        parse_single_kdl_value(&parse_test_kdl_node("x 5"))
            .unwrap()
            .as_integer(),
        Some(5)
    );
}

#[test]
fn parse_single_kdl_value_rejects_a_node_with_no_argument() {
    assert_eq!(
        parse_single_kdl_value(&parse_test_kdl_node("x")).unwrap_err(),
        "expected exactly one value"
    );
}

#[test]
fn parse_single_kdl_value_rejects_more_than_one_argument() {
    assert_eq!(
        parse_single_kdl_value(&parse_test_kdl_node("x 1 2")).unwrap_err(),
        "expected exactly one value"
    );
}

#[test]
fn parse_single_kdl_value_rejects_a_named_property() {
    // `x k=1` is a property, not an unnamed argument.
    assert_eq!(
        parse_single_kdl_value(&parse_test_kdl_node("x k=1")).unwrap_err(),
        "expected exactly one value"
    );
}

#[test]
fn parse_single_kdl_value_refuses_a_child_block() {
    assert_eq!(
        parse_single_kdl_value(&parse_test_kdl_node("x \"midnight\" { foo }")).unwrap_err(),
        "takes no children"
    );
}

#[test]
fn parse_boolean_kdl_value_reads_true_and_false() {
    assert!(parse_boolean_kdl_value(&parse_test_kdl_node("x #true")).unwrap());
    assert!(!parse_boolean_kdl_value(&parse_test_kdl_node("x #false")).unwrap());
}

#[test]
fn parse_boolean_kdl_value_rejects_a_non_boolean() {
    assert_eq!(
        parse_boolean_kdl_value(&parse_test_kdl_node("x 5")).unwrap_err(),
        "expected a boolean (#true or #false)"
    );
}

#[test]
fn parse_boolean_kdl_value_rejects_a_quoted_true() {
    // `#true` is the KDL boolean; `"true"` is a string.
    assert_eq!(
        parse_boolean_kdl_value(&parse_test_kdl_node("x \"true\"")).unwrap_err(),
        "expected a boolean (#true or #false)"
    );
}

#[test]
fn parse_string_kdl_value_reads_a_quoted_string() {
    assert_eq!(
        parse_string_kdl_value(&parse_test_kdl_node("x \"hi\"")).unwrap(),
        "hi"
    );
}

#[test]
fn parse_string_kdl_value_rejects_a_non_string() {
    assert_eq!(
        parse_string_kdl_value(&parse_test_kdl_node("x 5")).unwrap_err(),
        "expected a string"
    );
}

#[test]
fn parse_nonempty_string_kdl_value_accepts_real_text() {
    assert_eq!(
        parse_nonempty_string_kdl_value(&parse_test_kdl_node("x \"bash\"")).unwrap(),
        "bash"
    );
}

#[test]
fn parse_nonempty_string_kdl_value_rejects_empty_and_whitespace() {
    assert_eq!(
        parse_nonempty_string_kdl_value(&parse_test_kdl_node("x \"\"")).unwrap_err(),
        "must not be empty"
    );
    assert_eq!(
        parse_nonempty_string_kdl_value(&parse_test_kdl_node("x \"   \"")).unwrap_err(),
        "must not be empty"
    );
}

#[test]
fn parse_nonempty_string_kdl_value_trims_surrounding_whitespace() {
    assert_eq!(
        parse_nonempty_string_kdl_value(&parse_test_kdl_node("x \"  xterm-256color \t\"")).unwrap(),
        "xterm-256color"
    );
}

#[test]
fn parse_integer_kdl_value_reads_a_bare_integer() {
    assert_eq!(
        parse_integer_kdl_value(&parse_test_kdl_node("x 42")).unwrap(),
        42
    );
}

#[test]
fn parse_integer_kdl_value_reads_a_negative_integer() {
    assert_eq!(
        parse_integer_kdl_value(&parse_test_kdl_node("x -7")).unwrap(),
        -7
    );
}

#[test]
fn parse_integer_kdl_value_rejects_a_non_integer() {
    assert_eq!(
        parse_integer_kdl_value(&parse_test_kdl_node("x \"no\"")).unwrap_err(),
        "expected an integer"
    );
}

#[test]
fn parse_u16_kdl_value_reads_an_in_range_number() {
    assert_eq!(
        parse_u16_kdl_value(&parse_test_kdl_node("x 80")).unwrap(),
        80
    );
}

#[test]
fn parse_u16_kdl_value_rejects_out_of_range_values() {
    assert_eq!(
        parse_u16_kdl_value(&parse_test_kdl_node("x 70000")).unwrap_err(),
        "must be between 0 and 65535"
    );
    assert_eq!(
        parse_u16_kdl_value(&parse_test_kdl_node("x -1")).unwrap_err(),
        "must be between 0 and 65535"
    );
}

#[test]
fn parse_u16_kdl_value_accepts_both_ends_of_its_range() {
    assert_eq!(parse_u16_kdl_value(&parse_test_kdl_node("x 0")).unwrap(), 0);
    assert_eq!(
        parse_u16_kdl_value(&parse_test_kdl_node("x 65535")).unwrap(),
        u16::MAX
    );
}

#[test]
fn parse_u16_kdl_value_reports_the_integer_reason_for_a_non_integer() {
    assert_eq!(
        parse_u16_kdl_value(&parse_test_kdl_node("x \"80\"")).unwrap_err(),
        "expected an integer"
    );
}

#[test]
fn parse_u32_kdl_value_reads_an_in_range_number() {
    assert_eq!(
        parse_u32_kdl_value(&parse_test_kdl_node("x 100")).unwrap(),
        100
    );
}

#[test]
fn parse_u32_kdl_value_accepts_both_ends_of_its_range() {
    assert_eq!(parse_u32_kdl_value(&parse_test_kdl_node("x 0")).unwrap(), 0);
    assert_eq!(
        parse_u32_kdl_value(&parse_test_kdl_node("x 4294967295")).unwrap(),
        u32::MAX
    );
}

#[test]
fn parse_u32_kdl_value_rejects_an_out_of_range_value() {
    assert_eq!(
        parse_u32_kdl_value(&parse_test_kdl_node("x 5000000000")).unwrap_err(),
        "must be between 0 and 4294967295"
    );
    assert_eq!(
        parse_u32_kdl_value(&parse_test_kdl_node("x -1")).unwrap_err(),
        "must be between 0 and 4294967295"
    );
}

#[test]
fn set_parsed_field_stores_an_ok_value_and_adds_no_warning() {
    let mut parsed_value_slot: Option<u16> = None;
    let mut warnings: Vec<String> = Vec::new();
    set_parsed_field(
        &mut parsed_value_slot,
        Ok(20),
        "pane",
        "min-cols",
        &mut warnings,
    );
    assert_eq!(parsed_value_slot, Some(20));
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn set_parsed_field_leaves_the_slot_empty_and_names_the_field_on_error() {
    let mut parsed_value_slot: Option<u16> = None;
    let mut warnings: Vec<String> = Vec::new();
    set_parsed_field(
        &mut parsed_value_slot,
        Err("expected an integer".to_string()),
        "pane",
        "min-cols",
        &mut warnings,
    );
    assert_eq!(parsed_value_slot, None);
    assert_eq!(warnings, ["ignored `pane.min-cols`: expected an integer"]);
}

#[test]
fn set_parsed_field_keeps_an_earlier_value_when_the_next_one_fails() {
    let mut parsed_value_slot: Option<u16> = Some(20);
    let mut warnings: Vec<String> = vec!["earlier warning".to_string()];
    set_parsed_field(
        &mut parsed_value_slot,
        Err("must be between 0 and 65535".to_string()),
        "pane",
        "gap",
        &mut warnings,
    );
    assert_eq!(parsed_value_slot, Some(20));
    assert_eq!(
        warnings,
        [
            "earlier warning",
            "ignored `pane.gap`: must be between 0 and 65535",
        ]
    );
}

#[test]
fn format_unknown_key_names_the_nearest_allowed_key() {
    assert_eq!(
        format_unknown_key("pane.min-col", &["pane.min-cols", "pane.min-rows"]),
        "unknown key `pane.min-col`; did you mean `pane.min-cols`?"
    );
}

#[test]
fn format_unknown_key_picks_by_edit_distance_not_by_length() {
    // `xyz1` is one insertion away; `abc` is the same length but shares no
    // character. A length-based guess would answer `abc`.
    assert_eq!(
        format_unknown_key("xyz", &["abc", "xyz1"]),
        "unknown key `xyz`; did you mean `xyz1`?"
    );
}

#[test]
fn format_unknown_key_counts_distance_in_characters_not_bytes() {
    // `é` is one character but two bytes. Counted in characters it is one
    // substitution from `e` and two edits from `ab`, so `e` wins. Counted in
    // bytes both are two edits, and the earlier `ab` would win the tie.
    assert_eq!(
        format_unknown_key("é", &["ab", "e"]),
        "unknown key `é`; did you mean `e`?"
    );
}

#[test]
fn format_unknown_key_breaks_a_tie_on_the_first_allowed_key() {
    // `ab` and `ay` are both two edits from `x`.
    assert_eq!(
        format_unknown_key("x", &["ab", "ay"]),
        "unknown key `x`; did you mean `ab`?"
    );
}

#[test]
fn format_unknown_key_with_one_allowed_key_names_that_key() {
    assert_eq!(
        format_unknown_key("completely-different", &["version"]),
        "unknown key `completely-different`; did you mean `version`?"
    );
}

#[test]
fn format_unknown_key_handles_an_empty_key() {
    assert_eq!(
        format_unknown_key("", &["colors", "version"]),
        "unknown key ``; did you mean `colors`?"
    );
}

#[test]
fn format_unknown_key_matches_a_key_that_is_itself_allowed() {
    assert_eq!(
        format_unknown_key("version", &["tab", "version"]),
        "unknown key `version`; did you mean `version`?"
    );
}

#[test]
#[should_panic(expected = "every config key set is non-empty")]
fn format_unknown_key_panics_on_an_empty_allowed_list() {
    let _ = format_unknown_key("version", &[]);
}

#[test]
fn a_block_nested_past_the_limit_is_a_parse_error_not_a_stack_overflow() {
    let deeply_nested_config_text = format!(
        "{}{}",
        "a {".repeat(MAX_BLOCK_DEPTH + 1),
        "}".repeat(MAX_BLOCK_DEPTH + 1)
    );

    let config_error: ConfigError = parse_kdl(Path::new("koshi.kdl"), &deeply_nested_config_text)
        .expect_err("nesting past the limit is refused")
        .into();

    match config_error {
        ConfigError::Parse {
            config_path,
            parse_error_detail,
        } => {
            assert_eq!(config_path, "koshi.kdl");
            assert_eq!(
                parse_error_detail,
                format!("blocks nest more than {MAX_BLOCK_DEPTH} levels deep")
            );
        }
        other_error => panic!("expected a parse error, got {other_error:?}"),
    }
}

#[test]
fn a_block_nested_exactly_to_the_limit_still_parses() {
    let boundary_depth_config_text = format!(
        "{}{}",
        "a {".repeat(MAX_BLOCK_DEPTH),
        "}".repeat(MAX_BLOCK_DEPTH)
    );

    assert!(parse_kdl(Path::new("koshi.kdl"), &boundary_depth_config_text).is_ok());
}

#[test]
fn braces_inside_comments_and_strings_open_no_level() {
    // Each source below carries far more `{` than the limit, and not one of
    // them opens a block.
    let brace_sequence = "{".repeat(MAX_BLOCK_DEPTH + 1);
    for malformed_kdl_text in [
        format!("// {brace_sequence}\nkey 1"),
        format!("/* {brace_sequence} */\nkey 1"),
        format!("/* /* {brace_sequence} */ */\nkey 1"),
        format!("key \"{brace_sequence}\""),
        format!("key \"a\\\"{brace_sequence}\""),
        format!("key #\"{brace_sequence}\"#"),
        format!("key ##\"{brace_sequence}\"#\"##"),
        format!("key #true // {brace_sequence}"),
    ] {
        assert_eq!(
            find_first_brace_past_depth_limit(&malformed_kdl_text),
            None,
            "kdl text: {malformed_kdl_text:?}"
        );
    }
}

#[test]
fn the_scan_names_the_brace_that_opens_the_first_level_past_the_limit() {
    let deeply_nested_kdl_text = format!("{}x", "a {".repeat(MAX_BLOCK_DEPTH + 1));

    // Each level is the three bytes `a {`, so the offending `{` sits two
    // bytes into the last one.
    assert_eq!(
        find_first_brace_past_depth_limit(&deeply_nested_kdl_text),
        Some(MAX_BLOCK_DEPTH * 3 + 2)
    );
}

#[test]
fn find_string_end_offset_stops_at_the_closing_quote() {
    assert_eq!(find_string_end_offset(br#""a\"b" rest"#, 0), 6);
    assert_eq!(find_string_end_offset(br###"##"a"#b"## rest"###, 0), 10);
    assert_eq!(find_string_end_offset(b"#true", 0), 1);
    assert_eq!(find_string_end_offset(b"\"never closed", 0), 13);
    assert_eq!(find_string_end_offset(b"#\"never closed", 0), 14);
}

#[test]
fn parse_version_argument_reads_the_declared_number() {
    assert_eq!(
        parse_version_argument(&parse_test_kdl_node("version 1")),
        Ok(1)
    );
    assert_eq!(
        parse_version_argument(&parse_test_kdl_node("version 0")),
        Ok(0)
    );
    assert_eq!(
        parse_version_argument(&parse_test_kdl_node("version 4294967295")),
        Ok(4_294_967_295)
    );
}

/// The reason [`parse_version_argument`] gives for `version_text`, without its span.
fn version_reason(version_text: &str) -> &'static str {
    parse_version_argument(&parse_test_kdl_node(version_text))
        .expect_err("the node is wrong")
        .1
}

#[test]
fn parse_version_argument_names_each_way_the_node_can_be_wrong() {
    assert_eq!(
        version_reason("version 1 {}"),
        "`version` takes no children"
    );
    assert_eq!(
        version_reason("version"),
        "`version` takes exactly one integer argument"
    );
    assert_eq!(
        version_reason("version 1 2"),
        "`version` takes exactly one integer argument"
    );
    assert_eq!(
        version_reason("version schema=1"),
        "`version` takes exactly one integer argument"
    );
    assert_eq!(
        version_reason("version \"1\""),
        "`version` must be an integer from 1 to 4294967295"
    );
    assert_eq!(
        version_reason("version -1"),
        "`version` must be an integer from 1 to 4294967295"
    );
    assert_eq!(
        version_reason("version 4294967296"),
        "`version` must be an integer from 1 to 4294967295"
    );
}

#[test]
fn a_bad_version_argument_puts_the_caret_on_the_argument() {
    let invalid_version_text = "version -1";
    let (version_error_span, _) =
        parse_version_argument(&parse_test_kdl_node(invalid_version_text))
            .expect_err("-1 is not a u32");
    assert_eq!(
        &invalid_version_text
            [version_error_span.offset()..version_error_span.offset() + version_error_span.len()],
        "-1"
    );
}

#[test]
fn a_version_node_that_is_wrong_as_a_whole_puts_the_caret_on_the_node() {
    let invalid_version_text = "version 1 2";
    let (version_error_span, _) =
        parse_version_argument(&parse_test_kdl_node(invalid_version_text))
            .expect_err("two values is wrong");
    assert_eq!(
        &invalid_version_text
            [version_error_span.offset()..version_error_span.offset() + version_error_span.len()],
        invalid_version_text
    );
}

/// The first problem `koshi.kdl` reports for `config_text`.
fn app_version_detail(config_text: &str) -> String {
    match crate::app_config::parse_app_config(Path::new("koshi.kdl"), config_text) {
        Err(ConfigError::Validation {
            validation_detail, ..
        }) => validation_detail,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

/// The first problem a theme file reports for `config_text`.
fn theme_version_detail(config_text: &str) -> String {
    match crate::theme::parse_theme(Path::new("themes/midnight.kdl"), config_text) {
        Err(ConfigError::Validation {
            validation_detail, ..
        }) => validation_detail,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

/// The first problem `keybinding.kdl` reports for `config_text`.
fn keybinding_version_detail(config_text: &str) -> String {
    match crate::keybinding::parse_keybindings(Path::new("keybinding.kdl"), config_text) {
        Err(crate::keybinding::KeybindingParseError::Invalid { diagnostics, .. }) => {
            diagnostics[0].get_diagnostic_message().to_string()
        }
        other => panic!("expected schema diagnostics, got {other:?}"),
    }
}

/// The first problem a profile file reports for `config_text`.
fn profile_version_detail(config_text: &str) -> String {
    let profile_text = format!("{config_text}\ntab {{ pane }}");
    match crate::profile::parse_profile(Path::new("profile/dev.kdl"), &profile_text) {
        Err(crate::profile::ProfileError::Invalid { diagnostics, .. }) => {
            diagnostics[0].get_diagnostic_message().to_string()
        }
        other => panic!("expected schema diagnostics, got {other:?}"),
    }
}

/// The problem migration reports for `config_text`.
fn get_migration_version_error_detail(config_source_text: &str) -> String {
    match crate::migration::validate_config(
        crate::migration::ConfigFileKind::App,
        Path::new("koshi.kdl"),
        config_source_text,
    ) {
        Err(crate::migration::MigrationError::Version {
            version_error_detail,
            ..
        }) => version_error_detail,
        other => panic!("expected a version error, got {other:?}"),
    }
}

#[test]
fn every_config_file_words_a_bad_version_the_same_way() {
    for (version_text, expected_version_error_detail) in [
        ("version 1 {}", "`version` takes no children"),
        (
            "version 1 2",
            "`version` takes exactly one integer argument",
        ),
        (
            "version schema=1",
            "`version` takes exactly one integer argument",
        ),
        (
            "version \"1\"",
            "`version` must be an integer from 1 to 4294967295",
        ),
        (
            "version -1",
            "`version` must be an integer from 1 to 4294967295",
        ),
        (
            "version 4294967296",
            "`version` must be an integer from 1 to 4294967295",
        ),
    ] {
        assert_eq!(
            app_version_detail(version_text),
            expected_version_error_detail,
            "koshi.kdl: {version_text}"
        );
        assert_eq!(
            theme_version_detail(version_text),
            expected_version_error_detail,
            "theme: {version_text}"
        );
        assert_eq!(
            keybinding_version_detail(version_text),
            expected_version_error_detail,
            "keybinding.kdl: {version_text}"
        );
        assert_eq!(
            profile_version_detail(version_text),
            expected_version_error_detail,
            "profile: {version_text}"
        );
        assert_eq!(
            get_migration_version_error_detail(version_text),
            expected_version_error_detail,
            "migration: {version_text}"
        );
    }
}
