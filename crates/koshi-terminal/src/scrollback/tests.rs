//! Unit tests for the bounded scrollback buffer: byte accounting, the line and
//! byte caps, oldest-first dropping, and the truncation tallies.

use super::*;
use crate::style::Style;

/// A row of single-width ASCII cells — one byte each — from `line_text`.
fn build_line_cells(line_text: &str) -> Vec<Cell> {
    line_text
        .chars()
        .map(|character| Cell::from_character(character, 1, Style::default()))
        .collect()
}

/// A buffer bounded by exactly `maximum_line_count` rows and `maximum_byte_count` bytes.
fn build_bounded_scrollback(maximum_line_count: usize, maximum_byte_count: usize) -> Scrollback {
    Scrollback::from_scrollback_limit(ScrollbackLimit {
        maximum_line_count,
        maximum_byte_count,
    })
}

/// Replace the retained rows and preserve the current retained line count.
fn replace_retained_rows(
    scrollback: &mut Scrollback,
    retained_lines: Vec<(Vec<Cell>, RowMetadata)>,
) {
    let retained_line_count_before = scrollback.get_retained_line_count() as u64;
    scrollback.replace_retained_lines(retained_lines, retained_line_count_before);
}

/// The base characters of every retained row, front (oldest) to back.
fn list_retained_row_texts(scrollback: &Scrollback) -> Vec<String> {
    scrollback
        .list_retained_lines()
        .iter()
        .map(|(line_cells, _)| line_cells.iter().map(Cell::get_character).collect())
        .collect()
}

#[test]
fn a_new_buffer_is_empty_with_no_drops() {
    let scrollback = build_bounded_scrollback(10, 1000);
    assert!(scrollback.is_empty());
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 0);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
    assert_eq!(scrollback.get_dropped_byte_count(), 0);
}

#[test]
fn compute_line_byte_count_sums_base_and_combining_as_utf8_lengths() {
    // 'a' (1 byte) + '世' (3 bytes) + 'e' carrying a combining acute (1 + 2).
    let mut accented = Cell::from_character('e', 1, Style::default());
    accented.push_combining('\u{0301}'); // U+0301, two UTF-8 bytes
    let line_cells = vec![
        Cell::from_character('a', 1, Style::default()),
        Cell::from_character('世', 2, Style::default()),
        accented,
    ];
    assert_eq!(compute_line_byte_count(&line_cells), 1 + 3 + (1 + 2));
}

#[test]
fn compute_line_byte_count_skips_wide_glyph_continuation_placeholders() {
    // A wide glyph occupies two cells: a width-2 base carrying '世' (3 bytes)
    // and a width-0 continuation placeholder (a blank space). Only the base
    // carries text; the placeholder adds nothing.
    let line_cells = vec![
        Cell::from_character('世', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ];
    assert_eq!(compute_line_byte_count(&line_cells), 3); // the space placeholder is skipped
}

#[test]
fn pushing_within_both_caps_retains_every_row_in_order() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row(&build_line_cells("one"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("two"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("three"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 3);
    assert_eq!(
        list_retained_row_texts(&scrollback),
        vec!["one", "two", "three"]
    );
    assert_eq!(scrollback.get_dropped_line_count(), 0);
    assert_eq!(scrollback.get_dropped_byte_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 3 + 3 + 5);
}

#[test]
fn exceeding_the_line_cap_drops_oldest_first() {
    let mut scrollback = build_bounded_scrollback(3, 100_000);
    scrollback.push_row(&build_line_cells("L0"), RowMetadata::default()); // dropped by the fourth push
    scrollback.push_row(&build_line_cells("L1"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("L2"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("L3"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 3);
    assert_eq!(list_retained_row_texts(&scrollback), vec!["L1", "L2", "L3"]);
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 2); // "L0" is two bytes
    assert_eq!(scrollback.retained_byte_count, 6); // three two-byte rows remain
}

#[test]
fn exceeding_the_byte_cap_drops_oldest_until_within_budget() {
    // Four-byte rows, a ten-byte cap: a third row pushes the total to 12 and
    // forces exactly one drop back to 8.
    let mut scrollback = build_bounded_scrollback(100_000, 10);
    scrollback.push_row(&build_line_cells("aaaa"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("bbbb"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("cccc"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 2);
    assert_eq!(list_retained_row_texts(&scrollback), vec!["bbbb", "cccc"]);
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 4);
    assert_eq!(scrollback.retained_byte_count, 8);
}

#[test]
fn a_lone_row_larger_than_the_byte_cap_is_kept_not_dropped() {
    // The `len > 1` guard means the byte cap never empties the buffer: a single
    // oversized row is retained even though it busts the budget.
    let mut scrollback = build_bounded_scrollback(100_000, 2);
    scrollback.push_row(&build_line_cells("oversized"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 9);
}

#[test]
fn a_subsequent_push_drops_the_retained_oversized_row() {
    // With a second row present the guard no longer applies: the oversized
    // row is dropped and the total falls back under the cap.
    let mut scrollback = build_bounded_scrollback(100_000, 2);
    scrollback.push_row(&build_line_cells("oversized"), RowMetadata::default()); // 9 bytes, kept by the guard
    scrollback.push_row(&build_line_cells("x"), RowMetadata::default()); // 1 byte: total 10, len 2 -> drop the front
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(list_retained_row_texts(&scrollback), vec!["x"]);
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 9);
    assert_eq!(scrollback.retained_byte_count, 1);
}

#[test]
fn the_line_cap_can_drop_to_empty_unlike_the_byte_cap() {
    // The line cap has no `len > 1` guard: a zero cap retains nothing.
    let mut scrollback = build_bounded_scrollback(0, 100_000);
    scrollback.push_row(&build_line_cells("gone"), RowMetadata::default());
    assert!(scrollback.is_empty());
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 4);
    assert_eq!(scrollback.retained_byte_count, 0);
}

#[test]
fn retained_byte_count_stays_equal_to_the_sum_of_retained_rows() {
    let mut scrollback = build_bounded_scrollback(3, 100_000);
    for line_text in ["alpha", "beta", "gamma", "delta", "epsilon"] {
        scrollback.push_row(&build_line_cells(line_text), RowMetadata::default());
    }
    let expected_retained_byte_count: usize = scrollback
        .list_retained_lines()
        .iter()
        .map(|(line_cells, _)| compute_line_byte_count(line_cells))
        .sum();
    assert_eq!(scrollback.retained_byte_count, expected_retained_byte_count);
}

#[test]
fn dropped_tallies_accumulate_across_many_drops() {
    let mut scrollback = build_bounded_scrollback(1, 100_000); // every push past the first drops one row
    scrollback.push_row(&build_line_cells("aa"), RowMetadata::default()); // 2 bytes
    scrollback.push_row(&build_line_cells("bbb"), RowMetadata::default()); // 3 bytes, drops "aa"
    scrollback.push_row(&build_line_cells("c"), RowMetadata::default()); // 1 byte, drops "bbb"
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(list_retained_row_texts(&scrollback), vec!["c"]);
    assert_eq!(scrollback.get_dropped_line_count(), 2);
    assert_eq!(scrollback.get_dropped_byte_count(), 5);
}

#[test]
fn clear_scrollback_empties_the_buffer_but_keeps_the_drop_tallies() {
    let mut scrollback = build_bounded_scrollback(1, 100_000); // line cap of 1 forces a drop
    scrollback.push_row(&build_line_cells("aa"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("bbb"), RowMetadata::default()); // drops "aa": dropped_line_count 1, dropped_byte_count 2
    assert_eq!(scrollback.get_dropped_line_count(), 1);

    scrollback.clear_scrollback();
    assert!(scrollback.is_empty());
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 0);
    // An explicit erase leaves the tallies as they were.
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 2);
}

#[test]
fn total_pushed_line_count_counts_every_push_and_survives_clear_scrollback() {
    let mut scrollback = build_bounded_scrollback(2, 1000); // line cap 2: a push that drops still counts
    scrollback.push_row(&build_line_cells("a"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("b"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("c"), RowMetadata::default()); // drops "a"; the push itself still counts
    assert_eq!(scrollback.get_total_pushed_line_count(), 3);

    scrollback.clear_scrollback();
    assert_eq!(scrollback.get_total_pushed_line_count(), 3); // an erase never rewinds the counter

    scrollback.push_row(&build_line_cells("d"), RowMetadata::default());
    assert_eq!(scrollback.get_total_pushed_line_count(), 4);
}

/// A row of `line_text` padded out to `column_count` with default blanks, the shape every
/// row arrives in from the screen.
fn build_padded_line_cells(line_text: &str, column_count: usize) -> Vec<Cell> {
    let mut line_cells = build_line_cells(line_text);
    line_cells.resize(column_count, Cell::blank());
    line_cells
}

#[test]
fn a_hard_row_drops_the_blanks_padding_it_out_to_the_screen_width() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(
        &build_padded_line_cells("README.md", 200),
        RowMetadata::default(),
    );

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 9);
    assert_eq!(
        list_retained_row_texts(&scrollback),
        vec!["README.md".to_string()]
    );
}

#[test]
fn a_trimmed_row_releases_the_memory_and_does_not_just_hide_it() {
    // `Vec::truncate` keeps the capacity the padding needed; the trim releases
    // it. `Vec` promises a capacity of at least the length, never an exact
    // figure: the check is a bound, not an equality.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(&build_padded_line_cells("hi", 200), RowMetadata::default());

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 2);
    assert!(
        retained_line_cells.capacity() < 200,
        "the 200-cell allocation was kept: capacity {}",
        retained_line_cells.capacity()
    );
}

#[test]
fn a_soft_wrapped_row_keeps_every_cell() {
    // Every cell of a soft-wrapped row is content, trailing blanks included.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(
        &build_padded_line_cells("ab", 6),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 6);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer() {
    // The final blank stands in for the wide glyph that starts the next row.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(
        &build_padded_line_cells("ab", 6),
        RowMetadata {
            row_end: RowEnd::SoftWide,
            has_prompt_mark: false,
        },
    );

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 6);
}

#[test]
fn a_background_colored_blank_is_content_and_survives() {
    // A prompt segment painting color into blank cells: the colored cells are
    // content and stay.
    let mut red = Style::default();
    red.set_background_color(crate::style::Color::Indexed(1));
    let mut line_cells = build_line_cells("ab");
    line_cells.push(Cell::blank_with(red));
    line_cells.resize(200, Cell::blank());

    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(&line_cells, RowMetadata::default());

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 3);
    assert_eq!(
        retained_line_cells[2].get_style().get_background_color(),
        crate::style::Color::Indexed(1)
    );
}

#[test]
fn a_wide_glyphs_continuation_cell_is_content_and_survives() {
    // The zero-width right half of a CJK glyph is not a default blank: the
    // trim keeps it.
    let mut line_cells = vec![
        Cell::from_character('漢', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ];
    line_cells.resize(200, Cell::blank());

    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(&line_cells, RowMetadata::default());

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 2);
    assert_eq!(retained_line_cells[1].get_display_width(), 0);
}

#[test]
fn a_row_of_nothing_but_padding_stores_no_cells() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(&build_padded_line_cells("", 200), RowMetadata::default());

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert!(retained_line_cells.is_empty());
    assert_eq!(scrollback.get_retained_line_count(), 1); // the blank line itself is still a line
}

#[test]
fn a_reflow_rebuild_trims_the_same_way_a_push_does() {
    // A replacement trims a hard-ended row the same way a push does.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_rows(
        &mut scrollback,
        vec![
            (build_padded_line_cells("one", 200), RowMetadata::default()),
            (
                build_padded_line_cells("ab", 200),
                RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: false,
                },
            ),
        ],
    );

    let (hard_line_cells, _) = &scrollback.list_retained_lines()[0];
    let (soft_line_cells, _) = &scrollback.list_retained_lines()[1];
    assert_eq!(hard_line_cells.len(), 3);
    assert_eq!(soft_line_cells.len(), 200);
}

#[test]
fn a_reflow_rebuild_releases_the_memory_the_padding_held() {
    // A replacement releases the padding's capacity the same way a push does.
    // `Vec` never promises a capacity equal to the length: the check is a
    // bound, not an equality.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_rows(
        &mut scrollback,
        vec![(build_padded_line_cells("hi", 200), RowMetadata::default())],
    );

    let (retained_line_cells, _) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 2);
    assert!(
        retained_line_cells.capacity() < 200,
        "the 200-cell allocation was kept: capacity {}",
        retained_line_cells.capacity()
    );
}

#[test]
fn trimming_lets_the_byte_cap_hold_the_text_it_was_set_for() {
    // The cap counts retained characters: a 200-column row of `hi` charges 2,
    // and ten of them fit a cap of 20.
    let mut scrollback = build_bounded_scrollback(1000, 20);
    for _ in 0..10 {
        scrollback.push_row(&build_padded_line_cells("hi", 200), RowMetadata::default());
    }
    assert_eq!(scrollback.get_retained_line_count(), 10);
    assert_eq!(scrollback.retained_byte_count, 20);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
}

#[test]
fn prompt_marks_stay_with_rows_through_history_replacement() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_rows(
        &mut scrollback,
        vec![
            (
                build_line_cells("prompt"),
                RowMetadata {
                    row_end: RowEnd::Hard,
                    has_prompt_mark: true,
                },
            ),
            (
                build_line_cells("output"),
                RowMetadata {
                    row_end: RowEnd::Hard,
                    has_prompt_mark: false,
                },
            ),
        ],
    );

    assert!(scrollback.list_retained_lines()[0].1.has_prompt_mark);
    assert!(!scrollback.list_retained_lines()[1].1.has_prompt_mark);
}

#[test]
fn prompt_marks_are_evicted_with_their_rows() {
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
    );
    scrollback.push_row(
        &build_line_cells("output"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: false,
        },
    );

    assert_eq!(list_retained_row_texts(&scrollback), vec!["output"]);
    assert!(!scrollback.list_retained_lines()[0].1.has_prompt_mark);
}

#[test]
fn current_scrollback_rows_round_trip_with_prompt_metadata() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
    );

    let serialized_scrollback = serde_json::to_value(&scrollback).expect("scrollback serializes");
    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(
        restored_scrollback.list_retained_lines()[0].1.row_end,
        RowEnd::Hard
    );
    assert!(
        restored_scrollback.list_retained_lines()[0]
            .1
            .has_prompt_mark
    );
}

#[test]
fn legacy_scrollback_rows_deserialize_as_unmarked() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: true,
        },
    );
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    let serialized_scrollback_object = serialized_scrollback
        .as_object_mut()
        .expect("scrollback is an object");
    let serialized_line_rows = serialized_scrollback_object
        .get_mut("retained_lines")
        .and_then(serde_json::Value::as_array_mut)
        .expect("scrollback lines are an array");
    for serialized_line_row in serialized_line_rows {
        let serialized_row_metadata = serialized_line_row
            .as_array_mut()
            .expect("serialized row is an array")
            .pop()
            .expect("serialized row has metadata");
        let row_end_value = serialized_row_metadata["row_end"].clone();
        serialized_line_row
            .as_array_mut()
            .expect("serialized row is an array")
            .push(row_end_value);
    }

    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("legacy scrollback deserializes");

    assert_eq!(
        restored_scrollback.list_retained_lines()[0].1.row_end,
        RowEnd::Soft
    );
    assert!(
        !restored_scrollback.list_retained_lines()[0]
            .1
            .has_prompt_mark
    );
}

#[test]
fn from_default_scrollback_limit_uses_ten_thousand_lines_and_thirty_two_mebibytes() {
    let scrollback = Scrollback::from_scrollback_limit(ScrollbackLimit::default());
    assert_eq!(scrollback.maximum_line_count, 10_000);
    assert_eq!(scrollback.maximum_byte_count, 32 * 1024 * 1024);
}

#[test]
fn from_scrollback_limit_copies_both_caps_from_the_limit() {
    let scrollback =
        Scrollback::from_scrollback_limit(ScrollbackLimit::from_line_and_byte_limits(7, 99));
    assert_eq!(scrollback.maximum_line_count, 7);
    assert_eq!(scrollback.maximum_byte_count, 99);
    assert_eq!(scrollback.get_total_pushed_line_count(), 0);
}

#[test]
fn compute_line_byte_count_of_an_empty_row_is_zero() {
    assert_eq!(compute_line_byte_count(&[]), 0);
}

#[test]
fn pushing_an_empty_row_counts_a_line_of_zero_bytes() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row(&[], RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(scrollback.retained_byte_count, 0);
    assert_eq!(scrollback.get_total_pushed_line_count(), 1);
    assert_eq!(list_retained_row_texts(&scrollback), vec![String::new()]);
}

#[test]
fn a_retained_byte_count_exactly_at_the_cap_keeps_every_row() {
    let mut scrollback = build_bounded_scrollback(100, 8);
    scrollback.push_row(&build_line_cells("aaaa"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("bbbb"), RowMetadata::default()); // total 8: at the cap, not past it
    assert_eq!(scrollback.get_retained_line_count(), 2);
    assert_eq!(scrollback.retained_byte_count, 8);
    assert_eq!(scrollback.get_dropped_line_count(), 0);

    scrollback.push_row(&build_line_cells("c"), RowMetadata::default()); // total 9: one past, drops "aaaa"
    assert_eq!(list_retained_row_texts(&scrollback), vec!["bbbb", "c"]);
    assert_eq!(scrollback.retained_byte_count, 5);
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 4);
}

#[test]
fn a_row_count_exactly_at_the_line_cap_keeps_every_row() {
    let mut scrollback = build_bounded_scrollback(2, 1000);
    scrollback.push_row(&build_line_cells("a"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("b"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 2);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
}

#[test]
fn one_push_can_drop_for_the_line_cap_and_then_the_byte_cap() {
    let mut scrollback = build_bounded_scrollback(2, 2);
    scrollback.push_row(&build_line_cells("a"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("b"), RowMetadata::default()); // len 2, bytes 2: both caps hold
    scrollback.push_row(&build_line_cells("cc"), RowMetadata::default()); // len 3 drops "a"; bytes 3 then drops "b"
    assert_eq!(list_retained_row_texts(&scrollback), vec!["cc"]);
    assert_eq!(scrollback.retained_byte_count, 2);
    assert_eq!(scrollback.get_dropped_line_count(), 2);
    assert_eq!(scrollback.get_dropped_byte_count(), 2);
}

#[test]
fn a_soft_row_charges_its_padding_against_the_byte_cap() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(
        &build_padded_line_cells("ab", 6),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );
    assert_eq!(scrollback.retained_byte_count, 6);
}

#[test]
fn dropped_byte_count_counts_the_stored_row_not_the_screen_row() {
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row(&build_padded_line_cells("hi", 200), RowMetadata::default());
    scrollback.push_row(&build_line_cells("x"), RowMetadata::default()); // drops the trimmed "hi"
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 2);
}

#[test]
fn replacing_with_fewer_rows_leaves_total_pushed_line_count_unchanged() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    for line_text in ["one", "two", "three"] {
        scrollback.push_row(&build_line_cells(line_text), RowMetadata::default());
    }
    replace_retained_rows(
        &mut scrollback,
        vec![(build_line_cells("only"), RowMetadata::default())],
    );
    assert_eq!(list_retained_row_texts(&scrollback), vec!["only"]);
    assert_eq!(scrollback.retained_byte_count, 4);
    assert_eq!(scrollback.get_total_pushed_line_count(), 3);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
}

#[test]
fn replacing_with_more_rows_grows_total_pushed_line_count_by_the_difference() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row(&build_line_cells("one"), RowMetadata::default());
    replace_retained_rows(
        &mut scrollback,
        vec![
            (build_line_cells("a"), RowMetadata::default()),
            (build_line_cells("b"), RowMetadata::default()),
            (build_line_cells("c"), RowMetadata::default()),
        ],
    );
    assert_eq!(list_retained_row_texts(&scrollback), vec!["a", "b", "c"]);
    assert_eq!(scrollback.retained_byte_count, 3);
    assert_eq!(scrollback.get_total_pushed_line_count(), 3);
}

#[test]
fn replacing_past_the_line_cap_evicts_the_oldest_and_tallies_them() {
    let mut scrollback = build_bounded_scrollback(2, 1_000_000);
    replace_retained_rows(
        &mut scrollback,
        vec![
            (build_line_cells("aa"), RowMetadata::default()),
            (build_line_cells("bbb"), RowMetadata::default()),
            (build_line_cells("c"), RowMetadata::default()),
            (build_line_cells("dd"), RowMetadata::default()),
        ],
    );
    assert_eq!(list_retained_row_texts(&scrollback), vec!["c", "dd"]);
    assert_eq!(scrollback.retained_byte_count, 3);
    assert_eq!(scrollback.get_dropped_line_count(), 2);
    assert_eq!(scrollback.get_dropped_byte_count(), 5);
    // The increase is counted after eviction: two rows retained, none before.
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
}

#[test]
fn replacing_past_the_byte_cap_keeps_the_newest_rows_within_budget() {
    let mut scrollback = build_bounded_scrollback(100, 4);
    replace_retained_rows(
        &mut scrollback,
        vec![
            (build_line_cells("aaa"), RowMetadata::default()),
            (build_line_cells("bb"), RowMetadata::default()),
            (build_line_cells("cc"), RowMetadata::default()),
        ],
    );
    assert_eq!(list_retained_row_texts(&scrollback), vec!["bb", "cc"]);
    assert_eq!(scrollback.retained_byte_count, 4);
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 3);
}

#[test]
fn replacing_with_nothing_empties_the_buffer_and_keeps_the_counters() {
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row(&build_line_cells("aa"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("bb"), RowMetadata::default()); // drops "aa"
    replace_retained_rows(&mut scrollback, Vec::new());
    assert!(scrollback.is_empty());
    assert_eq!(scrollback.retained_byte_count, 0);
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
    assert_eq!(scrollback.get_dropped_line_count(), 1);
    assert_eq!(scrollback.get_dropped_byte_count(), 2);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer_through_replace_retained_lines() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_rows(
        &mut scrollback,
        vec![(
            build_padded_line_cells("ab", 6),
            RowMetadata {
                row_end: RowEnd::SoftWide,
                has_prompt_mark: false,
            },
        )],
    );

    let (retained_line_cells, row_metadata) = &scrollback.list_retained_lines()[0];
    assert_eq!(retained_line_cells.len(), 6);
    assert_eq!(row_metadata.row_end, RowEnd::SoftWide);
}

#[test]
fn a_push_after_clear_scrollback_starts_the_retained_byte_count_from_zero() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row(&build_line_cells("abc"), RowMetadata::default());
    scrollback.clear_scrollback();
    scrollback.push_row(&build_line_cells("de"), RowMetadata::default());
    assert_eq!(list_retained_row_texts(&scrollback), vec!["de"]);
    assert_eq!(scrollback.retained_byte_count, 2);
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
}

#[test]
fn clear_scrollback_on_an_empty_buffer_changes_nothing() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.clear_scrollback();
    assert_eq!(scrollback, build_bounded_scrollback(10, 1000));
}

#[test]
fn a_buffer_with_drops_round_trips_through_serde_field_for_field() {
    let mut scrollback = build_bounded_scrollback(2, 1000);
    for line_text in ["aa", "bbb", "c"] {
        scrollback.push_row(
            &build_line_cells(line_text),
            RowMetadata {
                row_end: RowEnd::Soft,
                has_prompt_mark: false,
            },
        );
    }
    scrollback.push_row(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
    );

    let serialized_scrollback = serde_json::to_value(&scrollback).expect("scrollback serializes");
    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(restored_scrollback, scrollback);
    assert_eq!(restored_scrollback.maximum_line_count, 2);
    assert_eq!(restored_scrollback.maximum_byte_count, 1000);
    assert_eq!(restored_scrollback.retained_byte_count, 7);
    assert_eq!(restored_scrollback.get_total_pushed_line_count(), 4);
    assert_eq!(restored_scrollback.get_dropped_line_count(), 2);
    assert_eq!(restored_scrollback.get_dropped_byte_count(), 5);
}

#[test]
fn an_empty_buffer_round_trips_through_serde() {
    let scrollback = build_bounded_scrollback(3, 40);
    let serialized_scrollback = serde_json::to_value(&scrollback).expect("scrollback serializes");
    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");
    assert_eq!(restored_scrollback, scrollback);
}

#[test]
fn a_stored_retained_byte_count_that_does_not_match_the_rows_is_recomputed() {
    // The byte total is derived from the rows on the way in, so a stored total
    // below their real size cannot underflow the first eviction.
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row(&build_line_cells("abc"), RowMetadata::default());
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["retained_byte_count"] = serde_json::json!(0);

    let mut restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");
    assert_eq!(restored_scrollback.retained_byte_count, 3);

    restored_scrollback.push_row(&build_line_cells("d"), RowMetadata::default());

    assert_eq!(restored_scrollback.retained_byte_count, 1);
    assert_eq!(list_retained_row_texts(&restored_scrollback), vec!["d"]);
}

#[test]
fn stored_rows_over_the_line_cap_are_dropped_on_the_way_in() {
    // A buffer serialized at a 4-line cap, re-read with the cap edited down to
    // 2: the two oldest rows are dropped at load and tallied, and the load is
    // already within the cap.
    let mut scrollback = build_bounded_scrollback(4, 1_000_000);
    for line_text in ["a", "bb", "ccc", "dddd"] {
        scrollback.push_row(&build_line_cells(line_text), RowMetadata::default());
    }
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["maximum_line_count"] = serde_json::json!(2);

    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(
        list_retained_row_texts(&restored_scrollback),
        vec!["ccc", "dddd"]
    );
    assert_eq!(restored_scrollback.get_retained_line_count(), 2);
    assert_eq!(restored_scrollback.retained_byte_count, 7);
    assert_eq!(restored_scrollback.get_dropped_line_count(), 2);
    assert_eq!(restored_scrollback.get_dropped_byte_count(), 3);
    assert_eq!(restored_scrollback.get_total_pushed_line_count(), 4);
}

#[test]
fn stored_rows_over_the_byte_cap_are_dropped_on_the_way_in() {
    let mut scrollback = build_bounded_scrollback(100, 1_000_000);
    for line_text in ["aaaa", "bbbb", "cc"] {
        scrollback.push_row(&build_line_cells(line_text), RowMetadata::default());
    }
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["maximum_byte_count"] = serde_json::json!(6);

    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(
        list_retained_row_texts(&restored_scrollback),
        vec!["bbbb", "cc"]
    );
    assert_eq!(restored_scrollback.retained_byte_count, 6);
    assert_eq!(restored_scrollback.get_dropped_line_count(), 1);
    assert_eq!(restored_scrollback.get_dropped_byte_count(), 4);
}

#[test]
fn take_retained_lines_empties_the_buffer_and_keeps_the_tallies() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row(&build_line_cells("one"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("two"), RowMetadata::default());

    let taken_retained_lines = scrollback.take_retained_lines();

    let taken_line_texts: Vec<String> = taken_retained_lines
        .iter()
        .map(|(line_cells, _)| line_cells.iter().map(Cell::get_character).collect())
        .collect();
    assert_eq!(taken_line_texts, vec!["one", "two"]);
    assert!(scrollback.is_empty());
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
    assert_eq!(scrollback.get_dropped_byte_count(), 0);

    scrollback.replace_retained_lines(Vec::from(taken_retained_lines), 2);
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
    assert_eq!(list_retained_row_texts(&scrollback), vec!["one", "two"]);
}

#[test]
fn pushes_after_take_retained_lines_start_from_a_zero_retained_byte_count() {
    let mut scrollback = build_bounded_scrollback(10, 10);
    scrollback.push_row(&build_line_cells("12345678"), RowMetadata::default());

    let _ = scrollback.take_retained_lines();

    scrollback.push_row(&build_line_cells("1234"), RowMetadata::default());
    scrollback.push_row(&build_line_cells("1234"), RowMetadata::default());
    assert_eq!(list_retained_row_texts(&scrollback), vec!["1234", "1234"]);
    assert_eq!(scrollback.get_dropped_line_count(), 0);
}
