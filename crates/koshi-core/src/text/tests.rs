//! Tests for `sanitize_reported_text` and `MAX_REPORTED_TEXT_BYTES`.
//!
//! Every refused character is asserted from a literal in the test.

use super::*;

/// Asserts that `sanitize_reported_text("a{c}b")` is `"ab"`.
#[track_caller]
fn refused(c: char) {
    let title = format!("a{c}b");
    assert_eq!(
        sanitize_reported_text(&title),
        "ab",
        "U+{:04X} survived sanitizing",
        c as u32
    );
}

/// Asserts that `sanitize_reported_text("a{c}b")` is `"a{c}b"`.
#[track_caller]
fn kept(c: char) {
    let title = format!("a{c}b");
    assert_eq!(
        sanitize_reported_text(&title),
        title,
        "U+{:04X} was removed",
        c as u32
    );
}

#[test]
fn ordinary_text_passes_through_unchanged() {
    assert_eq!(
        sanitize_reported_text("~/Projects/koshi"),
        "~/Projects/koshi"
    );
    assert_eq!(sanitize_reported_text(""), "");
    assert_eq!(sanitize_reported_text("日本語"), "日本語");
}

#[test]
fn every_c0_control_and_del_is_removed() {
    for code in 0x00u32..=0x1F {
        refused(char::from_u32(code).expect("C0 is a scalar value"));
    }
    refused('\u{7F}');
}

#[test]
fn every_c1_control_is_removed() {
    for code in 0x80u32..=0x9F {
        refused(char::from_u32(code).expect("C1 is a scalar value"));
    }
}

#[test]
fn every_bidi_control_is_removed_named_one_by_one() {
    // Unicode `Bidi_Control` is exactly twelve characters: nine explicit
    // formatting characters and three implicit marks.
    refused('\u{202A}'); // LEFT-TO-RIGHT EMBEDDING
    refused('\u{202B}'); // RIGHT-TO-LEFT EMBEDDING
    refused('\u{202C}'); // POP DIRECTIONAL FORMATTING
    refused('\u{202D}'); // LEFT-TO-RIGHT OVERRIDE
    refused('\u{202E}'); // RIGHT-TO-LEFT OVERRIDE
    refused('\u{2066}'); // LEFT-TO-RIGHT ISOLATE
    refused('\u{2067}'); // RIGHT-TO-LEFT ISOLATE
    refused('\u{2068}'); // FIRST STRONG ISOLATE
    refused('\u{2069}'); // POP DIRECTIONAL ISOLATE
    refused('\u{200E}'); // LEFT-TO-RIGHT MARK
    refused('\u{200F}'); // RIGHT-TO-LEFT MARK
    refused('\u{061C}'); // ARABIC LETTER MARK
    assert_eq!(sanitize_reported_text("\u{202E}gpj.exe"), "gpj.exe");
}

#[test]
fn line_and_paragraph_separators_are_removed() {
    refused('\u{2028}');
    refused('\u{2029}');
}

#[test]
fn noncharacters_and_annotation_marks_are_removed() {
    refused('\u{FFFE}');
    refused('\u{FFFF}');
    refused('\u{FFF9}');
    refused('\u{FFFA}');
    refused('\u{FFFB}');
}

#[test]
fn tag_characters_are_removed() {
    refused('\u{E0001}');
    refused('\u{E0020}');
    refused('\u{E0072}');
    refused('\u{E007F}');
    // `hidden` is `rm -rf /` spelled in tag characters: readable text that
    // takes zero display columns.
    let hidden: String = "rm -rf /"
        .chars()
        .map(|c| char::from_u32(0xE0000 + c as u32).expect("tag is a scalar value"))
        .collect();
    assert_eq!(sanitize_reported_text(&format!("bash{hidden}")), "bash");
}

#[test]
fn joiners_marks_and_variation_selectors_are_kept() {
    kept('\u{200D}'); // ZERO WIDTH JOINER
    kept('\u{200C}'); // ZERO WIDTH NON-JOINER
    kept('\u{0301}'); // COMBINING ACUTE ACCENT
    kept('\u{FE0F}'); // VARIATION SELECTOR-16
    assert_eq!(sanitize_reported_text("👩‍💻"), "👩‍💻");
}

#[test]
fn the_character_on_each_side_of_every_refused_range_is_kept() {
    kept('\u{0020}'); // SPACE, after C0
    kept('\u{007E}'); // TILDE, before DEL
    kept('\u{00A0}'); // NO-BREAK SPACE, after C1
    kept('\u{061B}'); // ARABIC SEMICOLON, before ARABIC LETTER MARK
    kept('\u{061D}'); // ARABIC END OF TEXT MARK, after ARABIC LETTER MARK
    kept('\u{200D}'); // ZERO WIDTH JOINER, before the implicit marks
    kept('\u{2010}'); // HYPHEN, after the implicit marks
    kept('\u{2027}'); // HYPHENATION POINT, before the separators
    kept('\u{202F}'); // NARROW NO-BREAK SPACE, after the embeddings
    kept('\u{2065}'); // unassigned, before the isolates
    kept('\u{206A}'); // INHIBIT SYMMETRIC SWAPPING, after the isolates
    kept('\u{FFF8}'); // unassigned, before the annotation marks
    kept('\u{FFFC}'); // OBJECT REPLACEMENT CHARACTER, after the annotation marks
    kept('\u{FFFD}'); // REPLACEMENT CHARACTER, before U+FFFE
    kept('\u{10000}'); // LINEAR B SYLLABLE B008 A, after U+FFFF
    kept('\u{DFFFF}'); // unassigned, before the tags
    kept('\u{E0080}'); // unassigned, after the tags
    kept('\u{E0100}'); // VARIATION SELECTOR-17
    kept('\u{10FFFF}'); // the last scalar value
}

#[test]
fn right_to_left_letters_are_kept_and_only_the_control_is_removed() {
    assert_eq!(sanitize_reported_text("שלום"), "שלום");
    // HEBREW LETTER ALEF, ARABIC LETTER MARK, ARABIC LETTER ALEF.
    assert_eq!(
        sanitize_reported_text("\u{05D0}\u{061C}\u{0627}"),
        "\u{05D0}\u{0627}"
    );
}

#[test]
fn an_escape_sequence_loses_only_its_escape_byte() {
    assert_eq!(
        sanitize_reported_text("\u{1b}[31mred\u{1b}[0m"),
        "[31mred[0m"
    );
}

#[test]
fn the_cap_is_exactly_max_pane_title_bytes() {
    assert_eq!(MAX_REPORTED_TEXT_BYTES, 512);
    // One byte under, exactly at, and one over.
    assert_eq!(sanitize_reported_text(&"a".repeat(511)), "a".repeat(511));
    assert_eq!(sanitize_reported_text(&"a".repeat(512)), "a".repeat(512));
    assert_eq!(sanitize_reported_text(&"a".repeat(513)), "a".repeat(512));
}

#[test]
fn a_long_title_is_cut_to_the_byte_cap() {
    assert_eq!(
        sanitize_reported_text(&"a".repeat(5_000_000)),
        "a".repeat(MAX_REPORTED_TEXT_BYTES)
    );
}

#[test]
fn the_cut_keeps_the_start_and_not_the_end() {
    assert_eq!(
        sanitize_reported_text(&format!("head{}", "x".repeat(1_000))),
        format!("head{}", "x".repeat(508))
    );
}

#[test]
fn the_cut_never_splits_a_character() {
    // Three bytes each: 170 fit in 510 bytes, the 171st does not.
    assert_eq!(
        sanitize_reported_text(&"日".repeat(1_000)),
        "日".repeat(170)
    );
    // Four bytes each: 128 fill 512 bytes exactly.
    assert_eq!(
        sanitize_reported_text(&"🙂".repeat(1_000)),
        "🙂".repeat(128)
    );
    // Two bytes each: 256 fill 512 bytes exactly.
    assert_eq!(sanitize_reported_text(&"é".repeat(1_000)), "é".repeat(256));
}

#[test]
fn the_cut_stops_at_the_first_character_that_does_not_fit() {
    // 510 bytes, then a four-byte character that does not fit, then a
    // one-byte character that would fit.
    let text = format!("{}🙂b", "a".repeat(510));
    assert_eq!(sanitize_reported_text(&text), "a".repeat(510));
}

#[test]
fn removed_characters_do_not_count_toward_the_cap() {
    let title = format!("{}{}", "\u{7f}".repeat(1_000), "shell");
    assert_eq!(sanitize_reported_text(&title), "shell");
}

#[test]
fn a_title_of_only_refused_characters_becomes_empty() {
    assert_eq!(
        sanitize_reported_text("\u{1b}\u{7f}\u{9b}\u{202e}\u{2028}"),
        ""
    );
}

#[test]
fn a_sanitized_title_is_stable_under_a_second_pass() {
    for raw in [
        "~/Projects/koshi",
        "a\u{7f}b",
        &"日".repeat(1_000),
        "👩‍💻",
        "\u{202E}gpj.exe",
    ] {
        let once = sanitize_reported_text(raw);
        assert_eq!(
            sanitize_reported_text(&once),
            once,
            "not stable for {raw:?}"
        );
    }
}
