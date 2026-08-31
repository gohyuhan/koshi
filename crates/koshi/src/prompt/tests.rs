//! Tests for the yes-or-no answer reader.

use super::*;

#[test]
fn y_and_yes_are_yes_in_any_letter_case() {
    for answer in ["y", "Y", "yes", "Yes", "YES", "yEs"] {
        assert!(is_yes(answer), "{answer} is a yes");
    }
}

#[test]
fn surrounding_whitespace_and_the_line_ending_are_trimmed() {
    assert!(is_yes("  y \n"));
    assert!(is_yes("yes\r\n"));
}

#[test]
fn every_other_answer_is_no() {
    for answer in ["", "  ", "\n", "n", "no", "yep", "ye", "yess", "1", "true"] {
        assert!(!is_yes(answer), "{answer:?} is not a yes");
    }
}
