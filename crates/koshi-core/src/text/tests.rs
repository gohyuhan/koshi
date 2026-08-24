//! Tests for pane-title sanitizing.
//!
//! Every refused character is named by its own literal. A loop over the
//! constant under test asserts nothing: deleting an entry deletes its
//! assertion with it.

use super::*;

/// Assert that `c` is removed from the middle of a title.
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

/// Assert that `c` is kept in the middle of a title.
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
    // A tag sequence spells readable text in zero display columns.
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
fn the_cap_is_exactly_max_pane_title_bytes() {
    assert_eq!(MAX_REPORTED_TEXT_BYTES, 512);
    // One byte under, exactly at, and one over.
    assert_eq!(sanitize_reported_text(&"a".repeat(511)).len(), 511);
    assert_eq!(sanitize_reported_text(&"a".repeat(512)).len(), 512);
    assert_eq!(sanitize_reported_text(&"a".repeat(513)).len(), 512);
}

#[test]
fn a_long_title_is_cut_to_the_byte_cap() {
    let cut = sanitize_reported_text(&"a".repeat(5_000_000));
    assert_eq!(cut.len(), MAX_REPORTED_TEXT_BYTES);
    assert!(cut.chars().all(|c| c == 'a'));
}

#[test]
fn the_cut_keeps_the_start_and_not_the_end() {
    // Which end survives is user-visible and must not drift.
    let cut = sanitize_reported_text(&format!("head{}", "x".repeat(1_000)));
    assert!(
        cut.starts_with("head"),
        "the cut kept the wrong end: {cut:?}"
    );
    assert_eq!(cut.len(), MAX_REPORTED_TEXT_BYTES);
}

#[test]
fn the_cut_never_splits_a_character() {
    // Three bytes: 512 is not a multiple, so the cut lands short of the cap.
    let three = sanitize_reported_text(&"日".repeat(1_000));
    assert_eq!(three.len(), 510);
    assert_eq!(three.chars().count(), 170);
    assert!(three.chars().all(|c| c == '日'));

    // Four bytes: 512 IS a multiple, so the cut lands exactly on the cap.
    let four = sanitize_reported_text(&"🙂".repeat(1_000));
    assert_eq!(four.len(), MAX_REPORTED_TEXT_BYTES);
    assert_eq!(four.chars().count(), 128);

    // Two bytes: divides evenly as well.
    let two = sanitize_reported_text(&"é".repeat(1_000));
    assert_eq!(two.len(), MAX_REPORTED_TEXT_BYTES);
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
    // Applying at ingest must not differ from applying twice, or a caller that
    // sanitizes again changes the value.
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
