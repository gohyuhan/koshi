//! Tests for `is_halfwidth_katakana_sound_mark`, `compute_character_display_width`
//! and `compute_text_display_width`.

use super::*;

#[test]
fn the_halfwidth_voiced_and_semi_voiced_marks_are_halfwidth_katakana_sound_marks() {
    assert!(is_halfwidth_katakana_sound_mark('\u{FF9E}'));
    assert!(is_halfwidth_katakana_sound_mark('\u{FF9F}'));
}

#[test]
fn the_characters_beside_and_like_the_halfwidth_sound_marks_are_not_halfwidth_sound_marks() {
    assert!(!is_halfwidth_katakana_sound_mark('\u{FF9D}')); // ﾝ
    assert!(!is_halfwidth_katakana_sound_mark('\u{FFA0}'));
    assert!(!is_halfwidth_katakana_sound_mark('\u{FF76}')); // ｶ
    assert!(!is_halfwidth_katakana_sound_mark('\u{3099}'));
    assert!(!is_halfwidth_katakana_sound_mark('\u{309A}'));
}

#[test]
fn a_halfwidth_sound_mark_on_its_own_takes_one_column() {
    assert_eq!(compute_character_display_width('\u{FF9E}'), Some(1));
    assert_eq!(compute_character_display_width('\u{FF9F}'), Some(1));
}

#[test]
fn the_khmer_sign_beyyal_takes_one_column_on_its_own_and_in_text() {
    assert_eq!(compute_character_display_width('\u{17D8}'), Some(1));
    assert_eq!(compute_text_display_width("\u{17D8}"), 1);
    assert_eq!(compute_text_display_width("\u{17D8}\u{17D8}x"), 3);
    assert_eq!(compute_text_display_width("\u{17D8}\u{301}"), 1);
}

#[test]
fn every_other_character_takes_the_columns_unicode_width_gives_it() {
    assert_eq!(compute_character_display_width('a'), Some(1));
    assert_eq!(compute_character_display_width('漢'), Some(2));
    assert_eq!(compute_character_display_width('\u{301}'), Some(0));
    assert_eq!(compute_character_display_width('\u{3099}'), Some(0));
    assert_eq!(compute_character_display_width('\u{17D7}'), Some(1));
    assert_eq!(compute_character_display_width('\u{17D9}'), Some(1));
    assert_eq!(compute_character_display_width('\u{7}'), None);
}

#[test]
fn text_takes_one_more_column_for_each_halfwidth_sound_mark() {
    assert_eq!(compute_text_display_width("\u{FF76}\u{FF9E}"), 2); // ｶﾞ
    assert_eq!(compute_text_display_width("\u{FF8A}\u{FF9F}"), 2); // ﾊﾟ
    assert_eq!(
        compute_text_display_width("\u{FF76}\u{FF9E}\u{FF8A}\u{FF9F}"),
        4
    );
    assert_eq!(compute_text_display_width("a\u{FF9E}"), 2);
    assert_eq!(compute_text_display_width("\u{FF9E}"), 1);
}

#[test]
fn text_with_no_halfwidth_sound_mark_takes_the_columns_unicode_width_gives_it() {
    assert_eq!(compute_text_display_width(""), 0);
    assert_eq!(compute_text_display_width("name"), 4);
    assert_eq!(compute_text_display_width("漢字"), 4);
    assert_eq!(compute_text_display_width("e\u{301}"), 1);
    assert_eq!(compute_text_display_width("\u{FF76}\u{3099}"), 1); // ｶ and the combining voiced sound mark
    assert_eq!(
        compute_text_display_width("\u{915}\u{94D}\u{937}\u{93F}"),
        3
    ); // क्षि
}
