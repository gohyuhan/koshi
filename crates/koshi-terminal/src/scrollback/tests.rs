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

/// Call [`Scrollback::replace_lines`] with the buffer's current retained
/// count as `retained_before`.
fn replace(sb: &mut Scrollback, lines: Vec<(Vec<Cell>, RowMeta)>) {
    let retained_before = sb.len() as u64;
    sb.replace_lines(lines, retained_before);
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
    // 'a' (1 byte) + '世' (3 bytes) + 'e' carrying a combining acute (1 + 2).
    let mut accented = Cell::new('e', 1, Style::default());
    accented.push_combining('\u{0301}'); // U+0301, two UTF-8 bytes
    let row = vec![
        Cell::new('a', 1, Style::default()),
        Cell::new('世', 2, Style::default()),
        accented,
    ];
    assert_eq!(line_bytes(&row), 1 + 3 + (1 + 2));
}

#[test]
fn line_bytes_skips_wide_glyph_continuation_placeholders() {
    // A wide glyph occupies two cells: a width-2 base carrying '世' (3 bytes)
    // and a width-0 continuation placeholder (a blank space). Only the base
    // carries text; the placeholder adds nothing.
    let row = vec![
        Cell::new('世', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
    ];
    assert_eq!(line_bytes(&row), 3); // the space placeholder is skipped
}

#[test]
fn pushing_within_both_caps_retains_every_row_in_order() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&line("one"), RowMeta::default());
    sb.push_row(&line("two"), RowMeta::default());
    sb.push_row(&line("three"), RowMeta::default());
    assert_eq!(sb.len(), 3);
    assert_eq!(retained(&sb), vec!["one", "two", "three"]);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.dropped_bytes(), 0);
    assert_eq!(sb.byte_total, 3 + 3 + 5);
}

#[test]
fn exceeding_the_line_cap_drops_oldest_first() {
    let mut sb = bounded(3, 100_000);
    sb.push_row(&line("L0"), RowMeta::default()); // dropped by the fourth push
    sb.push_row(&line("L1"), RowMeta::default());
    sb.push_row(&line("L2"), RowMeta::default());
    sb.push_row(&line("L3"), RowMeta::default());
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
    sb.push_row(&line("aaaa"), RowMeta::default());
    sb.push_row(&line("bbbb"), RowMeta::default());
    sb.push_row(&line("cccc"), RowMeta::default());
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
    sb.push_row(&line("oversized"), RowMeta::default());
    assert_eq!(sb.len(), 1);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.byte_total, 9);
}

#[test]
fn a_later_push_drops_the_retained_oversized_row() {
    // With a second row present the guard no longer applies: the oversized
    // row is dropped and the total falls back under the cap.
    let mut sb = bounded(100_000, 2);
    sb.push_row(&line("oversized"), RowMeta::default()); // 9 bytes, kept by the guard
    sb.push_row(&line("x"), RowMeta::default()); // 1 byte: total 10, len 2 -> drop the front
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
    sb.push_row(&line("gone"), RowMeta::default());
    assert!(sb.is_empty());
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 4);
    assert_eq!(sb.byte_total, 0);
}

#[test]
fn byte_total_stays_equal_to_the_sum_of_retained_rows() {
    let mut sb = bounded(3, 100_000);
    for s in ["alpha", "beta", "gamma", "delta", "epsilon"] {
        sb.push_row(&line(s), RowMeta::default());
    }
    let expected: usize = sb.lines().iter().map(|(row, _)| line_bytes(row)).sum();
    assert_eq!(sb.byte_total, expected);
}

#[test]
fn dropped_tallies_accumulate_across_many_drops() {
    let mut sb = bounded(1, 100_000); // every push past the first drops one row
    sb.push_row(&line("aa"), RowMeta::default()); // 2 bytes
    sb.push_row(&line("bbb"), RowMeta::default()); // 3 bytes, drops "aa"
    sb.push_row(&line("c"), RowMeta::default()); // 1 byte, drops "bbb"
    assert_eq!(sb.len(), 1);
    assert_eq!(retained(&sb), vec!["c"]);
    assert_eq!(sb.dropped_lines(), 2);
    assert_eq!(sb.dropped_bytes(), 5);
}

#[test]
fn clear_empties_the_buffer_but_keeps_the_drop_tallies() {
    let mut sb = bounded(1, 100_000); // line cap of 1 forces a drop
    sb.push_row(&line("aa"), RowMeta::default());
    sb.push_row(&line("bbb"), RowMeta::default()); // drops "aa": dropped_lines 1, dropped_bytes 2
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
    sb.push_row(&line("a"), RowMeta::default());
    sb.push_row(&line("b"), RowMeta::default());
    sb.push_row(&line("c"), RowMeta::default()); // drops "a"; the push itself still counts
    assert_eq!(sb.total_pushed(), 3);

    sb.clear();
    assert_eq!(sb.total_pushed(), 3); // an erase never rewinds the counter

    sb.push_row(&line("d"), RowMeta::default());
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
    sb.push_row(&padded("README.md", 200), RowMeta::default());

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
    sb.push_row(&padded("hi", 200), RowMeta::default());

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
    sb.push_row(
        &padded("ab", 6),
        RowMeta {
            end: RowEnd::Soft,
            prompt: false,
        },
    );

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 6);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer() {
    // The final blank stands in for the wide glyph that starts the next row.
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(
        &padded("ab", 6),
        RowMeta {
            end: RowEnd::SoftWide,
            prompt: false,
        },
    );

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
    sb.push_row(&row, RowMeta::default());

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
    sb.push_row(&row, RowMeta::default());

    let (stored, _) = &sb.lines()[0];
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].width(), 0);
}

#[test]
fn a_row_of_nothing_but_padding_stores_no_cells() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&padded("", 200), RowMeta::default());

    let (stored, _) = &sb.lines()[0];
    assert!(stored.is_empty());
    assert_eq!(sb.len(), 1); // the blank line itself is still a line
}

#[test]
fn a_reflow_rebuild_trims_the_same_way_a_push_does() {
    // A replacement trims a hard row the same way a push does.
    let mut sb = bounded(10, 1_000_000);
    replace(
        &mut sb,
        vec![
            (padded("one", 200), RowMeta::default()),
            (
                padded("ab", 200),
                RowMeta {
                    end: RowEnd::Soft,
                    prompt: false,
                },
            ),
        ],
    );

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
    replace(&mut sb, vec![(padded("hi", 200), RowMeta::default())]);

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
        sb.push_row(&padded("hi", 200), RowMeta::default());
    }
    assert_eq!(sb.len(), 10);
    assert_eq!(sb.byte_total, 20);
    assert_eq!(sb.dropped_lines(), 0);
}

#[test]
fn prompt_marks_stay_with_rows_through_history_replacement() {
    let mut sb = bounded(10, 1_000_000);
    replace(
        &mut sb,
        vec![
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
        ],
    );

    assert!(sb.lines()[0].1.prompt);
    assert!(!sb.lines()[1].1.prompt);
}

#[test]
fn prompt_marks_are_evicted_with_their_rows() {
    let mut sb = bounded(1, 1_000_000);
    sb.push_row(
        &line("prompt"),
        RowMeta {
            end: RowEnd::Hard,
            prompt: true,
        },
    );
    sb.push_row(
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
    sb.push_row(
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
    sb.push_row(
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
    assert_eq!(line_bytes(&[]), 0);
}

#[test]
fn pushing_an_empty_row_counts_a_line_of_zero_bytes() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&[], RowMeta::default());
    assert_eq!(sb.len(), 1);
    assert_eq!(sb.byte_total, 0);
    assert_eq!(sb.total_pushed(), 1);
    assert_eq!(retained(&sb), vec![String::new()]);
}

#[test]
fn a_byte_total_exactly_at_the_cap_keeps_every_row() {
    let mut sb = bounded(100, 8);
    sb.push_row(&line("aaaa"), RowMeta::default());
    sb.push_row(&line("bbbb"), RowMeta::default()); // total 8: at the cap, not past it
    assert_eq!(sb.len(), 2);
    assert_eq!(sb.byte_total, 8);
    assert_eq!(sb.dropped_lines(), 0);

    sb.push_row(&line("c"), RowMeta::default()); // total 9: one past, drops "aaaa"
    assert_eq!(retained(&sb), vec!["bbbb", "c"]);
    assert_eq!(sb.byte_total, 5);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 4);
}

#[test]
fn a_row_count_exactly_at_the_line_cap_keeps_every_row() {
    let mut sb = bounded(2, 1000);
    sb.push_row(&line("a"), RowMeta::default());
    sb.push_row(&line("b"), RowMeta::default());
    assert_eq!(sb.len(), 2);
    assert_eq!(sb.dropped_lines(), 0);
}

#[test]
fn one_push_can_drop_for_the_line_cap_and_then_the_byte_cap() {
    let mut sb = bounded(2, 2);
    sb.push_row(&line("a"), RowMeta::default());
    sb.push_row(&line("b"), RowMeta::default()); // len 2, bytes 2: both caps hold
    sb.push_row(&line("cc"), RowMeta::default()); // len 3 drops "a"; bytes 3 then drops "b"
    assert_eq!(retained(&sb), vec!["cc"]);
    assert_eq!(sb.byte_total, 2);
    assert_eq!(sb.dropped_lines(), 2);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn a_soft_row_charges_its_padding_against_the_byte_cap() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(
        &padded("ab", 6),
        RowMeta {
            end: RowEnd::Soft,
            prompt: false,
        },
    );
    assert_eq!(sb.byte_total, 6);
}

#[test]
fn dropped_bytes_count_the_stored_row_not_the_screen_row() {
    let mut sb = bounded(1, 1_000_000);
    sb.push_row(&padded("hi", 200), RowMeta::default());
    sb.push_row(&line("x"), RowMeta::default()); // drops the trimmed "hi"
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn replacing_with_fewer_rows_leaves_total_pushed_unchanged() {
    let mut sb = bounded(10, 1_000_000);
    for s in ["one", "two", "three"] {
        sb.push_row(&line(s), RowMeta::default());
    }
    replace(&mut sb, vec![(line("only"), RowMeta::default())]);
    assert_eq!(retained(&sb), vec!["only"]);
    assert_eq!(sb.byte_total, 4);
    assert_eq!(sb.total_pushed(), 3);
    assert_eq!(sb.dropped_lines(), 0);
}

#[test]
fn replacing_with_more_rows_grows_total_pushed_by_the_difference() {
    let mut sb = bounded(10, 1_000_000);
    sb.push_row(&line("one"), RowMeta::default());
    replace(
        &mut sb,
        vec![
            (line("a"), RowMeta::default()),
            (line("b"), RowMeta::default()),
            (line("c"), RowMeta::default()),
        ],
    );
    assert_eq!(retained(&sb), vec!["a", "b", "c"]);
    assert_eq!(sb.byte_total, 3);
    assert_eq!(sb.total_pushed(), 3);
}

#[test]
fn replacing_past_the_line_cap_evicts_the_oldest_and_tallies_them() {
    let mut sb = bounded(2, 1_000_000);
    replace(
        &mut sb,
        vec![
            (line("aa"), RowMeta::default()),
            (line("bbb"), RowMeta::default()),
            (line("c"), RowMeta::default()),
            (line("dd"), RowMeta::default()),
        ],
    );
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
    replace(
        &mut sb,
        vec![
            (line("aaa"), RowMeta::default()),
            (line("bb"), RowMeta::default()),
            (line("cc"), RowMeta::default()),
        ],
    );
    assert_eq!(retained(&sb), vec!["bb", "cc"]);
    assert_eq!(sb.byte_total, 4);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 3);
}

#[test]
fn replacing_with_nothing_empties_the_buffer_and_keeps_the_counters() {
    let mut sb = bounded(1, 1_000_000);
    sb.push_row(&line("aa"), RowMeta::default());
    sb.push_row(&line("bb"), RowMeta::default()); // drops "aa"
    replace(&mut sb, Vec::new());
    assert!(sb.is_empty());
    assert_eq!(sb.byte_total, 0);
    assert_eq!(sb.total_pushed(), 2);
    assert_eq!(sb.dropped_lines(), 1);
    assert_eq!(sb.dropped_bytes(), 2);
}

#[test]
fn a_wide_glyph_wrap_row_keeps_its_spacer_through_replace_lines() {
    let mut sb = bounded(10, 1_000_000);
    replace(
        &mut sb,
        vec![(
            padded("ab", 6),
            RowMeta {
                end: RowEnd::SoftWide,
                prompt: false,
            },
        )],
    );

    let (stored, meta) = &sb.lines()[0];
    assert_eq!(stored.len(), 6);
    assert_eq!(meta.end, RowEnd::SoftWide);
}

#[test]
fn a_push_after_clear_starts_the_byte_total_from_zero() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&line("abc"), RowMeta::default());
    sb.clear();
    sb.push_row(&line("de"), RowMeta::default());
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
        sb.push_row(
            &line(s),
            RowMeta {
                end: RowEnd::Soft,
                prompt: false,
            },
        );
    }
    sb.push_row(
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
    scrollback.push_row(&line("abc"), RowMeta::default());
    let mut value = serde_json::to_value(&scrollback).expect("scrollback serializes");
    value["byte_total"] = serde_json::json!(0);

    let mut restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");
    assert_eq!(restored.byte_total, 3);

    restored.push_row(&line("d"), RowMeta::default());

    assert_eq!(restored.byte_total, 1);
    assert_eq!(retained(&restored), vec!["d"]);
}

#[test]
fn stored_rows_over_the_line_cap_are_dropped_on_the_way_in() {
    // A buffer serialized at a 4-line cap, re-read with the cap edited down to
    // 2: the two oldest rows are dropped at load and tallied, and the load is
    // already within the cap.
    let mut scrollback = bounded(4, 1_000_000);
    for text in ["a", "bb", "ccc", "dddd"] {
        scrollback.push_row(&line(text), RowMeta::default());
    }
    let mut value = serde_json::to_value(&scrollback).expect("scrollback serializes");
    value["max_lines"] = serde_json::json!(2);

    let restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");

    assert_eq!(retained(&restored), vec!["ccc", "dddd"]);
    assert_eq!(restored.len(), 2);
    assert_eq!(restored.byte_total, 7);
    assert_eq!(restored.dropped_lines(), 2);
    assert_eq!(restored.dropped_bytes(), 3);
    assert_eq!(restored.total_pushed(), 4);
}

#[test]
fn stored_rows_over_the_byte_cap_are_dropped_on_the_way_in() {
    let mut scrollback = bounded(100, 1_000_000);
    for text in ["aaaa", "bbbb", "cc"] {
        scrollback.push_row(&line(text), RowMeta::default());
    }
    let mut value = serde_json::to_value(&scrollback).expect("scrollback serializes");
    value["max_bytes"] = serde_json::json!(6);

    let restored: Scrollback = serde_json::from_value(value).expect("scrollback deserializes");

    assert_eq!(retained(&restored), vec!["bbbb", "cc"]);
    assert_eq!(restored.byte_total, 6);
    assert_eq!(restored.dropped_lines(), 1);
    assert_eq!(restored.dropped_bytes(), 4);
}

#[test]
fn take_lines_empties_the_buffer_and_keeps_the_tallies() {
    let mut sb = bounded(10, 1000);
    sb.push_row(&line("one"), RowMeta::default());
    sb.push_row(&line("two"), RowMeta::default());

    let taken = sb.take_lines();

    let taken_text: Vec<String> = taken
        .iter()
        .map(|(row, _)| row.iter().map(Cell::ch).collect())
        .collect();
    assert_eq!(taken_text, vec!["one", "two"]);
    assert!(sb.is_empty());
    assert_eq!(sb.total_pushed(), 2);
    assert_eq!(sb.dropped_lines(), 0);
    assert_eq!(sb.dropped_bytes(), 0);

    sb.replace_lines(Vec::from(taken), 2);
    assert_eq!(sb.total_pushed(), 2);
    assert_eq!(retained(&sb), vec!["one", "two"]);
}

#[test]
fn pushes_after_take_lines_start_from_a_zero_byte_total() {
    let mut sb = bounded(10, 10);
    sb.push_row(&line("12345678"), RowMeta::default());

    let _ = sb.take_lines();

    sb.push_row(&line("1234"), RowMeta::default());
    sb.push_row(&line("1234"), RowMeta::default());
    assert_eq!(retained(&sb), vec!["1234", "1234"]);
    assert_eq!(sb.dropped_lines(), 0);
}
