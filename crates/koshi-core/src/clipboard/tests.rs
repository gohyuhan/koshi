//! Tests for `push_clipboard_target` and `filter_clipboard_targets`.

use super::*;

#[test]
fn filter_clipboard_targets_keeps_every_target_byte_in_order() {
    assert_eq!(filter_clipboard_targets(b"cpqs01234567"), "cpqs01234567");
    assert_eq!(filter_clipboard_targets(b"pc"), "pc");
}

#[test]
fn filter_clipboard_targets_removes_every_byte_outside_the_target_set() {
    assert_eq!(filter_clipboard_targets(b"pXc"), "pc");
    assert_eq!(filter_clipboard_targets(b"c8 C;\x07\x1b"), "c");
    assert_eq!(filter_clipboard_targets(b"x"), "");
}

#[test]
fn filter_clipboard_targets_keeps_an_empty_field_empty() {
    assert_eq!(filter_clipboard_targets(b""), "");
}

#[test]
fn filter_clipboard_targets_keeps_each_target_once_at_its_first_use() {
    assert_eq!(filter_clipboard_targets(b"pcpc"), "pc");
    let long_field = b"cpqs01234567".repeat(1000);
    assert_eq!(filter_clipboard_targets(&long_field), "cpqs01234567");
}

#[test]
fn push_clipboard_target_adds_only_a_new_target_byte() {
    let mut clipboard_targets = String::new();
    for target_byte in [b'p', b'X', b'c', b'p', 0xFF] {
        push_clipboard_target(&mut clipboard_targets, target_byte);
    }
    assert_eq!(clipboard_targets, "pc");
}
