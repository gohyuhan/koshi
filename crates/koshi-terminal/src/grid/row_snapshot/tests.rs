//! Tests for row snapshots: the cells they give back, their image cells, and
//! their stored form.

use super::*;
use crate::grid::state::tests::build_line_cells;
use crate::grid::state::{ImageCellFragment, ImagePlaceholder, RowEnd};
use crate::style::{Color, Style};

impl RowSnapshot {
    /// The cells that carry text: a hard-ended row without its trailing
    /// default blanks, every other row whole.
    pub(crate) fn decode_text_cells(&self) -> Vec<Cell> {
        let mut row_cells = self.decode_row_cells();
        if self.row_metadata.row_end == RowEnd::Hard {
            row_cells.truncate(crate::grid::state::count_row_content_cells(&row_cells));
        }
        row_cells
    }

    /// The heap bytes this snapshot holds: the encoded bytes' capacity, the
    /// image cells, and the decoded cells when they are kept. The snapshot's
    /// own struct is not counted.
    pub(crate) fn compute_heap_byte_count(&self) -> usize {
        self.encoded_row_bytes.capacity()
            + std::mem::size_of_val::<[RowImageCell]>(&self.image_cells)
            + self.decoded_row_cells.get().map_or(0, |decoded_row_cells| {
                std::mem::size_of_val::<[Cell]>(decoded_row_cells)
            })
    }
}

/// A cell carrying a native image fragment of image source `image_source_id`
/// at source cell `(1, 2)`.
fn build_fragment_cell(image_source_id: u64) -> Cell {
    let mut fragment_cell = Cell::build_blank();
    fragment_cell.set_image_fragment(
        ImageCellFragment {
            image_source_id,
            source_row_index: 1,
            source_column_index: 2,
        },
        false,
    );
    fragment_cell
}

/// A Kitty placeholder cell for image `7` with its source row and column set.
fn build_placeholder_cell() -> Cell {
    let mut placeholder_cell = Cell::build_blank();
    placeholder_cell.set_image_placeholder(ImagePlaceholder {
        image_id: 7,
        placement_id: Some(3),
        source_row: Some(0),
        source_column: Some(4),
        image_id_msb: None,
    });
    placeholder_cell
}

const SOFT_ROW_METADATA: RowMetadata = RowMetadata {
    row_end: RowEnd::Soft,
    has_prompt_mark: false,
};

#[test]
fn a_snapshot_decodes_to_the_cells_it_was_built_from() {
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    let mut accented_cell = Cell::from_character('e', 1, red_style);
    accented_cell.push_combining('\u{0301}');
    let row_cells = vec![
        accented_cell,
        Cell::from_character('漢', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
        Cell::build_blank(),
    ];

    let row_snapshot = RowSnapshot::from_row_cells(&row_cells, SOFT_ROW_METADATA, Vec::new());

    assert_eq!(row_snapshot.decode_row_cells(), row_cells);
    assert_eq!(row_snapshot.get_row_metadata(), SOFT_ROW_METADATA);
    assert_eq!(row_snapshot.get_text_byte_count(), (1 + 2) + 3 + 1);
    assert!(row_snapshot.list_image_cells().is_empty());
}

#[test]
fn a_snapshot_keeps_image_cells_beside_the_encoding_and_gives_them_back() {
    let row_cells = vec![
        build_placeholder_cell(),
        Cell::build_blank(),
        build_fragment_cell(9),
    ];

    let row_snapshot = RowSnapshot::from_row_cells(&row_cells, RowMetadata::default(), Vec::new());

    assert_eq!(
        row_snapshot
            .list_image_cells()
            .iter()
            .map(|image_cell| image_cell.column_index)
            .collect::<Vec<u16>>(),
        vec![0, 2]
    );
    assert_eq!(row_snapshot.decode_row_cells(), row_cells);
    let decoded_without_image_cells =
        row_encoding::decode_row_cells(row_snapshot.get_encoded_row_bytes())
            .expect("the bytes decode")
            .row_cells;
    assert_eq!(decoded_without_image_cells, vec![Cell::build_blank(); 3]);
}

#[test]
fn a_snapshot_encodes_into_the_buffer_it_is_given() {
    let mut encoded_row_bytes = Vec::with_capacity(34);
    encoded_row_bytes.extend_from_slice(b"stale bytes");
    let buffer_address = encoded_row_bytes.as_ptr();

    let row_snapshot = RowSnapshot::from_row_cells(
        &build_line_cells("ab"),
        RowMetadata::default(),
        encoded_row_bytes,
    );

    assert_eq!(
        row_snapshot.get_encoded_row_bytes().as_ptr(),
        buffer_address
    );
    assert_eq!(
        row_snapshot.get_encoded_row_bytes(),
        [
            0x10, 0x02, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x02,
            b'a', b'b', 0x00
        ]
    );
    let given_back_bytes = row_snapshot.into_encoded_row_bytes();
    assert_eq!(given_back_bytes.as_ptr(), buffer_address);
}

#[test]
fn a_given_buffer_over_twice_the_encoded_length_shrinks_to_the_encoded_length() {
    let row_snapshot = RowSnapshot::from_row_cells(
        &build_line_cells("ab"),
        RowMetadata::default(),
        Vec::with_capacity(35),
    );

    assert_eq!(row_snapshot.get_encoded_row_bytes().len(), 17);
    assert_eq!(row_snapshot.compute_heap_byte_count(), 17);
}

#[test]
fn list_row_cells_decodes_once_and_keeps_the_cells() {
    let row_snapshot =
        RowSnapshot::from_row_cells(&build_line_cells("abc"), RowMetadata::default(), Vec::new());
    let heap_byte_count_before = row_snapshot.compute_heap_byte_count();

    let first_row_cells = row_snapshot.list_row_cells();
    let second_row_cells = row_snapshot.list_row_cells();

    assert!(std::ptr::eq(first_row_cells, second_row_cells));
    assert_eq!(first_row_cells, build_line_cells("abc").as_slice());
    assert_eq!(
        row_snapshot.compute_heap_byte_count(),
        heap_byte_count_before + 3 * std::mem::size_of::<Cell>()
    );
}

#[test]
fn snapshots_that_differ_only_in_kept_cells_are_equal() {
    let first_row_snapshot =
        RowSnapshot::from_row_cells(&build_line_cells("abc"), RowMetadata::default(), Vec::new());
    let second_row_snapshot =
        RowSnapshot::from_row_cells(&build_line_cells("abc"), RowMetadata::default(), Vec::new());
    let _ = second_row_snapshot.list_row_cells();

    assert_eq!(first_row_snapshot, second_row_snapshot);
}

#[test]
fn snapshots_with_another_prompt_mark_are_not_equal() {
    let row_cells = build_line_cells("abc");
    let unmarked_row_snapshot =
        RowSnapshot::from_row_cells(&row_cells, RowMetadata::default(), Vec::new());
    let marked_row_snapshot = RowSnapshot::from_row_cells(
        &row_cells,
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
        Vec::new(),
    );

    assert_ne!(unmarked_row_snapshot, marked_row_snapshot);
}

#[test]
fn wire_bytes_read_their_row_end_and_keep_their_decoded_cells() {
    let sent_row_snapshot =
        RowSnapshot::from_row_cells(&build_line_cells("hello"), SOFT_ROW_METADATA, Vec::new());

    let received_row_snapshot =
        RowSnapshot::from_encoded_row_bytes(sent_row_snapshot.get_encoded_row_bytes().to_vec(), 5)
            .expect("the wire bytes decode");

    assert_eq!(received_row_snapshot.get_row_metadata(), SOFT_ROW_METADATA);
    assert_eq!(received_row_snapshot.get_text_byte_count(), 5);
    assert_eq!(
        received_row_snapshot.compute_heap_byte_count(),
        received_row_snapshot.get_encoded_row_bytes().len() + 5 * std::mem::size_of::<Cell>()
    );
    assert_eq!(
        received_row_snapshot.list_row_cells(),
        build_line_cells("hello").as_slice()
    );
    assert_eq!(received_row_snapshot, sent_row_snapshot);
}

#[test]
fn wire_bytes_that_do_not_decode_are_refused() {
    assert_eq!(
        RowSnapshot::from_encoded_row_bytes(Vec::new(), 1),
        Err(RowDecodeError::Truncated)
    );
}

#[test]
fn wire_bytes_of_another_width_are_refused_before_any_cell_is_decoded() {
    let sent_row_snapshot =
        RowSnapshot::from_row_cells(&build_line_cells("hello"), SOFT_ROW_METADATA, Vec::new());
    let sent_row_bytes = sent_row_snapshot.get_encoded_row_bytes();
    let header_only_bytes = sent_row_bytes[..4].to_vec();

    assert_eq!(
        RowSnapshot::from_encoded_row_bytes(header_only_bytes.clone(), 4),
        Err(RowDecodeError::ValueOutOfRange {
            field_name: "column_count",
            decoded_value: 5
        })
    );
    assert_eq!(
        RowSnapshot::from_encoded_row_bytes(header_only_bytes, 5),
        Err(RowDecodeError::BodyPastEnd {
            body_byte_count: sent_row_bytes.len() as u64 - 1,
            available_byte_count: 3
        })
    );
}

#[test]
fn a_stored_row_writes_base64_and_metadata_and_leaves_out_empty_image_cells() {
    let row_snapshot =
        RowSnapshot::from_row_cells(&build_line_cells("ab"), RowMetadata::default(), Vec::new());

    assert_eq!(
        serde_json::to_value(&row_snapshot).expect("the row serializes"),
        serde_json::json!({
            "encoded_row": "EAIAAAEEAAAAAAEAAgJhYgA=",
            "row_metadata": { "row_end": "Hard", "has_prompt_mark": false },
        })
    );
}

#[test]
fn a_stored_row_with_image_cells_reads_back_equal() {
    let row_cells = vec![
        build_fragment_cell(4),
        build_placeholder_cell(),
        Cell::from_character('x', 1, Style::default()),
    ];
    let row_snapshot = RowSnapshot::from_row_cells(
        &row_cells,
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
        Vec::new(),
    );

    let stored_row = serde_json::to_value(&row_snapshot).expect("the row serializes");
    let read_row_snapshot: RowSnapshot =
        serde_json::from_value(stored_row).expect("the row deserializes");

    assert_eq!(read_row_snapshot, row_snapshot);
    assert_eq!(read_row_snapshot.decode_row_cells(), row_cells);
}

#[test]
fn a_stored_row_in_a_longer_encoding_reads_back_in_the_shortest_one() {
    // Four `x` items written one by one, where the encoder writes one item
    // and a repeat exception.
    let longer_encoded_row_bytes = vec![
        0x12, 0x04, 0x00, 0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x04, b'x',
        b'x', b'x', b'x', 0x00,
    ];
    let stored_row = serde_json::json!({
        "encoded_row": STANDARD.encode(&longer_encoded_row_bytes),
        "row_metadata": { "row_end": "Hard", "has_prompt_mark": false },
    });

    let read_row_snapshot: RowSnapshot =
        serde_json::from_value(stored_row).expect("the row deserializes");

    assert_eq!(
        read_row_snapshot,
        RowSnapshot::from_row_cells(
            &build_line_cells("xxxx"),
            RowMetadata::default(),
            Vec::new()
        )
    );
}

/// The message a stored row gives when it is read from `stored_row`.
fn read_stored_row_error(stored_row: serde_json::Value) -> String {
    serde_json::from_value::<RowSnapshot>(stored_row)
        .expect_err("the stored row is refused")
        .to_string()
}

/// The stored form of the 3-cell row `abc`, with `image_cells`.
fn build_stored_row_with_image_cells(image_cells: serde_json::Value) -> serde_json::Value {
    let mut stored_row = serde_json::to_value(RowSnapshot::from_row_cells(
        &build_line_cells("abc"),
        RowMetadata::default(),
        Vec::new(),
    ))
    .expect("the row serializes");
    stored_row["image_cells"] = image_cells;
    stored_row
}

/// The stored form of one image cell at `column_index` with a native image
/// fragment.
fn build_stored_fragment_image_cell(column_index: u16) -> serde_json::Value {
    let fragment_cell = build_fragment_cell(5);
    serde_json::json!({
        "column_index": column_index,
        "cell_extra": serde_json::to_value(
            fragment_cell.get_image_cell_extra().expect("the cell carries a fragment")
        )
        .expect("the cell extra serializes"),
    })
}

#[test]
fn a_stored_row_that_is_not_base64_is_refused() {
    assert_eq!(
        read_stored_row_error(serde_json::json!({
            "encoded_row": "!!",
            "row_metadata": { "row_end": "Hard", "has_prompt_mark": false },
        })),
        "the stored row is not valid base64"
    );
}

#[test]
fn a_stored_row_whose_bytes_do_not_decode_is_refused() {
    assert_eq!(
        read_stored_row_error(serde_json::json!({
            "encoded_row": "",
            "row_metadata": { "row_end": "Hard", "has_prompt_mark": false },
        })),
        format!(
            "the stored row does not decode: {}",
            RowDecodeError::Truncated
        )
    );
}

#[test]
fn a_stored_image_cell_past_the_row_is_refused() {
    assert_eq!(
        read_stored_row_error(build_stored_row_with_image_cells(serde_json::json!([
            build_stored_fragment_image_cell(3)
        ]))),
        "the stored row's image cell at column 3 is past the row's end"
    );
}

#[test]
fn stored_image_cells_out_of_rising_order_are_refused() {
    assert_eq!(
        read_stored_row_error(build_stored_row_with_image_cells(serde_json::json!([
            build_stored_fragment_image_cell(2),
            build_stored_fragment_image_cell(1),
        ]))),
        "the stored row's image cells are not in rising column order"
    );
}

#[test]
fn two_stored_image_cells_in_one_column_are_refused() {
    assert_eq!(
        read_stored_row_error(build_stored_row_with_image_cells(serde_json::json!([
            build_stored_fragment_image_cell(1),
            build_stored_fragment_image_cell(1),
        ]))),
        "the stored row's image cells are not in rising column order"
    );
}

#[test]
fn a_stored_image_cell_with_no_image_is_refused() {
    assert_eq!(
        read_stored_row_error(build_stored_row_with_image_cells(serde_json::json!([{
            "column_index": 0,
            "cell_extra": { "combining": ["\u{0301}"] },
        }]))),
        "the stored row's image cell at column 0 carries no image"
    );
}

#[test]
fn repadding_a_hard_row_gives_the_snapshot_of_its_cells_at_the_new_width() {
    let mut row_cells = build_line_cells("ab");
    row_cells.push(build_fragment_cell(5));
    row_cells.extend([
        Cell::build_blank(),
        Cell::build_blank(),
        Cell::build_blank(),
    ]);
    let row_metadata = RowMetadata {
        row_end: RowEnd::Hard,
        has_prompt_mark: true,
    };
    let row_snapshot = RowSnapshot::from_row_cells(&row_cells, row_metadata, Vec::new());

    let repadded_row = row_snapshot
        .repad_hard_row(4)
        .expect("3 run cells fit 4 columns");

    assert_eq!(repadded_row.run_cell_count, 3);
    assert_eq!(
        repadded_row.row_snapshot,
        RowSnapshot::from_row_cells(&row_cells[..4], row_metadata, Vec::new())
    );
    assert_eq!(
        repadded_row.row_snapshot.get_text_byte_count(),
        row_snapshot.get_text_byte_count()
    );
}

#[test]
fn a_row_that_fills_the_new_width_or_does_not_end_hard_is_not_repadded() {
    let row_cells = build_line_cells("abc");
    let hard_row_snapshot =
        RowSnapshot::from_row_cells(&row_cells, RowMetadata::default(), Vec::new());
    let soft_row_snapshot = RowSnapshot::from_row_cells(&row_cells, SOFT_ROW_METADATA, Vec::new());

    assert!(hard_row_snapshot.repad_hard_row(3).is_none());
    assert!(soft_row_snapshot.repad_hard_row(8).is_none());
}
