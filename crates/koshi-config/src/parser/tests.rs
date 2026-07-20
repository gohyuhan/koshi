//! Tests for [`parse_kdl`], its [`ConfigParseDiagnostic`] error, and the shared
//! field-value readers.

use std::path::Path;

use kdl::{KdlDocument, KdlNode};
use miette::Diagnostic;

use super::{
    parse_kdl, single_value, value_bool, value_integer, value_nonempty_string, value_string,
    value_u16, value_u32,
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
    assert!(diag.source_code().is_some());
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
fn value_integer_reads_a_bare_integer() {
    assert_eq!(value_integer(&node("x 42")).unwrap(), 42);
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
fn value_u32_reads_an_in_range_number() {
    assert_eq!(value_u32(&node("x 100")).unwrap(), 100);
}

#[test]
fn value_u32_rejects_an_out_of_range_value() {
    assert_eq!(
        value_u32(&node("x 5000000000")).unwrap_err(),
        "must be between 0 and 4294967295"
    );
}
