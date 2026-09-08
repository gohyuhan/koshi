//! Tests for iTerm2 feature reporting.

use super::*;

#[test]
fn feature_parser_matches_whole_tokens_and_ignores_extensions() {
    assert!(iterm_feature_string_supports_file(b"F"));
    assert!(iterm_feature_string_supports_file(b"SxF"));
    assert!(iterm_feature_string_supports_file(b"F;unknown"));
    assert!(!iterm_feature_string_supports_file(b"Foo"));
    assert!(!iterm_feature_string_supports_file(b"XFf"));
    assert!(!iterm_feature_string_supports_file(b"F1"));
    assert!(!iterm_feature_string_supports_file(b"unknown"));
    assert!(!iterm_feature_string_supports_file(b";F"));
}

#[test]
fn feature_parser_matches_the_sixel_token_without_matching_prefixes() {
    assert!(iterm_feature_string_supports_sixel(b"Sx"));
    assert!(iterm_feature_string_supports_sixel(b"FSx"));
    assert!(iterm_feature_string_supports_sixel(b"Sx;unknown"));
    assert!(!iterm_feature_string_supports_sixel(b"S"));
    assert!(!iterm_feature_string_supports_sixel(b"Sxy"));
    assert!(iterm_feature_string_supports_sixel(b"XSx"));
    assert!(!iterm_feature_string_supports_sixel(b"XSxy"));
    assert!(!iterm_feature_string_supports_sixel(b";Sx"));
}

#[test]
fn feature_query_has_exact_osc_terminator() {
    assert_eq!(ITERM_CAPABILITIES_QUERY, b"\x1b]1337;Capabilities\x1b\\");
}
