//! Unit tests for the bounded scrollback buffer: byte accounting, the line and
//! byte caps, oldest-first dropping, history replacement, the stored form,
//! the heap bytes of encoded history lines, and fitting lines to a column
//! count.

use super::*;
use koshi_core::process::PtySize;
use koshi_test_support::output_workloads::{
    build_workload_bytes, OutputWorkload, DEFAULT_WORKLOAD_SEED,
};

use crate::engine::TerminalEngine;
use crate::grid::state::tests::{build_line_cells, build_padded_line_cells};
use crate::grid::state::{Cell, RowEnd, RowMetadata};
use crate::row_encoding::compute_text_byte_count;
use crate::style::Style;

impl Scrollback {
    /// `row_cells` encoded with `row_metadata`, pushed as the newest line.
    pub(crate) fn push_row_cells(&mut self, row_cells: &[Cell], row_metadata: RowMetadata) {
        self.push_row_with_evicted(build_row_snapshot(row_cells, row_metadata), |_| {});
    }
}

/// A buffer bounded by exactly `maximum_line_count` rows and `maximum_byte_count` bytes.
fn build_bounded_scrollback(maximum_line_count: usize, maximum_byte_count: usize) -> Scrollback {
    Scrollback::from_scrollback_limit(ScrollbackLimit {
        maximum_line_count,
        maximum_byte_count,
    })
}

/// `row_cells` encoded with `row_metadata`, as history holds a row.
fn build_row_snapshot(row_cells: &[Cell], row_metadata: RowMetadata) -> Arc<RowSnapshot> {
    Arc::new(RowSnapshot::from_row_cells(
        row_cells,
        row_metadata,
        Vec::new(),
    ))
}

/// Replace the retained rows with `retained_lines`, encoded, with the current
/// retained line count as `retained_line_count_before`.
fn replace_retained_lines(
    scrollback: &mut Scrollback,
    retained_lines: Vec<(Vec<Cell>, RowMetadata)>,
) {
    let retained_line_count_before = scrollback.get_retained_line_count() as u64;
    scrollback.replace_retained_lines(
        retained_lines
            .iter()
            .map(|(row_cells, row_metadata)| build_row_snapshot(row_cells, *row_metadata))
            .collect(),
        retained_line_count_before,
    );
}

/// The base characters of the text cells of every row of `retained_lines`,
/// front (oldest) to back.
pub(crate) fn list_line_texts(retained_lines: &VecDeque<Arc<RowSnapshot>>) -> Vec<String> {
    retained_lines
        .iter()
        .map(|row_snapshot| {
            row_snapshot
                .decode_text_cells()
                .iter()
                .map(Cell::get_character)
                .collect()
        })
        .collect()
}

#[test]
fn a_new_buffer_is_empty() {
    let scrollback = build_bounded_scrollback(10, 1000);
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 0);
}

#[test]
fn a_line_counts_base_and_combining_characters_as_utf8_lengths() {
    // 'a' (1 byte) + '世' (3 bytes) + 'e' carrying a combining acute (1 + 2).
    let mut accented_character_cell = Cell::from_character('e', 1, Style::default());
    accented_character_cell.push_combining('\u{0301}'); // U+0301, two UTF-8 bytes
    let line_cells = vec![
        Cell::from_character('a', 1, Style::default()),
        Cell::from_character('世', 2, Style::default()),
        accented_character_cell,
    ];
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row_cells(&line_cells, RowMetadata::default());
    assert_eq!(scrollback.retained_byte_count, 1 + 3 + (1 + 2));
}

#[test]
fn a_line_counts_no_byte_for_a_wide_glyph_continuation_placeholder() {
    // A wide glyph occupies two cells: a width-2 base carrying '世' (3 bytes)
    // and a width-0 continuation placeholder (a blank space). Only the base
    // carries text; the placeholder adds nothing.
    let line_cells = vec![
        Cell::from_character('世', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ];
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row_cells(&line_cells, RowMetadata::default());
    assert_eq!(scrollback.retained_byte_count, 3); // the space placeholder is skipped
}

#[test]
fn pushing_within_both_caps_retains_every_row_in_order() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row_cells(&build_line_cells("one"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("two"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("three"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 3);
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["one", "two", "three"]
    );
    assert_eq!(scrollback.retained_byte_count, 3 + 3 + 5);
}

#[test]
fn exceeding_the_line_cap_drops_oldest_first() {
    let mut scrollback = build_bounded_scrollback(3, 100_000);
    scrollback.push_row_cells(&build_line_cells("L0"), RowMetadata::default()); // dropped by the fourth push
    scrollback.push_row_cells(&build_line_cells("L1"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("L2"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("L3"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 3);
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["L1", "L2", "L3"]
    );
    assert_eq!(scrollback.retained_byte_count, 6); // three two-byte rows remain
}

#[test]
fn exceeding_the_byte_cap_drops_oldest_until_within_budget() {
    // Four-byte rows, a ten-byte cap: a third row pushes the total to 12 and
    // forces exactly one drop back to 8.
    let mut scrollback = build_bounded_scrollback(100_000, 10);
    scrollback.push_row_cells(&build_line_cells("aaaa"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("bbbb"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("cccc"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 2);
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["bbbb", "cccc"]
    );
    assert_eq!(scrollback.retained_byte_count, 8);
}

#[test]
fn a_lone_row_larger_than_the_byte_cap_is_kept_not_dropped() {
    // The `len > 1` guard means the byte cap never empties the buffer: a single
    // oversized row is retained even though it busts the budget.
    let mut scrollback = build_bounded_scrollback(100_000, 2);
    scrollback.push_row_cells(&build_line_cells("oversized"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(scrollback.retained_byte_count, 9);
}

#[test]
fn a_subsequent_push_drops_the_retained_oversized_row() {
    // With a second row present the guard no longer applies: the oversized
    // row is dropped and the total falls back under the cap.
    let mut scrollback = build_bounded_scrollback(100_000, 2);
    scrollback.push_row_cells(&build_line_cells("oversized"), RowMetadata::default()); // 9 bytes, kept by the guard
    scrollback.push_row_cells(&build_line_cells("x"), RowMetadata::default()); // 1 byte: total 10, len 2 -> drop the front
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(list_line_texts(scrollback.list_retained_lines()), vec!["x"]);
    assert_eq!(scrollback.retained_byte_count, 1);
}

#[test]
fn the_line_cap_can_drop_to_empty_unlike_the_byte_cap() {
    // The line cap has no `len > 1` guard: a zero cap retains nothing.
    let mut scrollback = build_bounded_scrollback(0, 100_000);
    scrollback.push_row_cells(&build_line_cells("gone"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 0);
}

#[test]
fn retained_byte_count_stays_equal_to_the_sum_of_retained_rows() {
    let mut scrollback = build_bounded_scrollback(3, 100_000);
    for line_text in ["alpha", "beta", "gamma", "delta", "epsilon"] {
        scrollback.push_row_cells(&build_line_cells(line_text), RowMetadata::default());
    }
    let expected_retained_byte_count: usize = scrollback
        .list_retained_lines()
        .iter()
        .map(|row_snapshot| {
            compute_text_byte_count(
                &row_snapshot.decode_row_cells(),
                row_snapshot.get_row_metadata().row_end,
            )
        })
        .sum();
    assert_eq!(scrollback.retained_byte_count, expected_retained_byte_count);
}

#[test]
fn a_one_line_cap_keeps_only_the_newest_row() {
    let mut scrollback = build_bounded_scrollback(1, 100_000); // every push past the first drops one row
    scrollback.push_row_cells(&build_line_cells("aa"), RowMetadata::default()); // 2 bytes
    scrollback.push_row_cells(&build_line_cells("bbb"), RowMetadata::default()); // 3 bytes, drops "aa"
    scrollback.push_row_cells(&build_line_cells("c"), RowMetadata::default()); // 1 byte, drops "bbb"
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(list_line_texts(scrollback.list_retained_lines()), vec!["c"]);
}

#[test]
fn clear_scrollback_empties_the_buffer_and_zeroes_the_byte_count() {
    let mut scrollback = build_bounded_scrollback(10, 100_000);
    scrollback.push_row_cells(&build_line_cells("aa"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("bbb"), RowMetadata::default());
    assert_eq!(scrollback.retained_byte_count, 5);

    scrollback.clear_scrollback();
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 0);
}

#[test]
fn total_pushed_line_count_counts_every_push_and_survives_clear_scrollback() {
    let mut scrollback = build_bounded_scrollback(2, 1000); // line cap 2: a push that drops still counts
    scrollback.push_row_cells(&build_line_cells("a"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("b"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("c"), RowMetadata::default()); // drops "a"; the push itself still counts
    assert_eq!(scrollback.get_total_pushed_line_count(), 3);

    scrollback.clear_scrollback();
    assert_eq!(scrollback.get_total_pushed_line_count(), 3); // an erase never rewinds the counter

    scrollback.push_row_cells(&build_line_cells("d"), RowMetadata::default());
    assert_eq!(scrollback.get_total_pushed_line_count(), 4);
}

#[test]
fn a_hard_row_counts_only_its_text_and_decodes_to_its_padded_width() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(
        &build_padded_line_cells("README.md", 200),
        RowMetadata::default(),
    );

    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["README.md".to_string()]
    );
    assert_eq!(scrollback.retained_byte_count, 9);
    assert_eq!(
        scrollback.list_retained_lines()[0].decode_row_cells(),
        build_padded_line_cells("README.md", 200)
    );
}

#[test]
fn a_padded_hard_row_stores_its_blanks_as_one_count() {
    // 200 columns, a hard end, 198 trailing blanks, the default style, and one
    // run holding `hi`.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(&build_padded_line_cells("hi", 200), RowMetadata::default());

    assert_eq!(
        scrollback.list_retained_lines()[0].get_encoded_row_bytes(),
        [
            0x12, 0xc8, 0x01, 0x00, 0xc6, 0x01, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x02, 0x02, b'h', b'i', 0x00,
        ]
    );
}

#[test]
fn a_soft_wrapped_row_keeps_every_cell() {
    // Every cell of a soft-wrapped row is content, trailing blanks included.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(
        &build_padded_line_cells("ab", 6),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );

    assert_eq!(
        scrollback.list_retained_lines()[0].decode_text_cells(),
        build_padded_line_cells("ab", 6)
    );
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer() {
    // The final blank stands in for the wide glyph that starts the next row.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(
        &build_padded_line_cells("ab", 6),
        RowMetadata {
            row_end: RowEnd::SoftWide,
            has_prompt_mark: false,
        },
    );

    assert_eq!(
        scrollback.list_retained_lines()[0].decode_text_cells(),
        build_padded_line_cells("ab", 6)
    );
}

#[test]
fn a_background_colored_blank_is_content_and_survives() {
    // A prompt segment painting color into blank cells: the colored cells are
    // content and stay.
    let mut background_style = Style::default();
    background_style.set_background_color(crate::style::Color::Indexed(1));
    let mut line_cells = build_line_cells("ab");
    line_cells.push(Cell::build_blank_with_style(background_style));
    line_cells.resize(200, Cell::build_blank());

    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(&line_cells, RowMetadata::default());

    let mut expected_line_cells = build_line_cells("ab");
    expected_line_cells.push(Cell::build_blank_with_style(background_style));
    assert_eq!(
        scrollback.list_retained_lines()[0].decode_text_cells(),
        expected_line_cells
    );
    assert_eq!(scrollback.retained_byte_count, 3);
}

#[test]
fn a_wide_glyphs_continuation_cell_is_content_and_survives() {
    // The zero-width right half of a CJK glyph is not a default blank: the
    // line holds it as a text cell.
    let mut line_cells = vec![
        Cell::from_character('漢', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ];
    line_cells.resize(200, Cell::build_blank());

    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(&line_cells, RowMetadata::default());

    assert_eq!(
        scrollback.list_retained_lines()[0].decode_text_cells(),
        vec![
            Cell::from_character('漢', 2, Style::default()),
            Cell::from_character(' ', 0, Style::default()),
        ]
    );
    assert_eq!(scrollback.retained_byte_count, 3);
}

#[test]
fn a_row_of_nothing_but_padding_counts_no_byte_and_is_still_a_line() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(&build_padded_line_cells("", 200), RowMetadata::default());

    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec![String::new()]
    );
    assert_eq!(scrollback.retained_byte_count, 0);
    assert_eq!(scrollback.get_retained_line_count(), 1); // the blank line itself is still a line
}

#[test]
fn a_reflow_rebuild_counts_bytes_the_same_way_a_push_does() {
    // A replacement counts a hard-ended row without its padding, the same way
    // a push does, and a soft row whole.
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_lines(
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

    assert_eq!(scrollback.retained_byte_count, 3 + 200);
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["one".to_string(), format!("ab{}", " ".repeat(198))]
    );
}

#[test]
fn the_byte_cap_counts_only_the_text_of_a_padded_hard_row() {
    // The cap counts retained characters: a 200-column row of `hi` charges 2,
    // and ten of them fit a cap of 20.
    let mut scrollback = build_bounded_scrollback(1000, 20);
    for _push_number in 1..=10 {
        scrollback.push_row_cells(&build_padded_line_cells("hi", 200), RowMetadata::default());
    }
    assert_eq!(scrollback.get_retained_line_count(), 10);
    assert_eq!(scrollback.retained_byte_count, 20);
}

#[test]
fn prompt_marks_stay_with_rows_through_history_replacement() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_lines(
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

    assert!(
        scrollback.list_retained_lines()[0]
            .get_row_metadata()
            .has_prompt_mark
    );
    assert!(
        !scrollback.list_retained_lines()[1]
            .get_row_metadata()
            .has_prompt_mark
    );
}

#[test]
fn prompt_marks_are_evicted_with_their_rows() {
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row_cells(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
    );
    scrollback.push_row_cells(
        &build_line_cells("output"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: false,
        },
    );

    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["output"]
    );
    assert!(
        !scrollback.list_retained_lines()[0]
            .get_row_metadata()
            .has_prompt_mark
    );
}

#[test]
fn current_scrollback_rows_round_trip_with_prompt_metadata() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(
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
        restored_scrollback.list_retained_lines()[0].get_row_metadata(),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        }
    );
    assert_eq!(restored_scrollback, scrollback);
}

#[test]
fn a_stored_row_whose_metadata_names_another_row_end_than_its_bytes_is_refused() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: true,
        },
    );
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["retained_lines"][0]["row_metadata"]["row_end"] =
        serde_json::json!("Hard");

    let parse_error = serde_json::from_value::<Scrollback>(serialized_scrollback)
        .expect_err("a row whose metadata disagrees with its bytes is refused");

    assert_eq!(
        parse_error.to_string(),
        "the stored row's metadata names another row end"
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
fn pushing_an_empty_row_counts_a_line_of_zero_bytes() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row_cells(&[], RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 1);
    assert_eq!(scrollback.retained_byte_count, 0);
    assert_eq!(scrollback.get_total_pushed_line_count(), 1);
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec![String::new()]
    );
}

#[test]
fn a_retained_byte_count_exactly_at_the_cap_keeps_every_row() {
    let mut scrollback = build_bounded_scrollback(100, 8);
    scrollback.push_row_cells(&build_line_cells("aaaa"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("bbbb"), RowMetadata::default()); // total 8: at the cap, not past it
    assert_eq!(scrollback.get_retained_line_count(), 2);
    assert_eq!(scrollback.retained_byte_count, 8);

    scrollback.push_row_cells(&build_line_cells("c"), RowMetadata::default()); // total 9: one past, drops "aaaa"
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["bbbb", "c"]
    );
    assert_eq!(scrollback.retained_byte_count, 5);
}

#[test]
fn a_row_count_exactly_at_the_line_cap_keeps_every_row() {
    let mut scrollback = build_bounded_scrollback(2, 1000);
    scrollback.push_row_cells(&build_line_cells("a"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("b"), RowMetadata::default());
    assert_eq!(scrollback.get_retained_line_count(), 2);
}

#[test]
fn one_push_can_drop_for_the_line_cap_and_then_the_byte_cap() {
    let mut scrollback = build_bounded_scrollback(2, 2);
    scrollback.push_row_cells(&build_line_cells("a"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("b"), RowMetadata::default()); // len 2, bytes 2: both caps hold
    scrollback.push_row_cells(&build_line_cells("cc"), RowMetadata::default()); // len 3 drops "a"; bytes 3 then drops "b"
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["cc"]
    );
    assert_eq!(scrollback.retained_byte_count, 2);
}

#[test]
fn a_soft_row_charges_its_padding_against_the_byte_cap() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(
        &build_padded_line_cells("ab", 6),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );
    assert_eq!(scrollback.retained_byte_count, 6);
}

#[test]
fn replacing_with_fewer_rows_leaves_total_pushed_line_count_unchanged() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    for line_text in ["one", "two", "three"] {
        scrollback.push_row_cells(&build_line_cells(line_text), RowMetadata::default());
    }
    replace_retained_lines(
        &mut scrollback,
        vec![(build_line_cells("only"), RowMetadata::default())],
    );
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["only"]
    );
    assert_eq!(scrollback.retained_byte_count, 4);
    assert_eq!(scrollback.get_total_pushed_line_count(), 3);
}

#[test]
fn replacing_with_more_rows_grows_total_pushed_line_count_by_the_difference() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    scrollback.push_row_cells(&build_line_cells("one"), RowMetadata::default());
    replace_retained_lines(
        &mut scrollback,
        vec![
            (build_line_cells("a"), RowMetadata::default()),
            (build_line_cells("b"), RowMetadata::default()),
            (build_line_cells("c"), RowMetadata::default()),
        ],
    );
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["a", "b", "c"]
    );
    assert_eq!(scrollback.retained_byte_count, 3);
    assert_eq!(scrollback.get_total_pushed_line_count(), 3);
}

#[test]
fn replacing_past_the_line_cap_evicts_the_oldest() {
    let mut scrollback = build_bounded_scrollback(2, 1_000_000);
    replace_retained_lines(
        &mut scrollback,
        vec![
            (build_line_cells("aa"), RowMetadata::default()),
            (build_line_cells("bbb"), RowMetadata::default()),
            (build_line_cells("c"), RowMetadata::default()),
            (build_line_cells("dd"), RowMetadata::default()),
        ],
    );
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["c", "dd"]
    );
    assert_eq!(scrollback.retained_byte_count, 3);
    // The increase is counted after eviction: two rows retained, none before.
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
}

#[test]
fn replacing_past_the_byte_cap_keeps_the_newest_rows_within_budget() {
    let mut scrollback = build_bounded_scrollback(100, 4);
    replace_retained_lines(
        &mut scrollback,
        vec![
            (build_line_cells("aaa"), RowMetadata::default()),
            (build_line_cells("bb"), RowMetadata::default()),
            (build_line_cells("cc"), RowMetadata::default()),
        ],
    );
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["bb", "cc"]
    );
    assert_eq!(scrollback.retained_byte_count, 4);
}

#[test]
fn replacing_with_nothing_empties_the_buffer_and_keeps_the_counters() {
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row_cells(&build_line_cells("aa"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("bb"), RowMetadata::default()); // drops "aa"
    replace_retained_lines(&mut scrollback, Vec::new());
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.retained_byte_count, 0);
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer_through_replace_retained_lines() {
    let mut scrollback = build_bounded_scrollback(10, 1_000_000);
    replace_retained_lines(
        &mut scrollback,
        vec![(
            build_padded_line_cells("ab", 6),
            RowMetadata {
                row_end: RowEnd::SoftWide,
                has_prompt_mark: false,
            },
        )],
    );

    assert_eq!(
        scrollback.list_retained_lines()[0].decode_text_cells(),
        build_padded_line_cells("ab", 6)
    );
    assert_eq!(
        scrollback.list_retained_lines()[0]
            .get_row_metadata()
            .row_end,
        RowEnd::SoftWide
    );
}

#[test]
fn a_push_after_clear_scrollback_starts_the_retained_byte_count_from_zero() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row_cells(&build_line_cells("abc"), RowMetadata::default());
    scrollback.clear_scrollback();
    scrollback.push_row_cells(&build_line_cells("de"), RowMetadata::default());
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["de"]
    );
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
        scrollback.push_row_cells(
            &build_line_cells(line_text),
            RowMetadata {
                row_end: RowEnd::Soft,
                has_prompt_mark: false,
            },
        );
    }
    scrollback.push_row_cells(
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
fn a_stored_buffer_carrying_a_dropped_line_count_key_still_reads() {
    let mut scrollback = build_bounded_scrollback(3, 1000);
    scrollback.push_row_cells(&build_line_cells("ab"), RowMetadata::default());
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["dropped_line_count"] = serde_json::json!(7);

    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(restored_scrollback, scrollback);
}

#[test]
fn a_stored_retained_byte_count_that_does_not_match_the_rows_is_recomputed() {
    // The byte total is derived from the rows on the way in: a stored total
    // of 0 reads back as the rows' 3 bytes, and the first eviction subtracts
    // from 3.
    let mut scrollback = build_bounded_scrollback(1, 1_000_000);
    scrollback.push_row_cells(&build_line_cells("abc"), RowMetadata::default());
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["retained_byte_count"] = serde_json::json!(0);

    let mut restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");
    assert_eq!(restored_scrollback.retained_byte_count, 3);

    restored_scrollback.push_row_cells(&build_line_cells("d"), RowMetadata::default());

    assert_eq!(restored_scrollback.retained_byte_count, 1);
    assert_eq!(
        list_line_texts(restored_scrollback.list_retained_lines()),
        vec!["d"]
    );
}

#[test]
fn stored_rows_over_the_line_cap_are_dropped_on_the_way_in() {
    // A buffer serialized at a 4-line cap, re-read with the cap edited down to
    // 2: the two oldest rows are dropped at load, and the load is already
    // within the cap.
    let mut scrollback = build_bounded_scrollback(4, 1_000_000);
    for line_text in ["a", "bb", "ccc", "dddd"] {
        scrollback.push_row_cells(&build_line_cells(line_text), RowMetadata::default());
    }
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["maximum_line_count"] = serde_json::json!(2);

    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(
        list_line_texts(restored_scrollback.list_retained_lines()),
        vec!["ccc", "dddd"]
    );
    assert_eq!(restored_scrollback.get_retained_line_count(), 2);
    assert_eq!(restored_scrollback.retained_byte_count, 7);
    assert_eq!(restored_scrollback.get_total_pushed_line_count(), 4);
}

#[test]
fn stored_rows_over_the_byte_cap_are_dropped_on_the_way_in() {
    let mut scrollback = build_bounded_scrollback(100, 1_000_000);
    for line_text in ["aaaa", "bbbb", "cc"] {
        scrollback.push_row_cells(&build_line_cells(line_text), RowMetadata::default());
    }
    let mut serialized_scrollback =
        serde_json::to_value(&scrollback).expect("scrollback serializes");
    serialized_scrollback["maximum_byte_count"] = serde_json::json!(6);

    let restored_scrollback: Scrollback =
        serde_json::from_value(serialized_scrollback).expect("scrollback deserializes");

    assert_eq!(
        list_line_texts(restored_scrollback.list_retained_lines()),
        vec!["bbbb", "cc"]
    );
    assert_eq!(restored_scrollback.retained_byte_count, 6);
}

#[test]
fn take_retained_lines_empties_the_buffer_and_keeps_total_pushed_line_count() {
    let mut scrollback = build_bounded_scrollback(10, 1000);
    scrollback.push_row_cells(&build_line_cells("one"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("two"), RowMetadata::default());

    let taken_retained_lines = scrollback.take_retained_lines();

    assert_eq!(list_line_texts(&taken_retained_lines), vec!["one", "two"]);
    assert_eq!(scrollback.get_retained_line_count(), 0);
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);

    scrollback.replace_retained_lines(Vec::from(taken_retained_lines), 2);
    assert_eq!(scrollback.get_total_pushed_line_count(), 2);
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["one", "two"]
    );
}

#[test]
fn pushes_after_take_retained_lines_start_from_a_zero_retained_byte_count() {
    let mut scrollback = build_bounded_scrollback(10, 10);
    scrollback.push_row_cells(&build_line_cells("12345678"), RowMetadata::default());

    let _ = scrollback.take_retained_lines();

    scrollback.push_row_cells(&build_line_cells("1234"), RowMetadata::default());
    scrollback.push_row_cells(&build_line_cells("1234"), RowMetadata::default());
    assert_eq!(
        list_line_texts(scrollback.list_retained_lines()),
        vec!["1234", "1234"]
    );
}

/// The heap bytes one history line costs: its slot in the line queue, the
/// `Arc` allocation that holds the strong count, the weak count and the
/// snapshot, and the snapshot's own heap bytes.
fn compute_history_line_heap_byte_count(row_snapshot: &RowSnapshot) -> usize {
    std::mem::size_of::<Arc<RowSnapshot>>()
        + 2 * std::mem::size_of::<usize>()
        + std::mem::size_of::<RowSnapshot>()
        + row_snapshot.compute_heap_byte_count()
}

/// The heap bytes the same line costs as cells: its slot in a queue of
/// `(Vec<Cell>, RowMetadata)` lines and one 32-byte cell per stored column,
/// a hard-ended line without its trailing default blanks.
fn compute_history_line_cell_byte_count(row_snapshot: &RowSnapshot) -> usize {
    std::mem::size_of::<(Vec<Cell>, RowMetadata)>()
        + row_snapshot.decode_text_cells().len() * std::mem::size_of::<Cell>()
}

#[test]
fn ten_thousand_history_lines_take_a_quarter_of_their_cell_bytes_and_a_tenth_on_plain() {
    for workload in OutputWorkload::ALL {
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 200,
            row_count: 50,
        });
        let mut workload_seed = DEFAULT_WORKLOAD_SEED;
        while terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count()
            < 10_000
        {
            // A `fullscreen` repaint scrolls into history after 50 line
            // feeds, and a `progress` update after `\r\n`.
            let mut workload_bytes = match workload {
                OutputWorkload::Fullscreen | OutputWorkload::Progress => {
                    build_workload_bytes(workload, 1, workload_seed)
                }
                _ => build_workload_bytes(workload, 64 * 1024, workload_seed),
            };
            match workload {
                OutputWorkload::Fullscreen => workload_bytes.extend_from_slice(&[b'\n'; 50]),
                OutputWorkload::Progress => workload_bytes.extend_from_slice(b"\r\n"),
                _ => {}
            }
            let _ = terminal_engine.process_pty_output(&workload_bytes);
            workload_seed += 1;
        }

        let retained_lines = terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines();
        let history_heap_byte_count: usize = retained_lines
            .iter()
            .map(|row_snapshot| compute_history_line_heap_byte_count(row_snapshot))
            .sum();
        let history_cell_byte_count: usize = retained_lines
            .iter()
            .map(|row_snapshot| compute_history_line_cell_byte_count(row_snapshot))
            .sum();
        let share_divisor = match workload {
            OutputWorkload::Plain => 10,
            _ => 4,
        };
        assert_eq!(retained_lines.len(), 10_000);
        assert!(
            history_heap_byte_count * share_divisor <= history_cell_byte_count,
            "{}: {history_heap_byte_count} heap bytes against {history_cell_byte_count} cell bytes",
            workload.get_workload_name()
        );
    }
}

#[test]
fn fitting_lines_pads_a_short_line_cuts_a_long_one_and_keeps_a_fitting_one() {
    let mut scrollback = build_bounded_scrollback(10, 1_000);
    let soft_row_metadata = RowMetadata {
        row_end: RowEnd::Soft,
        has_prompt_mark: false,
    };
    replace_retained_lines(
        &mut scrollback,
        vec![
            (build_line_cells("abc"), RowMetadata::default()),
            (build_line_cells("abcdefg"), soft_row_metadata),
            (build_line_cells("vwxyz"), RowMetadata::default()),
        ],
    );
    let fitting_line = Arc::clone(&scrollback.list_retained_lines()[2]);

    scrollback.fit_lines_to_column_count(5);

    assert_eq!(
        scrollback.list_retained_lines(),
        &VecDeque::from([
            build_row_snapshot(&build_padded_line_cells("abc", 5), RowMetadata::default()),
            build_row_snapshot(&build_line_cells("abcde"), soft_row_metadata),
            build_row_snapshot(&build_line_cells("vwxyz"), RowMetadata::default()),
        ])
    );
    assert!(Arc::ptr_eq(
        &scrollback.list_retained_lines()[2],
        &fitting_line
    ));
    assert_eq!(scrollback.retained_byte_count, 3 + 5 + 5);
    assert!(scrollback.has_only_lines_of_column_count(5));
    assert!(!scrollback.has_only_lines_of_column_count(4));
}

#[test]
fn fitting_lines_blanks_a_wide_glyph_whose_right_half_falls_past_the_column_count() {
    let mut scrollback = build_bounded_scrollback(10, 1_000);
    let soft_row_metadata = RowMetadata {
        row_end: RowEnd::Soft,
        has_prompt_mark: false,
    };
    let mut wide_line_cells = build_line_cells("abcd");
    wide_line_cells.push(Cell::from_character('漢', 2, Style::default()));
    wide_line_cells.push(Cell::from_character(' ', 0, Style::default()));
    replace_retained_lines(&mut scrollback, vec![(wide_line_cells, soft_row_metadata)]);

    scrollback.fit_lines_to_column_count(5);

    assert_eq!(
        scrollback.list_retained_lines(),
        &VecDeque::from([build_row_snapshot(
            &build_padded_line_cells("abcd", 5),
            soft_row_metadata
        )])
    );
    assert_eq!(scrollback.retained_byte_count, 5);
}

#[test]
fn a_format_4_history_line_reads_up_to_65535_cells_and_refuses_one_more() {
    let build_stored_history = |cell_count: usize| {
        serde_json::json!({
            "retained_lines": [(vec![Cell::build_blank(); cell_count], RowMetadata::default())],
            "maximum_line_count": 10,
            "maximum_byte_count": 100,
            "total_pushed_line_count": 1,
        })
    };

    let read_scrollback = serde_json::from_value::<CellRowScrollback>(build_stored_history(65_535))
        .expect("a line of 65 535 cells reads")
        .0;
    let history_read_error =
        serde_json::from_value::<CellRowScrollback>(build_stored_history(65_536))
            .map(|cell_row_scrollback| cell_row_scrollback.0)
            .expect_err("a line of 65 536 cells is refused");

    assert_eq!(read_scrollback.get_retained_line_count(), 1);
    assert!(read_scrollback.has_only_lines_of_column_count(65_535));
    assert_eq!(
        history_read_error.to_string(),
        "a history line holds more than 65 535 cells"
    );
}
