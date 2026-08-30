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
        .map(|(row, _)| row.iter().map(Cell::ch).collect())
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
fn line_bytes_skips_wide_glyph_continuation_placeholders() {
    let sb = bounded(10, 1000);
    // A wide glyph occupies two cells: a width-2 base carrying '世' (3 bytes)
    // and a width-0 continuation placeholder (a blank space). Only the base
    // carries text; the placeholder adds nothing.
    let row = vec![
        Cell::new('世', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
    ];
    assert_eq!(sb.line_bytes(&row), 3); // the space placeholder is skipped
}

#[test]
fn pushing_within_both_caps_retains_every_row_in_order() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&line("one"), RowEnd::Hard);
    sb.push_row(&line("two"), RowEnd::Hard);
    sb.push_row(&line("three"), RowEnd::Hard);
    assert_eq!(sb.len(), 3);
    assert_eq!(retained(&sb), vec!["one", "two", "three"]);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.dropped_bytes(), 0);
    assert_eq!(sb.byte_total, 3 + 3 + 5);
}

#[test]
fn exceeding_the_line_cap_drops_oldest_first() {
    let mut sb = bounded(3, 100_000);
    sb.push_row(&line("L0"), RowEnd::Hard); // dropped by the fourth push
    sb.push_row(&line("L1"), RowEnd::Hard);
    sb.push_row(&line("L2"), RowEnd::Hard);
    sb.push_row(&line("L3"), RowEnd::Hard);
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
    sb.push_row(&line("aaaa"), RowEnd::Hard);
    sb.push_row(&line("bbbb"), RowEnd::Hard);
    sb.push_row(&line("cccc"), RowEnd::Hard);
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
    sb.push_row(&line("oversized"), RowEnd::Hard);
    assert_eq!(sb.len(), 1);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.byte_total, 9);
}

#[test]
fn a_later_push_drops_the_retained_oversized_row() {
    // With a second row present the guard no longer applies: the oversized
    // row is dropped and the total falls back under the cap.
    let mut sb = bounded(100_000, 2);
    sb.push_row(&line("oversized"), RowEnd::Hard); // 9 bytes, kept by the guard
    sb.push_row(&line("x"), RowEnd::Hard); // 1 byte: total 10, len 2 -> drop the front
    assert_eq!(sb.len(), 1);
    assert_eq!(retained(&sb), vec!["x"]);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 9);
    assert_eq!(sb.byte_total, 1);
}

#[test]
fn the_line_cap_can_drop_to_empty_unlike_the_byte_cap() {
    // The line cap has no `len > 1` guard: a zero cap retains nothing.
    let mut sb = bounded(0, 100_000);
    sb.push_row(&line("gone"), RowEnd::Hard);
    assert!(sb.is_empty());
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 4);
    assert_eq!(sb.byte_total, 0);
}

#[test]
fn byte_total_stays_equal_to_the_sum_of_retained_rows() {
    let mut sb = bounded(3, 100_000);
    for s in ["alpha", "beta", "gamma", "delta", "epsilon"] {
        sb.push_row(&line(s), RowEnd::Hard);
    }
    let expected: usize = sb.lines().iter().map(|(row, _)| sb.line_bytes(row)).sum();
    assert_eq!(sb.byte_total, expected);
}

#[test]
fn dropped_tallies_accumulate_across_many_drops() {
    let mut sb = bounded(1, 100_000); // every push past the first drops one row
    sb.push_row(&line("aa"), RowEnd::Hard); // 2 bytes
    sb.push_row(&line("bbb"), RowEnd::Hard); // 3 bytes, drops "aa"
    sb.push_row(&line("c"), RowEnd::Hard); // 1 byte, drops "bbb"
    assert_eq!(sb.len(), 1);
    assert_eq!(retained(&sb), vec!["c"]);
    assert_eq!(sb.dropped_lines(), 2);
    assert_eq!(sb.dropped_bytes(), 5);
}

#[test]
fn clear_empties_the_buffer_but_keeps_the_drop_tallies() {
    let mut sb = bounded(1, 100_000); // line cap of 1 forces a drop
    sb.push_row(&line("aa"), RowEnd::Hard);
    sb.push_row(&line("bbb"), RowEnd::Hard); // drops "aa": dropped_lines 1, dropped_bytes 2
    assert_eq!(sb.dropped_lines(), 1);

    sb.clear();
    assert!(sb.is_empty());
    assert_eq!(sb.len(), 0);
    assert_eq!(sb.byte_total, 0);
    // An explicit erase leaves the tallies as they were.
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn total_pushed_counts_every_push_and_survives_a_clear() {
    let mut sb = bounded(2, 1000); // line cap 2: a push that drops still counts
    sb.push_row(&line("a"), RowEnd::Hard);
    sb.push_row(&line("b"), RowEnd::Hard);
    sb.push_row(&line("c"), RowEnd::Hard); // drops "a"; the push itself still counts
    assert_eq!(sb.total_pushed(), 3);

    sb.clear();
    assert_eq!(sb.total_pushed(), 3); // an erase never rewinds the counter

    sb.push_row(&line("d"), RowEnd::Hard);
    assert_eq!(sb.total_pushed(), 4);
}

/// A row of `text` padded out to `width` with default blanks, the shape every
/// row arrives in from the screen.
fn padded(text: &str, width: usize) -> Vec<Cell> {
    let mut row = line(text);
    row.resize(width, Cell::blank());
    row
}

#[test]
fn a_hard_row_drops_the_blanks_padding_it_out_to_the_screen_width() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("README.md", 200), RowEnd::Hard);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 9);
    assert_eq!(retained(&sb), vec!["README.md".to_string()]);
}

#[test]
fn a_trimmed_row_releases_the_memory_and_does_not_just_hide_it() {
    // `Vec::truncate` keeps the capacity the padding needed; the trim releases
    // it. `Vec` promises a capacity of at least the length, never an exact
    // figure: the check is a bound, not an equality.
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("hi", 200), RowEnd::Hard);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 2);
    assert!(
        stored.capacity() < 200,
        "the 200-cell allocation was kept: capacity {}",
        stored.capacity()
    );
}

#[test]
fn a_soft_wrapped_row_keeps_every_cell() {
    // Every cell of a soft-wrapped row is content, trailing blanks included.
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("ab", 6), RowEnd::Soft);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 6);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer() {
    // The final blank stands in for the wide glyph that starts the next row.
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("ab", 6), RowEnd::SoftWide);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 6);
}

#[test]
fn a_background_colored_blank_is_content_and_survives() {
    // A prompt segment painting color into blank cells: the colored cells are
    // content and stay.
    let mut red = Style::default();
    red.set_bg(crate::style::Color::Indexed(1));
    let mut row = line("ab");
    row.push(Cell::blank_with(red));
    row.resize(200, Cell::blank());

    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&row, RowEnd::Hard);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[2].style().bg(), crate::style::Color::Indexed(1));
}

#[test]
fn a_wide_glyphs_continuation_cell_is_content_and_survives() {
    // The zero-width right half of a CJK glyph is not a default blank: the
    // trim keeps it.
    let mut row = vec![
        Cell::new('漢', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
    ];
    row.resize(200, Cell::blank());

    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&row, RowEnd::Hard);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].width(), 0);
}

#[test]
fn a_row_of_nothing_but_padding_stores_no_cells() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("", 200), RowEnd::Hard);

    let (stored, _) = &sb.lines()[0];
    assert!(stored.is_empty());
    assert_eq!(sb.len(), 1); // the blank line itself is still a line
}

#[test]
fn a_reflow_rebuild_trims_the_same_way_a_push_does() {
    // A replacement trims a hard row the same way a push does.
    let mut sb = bounded(10, 1_000_000);
    sb.replace_lines(vec![
        (padded("one", 200), RowEnd::Hard),
        (padded("ab", 200), RowEnd::Soft),
    ]);

    let (hard, _) = &sb.lines()[0];
    let (soft, _) = &sb.lines()[1];
    assert_eq!(hard.len(), 3);
    assert_eq!(soft.len(), 200);
}

#[test]
fn a_reflow_rebuild_releases_the_memory_the_padding_held() {
    // A replacement releases the padding's capacity the same way a push does.
    // `Vec` never promises a capacity equal to the length: the check is a
    // bound, not an equality.
    let mut sb = bounded(10, 1_000_000);
    sb.replace_lines(vec![(padded("hi", 200), RowEnd::Hard)]);

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 2);
    assert!(
        stored.capacity() < 200,
        "the 200-cell allocation was kept: capacity {}",
        stored.capacity()
    );
}

#[test]
fn trimming_lets_the_byte_cap_hold_the_text_it_was_set_for() {
    // The cap counts stored characters: a 200-column row of `hi` charges 2,
    // and ten of them fit a cap of 20.
    let mut sb = bounded(1000, 20);
    for _ in 0..10 {
        sb.push_row(&padded("hi", 200), RowEnd::Hard);
    }
    assert_eq!(sb.len(), 10);
    assert_eq!(sb.byte_total, 20);
    assert_eq!(sb.dropped_lines(), 0);
}

#[test]
fn prompt_marks_stay_with_rows_through_history_replacement() {
    let mut sb = bounded(10, 1_000_000);
    sb.replace_lines_with_meta(vec![
        (
            line("prompt"),
            RowMeta {
                end: RowEnd::Hard,
                prompt: true,
            },
        ),
        (
            line("output"),
            RowMeta {
                end: RowEnd::Hard,
                prompt: false,
            },
        ),
    ]);

    assert!(sb.lines()[0].1.prompt);
    assert!(!sb.lines()[1].1.prompt);
}

#[test]
fn prompt_marks_are_evicted_with_their_rows() {
    let mut sb = bounded(1, 1_000_000);
    sb.push_row_with_meta(
        &line("prompt"),
        RowMeta {
            end: RowEnd::Hard,
            prompt: true,
        },
    );
    sb.push_row_with_meta(
        &line("output"),
        RowMeta {
            end: RowEnd::Hard,
            prompt: false,
        },
    );

    assert_eq!(retained(&sb), vec!["output"]);
    assert!(!sb.lines()[0].1.prompt);
}

#[test]
fn current_scrollback_rows_round_trip_with_prompt_metadata() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row_with_meta(
        &line("prompt"),
        RowMeta {
            end: RowEnd::Hard,
            prompt: true,
        },
    );

    let value = serde_json::to_value(&sb).expect("scrollback serializes");
    let restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");

    assert_eq!(restored.lines()[0].1.end, RowEnd::Hard);
    assert!(restored.lines()[0].1.prompt);
}

#[test]
fn legacy_scrollback_rows_deserialize_as_unmarked() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row_with_meta(
        &line("prompt"),
        RowMeta {
            end: RowEnd::Soft,
            prompt: true,
        },
    );
    let mut value = serde_json::to_value(&sb).expect("scrollback serializes");
    let object = value.as_object_mut().expect("scrollback is an object");
    let lines = object
        .get_mut("lines")
        .and_then(serde_json::Value::as_array_mut)
        .expect("scrollback lines are an array");
    for line in lines {
        let metadata = line
            .as_array_mut()
            .expect("serialized row is an array")
            .pop()
            .expect("serialized row has metadata");
        let end = metadata["end"].clone();
        line.as_array_mut()
            .expect("serialized row is an array")
            .push(end);
    }

    let restored: Scrollback =
        serde_json::from_value(value).expect("legacy scrollback deserializes");

    assert_eq!(restored.lines()[0].1.end, RowEnd::Soft);
    assert!(!restored.lines()[0].1.prompt);
}

#[test]
fn the_default_limit_is_ten_thousand_lines_and_thirty_two_mebibytes() {
    let sb = Scrollback::new(ScrollbackLimit::default());
    assert_eq!(sb.max_lines, 10_000);
    assert_eq!(sb.max_bytes, 32 * 1024 * 1024);
}

#[test]
fn new_copies_both_caps_from_the_limit() {
    let sb = Scrollback::new(ScrollbackLimit::new(7, 99));
    assert_eq!(sb.max_lines, 7);
    assert_eq!(sb.max_bytes, 99);
    assert_eq!(sb.total_pushed(), 0);
}

#[test]
fn line_bytes_of_an_empty_row_is_zero() {
    let sb = bounded(10, 1000);
    assert_eq!(sb.line_bytes(&[]), 0);
}

#[test]
fn pushing_an_empty_row_counts_a_line_of_zero_bytes() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&[], RowEnd::Hard);
    assert_eq!(sb.len(), 1);
    assert_eq!(sb.byte_total, 0);
    assert_eq!(sb.total_pushed(), 1);
    assert_eq!(retained(&sb), vec![String::new()]);
}

#[test]
fn a_byte_total_exactly_at_the_cap_keeps_every_row() {
    let mut sb = bounded(100, 8);
    sb.push_row(&line("aaaa"), RowEnd::Hard);
    sb.push_row(&line("bbbb"), RowEnd::Hard); // total 8: at the cap, not past it
    assert_eq!(sb.len(), 2);
    assert_eq!(sb.byte_total, 8);
    assert_eq!(sb.dropped_lines(), 0);

    sb.push_row(&line("c"), RowEnd::Hard); // total 9: one past, drops "aaaa"
    assert_eq!(retained(&sb), vec!["bbbb", "c"]);
    assert_eq!(sb.byte_total, 5);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 4);
}

#[test]
fn a_row_count_exactly_at_the_line_cap_keeps_every_row() {
    let mut sb = bounded(2, 1000);
    sb.push_row(&line("a"), RowEnd::Hard);
    sb.push_row(&line("b"), RowEnd::Hard);
    assert_eq!(sb.len(), 2);
    assert_eq!(sb.dropped_lines(), 0);
}

#[test]
fn one_push_can_drop_for_the_line_cap_and_then_the_byte_cap() {
    let mut sb = bounded(2, 2);
    sb.push_row(&line("a"), RowEnd::Hard);
    sb.push_row(&line("b"), RowEnd::Hard); // len 2, bytes 2: both caps hold
    sb.push_row(&line("cc"), RowEnd::Hard); // len 3 drops "a"; bytes 3 then drops "b"
    assert_eq!(retained(&sb), vec!["cc"]);
    assert_eq!(sb.byte_total, 2);
    assert_eq!(sb.dropped_lines(), 2);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn a_soft_row_charges_its_padding_against_the_byte_cap() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("ab", 6), RowEnd::Soft);
    assert_eq!(sb.byte_total, 6);
}

#[test]
fn dropped_bytes_count_the_stored_row_not_the_screen_row() {
    let mut sb = bounded(1, 1_000_000);
    sb.push_row(&padded("hi", 200), RowEnd::Hard);
    sb.push_row(&line("x"), RowEnd::Hard); // drops the trimmed "hi"
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn replacing_with_fewer_rows_leaves_total_pushed_unchanged() {
    let mut sb = bounded(10, 1_000_000);
    for s in ["one", "two", "three"] {
        sb.push_row(&line(s), RowEnd::Hard);
    }
    sb.replace_lines(vec![(line("only"), RowEnd::Hard)]);
    assert_eq!(retained(&sb), vec!["only"]);
    assert_eq!(sb.byte_total, 4);
    assert_eq!(sb.total_pushed(), 3);
    assert_eq!(sb.dropped_lines(), 0);
}

#[test]
fn replacing_with_more_rows_grows_total_pushed_by_the_difference() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&line("one"), RowEnd::Hard);
    sb.replace_lines(vec![
        (line("a"), RowEnd::Hard),
        (line("b"), RowEnd::Hard),
        (line("c"), RowEnd::Hard),
    ]);
    assert_eq!(retained(&sb), vec!["a", "b", "c"]);
    assert_eq!(sb.byte_total, 3);
    assert_eq!(sb.total_pushed(), 3);
}

#[test]
fn replacing_past_the_line_cap_evicts_the_oldest_and_tallies_them() {
    let mut sb = bounded(2, 1_000_000);
    sb.replace_lines(vec![
        (line("aa"), RowEnd::Hard),
        (line("bbb"), RowEnd::Hard),
        (line("c"), RowEnd::Hard),
        (line("dd"), RowEnd::Hard),
    ]);
    assert_eq!(retained(&sb), vec!["c", "dd"]);
    assert_eq!(sb.byte_total, 3);
    assert_eq!(sb.dropped_lines(), 2);
    assert_eq!(sb.dropped_bytes(), 5);
    // The increase is counted after eviction: two rows retained, none before.
    assert_eq!(sb.total_pushed(), 2);
}

#[test]
fn replacing_past_the_byte_cap_keeps_the_newest_rows_within_budget() {
    let mut sb = bounded(100, 4);
    sb.replace_lines(vec![
        (line("aaa"), RowEnd::Hard),
        (line("bb"), RowEnd::Hard),
        (line("cc"), RowEnd::Hard),
    ]);
    assert_eq!(retained(&sb), vec!["bb", "cc"]);
    assert_eq!(sb.byte_total, 4);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 3);
}

#[test]
fn replacing_with_nothing_empties_the_buffer_and_keeps_the_counters() {
    let mut sb = bounded(1, 1_000_000);
    sb.push_row(&line("aa"), RowEnd::Hard);
    sb.push_row(&line("bb"), RowEnd::Hard); // drops "aa"
    sb.replace_lines(Vec::new());
    assert!(sb.is_empty());
    assert_eq!(sb.byte_total, 0);
    assert_eq!(sb.total_pushed(), 2);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer_through_replace_lines() {
    let mut sb = bounded(10, 1_000_000);
    sb.replace_lines(vec![(padded("ab", 6), RowEnd::SoftWide)]);

    let (stored, meta) = &sb.lines()[0];
    assert_eq!(stored.len(), 6);
    assert_eq!(meta.end, RowEnd::SoftWide);
}

#[test]
fn a_push_after_clear_starts_the_byte_total_from_zero() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&line("abc"), RowEnd::Hard);
    sb.clear();
    sb.push_row(&line("de"), RowEnd::Hard);
    assert_eq!(retained(&sb), vec!["de"]);
    assert_eq!(sb.byte_total, 2);
    assert_eq!(sb.total_pushed(), 2);
}

#[test]
fn clear_on_an_empty_buffer_changes_nothing() {
    let mut sb = bounded(10, 1000);
    sb.clear();
    assert_eq!(sb, bounded(10, 1000));
}

#[test]
fn a_buffer_with_drops_round_trips_through_serde_field_for_field() {
    let mut sb = bounded(2, 1000);
    for s in ["aa", "bbb", "c"] {
        sb.push_row(&line(s), RowEnd::Soft);
    }
    sb.push_row_with_meta(
        &line("prompt"),
        RowMeta {
            end: RowEnd::Hard,
            prompt: true,
        },
    );

    let value = serde_json::to_value(&sb).expect("scrollback serializes");
    let restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");

    assert_eq!(restored, sb);
    assert_eq!(restored.max_lines, 2);
    assert_eq!(restored.max_bytes, 1000);
    assert_eq!(restored.byte_total, 7);
    assert_eq!(restored.total_pushed(), 4);
    assert_eq!(restored.dropped_lines(), 2);
    assert_eq!(restored.dropped_bytes(), 5);
}

#[test]
fn an_empty_buffer_round_trips_through_serde() {
    let sb = bounded(3, 40);
    let value = serde_json::to_value(&sb).expect("scrollback serializes");
    let restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");
    assert_eq!(restored, sb);
}

#[test]
fn a_stored_byte_total_that_does_not_match_the_rows_is_recomputed() {
    // The byte total is derived from the rows on the way in, so a stored total
    // below their real size cannot underflow the first eviction.
    let mut scrollback = bounded(1, 1_000_000);
    scrollback.push_row(&line("abc"), RowEnd::Hard);
    let mut value = serde_json::to_value(&scrollback).expect("scrollback serializes");
    value["byte_total"] = serde_json::json!(0);

    let mut restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");
    assert_eq!(restored.byte_total, 3);

    restored.push_row(&line("d"), RowEnd::Hard);

    assert_eq!(restored.byte_total, 1);
    assert_eq!(retained(&restored), vec!["d"]);
}
