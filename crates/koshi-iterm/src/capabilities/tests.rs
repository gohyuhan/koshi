//! Tests for iTerm2 feature reporting.

use super::*;

#[test]
fn feature_parser_matches_whole_tokens_and_ignores_extensions() {
    assert!(supports_iterm_file_feature(b"F"));
    assert!(supports_iterm_file_feature(b"SxF"));
    assert!(supports_iterm_file_feature(b"F;unknown"));
    assert!(!supports_iterm_file_feature(b"Foo"));
    assert!(!supports_iterm_file_feature(b"XFf"));
    assert!(!supports_iterm_file_feature(b"F1"));
    assert!(!supports_iterm_file_feature(b"unknown"));
    assert!(!supports_iterm_file_feature(b";F"));
}

#[test]
fn feature_parser_matches_the_sixel_token_without_matching_prefixes() {
    assert!(supports_iterm_sixel_feature(b"Sx"));
    assert!(supports_iterm_sixel_feature(b"FSx"));
    assert!(supports_iterm_sixel_feature(b"Sx;unknown"));
    assert!(!supports_iterm_sixel_feature(b"S"));
    assert!(!supports_iterm_sixel_feature(b"Sxy"));
    assert!(supports_iterm_sixel_feature(b"XSx"));
    assert!(!supports_iterm_sixel_feature(b"XSxy"));
    assert!(!supports_iterm_sixel_feature(b";Sx"));
}

#[test]
fn feature_query_has_exact_osc_terminator() {
    assert_eq!(ITERM_CAPABILITIES_QUERY, b"\x1b]1337;Capabilities\x1b\\");
}
