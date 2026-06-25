//! Unit tests for the bounded scrollback buffer: byte accounting, the line and
//! byte caps, oldest-first dropping, and the truncation tallies.

use super::*;
use crate::style::Style;

/// A row of single-width ASCII cells — one byte each — from `s`.
fn line(s: &str) -> Vec<Cell> {
    s.chars()
        .map(|c| Cell::new(c, 1, Style::default()))
        .collect()
}

/// A buffer bounded by exactly `max_lines` rows and `max_bytes` bytes.
fn bounded(max_lines: usize, max_bytes: usize) -> Scrollback {
    Scrollback::new(ScrollbackLimit {
        max_lines,
        max_bytes,
    })
}

/// The base characters of every retained row, front (oldest) to back.
fn retained(sb: &Scrollback) -> Vec<String> {
    sb.lines()
        .iter()
        .map(|row| row.iter().map(Cell::ch).collect())
        .collect()
}

#[test]
fn a_new_buffer_is_empty_with_no_drops() {
    let sb = bounded(10, 1000);
    assert!(sb.is_empty());
    assert_eq!(sb.len(), 0);
    assert_eq!(sb.byte_total, 0);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.dropped_bytes(), 0);
}

#[test]
fn line_bytes_sums_base_and_combining_as_utf8_lengths() {
    let sb = bounded(10, 1000);
    // 'a' (1 byte) + '世' (3 bytes) + 'e' carrying a combining acute (1 + 2).
    let mut accented = Cell::new('e', 1, Style::default());
    accented.push_combining('\u{0301}'); // U+0301, two UTF-8 bytes
    let row = vec![
        Cell::new('a', 1, Style::default()),
        Cell::new('世', 2, Style::default()),
        accented,
    ];
    assert_eq!(sb.line_bytes(&row), 1 + 3 + (1 + 2));
}

#[test]
fn pushing_within_both_caps_retains_every_row_in_order() {
    let mut sb = bounded(10, 1000);
    sb.push_line(line("one"));
    sb.push_line(line("two"));
    sb.push_line(line("three"));
    assert_eq!(sb.len(), 3);
    assert_eq!(retained(&sb), vec!["one", "two", "three"]);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.dropped_bytes(), 0);
    assert_eq!(sb.byte_total, 3 + 3 + 5);
}

#[test]
fn exceeding_the_line_cap_drops_oldest_first() {
    let mut sb = bounded(3, 100_000);
    sb.push_line(line("L0")); // dropped by the fourth push
    sb.push_line(line("L1"));
    sb.push_line(line("L2"));
    sb.push_line(line("L3"));
    assert_eq!(sb.len(), 3);
    assert_eq!(retained(&sb), vec!["L1", "L2", "L3"]);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 2); // "L0" is two bytes
    assert_eq!(sb.byte_total, 6); // three two-byte rows remain
}

#[test]
fn exceeding_the_byte_cap_drops_oldest_until_within_budget() {
    // Four-byte rows, a ten-byte cap: a third row pushes the total to 12 and
    // forces exactly one drop back to 8.
    let mut sb = bounded(100_000, 10);
    sb.push_line(line("aaaa"));
    sb.push_line(line("bbbb"));
    sb.push_line(line("cccc"));
    assert_eq!(sb.len(), 2);
    assert_eq!(retained(&sb), vec!["bbbb", "cccc"]);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 4);
    assert_eq!(sb.byte_total, 8);
}

#[test]
fn a_lone_row_larger_than_the_byte_cap_is_kept_not_dropped() {
    // The `len > 1` guard means the byte cap never empties the buffer: a single
    // oversized row is retained even though it busts the budget.
    let mut sb = bounded(100_000, 2);
    sb.push_line(line("oversized"));
    assert_eq!(sb.len(), 1);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.byte_total, 9);
}

#[test]
fn a_later_push_drops_the_retained_oversized_row() {
    // Once a second row arrives the guard no longer applies, so the oversized
    // row is dropped to bring the total back under the cap.
    let mut sb = bounded(100_000, 2);
    sb.push_line(line("oversized")); // 9 bytes, kept by the guard
    sb.push_line(line("x")); // 1 byte: total 10, len 2 -> drop the front
    assert_eq!(sb.len(), 1);
    assert_eq!(retained(&sb), vec!["x"]);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 9);
    assert_eq!(sb.byte_total, 1);
}

#[test]
fn the_line_cap_can_drop_to_empty_unlike_the_byte_cap() {
    // The line cap has no `len > 1` guard, so a zero cap retains nothing.
    let mut sb = bounded(0, 100_000);
    sb.push_line(line("gone"));
    assert!(sb.is_empty());
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 4);
    assert_eq!(sb.byte_total, 0);
}

#[test]
fn byte_total_stays_equal_to_the_sum_of_retained_rows() {
    let mut sb = bounded(3, 100_000);
    for s in ["alpha", "beta", "gamma", "delta", "epsilon"] {
        sb.push_line(line(s));
    }
    let expected: usize = sb.lines().iter().map(|row| sb.line_bytes(row)).sum();
    assert_eq!(sb.byte_total, expected);
}

#[test]
fn dropped_tallies_accumulate_across_many_drops() {
    let mut sb = bounded(1, 100_000); // every push past the first drops one row
    sb.push_line(line("aa")); // 2 bytes
    sb.push_line(line("bbb")); // 3 bytes, drops "aa"
    sb.push_line(line("c")); // 1 byte, drops "bbb"
    assert_eq!(sb.len(), 1);
    assert_eq!(retained(&sb), vec!["c"]);
    assert_eq!(sb.dropped_lines(), 2);
    assert_eq!(sb.dropped_bytes(), 5);
}
