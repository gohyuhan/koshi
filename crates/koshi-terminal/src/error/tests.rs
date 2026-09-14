//! Unit tests for the terminal domain error.

use super::*;

#[test]
fn a_parse_failure_displays_its_detail_after_the_prefix() {
    let parse_error = TerminalError::Parse {
        detail: "bad CSI".into(),
    };
    assert_eq!(parse_error.to_string(), "terminal parse error: bad CSI");
}

#[test]
fn an_empty_detail_leaves_the_prefix_and_its_trailing_space() {
    let parse_error = TerminalError::Parse {
        detail: String::new(),
    };
    assert_eq!(parse_error.to_string(), "terminal parse error: ");
}

#[test]
fn a_detail_holding_braces_and_non_ascii_reaches_display_unchanged() {
    let parse_error = TerminalError::Parse {
        detail: "権限がありません {0} {}".into(),
    };
    assert_eq!(
        parse_error.to_string(),
        "terminal parse error: 権限がありません {0} {}"
    );
}

#[test]
fn a_parse_failure_classifies_as_terminal_and_recoverable() {
    let parse_error = TerminalError::Parse {
        detail: "unterminated CSI".into(),
    };
    assert_eq!(parse_error.category(), DomainCategory::Terminal);
    assert_eq!(parse_error.get_severity(), Severity::Recoverable);
}
