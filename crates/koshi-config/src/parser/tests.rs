//! Tests for [`parse_kdl`], its [`ConfigParseDiagnostic`] error, the shared
//! field-value readers, and the unknown-key suggestion.

use std::path::Path;

use kdl::{KdlDocument, KdlNode};
use miette::{Diagnostic, SourceSpan};

use super::{
    parse_kdl, set, single_value, unknown_key, value_bool, value_integer, value_nonempty_string,
    value_string, value_u16, value_u32,
};
use crate::error::ConfigError;

/// Parse a single-node source and hand back that one node, so a reader can be
/// exercised against a real `key value` field.
fn node(source: &str) -> KdlNode {
    let doc = parse_kdl(Path::new("t.kdl"), source).unwrap();
    doc.nodes()[0].clone()
}

#[test]
fn valid_kdl_parses_to_document() {
    let doc = parse_kdl(Path::new("cfg.kdl"), "pane width=80\n").unwrap();
    let names: Vec<&str> = doc.nodes().iter().map(|n| n.name().value()).collect();
    assert_eq!(names, vec!["pane"]);
}

#[test]
fn empty_source_is_ok() {
    let doc = parse_kdl(Path::new("cfg.kdl"), "").unwrap();
    assert_eq!(doc.nodes().len(), 0);
}

#[test]
fn whitespace_only_is_ok() {
    let doc = parse_kdl(Path::new("cfg.kdl"), "   \n\t\n").unwrap();
    assert_eq!(doc.nodes().len(), 0);
}

#[test]
fn nested_children_survive_the_parse() {
    let doc = parse_kdl(
        Path::new("cfg.kdl"),
        "pane {\n  min-cols 20\n}\ntheme \"midnight\"\n",
    )
    .unwrap();
    let names: Vec<&str> = doc.nodes().iter().map(|n| n.name().value()).collect();
    assert_eq!(names, vec!["pane", "theme"]);
    let inner: Vec<&str> = doc.nodes()[0]
        .children()
        .expect("`pane` keeps its child block")
        .nodes()
        .iter()
        .map(|n| n.name().value())
        .collect();
    assert_eq!(inner, vec!["min-cols"]);
}

#[test]
fn invalid_syntax_returns_diagnostic_with_path() {
    let err = parse_kdl(Path::new("bad.kdl"), "pane { width").unwrap_err();
    assert_eq!(err.to_string(), "config parse error in bad.kdl");
}

#[test]
fn diagnostic_preserves_spans_from_kdl() {
    let bad = "pane { width";
    // The KDL crate carries each span as a `related` sub-diagnostic; the raw
    // error for the same input is the source of truth for their count.
    let raw = bad.parse::<KdlDocument>().unwrap_err();
    let raw_related = raw.related().map_or(0, Iterator::count);

    let diag = parse_kdl(Path::new("bad.kdl"), bad).unwrap_err();
    let diag_related = diag.related().map_or(0, Iterator::count);

    assert!(
        raw_related > 0,
        "kdl should report at least one sub-diagnostic"
    );
    assert_eq!(diag_related, raw_related);

    let source = diag
        .source_code()
        .expect("parse diagnostic carries the source text");
    let contents = source
        .read_span(&SourceSpan::from(0..bad.len()), 0, 0)
        .expect("the span covers the whole source");
    assert_eq!(
        std::str::from_utf8(contents.data()).unwrap(),
        "pane { width"
    );
}

#[test]
fn diagnostic_flattens_to_config_error() {
    let bad = "pane { width";
    // The flattened detail is the first sub-diagnostic's specific message, not
    // kdl's generic top-level Display.
    let raw = bad.parse::<KdlDocument>().unwrap_err();
    let expected = raw.diagnostics.first().unwrap().to_string();
    let diag = parse_kdl(Path::new("bad.kdl"), bad).unwrap_err();

    match ConfigError::from(diag) {
        ConfigError::Parse { path, detail: got } => {
            assert_eq!(path, "bad.kdl");
            assert_eq!(got, expected);
            assert_ne!(got, "Failed to parse KDL document");
        }
        other => panic!("expected ConfigError::Parse, got {other:?}"),
    }
}

#[test]
fn diagnostic_code_is_stable() {
    let diag = parse_kdl(Path::new("bad.kdl"), "pane { width").unwrap_err();
    let code = diag
        .code()
        .expect("parse diagnostic has a code")
        .to_string();
    assert_eq!(code, "koshi::config::parse");
}

#[test]
fn single_value_returns_the_lone_argument() {
    assert_eq!(single_value(&node("x 5")).unwrap().as_integer(), Some(5));
}

#[test]
fn single_value_rejects_a_node_with_no_argument() {
    assert_eq!(
        single_value(&node("x")).unwrap_err(),
        "expected exactly one value"
    );
}

#[test]
fn single_value_rejects_more_than_one_argument() {
    assert_eq!(
        single_value(&node("x 1 2")).unwrap_err(),
        "expected exactly one value"
    );
}

#[test]
fn single_value_rejects_a_named_property() {
    // `x k=1` is a property, not an unnamed argument.
    assert_eq!(
        single_value(&node("x k=1")).unwrap_err(),
        "expected exactly one value"
    );
}

#[test]
fn single_value_refuses_a_child_block() {
    assert_eq!(
        single_value(&node("x \"midnight\" { foo }")).unwrap_err(),
        "takes no children"
    );
}

#[test]
fn value_bool_reads_true_and_false() {
    assert!(value_bool(&node("x #true")).unwrap());
    assert!(!value_bool(&node("x #false")).unwrap());
}

#[test]
fn value_bool_rejects_a_non_boolean() {
    assert_eq!(
        value_bool(&node("x 5")).unwrap_err(),
        "expected a boolean (#true or #false)"
    );
}

#[test]
fn value_bool_rejects_a_quoted_true() {
    // `#true` is the KDL boolean; `"true"` is a string.
    assert_eq!(
        value_bool(&node("x \"true\"")).unwrap_err(),
        "expected a boolean (#true or #false)"
    );
}

#[test]
fn value_string_reads_a_quoted_string() {
    assert_eq!(value_string(&node("x \"hi\"")).unwrap(), "hi");
}

#[test]
fn value_string_rejects_a_non_string() {
    assert_eq!(value_string(&node("x 5")).unwrap_err(), "expected a string");
}

#[test]
fn value_nonempty_string_accepts_real_text() {
    assert_eq!(value_nonempty_string(&node("x \"bash\"")).unwrap(), "bash");
}

#[test]
fn value_nonempty_string_rejects_empty_and_whitespace() {
    assert_eq!(
        value_nonempty_string(&node("x \"\"")).unwrap_err(),
        "must not be empty"
    );
    assert_eq!(
        value_nonempty_string(&node("x \"   \"")).unwrap_err(),
        "must not be empty"
    );
}

#[test]
fn value_nonempty_string_trims_surrounding_whitespace() {
    assert_eq!(
        value_nonempty_string(&node("x \"  xterm-256color \t\"")).unwrap(),
        "xterm-256color"
    );
}

#[test]
fn value_integer_reads_a_bare_integer() {
    assert_eq!(value_integer(&node("x 42")).unwrap(), 42);
}

#[test]
fn value_integer_reads_a_negative_integer() {
    assert_eq!(value_integer(&node("x -7")).unwrap(), -7);
}

#[test]
fn value_integer_rejects_a_non_integer() {
    assert_eq!(
        value_integer(&node("x \"no\"")).unwrap_err(),
        "expected an integer"
    );
}

#[test]
fn value_u16_reads_an_in_range_number() {
    assert_eq!(value_u16(&node("x 80")).unwrap(), 80);
}

#[test]
fn value_u16_rejects_out_of_range_values() {
    assert_eq!(
        value_u16(&node("x 70000")).unwrap_err(),
        "must be between 0 and 65535"
    );
    assert_eq!(
        value_u16(&node("x -1")).unwrap_err(),
        "must be between 0 and 65535"
    );
}

#[test]
fn value_u16_accepts_both_ends_of_its_range() {
    assert_eq!(value_u16(&node("x 0")).unwrap(), 0);
    assert_eq!(value_u16(&node("x 65535")).unwrap(), u16::MAX);
}

#[test]
fn value_u16_reports_the_integer_reason_for_a_non_integer() {
    assert_eq!(
        value_u16(&node("x \"80\"")).unwrap_err(),
        "expected an integer"
    );
}

#[test]
fn value_u32_reads_an_in_range_number() {
    assert_eq!(value_u32(&node("x 100")).unwrap(), 100);
}

#[test]
fn value_u32_accepts_both_ends_of_its_range() {
    assert_eq!(value_u32(&node("x 0")).unwrap(), 0);
    assert_eq!(value_u32(&node("x 4294967295")).unwrap(), u32::MAX);
}

#[test]
fn value_u32_rejects_an_out_of_range_value() {
    assert_eq!(
        value_u32(&node("x 5000000000")).unwrap_err(),
        "must be between 0 and 4294967295"
    );
    assert_eq!(
        value_u32(&node("x -1")).unwrap_err(),
        "must be between 0 and 4294967295"
    );
}

#[test]
fn set_stores_an_ok_value_and_adds_no_warning() {
    let mut slot: Option<u16> = None;
    let mut warnings: Vec<String> = Vec::new();
    set(&mut slot, Ok(20), "pane", "min-cols", &mut warnings);
    assert_eq!(slot, Some(20));
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn set_leaves_the_slot_empty_and_names_the_field_on_err() {
    let mut slot: Option<u16> = None;
    let mut warnings: Vec<String> = Vec::new();
    set(
        &mut slot,
        Err("expected an integer".to_string()),
        "pane",
        "min-cols",
        &mut warnings,
    );
    assert_eq!(slot, None);
    assert_eq!(warnings, ["ignored `pane.min-cols`: expected an integer"]);
}

#[test]
fn set_keeps_an_earlier_value_when_the_next_one_fails() {
    let mut slot: Option<u16> = Some(20);
    let mut warnings: Vec<String> = vec!["earlier warning".to_string()];
    set(
        &mut slot,
        Err("must be between 0 and 65535".to_string()),
        "pane",
        "gap",
        &mut warnings,
    );
    assert_eq!(slot, Some(20));
    assert_eq!(
        warnings,
        [
            "earlier warning",
            "ignored `pane.gap`: must be between 0 and 65535",
        ]
    );
}

#[test]
fn unknown_key_names_the_nearest_allowed_key() {
    assert_eq!(
        unknown_key("pane.min-col", &["pane.min-cols", "pane.min-rows"]),
        "unknown key `pane.min-col`; did you mean `pane.min-cols`?"
    );
}

#[test]
fn unknown_key_picks_by_edit_distance_not_by_length() {
    // `xyz1` is one insertion away; `abc` is the same length but shares no
    // character. A length-based guess would answer `abc`.
    assert_eq!(
        unknown_key("xyz", &["abc", "xyz1"]),
        "unknown key `xyz`; did you mean `xyz1`?"
    );
}

#[test]
fn unknown_key_counts_distance_in_characters_not_bytes() {
    // `é` is one character but two bytes. Counted in characters it is one
    // substitution from `e` and two edits from `ab`, so `e` wins. Counted in
    // bytes both are two edits, and the earlier `ab` would win the tie.
    assert_eq!(
        unknown_key("é", &["ab", "e"]),
        "unknown key `é`; did you mean `e`?"
    );
}

#[test]
fn unknown_key_breaks_a_tie_on_the_first_allowed_key() {
    // `ab` and `ay` are both two edits from `x`.
    assert_eq!(
        unknown_key("x", &["ab", "ay"]),
        "unknown key `x`; did you mean `ab`?"
    );
}

#[test]
fn unknown_key_with_one_allowed_key_names_that_key() {
    assert_eq!(
        unknown_key("completely-different", &["version"]),
        "unknown key `completely-different`; did you mean `version`?"
    );
}

#[test]
fn unknown_key_handles_an_empty_key() {
    assert_eq!(
        unknown_key("", &["colors", "version"]),
        "unknown key ``; did you mean `colors`?"
    );
}

#[test]
fn unknown_key_matches_a_key_that_is_itself_allowed() {
    assert_eq!(
        unknown_key("version", &["tab", "version"]),
        "unknown key `version`; did you mean `version`?"
    );
}

#[test]
#[should_panic(expected = "every config key set is non-empty")]
fn unknown_key_panics_on_an_empty_allowed_list() {
    let _ = unknown_key("version", &[]);
}
