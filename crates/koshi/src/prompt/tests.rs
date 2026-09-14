//! Tests for the yes-or-no answer reader.

use super::*;

#[test]
fn y_and_yes_are_yes_in_any_letter_case() {
    for answer_text in ["y", "Y", "yes", "Yes", "YES", "yEs"] {
        assert!(is_yes_answer(answer_text), "{answer_text} is a yes");
    }
}

#[test]
fn surrounding_whitespace_and_the_line_ending_are_trimmed() {
    assert!(is_yes_answer("  y \n"));
    assert!(is_yes_answer("yes\r\n"));
}

#[test]
fn every_other_answer_is_no() {
    for answer_text in ["", "  ", "\n", "n", "no", "yep", "ye", "yess", "1", "true"] {
        assert!(!is_yes_answer(answer_text), "{answer_text:?} is not a yes");
    }
}
