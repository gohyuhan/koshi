//! Unit tests for per-pane terminal state.

use super::*;
use crate::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
};
use crate::grid::state::{Cell, Grid, RowEnd, RowMeta};
use crate::scrollback::ScrollbackLimit;
use crate::state::images::{MAX_IMAGE_PLACEMENTS, MAX_IMAGE_STORAGE_BYTES};
use crate::style::{Color, Style};

/// Overwrite the cell at (`row`, `col`) of the active grid with `ch` of the
/// given display `width`, in the default style. Plants wide glyphs as a base
/// `width == 2` cell followed by a `width == 0` continuation.
fn put(state: &mut TerminalState, row: u16, col: u16, ch: char, width: u8) {
    *state.active_grid_mut().cell_mut(row, col).unwrap() = Cell::new(ch, width, Style::default());
}

fn image_record(display: ImageDisplay, anchor: (u16, u16), columns: u32, rows: u32) -> ImageRecord {
    ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: columns,
            height: rows,
            rgba: vec![255; (columns * rows * 4) as usize],
        },
        action: ImageAction::Display,
        display,
        anchor,
    }
}

fn advance(state: &mut TerminalState, bytes: &[u8]) {
    let mut parser = vte::Parser::<{ crate::engine::OSC_CAPACITY }>::new_with_size();
    parser.advance(state, bytes);
}

#[test]
fn new_initializes_both_screens_to_blank_of_size() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 3 });
    assert_eq!(*state.primary, Grid::blank(3, 5, Style::default()));
    assert_eq!(*state.alternate, Grid::blank(3, 5, Style::default()));
}

#[test]
fn new_starts_on_primary_with_default_cursor_style_and_no_title() {
    let state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    assert_eq!(state.active, Screen::Primary);
    let expected_cursor = Cursor {
        row: 0,
        col: 0,
        is_visible: true,
        pending_wrap: false,
        saved: None,
    };
    assert_eq!(state.primary_cursor, expected_cursor);
    assert_eq!(state.alternate_cursor, expected_cursor);
    assert_eq!(state.active_render().charsets, [Charset::default(); 4]);
    assert_eq!(state.active_render().gl, 0);
    assert_eq!(state.active_render().style, Style::default());
    assert_eq!(state.primary_render, state.alternate_render);
    assert_eq!(state.modes, TerminalModes::default());
    assert_eq!(state.title, None);
}

#[test]
fn state_without_shell_metadata_deserializes_as_prompt() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 3 });
    let mut value = serde_json::to_value(&state).expect("state serializes");
    value
        .as_object_mut()
        .expect("state is an object")
        .remove("shell_integration_state");

    let restored: TerminalState = serde_json::from_value(value).expect("legacy state deserializes");

    assert_eq!(
        restored.shell_integration_state,
        ShellIntegrationState::Prompt
    );
}

#[test]
fn state_without_image_fields_deserializes_with_empty_image_state() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 3 });
    let mut value = serde_json::to_value(&state).expect("state serializes");
    let object = value.as_object_mut().expect("state is an object");
    object.remove("primary_image_placements");
    object.remove("primary_image_history");
    object.remove("alternate_image_placements");
    object.remove("next_image_placement_id");

    let restored: TerminalState = serde_json::from_value(value).expect("legacy state deserializes");

    assert_eq!(restored.image_placements(), &[]);
    assert_eq!(restored.next_image_placement_id, 1);
}

#[test]
fn image_placement_records_its_identity_anchor_dimensions_and_cells() {
    let mut state = TerminalState::new(PtySize { cols: 12, rows: 8 });
    let record = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(3),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (4, 7),
        3,
        2,
    );

    state
        .apply_image_record(&record)
        .expect("the image fits the grid");

    let placement = &state.image_placements()[0];
    assert_eq!(placement.id(), 1);
    assert_eq!(placement.record(), &record);
    assert_eq!(placement.anchor(), (4, 7));
    assert_eq!(placement.dimensions(), (2, 3));
    assert_eq!(
        placement.covered_cells().collect::<Vec<_>>(),
        [(4, 7), (4, 8), (4, 9), (5, 7), (5, 8), (5, 9)]
    );
    assert!(placement.covers(4, 7));
    assert!(placement.covers(5, 9));
    assert!(!placement.covers(5, 10));
}

#[test]
fn image_placements_allow_overlap_and_replace_the_same_kitty_identity() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let first = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    let overlap = image_record(
        ImageDisplay {
            image_id: Some(8),
            placement_id: Some(4),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (2, 2),
        1,
        1,
    );
    let replacement = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (4, 4),
        2,
        2,
    );

    state
        .apply_image_record(&first)
        .expect("the first image fits");
    state
        .apply_image_record(&overlap)
        .expect("overlap is supported");
    assert_eq!(state.image_placements().len(), 2);
    assert!(state.image_placements()[0].covers(2, 2));
    assert!(state.image_placements()[1].covers(2, 2));

    state
        .apply_image_record(&replacement)
        .expect("the replacement fits");
    assert_eq!(state.image_placements().len(), 2);
    assert_eq!(state.image_placements()[0].anchor(), (4, 4));
    assert_eq!(state.image_placements()[0].id(), 1);
    assert_eq!(state.image_placements()[1].record(), &overlap);
}

#[test]
fn kitty_transmit_and_display_replaces_all_old_placements_after_validation() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        cell_columns: Some(1),
        cell_rows: Some(1),
        move_cursor: false,
        ..ImageDisplay::default()
    };
    state
        .apply_image_record(&image_record(display.clone(), (0, 0), 1, 1))
        .expect("the primary image fits");
    state.active = Screen::Alternate;
    state
        .apply_image_record(&image_record(display, (1, 1), 1, 1))
        .expect("the alternate image fits");

    let mut retransmit = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(5),
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (2, 2),
        2,
        2,
    );
    retransmit.action = ImageAction::TransmitAndDisplay;
    state
        .apply_image_record(&retransmit)
        .expect("the retransmitted image fits");

    assert_eq!(state.image_placements().len(), 1);
    assert_eq!(state.image_placements()[0].record(), &retransmit);
    state.active = Screen::Primary;
    assert_eq!(state.image_placements(), &[]);
    let restored: TerminalState = serde_json::from_value(
        serde_json::to_value(&state).expect("the retransmitted state serializes"),
    )
    .expect("the retransmitted state deserializes");
    assert_eq!(restored, state);
}

#[test]
fn failed_kitty_transmit_and_display_keeps_old_placements() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let old = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&old).expect("the old image fits");
    let before = state.clone();

    let mut invalid = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(4),
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (7, 7),
        2,
        2,
    );
    invalid.action = ImageAction::TransmitAndDisplay;

    assert_eq!(
        state.apply_image_record(&invalid),
        Err(ImagePlacementError::OutOfBounds {
            row: 7,
            column: 7,
            columns: 2,
            rows: 2,
            grid_rows: 8,
            grid_columns: 8,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn zero_kitty_image_id_does_not_create_a_placement_identity() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let display = ImageDisplay {
        image_id: Some(0),
        placement_id: Some(3),
        cell_columns: Some(1),
        cell_rows: Some(1),
        move_cursor: false,
        ..ImageDisplay::default()
    };
    state
        .apply_image_record(&image_record(display.clone(), (0, 0), 1, 1))
        .expect("the first anonymous image fits");
    state
        .apply_image_record(&image_record(display, (1, 1), 1, 1))
        .expect("the second anonymous image fits");

    assert_eq!(state.image_placements().len(), 2);
    assert_eq!(state.image_placements()[0].anchor(), (0, 0));
    assert_eq!(state.image_placements()[1].anchor(), (1, 1));
}

#[test]
fn rejected_image_placement_leaves_the_complete_state_unchanged() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let valid = image_record(
        ImageDisplay {
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    state.apply_image_record(&valid).expect("the image fits");
    let before = state.clone();

    let zero = image_record(
        ImageDisplay {
            cell_columns: Some(0),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        2,
    );
    assert_eq!(
        state.apply_image_record(&zero),
        Err(ImagePlacementError::ZeroSize {
            columns: 0,
            rows: 2,
        })
    );
    assert_eq!(state, before);

    let out_of_bounds = image_record(
        ImageDisplay {
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (7, 7),
        2,
        2,
    );
    assert_eq!(
        state.apply_image_record(&out_of_bounds),
        Err(ImagePlacementError::OutOfBounds {
            row: 7,
            column: 7,
            columns: 2,
            rows: 2,
            grid_rows: 8,
            grid_columns: 8,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn image_placement_count_limit_leaves_state_unchanged() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    for _ in 0..MAX_IMAGE_PLACEMENTS {
        state
            .apply_image_record(&record)
            .expect("the placement fits the count limit");
    }
    let before = state.clone();

    assert_eq!(
        state.apply_image_record(&record),
        Err(ImagePlacementError::TooManyPlacements {
            count: MAX_IMAGE_PLACEMENTS + 1,
            limit: MAX_IMAGE_PLACEMENTS,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn image_placement_storage_limit_leaves_state_unchanged() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let display = ImageDisplay {
        cell_columns: Some(1),
        cell_rows: Some(1),
        move_cursor: false,
        ..ImageDisplay::default()
    };
    let full = image_record(
        display.clone(),
        (0, 0),
        u32::try_from(MAX_IMAGE_STORAGE_BYTES / 4).expect("the storage limit fits in u32"),
        1,
    );
    state
        .apply_image_record(&full)
        .expect("the image fits the byte limit");
    let before = state.clone();
    let extra = image_record(display, (0, 0), 1, 1);

    assert_eq!(
        state.apply_image_record(&extra),
        Err(ImagePlacementError::StorageLimit {
            used_bytes: MAX_IMAGE_STORAGE_BYTES,
            requested_bytes: 4,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn same_kitty_identity_replacement_reuses_the_storage_budget() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let full_display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        cell_columns: Some(1),
        cell_rows: Some(1),
        move_cursor: false,
        ..ImageDisplay::default()
    };
    let full = image_record(
        full_display.clone(),
        (0, 0),
        u32::try_from(MAX_IMAGE_STORAGE_BYTES / 4).expect("the storage limit fits in u32"),
        1,
    );
    state
        .apply_image_record(&full)
        .expect("the full image fits the byte limit");

    let replacement = image_record(full_display, (1, 1), 1, 1);
    state
        .apply_image_record(&replacement)
        .expect("the same identity may reuse the released bytes");

    assert_eq!(state.image_placements().len(), 1);
    assert_eq!(state.image_placements()[0].record(), &replacement);
    assert_eq!(state.image_placements()[0].id(), 1);
}

#[test]
fn image_dimensions_derive_one_kitty_axis_and_reject_pixel_only_sizes() {
    let mut state = TerminalState::new(PtySize { cols: 12, rows: 8 });
    let derived = image_record(
        ImageDisplay {
            image_id: Some(1),
            cell_columns: Some(3),
            width: Some(ImageDimension::Pixels(4)),
            height: Some(ImageDimension::Pixels(6)),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        4,
        6,
    );
    state
        .apply_image_record(&derived)
        .expect("the missing kitty row count is derived");
    assert_eq!(state.image_placements()[0].dimensions(), (5, 3));

    let pixel_only = image_record(
        ImageDisplay {
            width: Some(ImageDimension::Pixels(4)),
            height: Some(ImageDimension::Pixels(6)),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        4,
        6,
    );
    let before = state.clone();
    assert_eq!(
        state.apply_image_record(&pixel_only),
        Err(ImagePlacementError::MissingCellDimensions {
            width: Some(ImageDimension::Pixels(4)),
            height: Some(ImageDimension::Pixels(6)),
        })
    );
    assert_eq!(state, before);
}

#[test]
fn kitty_source_rectangle_is_validated_before_placement_mutation() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            width: Some(ImageDimension::Pixels(2)),
            source_offset_x: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        3,
        1,
    );
    let before = state.clone();

    assert_eq!(
        state.apply_image_record(&record),
        Err(ImagePlacementError::SourceOutOfBounds {
            x: 2,
            y: 0,
            width: 2,
            height: 1,
            image_width: 3,
            image_height: 1,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn image_placement_rejects_mixed_iterm_cell_and_pixel_units_without_mutation() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let mut record = image_record(
        ImageDisplay {
            width: Some(ImageDimension::Pixels(4)),
            height: Some(ImageDimension::Cells(2)),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        4,
        2,
    );
    record.protocol = GraphicsProtocol::Iterm2;
    let before = state.clone();

    assert_eq!(
        state.apply_image_record(&record),
        Err(ImagePlacementError::UnsupportedCellDimensions {
            width: Some(ImageDimension::Pixels(4)),
            height: Some(ImageDimension::Cells(2)),
        })
    );
    assert_eq!(state, before);
}

#[test]
fn kitty_transmit_removes_matching_images_from_both_screens() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        cell_columns: Some(1),
        cell_rows: Some(1),
        move_cursor: false,
        ..ImageDisplay::default()
    };
    let primary = image_record(display.clone(), (0, 0), 1, 1);
    state
        .apply_image_record(&primary)
        .expect("the primary image fits");

    state.active = Screen::Alternate;
    state
        .apply_image_record(&image_record(display, (1, 1), 1, 1))
        .expect("the alternate image fits");
    let transmit = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        },
        action: ImageAction::Transmit,
        display: ImageDisplay {
            image_id: Some(7),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    state
        .apply_image_record(&transmit)
        .expect("transmit cleanup is not a placement");
    assert_eq!(state.image_placements(), &[]);
    state.active = Screen::Primary;
    assert_eq!(state.image_placements(), &[]);
}

#[test]
fn kitty_transmit_does_not_remove_a_non_kitty_record_with_the_same_id() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let mut iterm = image_record(
        ImageDisplay {
            width: Some(ImageDimension::Cells(1)),
            height: Some(ImageDimension::Cells(1)),
            image_id: Some(7),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    iterm.protocol = GraphicsProtocol::Iterm2;
    state
        .apply_image_record(&iterm)
        .expect("the iTerm2 image fits");

    let transmit = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        },
        action: ImageAction::Transmit,
        display: ImageDisplay {
            image_id: Some(7),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    state
        .apply_image_record(&transmit)
        .expect("the Kitty transmit cleanup succeeds");

    assert_eq!(state.image_placements().len(), 1);
    assert_eq!(state.image_placements()[0].record(), &iterm);
}

#[test]
fn kitty_replacement_does_not_count_non_kitty_records_with_the_same_id() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let mut iterm = image_record(
        ImageDisplay {
            width: Some(ImageDimension::Cells(1)),
            height: Some(ImageDimension::Cells(1)),
            image_id: Some(7),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    iterm.protocol = GraphicsProtocol::Iterm2;
    for _ in 0..MAX_IMAGE_PLACEMENTS {
        state
            .apply_image_record(&iterm)
            .expect("the non-Kitty placement fits");
    }

    let retransmit = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255; 4],
        },
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            image_id: Some(7),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    assert_eq!(
        state.apply_image_record(&retransmit),
        Err(ImagePlacementError::TooManyPlacements {
            count: MAX_IMAGE_PLACEMENTS + 1,
            limit: MAX_IMAGE_PLACEMENTS,
        })
    );
    assert_eq!(state.image_placements().len(), MAX_IMAGE_PLACEMENTS);
}

#[test]
fn image_placements_survive_serde_and_primary_reflow() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    state.apply_image_record(&record).expect("the image fits");

    let restored: TerminalState =
        serde_json::from_value(serde_json::to_value(&state).expect("state serializes"))
            .expect("state deserializes");
    assert_eq!(restored, state);

    state.resize(PtySize { cols: 4, rows: 4 });
    assert_eq!(state.image_placements().len(), 1);
    assert_eq!(state.image_placements()[0].anchor(), (1, 1));
    assert_eq!(state.image_placements()[0].record(), &record);
    assert_eq!(
        serde_json::from_value::<TerminalState>(
            serde_json::to_value(&state).expect("reflowed state serializes")
        )
        .expect("reflowed state deserializes"),
        state
    );
}

#[test]
fn primary_image_placement_follows_rows_into_history_and_scrolled_views() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(8, 1024));
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        2,
    );
    state.apply_image_record(&record).expect("the image fits");

    advance(&mut state, b"\x1b[2;1H\n");
    assert_eq!(state.scrollback().len(), 1);
    assert_eq!(state.image_placements(), &[]);
    let first_view = state.image_placements_for_view(1);
    assert_eq!(first_view.len(), 1);
    assert_eq!(first_view[0].anchor(), (0, 0));
    assert_eq!(first_view[0].dimensions(), (2, 1));

    advance(&mut state, b"\x1b[2;1H\n");
    assert_eq!(state.scrollback().len(), 2);
    assert_eq!(state.image_placements(), &[]);
    let second_view = state.image_placements_for_view(2);
    assert_eq!(second_view.len(), 1);
    assert_eq!(second_view[0].anchor(), (0, 0));
    assert_eq!(second_view[0].record(), &record);
    assert!(state.image_placements_for_view(0).is_empty());

    let restored: TerminalState =
        serde_json::from_value(serde_json::to_value(&state).expect("history state serializes"))
            .expect("history state deserializes");
    assert_eq!(restored, state);
    assert_eq!(restored.image_placements_for_view(2), second_view);
}

#[test]
fn serialized_primary_history_image_must_fit_the_primary_width() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(8, 1024));
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");
    advance(&mut state, b"\x1b[2;1H\n");

    let mut value = serde_json::to_value(&state).expect("history state serializes");
    value["primary_image_history"][0]["anchor"] = serde_json::json!([0, 4]);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("a history image outside the primary width must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement at primary row 0, column 4 with 1 columns exceeds the 4-column primary grid"
    );
}

#[test]
fn serialized_primary_history_image_must_fit_the_retained_row_range() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(8, 1024));
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");
    advance(&mut state, b"\x1b[2;1H\n");

    let mut value = serde_json::to_value(&state).expect("history state serializes");
    value["primary_image_history"][0]["anchor"] = serde_json::json!([2, 0]);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("a history image outside the retained rows must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement at primary row 2, column 0 with 1 columns by 1 rows exceeds retained primary rows 0 up to but not including 3"
    );
}

#[test]
fn serialized_primary_row_counter_must_leave_room_for_live_rows() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    let mut value = serde_json::to_value(&state).expect("state serializes");
    value["scrollback"]["total_pushed"] = serde_json::json!(u64::MAX);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("a live-row range that overflows u64 must be rejected");
    assert_eq!(
        error.to_string(),
        "primary image row range at 18446744073709551615 with 2 live rows overflows u64"
    );
}

#[test]
fn serialized_scrollback_cannot_exceed_its_absolute_row_count() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    state
        .scrollback
        .push_row(&[Cell::blank()], RowMeta::default());

    let mut value = serde_json::to_value(&state).expect("state serializes");
    value["scrollback"]["total_pushed"] = serde_json::json!(0);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("retained rows must have nonnegative absolute row numbers");
    assert_eq!(
        error.to_string(),
        "primary scrollback row count 1 exceeds total pushed count 0"
    );
}

#[test]
fn primary_image_placement_is_removed_as_one_rectangle_when_history_evicts_it() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(1, 1024));
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    advance(&mut state, b"\x1b[2;1H\n\x1b[2;1H\n");
    assert_eq!(state.scrollback().len(), 1);
    assert!(state.image_placements().is_empty());
    assert!(state.primary_image_history.is_empty());
    assert!(state.image_placements_for_view(1).is_empty());
}

#[test]
fn primary_image_placement_crossing_the_live_screen_is_cleared_by_ed_2() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(8, 1024));
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        2,
    );
    state.apply_image_record(&record).expect("the image fits");

    advance(&mut state, b"\x1b[2;1H\n");
    assert!(state.image_placements().is_empty());
    let crossing_view = state.image_placements_for_view(1);
    assert_eq!(crossing_view.len(), 1);
    assert_eq!(crossing_view[0].anchor(), (0, 0));

    advance(&mut state, b"\x1b[2J");
    assert!(state.image_placements().is_empty());
    assert!(state.image_placements_for_view(1).is_empty());
}

#[test]
fn ed_3_removes_primary_image_placements_from_cleared_history() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(8, 1024));
    let record = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    advance(&mut state, b"\x1b[2;1H\n");
    assert_eq!(state.image_placements_for_view(1).len(), 1);

    advance(&mut state, b"\x1b[3J");
    assert!(state.scrollback().is_empty());
    assert!(state.image_placements_for_view(1).is_empty());
}

#[test]
fn kitty_replacement_replaces_a_primary_history_placement() {
    let mut state =
        TerminalState::with_scrollback(PtySize { cols: 4, rows: 2 }, ScrollbackLimit::new(8, 1024));
    let first = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state
        .apply_image_record(&first)
        .expect("the first image fits");
    advance(&mut state, b"\x1b[2;1H\n");
    assert_eq!(state.image_placements_for_view(1).len(), 1);

    let other = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 2),
        1,
        1,
    );
    state
        .apply_image_record(&other)
        .expect("the other image fits");

    let replacement = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 1),
        1,
        1,
    );
    state
        .apply_image_record(&replacement)
        .expect("the replacement fits");

    assert_eq!(state.image_placements().len(), 2);
    assert_eq!(state.image_placements()[0].id(), 1);
    assert_eq!(state.image_placements()[0].anchor(), (0, 1));
    assert_eq!(state.image_placements()[0].record(), &replacement);
    assert_eq!(state.image_placements()[1].id(), 2);
    assert_eq!(state.image_placements()[1].anchor(), (1, 2));
    let live_view = state.image_placements_for_view(0);
    assert_eq!(live_view.len(), 2);
    assert_eq!(live_view[0].anchor(), (0, 1));
    assert_eq!(live_view[1].anchor(), (1, 2));
    let scrolled_view = state.image_placements_for_view(1);
    assert_eq!(scrolled_view.len(), 1);
    assert_eq!(scrolled_view[0].anchor(), (1, 1));
}

#[test]
fn primary_image_drops_when_reflow_width_has_no_columns() {
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    state.resize(PtySize { cols: 0, rows: 2 });

    assert!(state.image_placements().is_empty());
    assert!(state.image_placements_for_view(0).is_empty());
}

#[test]
fn primary_image_anchor_follows_text_through_width_reflow() {
    let mut state = TerminalState::new(PtySize { cols: 6, rows: 3 });
    advance(&mut state, b"abcdef");
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 4),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    state.resize(PtySize { cols: 3, rows: 3 });

    assert_eq!(state.image_placements().len(), 1);
    assert_eq!(state.image_placements()[0].anchor(), (1, 1));
    assert_eq!(state.image_placements()[0].record(), &record);
}

#[test]
fn primary_image_rectangle_is_dropped_when_reflow_cannot_fit_it() {
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 3 });
    advance(&mut state, b"abc");
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(2),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 1),
        2,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    state.resize(PtySize { cols: 2, rows: 3 });

    assert!(state.image_placements().is_empty());
    assert!(state.image_placements_for_view(0).is_empty());
}

#[test]
fn primary_multi_row_image_drops_when_reflow_changes_its_columns() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 3 });
    for (column, ch) in "abcd".chars().enumerate() {
        put(&mut state, 0, column as u16, ch, 1);
    }
    for (column, ch) in "ef".chars().enumerate() {
        put(&mut state, 1, column as u16, ch, 1);
    }
    state.active_grid_mut().set_row_end(0, RowEnd::Soft);

    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 1),
        1,
        2,
    );
    state.apply_image_record(&record).expect("the image fits");

    state.resize(PtySize { cols: 3, rows: 3 });

    assert!(state.image_placements().is_empty());
    assert!(state.image_placements_for_view(0).is_empty());
}

#[test]
fn malformed_serialized_image_placement_is_rejected_before_use() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    state.apply_image_record(&record).expect("the image fits");

    let mut value = serde_json::to_value(&state).expect("state serializes");
    let placement = value
        .get_mut("primary_image_placements")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|placements| placements.first_mut())
        .expect("the serialized placement exists");
    placement["anchor"] = serde_json::json!([u16::MAX, 0]);
    placement["record"]["anchor"] = serde_json::json!([u16::MAX, 0]);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("an overflowing placement must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement coordinate extent does not fit in u16"
    );
}

#[test]
fn serialized_image_placement_outside_grid_is_rejected() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(2),
            cell_rows: Some(2),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    state.apply_image_record(&record).expect("the image fits");

    let mut value = serde_json::to_value(&state).expect("state serializes");
    let placement = value
        .get_mut("primary_image_placements")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|placements| placements.first_mut())
        .expect("the serialized placement exists");
    placement["anchor"] = serde_json::json!([7, 7]);
    placement["record"]["anchor"] = serde_json::json!([7, 7]);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("an out-of-grid placement must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement at row 7, column 7 with 2 columns by 2 rows exceeds the 8-row by 8-column grid"
    );
}

#[test]
fn duplicate_serialized_image_placement_identity_is_rejected() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    let mut value = serde_json::to_value(&state).expect("state serializes");
    let placement = value["primary_image_placements"][0].clone();
    value["primary_image_placements"] = serde_json::json!([placement.clone(), placement]);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("duplicate placement state must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement identities must be unique per screen"
    );
}

#[test]
fn serialized_image_placement_identity_cannot_repeat_across_screens() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    let mut value = serde_json::to_value(&state).expect("state serializes");
    let placement = value["primary_image_placements"][0].clone();
    value["alternate_image_placements"] = serde_json::json!([placement]);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("a cross-screen identity collision must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement identities must be unique across screens"
    );
}

#[test]
fn serialized_image_placement_count_is_bounded_before_state_use() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    state.apply_image_record(&record).expect("the image fits");

    let mut value = serde_json::to_value(&state).expect("state serializes");
    let placement = value["primary_image_placements"][0].clone();
    value["primary_image_placements"] = serde_json::Value::Array(
        (1..=MAX_IMAGE_PLACEMENTS + 1)
            .map(|id| {
                let mut placement = placement.clone();
                placement["id"] = serde_json::json!(id);
                placement
            })
            .collect(),
    );

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("a placement-count overflow must be rejected");
    assert_eq!(
        error.to_string(),
        format!(
            "image placement count {} exceeds the limit of {}",
            MAX_IMAGE_PLACEMENTS + 1,
            MAX_IMAGE_PLACEMENTS
        )
    );
}

#[test]
fn serialized_image_placement_next_identity_must_be_nonzero() {
    let state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    let mut value = serde_json::to_value(&state).expect("state serializes");
    value["next_image_placement_id"] = serde_json::json!(0);

    let error = serde_json::from_value::<TerminalState>(value)
        .expect_err("a zero next identity must be rejected");
    assert_eq!(
        error.to_string(),
        "next image placement identity must be nonzero"
    );
}

#[test]
fn image_placement_identity_exhaustion_leaves_state_unchanged() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 8 });
    state.next_image_placement_id = ImagePlacementId::MAX;
    let record = image_record(
        ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    let before = state.clone();

    assert_eq!(
        state.apply_image_record(&record),
        Err(ImagePlacementError::IdentityExhausted)
    );
    assert_eq!(state, before);
}

#[test]
fn active_grid_follows_active_screen() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    assert!(std::ptr::eq(state.active_grid(), state.primary.as_ref()));
    state.active = Screen::Alternate;
    assert!(std::ptr::eq(state.active_grid(), state.alternate.as_ref()));
}

#[test]
fn active_grid_mut_follows_active_screen() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    assert_eq!(
        state.active_grid_mut(),
        &Grid::blank(2, 4, Style::default())
    );
    state.active = Screen::Alternate;
    assert_eq!(
        state.active_grid_mut(),
        &Grid::blank(2, 4, Style::default())
    );
}

#[test]
fn resize_reallocs_both_grids_to_new_size() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(*state.primary, Grid::blank(5, 10, Style::default()));
    assert_eq!(*state.alternate, Grid::blank(5, 10, Style::default()));
}

#[test]
fn resize_pads_each_grid_with_its_own_screen_background() {
    // Padding a resize creates is filled with that screen's own render
    // background, never the other screen's. On the reflowed primary,
    // fully-default blanks count as padding, so they re-fill too — the same
    // background-color-erase fill every erase and scroll uses. Content cells
    // (anything non-default) keep their own styles.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    put(&mut state, 0, 0, 'x', 1);
    state.primary_render.style.set_bg(Color::Indexed(4)); // primary: blue
    state.alternate_render.style.set_bg(Color::Indexed(1)); // alternate: red
    state.resize(PtySize { cols: 6, rows: 3 });

    let mut blue_fill = Style::default();
    blue_fill.set_bg(Color::Indexed(4)); // bg-only: fg + attrs stay default
    let mut red_fill = Style::default();
    red_fill.set_bg(Color::Indexed(1));

    // Content keeps its own style.
    assert_eq!(
        state.primary.cell(0, 0),
        Some(&Cell::new('x', 1, Style::default()))
    );
    // Primary padding — re-created row tails and the new bottom row — takes
    // the primary fill.
    assert_eq!(state.primary.cell(0, 5), Some(&Cell::blank_with(blue_fill)));
    assert_eq!(state.primary.cell(2, 3), Some(&Cell::blank_with(blue_fill)));
    // The alternate crops in place: its untouched cells stay default and
    // only the grown region takes the alternate fill.
    assert_eq!(state.alternate.cell(0, 0), Some(&Cell::blank()));
    assert_eq!(
        state.alternate.cell(0, 5),
        Some(&Cell::blank_with(red_fill))
    );
    assert_eq!(
        state.alternate.cell(2, 3),
        Some(&Cell::blank_with(red_fill))
    );
}

#[test]
fn resize_clamps_out_of_bounds_cursor_to_last_cell() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_cursor.row = 23;
    state.primary_cursor.col = 79;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(state.primary_cursor.row, 4);
    assert_eq!(state.primary_cursor.col, 9);
}

#[test]
fn resize_leaves_in_bounds_cursor_untouched() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_cursor.row = 2;
    state.primary_cursor.col = 3;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(state.primary_cursor.row, 2);
    assert_eq!(state.primary_cursor.col, 3);
}

#[test]
fn resize_clears_a_pending_wrap_latched_to_the_old_edge() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_cursor.pending_wrap = true;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert!(!state.primary_cursor.pending_wrap);
}

#[test]
fn resize_preserves_cell_contents_across_width_and_height_changes() {
    let mut state = TerminalState::new(PtySize { cols: 6, rows: 4 });
    put(&mut state, 0, 0, 'h', 1);
    put(&mut state, 0, 1, 'i', 1);
    put(&mut state, 1, 0, '!', 1);
    state.primary_cursor.row = 1;

    // Shrink: trailing blank rows go first, the written rows stay put.
    state.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'h');
    assert_eq!(state.primary.cell(0, 1).unwrap().ch(), 'i');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), '!');
    assert_eq!(state.scrollback.len(), 0);

    // Grow back: the content is still where it was, new space is blank.
    state.resize(PtySize { cols: 6, rows: 4 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'h');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), '!');
    assert_eq!(state.primary.cell(3, 5), Some(&Cell::blank()));
}

#[test]
fn resize_shrink_pushes_top_rows_to_scrollback_and_grow_pulls_them_back() {
    // Every row written, cursor on the last row: nothing blank to trim, so a
    // 2-row shrink scrolls the top two rows into history.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 4 });
    for row in 0..4 {
        put(&mut state, row, 0, char::from(b'a' + row as u8), 1);
    }
    state.primary_cursor.row = 3;

    state.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(state.scrollback.len(), 2);
    assert_eq!(state.scrollback.lines()[0].0[0].ch(), 'a');
    assert_eq!(state.scrollback.lines()[1].0[0].ch(), 'b');
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'c');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), 'd');
    // The cursor followed its row up.
    assert_eq!(state.primary_cursor.row, 1);

    // Growing pulls the same rows back in at the top, newest first.
    state.resize(PtySize { cols: 4, rows: 4 });
    assert_eq!(state.scrollback.len(), 0);
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'a');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), 'b');
    assert_eq!(state.primary.cell(2, 0).unwrap().ch(), 'c');
    assert_eq!(state.primary.cell(3, 0).unwrap().ch(), 'd');
    assert_eq!(state.primary_cursor.row, 3);
}

#[test]
fn resize_width_shrink_wraps_a_wide_glyph_whole() {
    // 世 occupies cols 2–3; at width 3 its base would land in the last
    // column, so the reflow leaves a spacer there and wraps the glyph whole
    // onto the next row — never a dangling half.
    let mut state = TerminalState::new(PtySize { cols: 5, rows: 2 });
    put(&mut state, 0, 0, 'a', 1);
    put(&mut state, 0, 2, '世', 2);
    put(&mut state, 0, 3, ' ', 0);

    state.resize(PtySize { cols: 3, rows: 2 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'a');
    assert_eq!(state.primary.cell(0, 2), Some(&Cell::blank()));
    assert_eq!(state.primary.row_end(0), RowEnd::SoftWide);
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), '世');
    assert_eq!(state.primary.cell(1, 0).unwrap().width(), 2);
    assert_eq!(state.primary.cell(1, 1).unwrap().width(), 0);
}

#[test]
fn resize_to_zero_rows_pushes_all_content_into_scrollback_without_panicking() {
    // A pane driven to zero height (e.g. mid-drag in the layout) must not
    // panic; every row it held becomes history, and growing back pulls the
    // same rows back in, in order.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 3 });
    put(&mut state, 0, 0, 'a', 1);
    put(&mut state, 1, 0, 'b', 1);
    put(&mut state, 2, 0, 'c', 1);

    state.resize(PtySize { cols: 4, rows: 0 });
    // A zero-row grid reports zero columns too: `dimensions()` derives cols
    // from the first row, and there is no first row (see the grid-level
    // `dimensions_of_grids_with_a_zero_axis` test for the same rule).
    assert_eq!(state.primary.dimensions(), (0, 0));
    assert_eq!(state.scrollback.len(), 3);
    assert_eq!(state.primary_cursor.row, 0); // clamped: no row to sit on
    assert_eq!(state.primary_cursor.col, 0);

    state.resize(PtySize { cols: 4, rows: 3 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'a');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), 'b');
    assert_eq!(state.primary.cell(2, 0).unwrap().ch(), 'c');
    assert_eq!(state.scrollback.len(), 0);
}

#[test]
fn empty_zero_row_resize_does_not_create_scrollback_history() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 0 });

    state.resize(PtySize { cols: 4, rows: 0 });

    assert_eq!(state.primary.dimensions(), (0, 0));
    assert_eq!(state.scrollback.len(), 0);
    assert_eq!(state.scrollback.total_pushed(), 0);
}

#[test]
fn resize_to_zero_cols_yields_a_zero_width_grid_without_panicking() {
    // A zero-width grid has no cells to hold text; the erased content (there
    // is nowhere for it to live at width 0) does not resurface on regrow.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    put(&mut state, 0, 0, 'h', 1);
    put(&mut state, 0, 1, 'i', 1);

    state.resize(PtySize { cols: 0, rows: 2 });
    assert_eq!(state.primary.dimensions(), (2, 0));
    assert_eq!(state.primary_cursor.col, 0);

    state.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(state.primary.dimensions(), (2, 4));
    assert_eq!(state.primary.cell(0, 0), Some(&Cell::blank()));
}

#[test]
fn resize_alternate_screen_crops_without_touching_scrollback() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 3 });
    state.active = Screen::Alternate;
    for row in 0..3 {
        put(&mut state, row, 0, char::from(b'x' + row as u8), 1);
    }
    state.active_grid_mut().set_prompt_mark(1, true);

    state.resize(PtySize { cols: 4, rows: 2 });
    // The top row is cropped away — the alternate screen has no history.
    assert_eq!(state.scrollback.len(), 0);
    assert_eq!(state.alternate.cell(0, 0).unwrap().ch(), 'y');
    assert!(state.alternate.prompt_mark(0));
    assert_eq!(state.alternate.cell(1, 0).unwrap().ch(), 'z');
    assert!(!state.alternate.prompt_mark(1));
}

#[test]
fn new_starts_with_an_empty_scrollback() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 3 });
    assert!(state.scrollback().is_empty());
    assert_eq!(state.scrollback().len(), 0);
    assert_eq!(state.scrollback().dropped_lines(), 0);
    assert_eq!(state.scrollback().dropped_bytes(), 0);
}

/// A row of `s`, one default-styled cell per char — a scrollback line fixture.
fn line(s: &str) -> Vec<Cell> {
    s.chars()
        .map(|ch| Cell::new(ch, 1, Style::default()))
        .collect()
}

/// Read `row` of `grid` as a string; blank cells read as spaces.
fn grid_row(grid: &Grid, row: u16) -> String {
    let (_, cols) = grid.dimensions();
    (0..cols)
        .map(|c| grid.cell(row, c).map(Cell::ch).unwrap_or(' '))
        .collect()
}

/// A 3-wide, 2-row primary screen with live rows `L0`/`L1` and three retained
/// history rows `h0`/`h1`/`h2` (oldest first).
fn state_with_history() -> TerminalState {
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    for (col, ch) in "L0.".chars().enumerate() {
        *state.active_grid_mut().cell_mut(0, col as u16).unwrap() =
            Cell::new(ch, 1, Style::default());
    }
    for (col, ch) in "L1.".chars().enumerate() {
        *state.active_grid_mut().cell_mut(1, col as u16).unwrap() =
            Cell::new(ch, 1, Style::default());
    }
    state.scrollback.push_row(&line("h0."), RowMeta::default());
    state.scrollback.push_row(&line("h1."), RowMeta::default());
    state.scrollback.push_row(&line("h2."), RowMeta::default());
    state
}

#[test]
fn scrolled_view_at_offset_zero_shares_the_live_buffer() {
    let state = state_with_history();
    // Offset 0 follows live: the same Arc (no compose, no copy) and effective 0.
    let (grid, effective) = state.scrolled_view(0);
    assert!(Arc::ptr_eq(&grid, &state.active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_composes_history_above_the_live_screen() {
    let state = state_with_history();
    // Offset 1: newest history row on top, top live row below.
    let (grid, effective) = state.scrolled_view(1);
    assert_eq!(grid.dimensions(), (2, 3));
    assert_eq!(grid_row(&grid, 0), "h2.");
    assert_eq!(grid_row(&grid, 1), "L0.");
    assert_eq!(effective, 1);
}

#[test]
fn scrolled_view_keeps_a_history_row_prompt_mark() {
    let mut state = state_with_history();
    state.scrollback.push_row(
        &line("prompt"),
        RowMeta {
            end: RowEnd::Hard,
            prompt: true,
        },
    );

    let (grid, _) = state.scrolled_view(1);

    assert!(grid.prompt_mark(0));
    assert!(!grid.prompt_mark(1));
}

#[test]
fn scrolled_view_at_the_screen_height_shows_only_history() {
    let state = state_with_history();
    // Offset 2 == the 2-row screen height: both rows come from history.
    let (grid, effective) = state.scrolled_view(2);
    assert_eq!(grid_row(&grid, 0), "h1.");
    assert_eq!(grid_row(&grid, 1), "h2.");
    assert_eq!(effective, 2);
}

#[test]
fn scrolled_view_clamps_an_over_scroll_to_the_oldest_line() {
    let state = state_with_history();
    // Three history rows, screen height 2: offset 3 shows the oldest window,
    // and any larger offset clamps — grid and effective offset both — to that
    // same window rather than reading past.
    let (grid, effective) = state.scrolled_view(3);
    assert_eq!(grid_row(&grid, 0), "h0.");
    assert_eq!(grid_row(&grid, 1), "h1.");
    assert_eq!(effective, 3);

    let (over, over_effective) = state.scrolled_view(99);
    assert_eq!(grid_row(&over, 0), "h0.");
    assert_eq!(grid_row(&over, 1), "h1.");
    assert_eq!(over_effective, 3); // clamped to the retained count
}

#[test]
fn scrolled_view_on_the_alternate_screen_reports_a_live_zero_offset() {
    let mut state = state_with_history();
    state.active = Screen::Alternate; // full-screen apps keep no scrollback
    let (grid, effective) = state.scrolled_view(5);
    // The alternate screen always shows live: the live Arc and a zero effective
    // offset, so the indicator and cursor never treat it as scrolled.
    assert!(Arc::ptr_eq(&grid, &state.active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_with_empty_history_follows_live() {
    let state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    let (grid, effective) = state.scrolled_view(5);
    assert!(Arc::ptr_eq(&grid, &state.active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_pads_history_rows_with_the_blanks_that_were_trimmed() {
    // A history row holding `ab` on a 3-wide screen: its third cell was a
    // default blank and is not stored. The app has since set a background pen
    // (SGR 48), which must not reach back and repaint that column — the cell
    // was default when the line scrolled off, and scrolling back shows the
    // line as it was, not as the running program currently paints.
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    state.primary_render.style.set_bg(Color::Indexed(4));
    state.scrollback.push_row(&line("ab"), RowMeta::default());

    let (grid, _) = state.scrolled_view(1);
    let padded = grid.cell(0, 2).unwrap();
    assert_eq!(padded.ch(), ' ');
    assert_eq!(padded.style(), Style::default());
}

#[test]
fn scrolled_view_keeps_a_history_rows_own_background() {
    // The other half: color a program actually painted into a blank cell is
    // content, so it is stored and drawn — a full-width colored bar in history
    // still shows its color after scrolling back.
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    let mut red = Style::default();
    red.set_bg(Color::Indexed(1));
    state
        .scrollback
        .push_row(&vec![Cell::blank_with(red); 3], RowMeta::default());

    let (grid, _) = state.scrolled_view(1);
    for col in 0..3 {
        assert_eq!(grid.cell(0, col).unwrap().style().bg(), Color::Indexed(1));
    }
}

#[test]
fn text_view_on_the_alternate_screen_reads_its_grid_alone() {
    // The scrollback belongs to the primary and is still retained while the
    // alternate screen is up, so the alternate's view must hold its own grid
    // alone: its top row is the first readable row, and the primary's history
    // rows read as gone.
    let mut state = state_with_history();
    state.active = Screen::Alternate;

    // Three rows were pushed into history, so the live top row is absolute
    // row 3 and the screen's two rows are 3 and 4.
    let view = state.text_view();
    assert_eq!(view.first_row(), 3);
    assert_eq!(view.last_row(), 4);
    assert_eq!(view.row(2).map(|(cells, _)| cells.len()), None);

    // The primary's own view still reaches back over the same history.
    state.active = Screen::Primary;
    assert_eq!(state.text_view().first_row(), 0);
}

#[test]
fn resize_blanks_a_wide_glyph_the_alternate_screen_cuts_in_half() {
    // The alternate screen crops instead of reflowing. 世 occupies cols 2-3;
    // cropping to 3 columns drops its right half, so the base left in the last
    // column is blanked rather than drawn as a half glyph.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 1 });
    state.active = Screen::Alternate;
    put(&mut state, 0, 0, 'a', 1);
    put(&mut state, 0, 2, '世', 2);
    put(&mut state, 0, 3, ' ', 0);

    state.resize(PtySize { cols: 3, rows: 1 });
    assert_eq!(state.alternate.cell(0, 0).unwrap().ch(), 'a');
    assert_eq!(state.alternate.cell(0, 2), Some(&Cell::blank()));
}

#[test]
fn resize_moves_the_alternate_cursor_up_by_the_rows_cropped_off_the_top() {
    // Alternate height 4 -> 2 crops the two top rows away. The cursor sat on
    // row 2 holding `c`, so it lands on row 0 with `c` still under it — not on
    // the last row, which is where a bare clamp would leave it.
    let mut state = TerminalState::new(PtySize { cols: 2, rows: 4 });
    state.active = Screen::Alternate;
    for (row, ch) in "abcd".chars().enumerate() {
        put(&mut state, row as u16, 0, ch, 1);
    }
    state.alternate_cursor.row = 2;
    state.alternate_cursor.col = 1;

    state.resize(PtySize { cols: 2, rows: 2 });
    assert_eq!(state.alternate.cell(0, 0).unwrap().ch(), 'c');
    assert_eq!(state.alternate_cursor.row, 0);
    assert_eq!(state.alternate_cursor.col, 1);
}
