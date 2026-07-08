//! Tests for [`parse_kdl`] and its [`ConfigParseDiagnostic`] error.

use std::path::Path;

use kdl::KdlDocument;
use miette::Diagnostic;

use super::parse_kdl;
use crate::error::ConfigError;

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
