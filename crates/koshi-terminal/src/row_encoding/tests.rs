//! Tests for the row encoding: exact bytes for small rows, round trips of
//! seeded rows and of every output workload, and one malformed input per
//! decode error.

use super::*;

use koshi_core::process::PtySize;
use koshi_test_support::output_workloads::{
    build_workload_bytes, compute_fnv1a_hash, OutputWorkload, SeededSequence, DEFAULT_WORKLOAD_SEED,
};

use crate::engine::TerminalEngine;
use crate::grid::state::tests::build_padded_line_cells;
use crate::grid::state::{ImageCellFragment, ImagePlaceholder, RowMetadata};
use crate::scrollback::{Scrollback, ScrollbackLimit};

fn encode_to_bytes(row_cells: &[Cell], row_end: RowEnd) -> (Vec<u8>, usize) {
    let mut encoded_row_bytes = Vec::new();
    let text_byte_count = encode_row_cells(row_cells, row_end, &mut encoded_row_bytes);
    (encoded_row_bytes, text_byte_count)
}

/// `body_bytes` with its body byte count in front.
fn prefix_byte_count(body_bytes: &[u8]) -> Vec<u8> {
    let mut encoded_row_bytes = Vec::new();
    write_varint(body_bytes.len() as u64, &mut encoded_row_bytes);
    encoded_row_bytes.extend_from_slice(body_bytes);
    encoded_row_bytes
}

#[test]
fn encode_row_cells_clears_the_output_and_writes_the_documented_bytes_for_ab() {
    let mut encoded_row_bytes = vec![0xff, 0xff];
    let text_byte_count = encode_row_cells(
        &build_padded_line_cells("ab", 4),
        RowEnd::Hard,
        &mut encoded_row_bytes,
    );
    assert_eq!(
        encoded_row_bytes,
        vec![
            0x10, 0x04, 0x00, 0x02, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x02,
            0x61, 0x62, 0x00
        ]
    );
    assert_eq!(text_byte_count, 2);
}

#[test]
fn plain_200_column_row_encodes_to_219_bytes() {
    let row_text: String = (0..200u32)
        .map(|character_index| char::from(b'!' + (character_index % 90) as u8))
        .collect();
    let (encoded_row_bytes, text_byte_count) =
        encode_to_bytes(&build_padded_line_cells(&row_text, 200), RowEnd::Soft);
    assert_eq!(encoded_row_bytes.len(), 219);
    assert_eq!(text_byte_count, 200);
}

#[test]
fn hard_row_text_byte_count_excludes_trailing_default_blanks() {
    let row_cells = build_padded_line_cells("README.md", 200);
    assert_eq!(encode_to_bytes(&row_cells, RowEnd::Hard).1, 9);
    assert_eq!(encode_to_bytes(&row_cells, RowEnd::Soft).1, 200);
    assert_eq!(encode_to_bytes(&row_cells, RowEnd::SoftWide).1, 200);
}

#[test]
fn a_styled_blank_is_content_and_not_a_trailing_blank() {
    let mut red_background_style = Style::default();
    red_background_style.set_background_color(Color::Indexed(1));
    let mut row_cells = build_padded_line_cells("a", 6);
    row_cells[3] = Cell::build_blank_with_style(red_background_style);
    let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&row_cells, RowEnd::Hard);
    assert_eq!(text_byte_count, 4);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells,
            row_end: RowEnd::Hard
        })
    );
}

#[test]
fn a_run_of_equal_cells_travels_once_with_its_count() {
    let row_cells = vec![Cell::from_character('─', 1, Style::default()); 200];
    let (encoded_row_bytes, _) = encode_to_bytes(&row_cells, RowEnd::Hard);
    assert_eq!(
        encoded_row_bytes,
        vec![
            0x17, 0xc8, 0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0xc8,
            0x01, 0x03, 0xe2, 0x94, 0x80, 0x01, 0x00, 0x03, 0xc7, 0x01
        ]
    );
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells,
            row_end: RowEnd::Hard
        })
    );
}

#[test]
fn three_equal_cells_travel_as_text_and_four_as_a_repeat() {
    let three_cells = build_padded_line_cells("xxx", 3);
    let four_cells = build_padded_line_cells("xxxx", 4);
    let (three_cell_bytes, _) = encode_to_bytes(&three_cells, RowEnd::Hard);
    let (four_cell_bytes, _) = encode_to_bytes(&four_cells, RowEnd::Hard);
    assert_eq!(
        three_cell_bytes,
        vec![
            0x11, 0x03, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x03, 0x03,
            b'x', b'x', b'x', 0x00
        ]
    );
    assert_eq!(
        four_cell_bytes,
        vec![
            0x12, 0x04, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x01,
            b'x', 0x01, 0x00, 0x03, 0x03
        ]
    );
    assert_eq!(
        decode_row_cells(&four_cell_bytes).map(|decoded_row| decoded_row.row_cells),
        Ok(four_cells)
    );
}

#[test]
fn non_ascii_narrow_cells_travel_as_text_and_four_equal_ones_as_a_repeat() {
    let row_cells = build_padded_line_cells("\u{e9}\u{e9}\u{e9}\u{e9}\u{436}\u{436}\u{436}x", 8);
    let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&row_cells, RowEnd::Hard);
    assert_eq!(
        encoded_row_bytes,
        vec![
            0x1a, 0x08, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x09,
            0xc3, 0xa9, 0xd0, 0xb6, 0xd0, 0xb6, 0xd0, 0xb6, b'x', 0x01, 0x00, 0x03, 0x03
        ]
    );
    assert_eq!(text_byte_count, 15);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes).map(|decoded_row| decoded_row.row_cells),
        Ok(row_cells)
    );
}

#[test]
fn wide_pairs_lone_widths_and_continuations_round_trip() {
    let mut emoji_cell = Cell::from_character('\u{2764}', 2, Style::default());
    emoji_cell.push_combining('\u{fe0f}');
    let row_cells = vec![
        Cell::from_character('漢', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
        emoji_cell,
        Cell::from_character(' ', 0, Style::default()),
        Cell::from_character('字', 2, Style::default()),
        Cell::from_character('x', 0, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ];
    let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&row_cells, RowEnd::SoftWide);
    assert_eq!(text_byte_count, "漢❤\u{fe0f}字".len());
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells,
            row_end: RowEnd::SoftWide
        })
    );
}

#[test]
fn a_cell_with_32_continuation_code_points_round_trips() {
    let mut combined_cell = Cell::from_character('a', 1, Style::default());
    for _ in 0..MAX_GRAPHEME_CONTINUATION_COUNT {
        combined_cell.push_combining('\u{0301}');
    }
    let row_cells = vec![combined_cell];
    let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&row_cells, RowEnd::Hard);
    assert_eq!(text_byte_count, 1 + 2 * 32);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells,
            row_end: RowEnd::Hard
        })
    );
}

#[test]
fn a_row_of_65535_cells_round_trips() {
    let row_cells = build_padded_line_cells("row", 65_535);
    let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&row_cells, RowEnd::Soft);
    assert_eq!(text_byte_count, 65_535);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells,
            row_end: RowEnd::Soft
        })
    );
}

#[test]
#[should_panic(expected = "a row holds at most 65 535 cells")]
fn encode_row_cells_panics_on_65536_cells() {
    let _ = encode_to_bytes(&build_padded_line_cells("", 65_536), RowEnd::Hard);
}

#[test]
fn a_row_of_forty_styles_used_twice_holds_each_style_once_and_round_trips() {
    let row_cells: Vec<Cell> = (0..80u8)
        .map(|column_index| {
            let mut style = Style::default();
            style.set_foreground_color(Color::Indexed(column_index % 40));
            Cell::from_character('x', 1, style)
        })
        .collect();

    let (encoded_row_bytes, _) = encode_to_bytes(&row_cells, RowEnd::Hard);

    // A 645-byte body, 80 columns, a hard end, no trailing blank, 40 styles.
    assert_eq!(encoded_row_bytes.len(), 647);
    assert_eq!(encoded_row_bytes[..6], [0x85, 0x05, 0x50, 0x00, 0x00, 0x28]);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells,
            row_end: RowEnd::Hard
        })
    );
}

#[test]
fn repadding_an_encoded_row_rewrites_its_column_and_trailing_blank_counts() {
    let (encoded_row_bytes, _) = encode_to_bytes(&build_padded_line_cells("ab", 4), RowEnd::Hard);
    let mut repadded_row_bytes = vec![0xff];

    assert_eq!(
        repad_encoded_row(&encoded_row_bytes, 6, &mut repadded_row_bytes),
        Ok(Some(2))
    );
    assert_eq!(
        repadded_row_bytes,
        [
            0x10, 0x06, 0x00, 0x04, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x02,
            b'a', b'b', 0x00
        ]
    );
    assert_eq!(
        repad_encoded_row(&encoded_row_bytes, 2, &mut repadded_row_bytes),
        Ok(None)
    );
    assert_eq!(
        repadded_row_bytes,
        [
            0x10, 0x06, 0x00, 0x04, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x02,
            b'a', b'b', 0x00
        ]
    );
}

#[test]
fn an_empty_row_round_trips() {
    let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&[], RowEnd::Hard);
    assert_eq!(encoded_row_bytes, vec![0x05, 0x00, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(text_byte_count, 0);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells: Vec::new(),
            row_end: RowEnd::Hard
        })
    );
}

fn build_seeded_color(seeded_sequence: &mut SeededSequence) -> Color {
    match seeded_sequence.compute_next_index(3) {
        0 => Color::Default,
        1 => Color::Indexed(seeded_sequence.compute_next_index(256) as u8),
        _ => Color::Rgb(
            seeded_sequence.compute_next_index(256) as u8,
            seeded_sequence.compute_next_index(256) as u8,
            seeded_sequence.compute_next_index(256) as u8,
        ),
    }
}

fn build_seeded_style(seeded_sequence: &mut SeededSequence) -> Style {
    let underline_color = match seeded_sequence.compute_next_index(2) {
        0 => None,
        _ => Some(build_seeded_color(seeded_sequence)),
    };
    Style::from_style_parts(
        build_seeded_color(seeded_sequence),
        build_seeded_color(seeded_sequence),
        AttributeFlags::from_attribute_word(seeded_sequence.compute_next_value() as u16),
        underline_color,
    )
}

fn build_seeded_row(seeded_sequence: &mut SeededSequence) -> (Vec<Cell>, RowEnd) {
    let content_column_count = seeded_sequence.compute_next_in_range(0, 200);
    let mut row_cells = Vec::with_capacity(content_column_count + 10);
    let mut style = Style::default();
    while row_cells.len() < content_column_count {
        if seeded_sequence.compute_next_index(10) < 3 {
            style = build_seeded_style(seeded_sequence);
        }
        match seeded_sequence.compute_next_index(10) {
            0..=3 => row_cells.push(Cell::from_character(
                char::from(b' ' + seeded_sequence.compute_next_index(95) as u8),
                1,
                style,
            )),
            4 => {
                let wide_character =
                    char::from_u32(0x4e00 + seeded_sequence.compute_next_index(0x5000) as u32)
                        .expect("CJK ideographs are scalar values");
                row_cells.push(Cell::from_character(wide_character, 2, style));
                row_cells.push(Cell::from_character(' ', 0, style));
            }
            5 => {
                let mut combined_cell = Cell::from_character(
                    char::from(b'a' + seeded_sequence.compute_next_index(26) as u8),
                    seeded_sequence.compute_next_index(3) as u8,
                    style,
                );
                for _ in 0..seeded_sequence.compute_next_in_range(1, 3) {
                    combined_cell.push_combining(
                        char::from_u32(0x300 + seeded_sequence.compute_next_index(0x70) as u32)
                            .expect("combining marks are scalar values"),
                    );
                }
                row_cells.push(combined_cell);
            }
            6 => row_cells.push(Cell::from_character(
                ' ',
                seeded_sequence.compute_next_index(3) as u8,
                style,
            )),
            7 => {
                let repeated_character =
                    char::from(b'!' + seeded_sequence.compute_next_index(90) as u8);
                let is_wide_pair = seeded_sequence.compute_next_index(2) == 0;
                for _ in 0..seeded_sequence.compute_next_in_range(2, 20) {
                    if is_wide_pair {
                        row_cells.push(Cell::from_character('漢', 2, style));
                        row_cells.push(Cell::from_character(' ', 0, style));
                    } else {
                        row_cells.push(Cell::from_character(repeated_character, 1, style));
                    }
                }
            }
            8 => row_cells.push(Cell::build_blank()),
            _ => row_cells.push(Cell::build_blank_with_style(style)),
        }
    }
    for _ in 0..seeded_sequence.compute_next_index(10) {
        row_cells.push(Cell::build_blank());
    }
    let row_end = match seeded_sequence.compute_next_index(3) {
        0 => RowEnd::Hard,
        1 => RowEnd::Soft,
        _ => RowEnd::SoftWide,
    };
    (row_cells, row_end)
}

#[test]
fn ten_thousand_seeded_rows_round_trip_cell_for_cell() {
    let mut seeded_sequence = SeededSequence::from_seed(DEFAULT_WORKLOAD_SEED);
    for _ in 0..10_000 {
        let (row_cells, row_end) = build_seeded_row(&mut seeded_sequence);
        let (encoded_row_bytes, text_byte_count) = encode_to_bytes(&row_cells, row_end);
        assert_eq!(
            text_byte_count,
            compute_text_byte_count(&row_cells, row_end)
        );
        assert_eq!(
            decode_row_cells(&encoded_row_bytes),
            Ok(DecodedRow { row_cells, row_end })
        );
    }
}

/// Adds seeded image data to `row_cells`. Each cell has a 1 in 40 chance to
/// become a blank that carries a native image fragment, a 1 in 40 chance to
/// keep its glyph under an overlaid fragment, and a 1 in 40 chance to carry
/// Kitty placeholder metadata.
fn add_seeded_image_cells(row_cells: &mut [Cell], seeded_sequence: &mut SeededSequence) {
    for row_cell in row_cells {
        let image_cell_fragment = ImageCellFragment {
            image_source_id: seeded_sequence.compute_next_in_range(1, 8) as u64,
            source_row_index: seeded_sequence.compute_next_index(1_000) as u16,
            source_column_index: seeded_sequence.compute_next_index(1_000) as u16,
        };
        match seeded_sequence.compute_next_index(40) {
            0 => row_cell.set_image_fragment(image_cell_fragment, false),
            1 => row_cell.set_image_fragment(image_cell_fragment, true),
            2 => row_cell.set_image_placeholder(ImagePlaceholder {
                image_id: seeded_sequence.compute_next_index(1 << 24) as u32,
                placement_id: Some(seeded_sequence.compute_next_index(1_000) as u32),
                source_row: Some(seeded_sequence.compute_next_index(1_000) as u16),
                source_column: None,
                image_id_msb: Some(seeded_sequence.compute_next_index(256) as u8),
            }),
            _ => {}
        }
    }
}

#[test]
fn ten_thousand_seeded_rows_encode_to_their_recorded_bytes() {
    let mut seeded_sequence = SeededSequence::from_seed(DEFAULT_WORKLOAD_SEED);
    let mut encoded_rows_bytes = Vec::new();
    let mut encoded_row_bytes = Vec::new();
    let mut text_byte_count_sum = 0usize;
    for _ in 0..10_000 {
        let (mut row_cells, row_end) = build_seeded_row(&mut seeded_sequence);
        add_seeded_image_cells(&mut row_cells, &mut seeded_sequence);
        text_byte_count_sum += encode_row_cells(&row_cells, row_end, &mut encoded_row_bytes);
        encoded_rows_bytes.extend_from_slice(&encoded_row_bytes);
    }

    assert_eq!(
        (
            encoded_rows_bytes.len(),
            compute_fnv1a_hash(&encoded_rows_bytes),
            text_byte_count_sum
        ),
        (3_353_015, 0xa73d_839b_1e5a_f289, 1_412_568)
    );
}

#[test]
fn ten_thousand_seeded_rows_come_back_from_history_with_their_image_cells_and_prompt_marks() {
    let mut seeded_sequence = SeededSequence::from_seed(DEFAULT_WORKLOAD_SEED);
    let mut scrollback = Scrollback::from_scrollback_limit(
        ScrollbackLimit::from_line_and_byte_limits(10_000, usize::MAX),
    );
    let mut pushed_rows: Vec<(Vec<Cell>, RowMetadata)> = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let (mut row_cells, row_end) = build_seeded_row(&mut seeded_sequence);
        add_seeded_image_cells(&mut row_cells, &mut seeded_sequence);
        let row_metadata = RowMetadata {
            row_end,
            has_prompt_mark: seeded_sequence.compute_next_index(4) == 0,
        };
        scrollback.push_row_cells(&row_cells, row_metadata);
        pushed_rows.push((row_cells, row_metadata));
    }

    let retained_lines = scrollback.list_retained_lines();
    assert_eq!(retained_lines.len(), 10_000);
    for (line_index, (row_snapshot, (row_cells, row_metadata))) in
        retained_lines.iter().zip(&pushed_rows).enumerate()
    {
        assert_eq!(
            row_snapshot.decode_row_cells(),
            *row_cells,
            "line {line_index}"
        );
        assert_eq!(
            row_snapshot.get_row_metadata(),
            *row_metadata,
            "line {line_index}"
        );
    }
}

#[test]
fn every_output_workload_round_trips_every_screen_row_and_keeps_every_history_row_whole() {
    for workload in OutputWorkload::ALL {
        let workload_bytes = build_workload_bytes(workload, 256 * 1024, DEFAULT_WORKLOAD_SEED);
        let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 200,
            row_count: 50,
        });
        let _ = terminal_engine.process_pty_output(&workload_bytes);
        let terminal_state = terminal_engine.get_terminal_state();
        let active_grid = terminal_state.get_active_grid();
        for (row_index, row_cells) in active_grid.list_row_cells().enumerate() {
            let row_end = active_grid.get_row_end(row_index as u16);
            let (encoded_row_bytes, _) = encode_to_bytes(row_cells, row_end);
            assert_eq!(
                decode_row_cells(&encoded_row_bytes),
                Ok(DecodedRow {
                    row_cells: row_cells.to_vec(),
                    row_end
                }),
                "{} screen row {row_index}",
                workload.get_workload_name()
            );
        }
        // The same output on a screen tall enough to keep every line: its top
        // rows are the history rows of the 50-row screen, oldest first.
        let history_lines = terminal_state.get_scrollback().list_retained_lines();
        let tall_row_count =
            u16::try_from(history_lines.len() + 50).expect("the workload fits a u16 screen");
        let mut tall_terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 200,
            row_count: tall_row_count,
        });
        let _ = tall_terminal_engine.process_pty_output(&workload_bytes);
        let tall_grid = tall_terminal_engine.get_terminal_state().get_active_grid();
        for (history_row_index, row_snapshot) in history_lines.iter().enumerate() {
            let tall_row_index = history_row_index as u16;
            assert_eq!(
                row_snapshot.decode_row_cells(),
                tall_grid
                    .get_row_cells(tall_row_index)
                    .expect("the tall screen holds the row"),
                "{} history row {history_row_index}",
                workload.get_workload_name()
            );
            assert_eq!(
                row_snapshot.get_row_metadata(),
                tall_grid.get_row_metadata(tall_row_index),
                "{} history row {history_row_index}",
                workload.get_workload_name()
            );
        }
    }
}

#[test]
fn a_style_entry_with_unknown_trailing_fields_decodes_and_skips_them() {
    let encoded_row_bytes = prefix_byte_count(&[
        0x01, 0x00, 0x00, 0x01, 0x08, 0x00, 0x01, 0x05, 0x00, 0x00, 0xaa, 0xbb, 0xcc, 0x01, 0x00,
        0x01, 0x01, b'z', 0x00,
    ]);
    let mut expected_style = Style::default();
    expected_style.set_background_color(Color::Indexed(5));
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Ok(DecodedRow {
            row_cells: vec![Cell::from_character('z', 1, expected_style)],
            row_end: RowEnd::Hard
        })
    );
}

#[test]
fn split_encoded_row_returns_the_first_row_and_the_rest() {
    let (first_row_bytes, _) = encode_to_bytes(&build_padded_line_cells("ab", 4), RowEnd::Hard);
    let mut stream_bytes = first_row_bytes.clone();
    stream_bytes.extend_from_slice(&[0xde, 0xad]);
    assert_eq!(
        split_encoded_row(&stream_bytes),
        Ok((first_row_bytes.as_slice(), [0xde, 0xad].as_slice()))
    );
}

#[test]
fn decode_reports_truncated_for_a_body_that_ends_inside_a_varint() {
    assert_eq!(
        decode_row_cells(&[0x01, 0x80]),
        Err(RowDecodeError::Truncated)
    );
}

#[test]
fn decode_reads_a_ten_byte_varint_and_reports_a_longer_one() {
    let mut largest_varint_bytes = vec![0xff; 9];
    largest_varint_bytes.push(0x01);
    assert_eq!(
        decode_row_cells(&largest_varint_bytes),
        Err(RowDecodeError::BodyPastEnd {
            body_byte_count: u64::MAX,
            available_byte_count: 0
        })
    );
    assert_eq!(
        decode_row_cells(&[0x80; 11]),
        Err(RowDecodeError::VarintTooLong)
    );
    let mut column_count_bytes = vec![0x80; 9];
    column_count_bytes.push(0x02);
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&column_count_bytes)),
        Err(RowDecodeError::VarintTooLong)
    );
    assert_eq!(
        RowDecodeError::VarintTooLong.to_string(),
        "a varint runs past 10 bytes or past the range of a u64"
    );
}

#[test]
fn decode_reports_a_body_past_the_end() {
    assert_eq!(
        decode_row_cells(&[0x05, 0x01]),
        Err(RowDecodeError::BodyPastEnd {
            body_byte_count: 5,
            available_byte_count: 1
        })
    );
}

#[test]
fn decode_reports_bytes_after_the_body() {
    let (mut encoded_row_bytes, _) =
        encode_to_bytes(&build_padded_line_cells("ab", 4), RowEnd::Hard);
    encoded_row_bytes.push(0x00);
    assert_eq!(
        decode_row_cells(&encoded_row_bytes),
        Err(RowDecodeError::TrailingBytes {
            trailing_byte_count: 1
        })
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x07])),
        Err(RowDecodeError::TrailingBytes {
            trailing_byte_count: 1
        })
    );
}

#[test]
fn decode_reports_a_column_count_over_65535() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x80, 0x80, 0x04, 0x00, 0x00, 0x00, 0x00
        ])),
        Err(RowDecodeError::ValueOutOfRange {
            field_name: "column_count",
            decoded_value: 65_536
        })
    );
}

#[test]
fn decode_reports_an_unknown_row_end() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[0x00, 0x03, 0x00, 0x00, 0x00])),
        Err(RowDecodeError::UnknownRowEnd { row_end_code: 3 })
    );
}

#[test]
fn decode_reports_an_unknown_color_kind() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x00, 0x00, 0x00, 0x01, 0x04, 0x03, 0x00, 0x00, 0x00, 0x00
        ])),
        Err(RowDecodeError::UnknownColorKind { color_kind: 3 })
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x00, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x02, 0x00
        ])),
        Err(RowDecodeError::UnknownColorKind { color_kind: 2 })
    );
}

#[test]
fn decode_reports_a_style_entry_past_the_row_and_a_field_past_the_entry() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x00, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00
        ])),
        Err(RowDecodeError::StyleEntryPastEnd)
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x00
        ])),
        Err(RowDecodeError::Truncated)
    );
}

#[test]
fn decode_reports_a_style_index_outside_the_table() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, b'a',
            0x00
        ])),
        Err(RowDecodeError::StyleIndexOutOfTable {
            style_index: 1,
            style_count: 1
        })
    );
}

#[test]
fn decode_reports_invalid_utf8_in_text_and_in_continuations() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, 0xff,
            0x00
        ])),
        Err(RowDecodeError::InvalidUtf8)
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, b'a',
            0x01, 0x00, 0x02, 0x01, 0xc3
        ])),
        Err(RowDecodeError::InvalidUtf8)
    );
}

#[test]
fn decode_reports_a_width_above_two() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, b'a',
            0x01, 0x00, 0x01, 0x03
        ])),
        Err(RowDecodeError::WidthTooLarge { width: 3 })
    );
}

#[test]
fn decode_reports_an_unknown_exception_kind() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, b'a',
            0x01, 0x00, 0x04
        ])),
        Err(RowDecodeError::UnknownExceptionKind { exception_kind: 4 })
    );
}

#[test]
fn decode_reports_exceptions_out_of_order() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, b'a',
            0x02, 0x00, 0x02, 0x02, 0xcc, 0x81, 0x00, 0x01, 0x01
        ])),
        Err(RowDecodeError::ExceptionOutOfOrder)
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x02, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x01, b'a',
            0x02, 0x00, 0x00, 0x00, 0x01, 0x02
        ])),
        Err(RowDecodeError::ExceptionOutOfOrder)
    );
}

#[test]
fn decode_reports_an_exception_past_the_run() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, b'a',
            0x01, 0x05, 0x01, 0x00
        ])),
        Err(RowDecodeError::ExceptionPastRun)
    );
}

#[test]
fn decode_reports_more_than_32_continuation_code_points() {
    let mut body_bytes = vec![
        0x01, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x01, b'a', 0x01,
        0x00, 0x02, 66,
    ];
    for _ in 0..33 {
        body_bytes.extend_from_slice("\u{0301}".as_bytes());
    }
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&body_bytes)),
        Err(RowDecodeError::TooManyCombiningCharacters {
            combining_count: 33
        })
    );
}

#[test]
fn decode_reports_cell_counts_that_disagree_with_the_column_count() {
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x03, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x03, 0x02, b'a',
            b'b', 0x00
        ])),
        Err(RowDecodeError::CellCountMismatch {
            column_count: 3,
            cell_count: 2
        })
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x02, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00
        ])),
        Err(RowDecodeError::CellCountMismatch {
            column_count: 2,
            cell_count: 3
        })
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[
            0x02, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x01, b'a',
            0x01, 0x00, 0x03, 0x05
        ])),
        Err(RowDecodeError::CellCountMismatch {
            column_count: 2,
            cell_count: 6
        })
    );
    assert_eq!(
        decode_row_cells(&prefix_byte_count(&[0x03, 0x00, 0x01, 0x00, 0x00])),
        Err(RowDecodeError::CellCountMismatch {
            column_count: 3,
            cell_count: 1
        })
    );
}

#[test]
fn a_plain_row_encodes_to_the_same_bytes_with_and_without_its_style() {
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    for row_style in [Style::default(), red_style] {
        for row_text in [
            "",
            "ab",
            "aaaab",
            "abbbbc",
            "abcdddd",
            "zzzzzzzzzz",
            "a   b    ",
        ] {
            let row_cells: Vec<Cell> = row_text
                .chars()
                .map(|row_character| Cell::from_character(row_character, 1, row_style))
                .collect();
            for row_end in [RowEnd::Hard, RowEnd::Soft] {
                let mut plain_row_bytes = Vec::new();
                let mut compared_row_bytes = Vec::new();
                let plain_text_byte_count = encode_padded_row_cells(
                    &row_cells,
                    10,
                    Some(row_style),
                    row_end,
                    &mut plain_row_bytes,
                );
                let compared_text_byte_count =
                    encode_padded_row_cells(&row_cells, 10, None, row_end, &mut compared_row_bytes);

                assert_eq!(
                    plain_row_bytes, compared_row_bytes,
                    "{row_text:?} {row_end:?}"
                );
                assert_eq!(
                    plain_text_byte_count, compared_text_byte_count,
                    "{row_text:?} {row_end:?}"
                );
            }
        }
    }
}
