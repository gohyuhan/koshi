//! Tests for iTerm2 feature reporting.

use super::*;

#[test]
fn iterm_file_feature_matches_whole_tokens_and_ignores_extensions() {
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
fn iterm_sixel_feature_matches_the_whole_token_not_prefixes() {
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
fn iterm_capabilities_query_has_the_exact_osc_terminator() {
    assert_eq!(ITERM_CAPABILITIES_QUERY, b"\x1b]1337;Capabilities\x1b\\");
}
