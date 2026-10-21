//! Unit tests for per-pane terminal state.

use super::*;

impl TerminalState {
    /// Whether autowrap (DECAWM `?7`) is active. Default on.
    pub(crate) fn is_autowrap_enabled(&self) -> bool {
        self.modes.is_autowrap_enabled
    }
}
use crate::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
};
use crate::grid::state::tests::build_line_cells;
use crate::grid::state::{Cell, Grid, RowEnd, RowMetadata};
use crate::scrollback::ScrollbackLimit;
use crate::state::images::{MAX_IMAGE_PLACEMENT_COUNT, MAX_IMAGE_STORAGE_BYTE_COUNT};
use crate::style::{Color, Style};

/// Make the alternate screen active, with its grid allocated, as a switch
/// to it does. The render state stays as it is.
pub(super) fn switch_to_alternate_screen(terminal_state: &mut TerminalState) {
    terminal_state.allocate_alternate_grid();
    terminal_state.active_screen = Screen::Alternate;
}

/// The alternate screen's grid.
///
/// # Panics
/// Panics when the alternate screen holds no grid.
fn get_alternate_grid(terminal_state: &TerminalState) -> &Grid {
    terminal_state
        .alternate
        .get_allocated_grid()
        .expect("the alternate screen holds a grid")
}

/// Overwrite the cell at (`row_index`, `column_index`) of the active grid with
/// `character` of the given display `cell_width`, in the default style. Plants
/// wide glyphs as a base `cell_width == 2` cell followed by a `cell_width == 0`
/// continuation.
fn set_terminal_cell(
    terminal_state: &mut TerminalState,
    row_index: u16,
    column_index: u16,
    character: char,
    cell_width: u8,
) {
    *terminal_state
        .get_active_grid_mut()
        .get_cell_mut(row_index, column_index)
        .unwrap() = Cell::from_character(character, cell_width, Style::default());
}

fn build_image_record(
    image_display: ImageDisplay,
    image_anchor: (u16, u16),
    column_count: u32,
    row_count: u32,
) -> ImageRecord {
    ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: column_count,
            pixel_height: row_count,
            rgba_bytes: vec![255; (column_count * row_count * 4) as usize],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: image_display,
        anchor: image_anchor,
    }
}

/// Feed `input_bytes` through a fresh parser into `terminal_state` as the
/// engine does: each run of printed characters goes to
/// [`TerminalState::print_text`] at once. A clone fed the same bytes one
/// printed character at a time through `print` must end equal to
/// `terminal_state`, and the call panics when it does not.
pub(crate) fn process_terminal_bytes(terminal_state: &mut TerminalState, input_bytes: &[u8]) {
    let mut single_character_state = terminal_state.clone();
    let mut single_character_parser =
        vte::Parser::<{ crate::engine::OSC_BUFFER_BYTE_CAPACITY }>::new_with_size();
    single_character_parser.advance(&mut single_character_state, input_bytes);

    let mut parser = vte::Parser::<{ crate::engine::OSC_BUFFER_BYTE_CAPACITY }>::new_with_size();
    crate::engine::advance_terminal_parser(
        &mut parser,
        terminal_state,
        &mut String::new(),
        input_bytes,
    );

    assert!(
        *terminal_state == single_character_state,
        "printed runs and single-character prints of {:?} leave different states",
        String::from_utf8_lossy(input_bytes)
    );
}

#[test]
fn new_starts_the_primary_blank_and_the_alternate_with_no_grid() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    assert_eq!(
        terminal_state.primary,
        Grid::build_blank(3, 5, Style::default())
    );
    assert_eq!(
        terminal_state.alternate,
        AlternateScreen::Blank {
            fill_style: Style::default()
        }
    );
}

#[test]
fn new_starts_on_primary_with_default_cursor_style_and_no_title() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    let expected_cursor = Cursor {
        row: 0,
        column: 0,
        is_visible: true,
        is_wrap_pending: false,
        is_origin_mode_enabled: false,
        saved: None,
    };
    assert_eq!(terminal_state.primary_cursor, expected_cursor);
    assert_eq!(terminal_state.alternate_cursor, expected_cursor);
    assert_eq!(
        terminal_state.get_active_render().charsets,
        [Charset::default(); 4]
    );
    assert_eq!(terminal_state.get_active_render().gl, 0);
    assert_eq!(terminal_state.get_active_render().style, Style::default());
    assert_eq!(
        terminal_state.primary_render,
        terminal_state.alternate_render
    );
    assert_eq!(terminal_state.modes, TerminalModes::default());
    assert_eq!(terminal_state.title, None);
}

#[test]
fn state_missing_any_field_the_writer_emits_is_refused() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let serialized_field_names: Vec<String> = serialized_state
        .as_object()
        .expect("state is an object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(serialized_field_names.len(), 32);

    for serialized_field_name in serialized_field_names {
        let mut incomplete_state = serialized_state.clone();
        incomplete_state
            .as_object_mut()
            .expect("state is an object")
            .remove(&serialized_field_name);

        let parse_error = serde_json::from_value::<TerminalState>(incomplete_state)
            .expect_err("a state missing a written field is refused");

        assert_eq!(
            parse_error.to_string(),
            format!("missing field `{serialized_field_name}`")
        );
    }
}

#[test]
fn state_round_trip_preserves_origin_and_horizontal_margins() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 5,
    });
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[2;4r\x1b[?69h\x1b[2;6s\x1b[?6h\x1b[?47h\x1b[3;7s",
    );

    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    assert_eq!(
        serialized_state["primary_horizontal_margins"],
        serde_json::json!([1, 5])
    );
    assert_eq!(
        serialized_state["alternate_horizontal_margins"],
        serde_json::json!([2, 6])
    );

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("state deserializes");

    assert_eq!(restored.primary_horizontal_margins, Some((1, 5)));
    assert_eq!(restored.alternate_horizontal_margins, Some((2, 6)));
    assert!(restored.modes.is_left_right_margin_mode_enabled);
    assert!(restored.primary_cursor.is_origin_mode_enabled);
}

#[test]
fn state_round_trip_preserves_each_screen_keyboard_stack() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 5,
    });
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[>1u\x1b[>4u\x1b[?1049h\x1b[>8u\x1b[>17u",
    );

    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    assert_eq!(
        serialized_state["primary_keyboard_stack"],
        serde_json::json!([1, 4])
    );
    assert_eq!(
        serialized_state["alternate_keyboard_stack"],
        serde_json::json!([8, 17])
    );

    let mut restored: TerminalState =
        serde_json::from_value(serialized_state).expect("state deserializes");

    assert_eq!(restored.get_keyboard_flags(), 17);
    process_terminal_bytes(&mut restored, b"\x1b[?1049l");
    assert_eq!(restored.get_keyboard_flags(), 4);
}

#[test]
fn state_deserialization_cuts_an_overlong_keyboard_stack_to_its_newest_entries() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["primary_keyboard_stack"] = serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 16]);

    let mut restored: TerminalState =
        serde_json::from_value(serialized_state).expect("state deserializes");

    assert_eq!(restored.get_keyboard_flags(), 16);
    process_terminal_bytes(&mut restored, b"\x1b[<8u");
    assert_eq!(restored.get_keyboard_flags(), 0);
}

#[test]
fn state_round_trip_keeps_one_screen_stack_empty() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h\x1b[>8u");

    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    assert_eq!(
        serialized_state["primary_keyboard_stack"],
        serde_json::json!([])
    );
    assert_eq!(
        serialized_state["alternate_keyboard_stack"],
        serde_json::json!([8])
    );

    let mut restored: TerminalState =
        serde_json::from_value(serialized_state).expect("state deserializes");

    assert_eq!(restored.get_keyboard_flags(), 8);
    process_terminal_bytes(&mut restored, b"\x1b[?1049l");
    assert_eq!(restored.get_keyboard_flags(), 0);
}

#[test]
fn state_deserialization_rejects_reversed_horizontal_margins() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["primary_horizontal_margins"] = serde_json::json!([4, 2]);

    let deserialization_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("reversed margins must be rejected");
    assert_eq!(
        deserialization_error.to_string(),
        "primary horizontal margins (4, 2) must satisfy 0 <= left < right <= 4"
    );
}

#[test]
fn state_deserialization_rejects_horizontal_margins_past_grid_edge() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["alternate_horizontal_margins"] = serde_json::json!([1, 5]);

    let deserialization_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("out-of-bounds margins must be rejected");
    assert_eq!(
        deserialization_error.to_string(),
        "alternate horizontal margins (1, 5) must satisfy 0 <= left < right <= 4"
    );
}

#[test]
fn state_deserialization_rejects_single_column_horizontal_margins() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["modes"]["is_left_right_margin_mode_enabled"] = serde_json::json!(true);
    serialized_state["primary_horizontal_margins"] = serde_json::json!([2, 2]);

    let deserialization_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("single-column margins must be rejected");
    assert_eq!(
        deserialization_error.to_string(),
        "primary horizontal margins (2, 2) must satisfy 0 <= left < right <= 4"
    );
}

#[test]
fn state_deserialization_normalizes_single_column_full_width_margins() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 1,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["modes"]["is_left_right_margin_mode_enabled"] = serde_json::json!(true);
    serialized_state["primary_horizontal_margins"] = serde_json::json!([0, 0]);

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("full-width margins are valid");

    assert_eq!(restored.primary_horizontal_margins, None);
}

#[test]
fn state_deserialization_clears_margins_when_declrmm_is_disabled() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["primary_horizontal_margins"] = serde_json::json!([1, 3]);
    serialized_state["alternate_horizontal_margins"] = serde_json::json!([0, 4]);

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("disabled margins can be normalized");

    assert_eq!(restored.primary_horizontal_margins, None);
    assert_eq!(restored.alternate_horizontal_margins, None);
    assert!(!restored.modes.is_left_right_margin_mode_enabled);
}

#[test]
fn state_deserialization_normalizes_full_width_horizontal_margins() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["modes"]["is_left_right_margin_mode_enabled"] = serde_json::json!(true);
    serialized_state["primary_horizontal_margins"] = serde_json::json!([0, 4]);
    serialized_state["alternate_horizontal_margins"] = serde_json::json!([1, 3]);

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("full-width margins are valid");

    assert_eq!(restored.primary_horizontal_margins, None);
    assert_eq!(restored.alternate_horizontal_margins, Some((1, 3)));
    assert!(restored.modes.is_left_right_margin_mode_enabled);
}

#[test]
fn image_placement_records_its_identity_anchor_dimensions_and_cells() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 12,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(3),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (4, 7),
        3,
        2,
    );

    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");

    let placement = &terminal_state.list_image_placements()[0];
    assert_eq!(placement.get_image_placement_id(), 1);
    assert_eq!(placement.get_image_record(), &image_record);
    assert_eq!(placement.get_image_anchor(), (4, 7));
    assert_eq!(placement.get_image_cell_dimensions(), (2, 3));
    assert_eq!(
        placement.list_covered_cells().collect::<Vec<_>>(),
        [(4, 7), (4, 8), (4, 9), (5, 7), (5, 8), (5, 9)]
    );
    assert!(placement.is_cell_covered(4, 7));
    assert!(placement.is_cell_covered(5, 9));
    assert!(!placement.is_cell_covered(5, 10));
}

#[test]
fn independent_static_records_keep_distinct_content_identities() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let shared_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255],
    });
    let image_display = ImageDisplay {
        requested_width: Some(ImageDimension::Cells(1)),
        requested_height: Some(ImageDimension::Cells(1)),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let first_image_record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::clone(&shared_image),
        animation: None,
        action: ImageAction::Display,
        display: image_display.clone(),
        anchor: (0, 0),
    };
    let second_image_record = ImageRecord {
        anchor: (1, 1),
        ..first_image_record.clone()
    };

    terminal_state
        .apply_image_record(&first_image_record)
        .expect("the first static image fits");
    terminal_state
        .apply_image_record(&second_image_record)
        .expect("the second static image fits");

    let placements = terminal_state.list_image_placements();
    assert_eq!(placements.len(), 2);
    assert!(Arc::ptr_eq(
        &placements[0].get_image_record().image,
        &shared_image
    ));
    assert!(Arc::ptr_eq(
        &placements[1].get_image_record().image,
        &shared_image
    ));
    assert_ne!(
        placements[0].get_image_content_id(),
        placements[1].get_image_content_id()
    );
}

#[test]
fn serialized_content_table_is_deduplicated_and_rebuilds_the_same_state() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let shared_image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![255, 0, 0, 255],
    });
    let image_display = ImageDisplay {
        requested_width: Some(ImageDimension::Cells(1)),
        requested_height: Some(ImageDimension::Cells(1)),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let first_image_record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::clone(&shared_image),
        animation: None,
        action: ImageAction::Display,
        display: image_display.clone(),
        anchor: (0, 0),
    };
    let second_image_record = ImageRecord {
        anchor: (1, 1),
        ..first_image_record.clone()
    };
    terminal_state
        .apply_image_record(&first_image_record)
        .expect("the first static image fits");
    terminal_state
        .apply_image_record(&second_image_record)
        .expect("the second static image fits");

    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    assert_eq!(
        serialized_state["image_contents"].as_array().map(Vec::len),
        Some(2)
    );
    for placement in serialized_state["primary_image_placements"]
        .as_array()
        .expect("placements are an array")
    {
        assert!(placement["image_record"].get("decoded_image").is_none());
        assert!(placement["raster"].is_null());
    }

    let restored: TerminalState = serde_json::from_value(serialized_state).expect("state restores");
    assert_eq!(restored, terminal_state);
    assert_ne!(
        restored.list_image_placements()[0].get_image_content_id(),
        restored.list_image_placements()[1].get_image_content_id()
    );
}

#[test]
fn serialized_content_table_rejects_duplicate_dangling_and_extra_entries() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");
    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let image_content = serialized_state["image_contents"][0].clone();

    let mut duplicate = serialized_state.clone();
    duplicate["image_contents"] = serde_json::json!([image_content.clone(), image_content.clone()]);
    let deserialize_error = serde_json::from_value::<TerminalState>(duplicate)
        .expect_err("duplicate content identities must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image content identities must be unique"
    );

    let mut dangling = serialized_state.clone();
    dangling["primary_image_placements"][0]["image_content_id"] = serde_json::json!(999);
    let deserialize_error = serde_json::from_value::<TerminalState>(dangling)
        .expect_err("a missing content identity must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement refers to missing content"
    );

    let mut unreferenced_content_state = serialized_state.clone();
    let mut unreferenced_image_content = image_content.clone();
    unreferenced_image_content["image_content_id"] = serde_json::json!(999);
    unreferenced_content_state["image_contents"] =
        serde_json::json!([image_content.clone(), unreferenced_image_content]);
    let deserialize_error = serde_json::from_value::<TerminalState>(unreferenced_content_state)
        .expect_err("unreferenced content must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image content table contains unreferenced entries"
    );

    let mut collision = serialized_state;
    collision["next_image_content_id"] = image_content["image_content_id"].clone();
    let deserialize_error = serde_json::from_value::<TerminalState>(collision)
        .expect_err("the next content identity cannot collide");
    assert_eq!(
        deserialize_error.to_string(),
        "next image content identity collides with retained content"
    );
}

#[test]
fn serialized_image_placements_require_a_raster_plan_and_a_bounded_combined_count() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");
    let serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let mut planless_state = serialized_state.clone();
    planless_state["primary_image_placements"][0]
        .as_object_mut()
        .expect("placement is an object")
        .remove("plan");
    let deserialize_error = serde_json::from_value::<TerminalState>(planless_state)
        .expect_err("a placement must carry its raster plan");
    assert_eq!(deserialize_error.to_string(), "missing field `plan`");

    let placement = serialized_state["primary_image_placements"][0].clone();
    let mut placements = Vec::with_capacity(MAX_IMAGE_PLACEMENT_COUNT);
    for placement_id in 1..=MAX_IMAGE_PLACEMENT_COUNT {
        let mut placement = placement.clone();
        placement["image_placement_id"] = serde_json::json!(placement_id);
        placements.push(placement);
    }
    let mut count_overflow = serialized_state;
    count_overflow["primary_image_placements"] = serde_json::Value::Array(placements);
    count_overflow["alternate_image_placements"] = serde_json::json!([placement]);
    let deserialize_error = serde_json::from_value::<TerminalState>(count_overflow)
        .expect_err("combined image placement lists must be bounded");
    assert_eq!(
        deserialize_error.to_string(),
        format!(
            "image placement count {} exceeds the limit of {}",
            MAX_IMAGE_PLACEMENT_COUNT + 1,
            MAX_IMAGE_PLACEMENT_COUNT
        )
    );
}

#[test]
fn raw_image_state_deserialization_charges_every_image_content_to_one_budget() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let image_content = serialized_state["image_contents"][0].clone();
    let image_contents = (1..=5)
        .map(|raw_image_content_id| {
            let mut numbered_image_content = image_content.clone();
            numbered_image_content["image_content_id"] = serde_json::json!(raw_image_content_id);
            numbered_image_content
        })
        .collect();
    serialized_state["image_contents"] = serde_json::Value::Array(image_contents);

    let mut budget = images::ImageStateBudget::with_byte_limit(16);
    use serde::de::IntoDeserializer;

    let deserializer = serialized_state.into_deserializer();
    let deserialize_error = match RawTerminalStateFields::deserialize_with_budget(
        deserializer,
        &mut budget,
        StoredStateShape::Current,
    ) {
        Ok(_) => panic!("the fifth 4-byte image content must exceed the 16-byte budget"),
        Err(deserialize_error) => deserialize_error,
    };
    assert_eq!(
        deserialize_error.to_string(),
        "image state RGBA data exceeds the remaining storage budget of 0 bytes"
    );
}

#[test]
fn image_placements_allow_overlap_and_replace_the_same_kitty_identity() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let initial_image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    let overlap = build_image_record(
        ImageDisplay {
            image_id: Some(8),
            placement_id: Some(4),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (2, 2),
        1,
        1,
    );
    let replacement = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (4, 4),
        2,
        2,
    );

    terminal_state
        .apply_image_record(&initial_image_record)
        .expect("the first image fits");
    terminal_state
        .apply_image_record(&overlap)
        .expect("overlap is supported");
    assert_eq!(terminal_state.list_image_placements().len(), 2);
    assert!(terminal_state.list_image_placements()[0].is_cell_covered(2, 2));
    assert!(terminal_state.list_image_placements()[1].is_cell_covered(2, 2));

    terminal_state
        .apply_image_record(&replacement)
        .expect("the replacement fits");
    assert_eq!(terminal_state.list_image_placements().len(), 2);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (4, 4)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_placement_id(),
        1
    );
    assert_eq!(
        terminal_state.list_image_placements()[1].get_image_record(),
        &overlap
    );
}

#[test]
fn kitty_transmit_and_display_replaces_all_old_placements_after_validation() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    terminal_state
        .apply_image_record(&build_image_record(image_display.clone(), (0, 0), 1, 1))
        .expect("the primary image fits");
    switch_to_alternate_screen(&mut terminal_state);
    terminal_state
        .apply_image_record(&build_image_record(image_display, (1, 1), 1, 1))
        .expect("the alternate image fits");

    let mut retransmit = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(5),
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (2, 2),
        2,
        2,
    );
    retransmit.action = ImageAction::TransmitAndDisplay;
    terminal_state
        .apply_image_record(&retransmit)
        .expect("the retransmitted image fits");

    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_record(),
        &retransmit
    );
    terminal_state.active_screen = Screen::Primary;
    assert_eq!(terminal_state.list_image_placements(), &[]);
    let restored: TerminalState = serde_json::from_value(
        serde_json::to_value(&terminal_state).expect("the retransmitted state serializes"),
    )
    .expect("the retransmitted state deserializes");
    assert_eq!(restored, terminal_state);
}

#[test]
fn failed_kitty_transmit_and_display_keeps_old_placements() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let existing_image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&existing_image_record)
        .expect("the old image fits");
    let state_before_invalid_record = terminal_state.clone();

    let mut invalid_image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(4),
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (8, 8),
        2,
        2,
    );
    invalid_image_record.action = ImageAction::TransmitAndDisplay;

    assert_eq!(
        terminal_state.apply_image_record(&invalid_image_record),
        Err(ImagePlacementError::OutOfBounds {
            anchor_row: 8,
            anchor_column: 8,
            column_count: 2,
            row_count: 2,
            grid_rows: 8,
            grid_columns: 8,
        })
    );
    assert_eq!(terminal_state, state_before_invalid_record);
}

#[test]
fn zero_kitty_image_id_does_not_create_a_placement_identity() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_display = ImageDisplay {
        image_id: Some(0),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    terminal_state
        .apply_image_record(&build_image_record(image_display.clone(), (0, 0), 1, 1))
        .expect("the first anonymous image fits");
    terminal_state
        .apply_image_record(&build_image_record(image_display, (1, 1), 1, 1))
        .expect("the second anonymous image fits");

    assert_eq!(terminal_state.list_image_placements().len(), 2);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (0, 0)
    );
    assert_eq!(
        terminal_state.list_image_placements()[1].get_image_anchor(),
        (1, 1)
    );
}

#[test]
fn rejected_image_placement_leaves_the_complete_state_unchanged() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let valid_image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    terminal_state
        .apply_image_record(&valid_image_record)
        .expect("the image fits");
    let state_before_rejected_records = terminal_state.clone();

    let zero_width_image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(0),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        2,
    );
    assert_eq!(
        terminal_state.apply_image_record(&zero_width_image_record),
        Err(ImagePlacementError::ZeroSize {
            column_count: 0,
            row_count: 2,
        })
    );
    assert_eq!(terminal_state, state_before_rejected_records);

    let out_of_bounds = build_image_record(
        ImageDisplay {
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (8, 8),
        2,
        2,
    );
    assert_eq!(
        terminal_state.apply_image_record(&out_of_bounds),
        Err(ImagePlacementError::OutOfBounds {
            anchor_row: 8,
            anchor_column: 8,
            column_count: 2,
            row_count: 2,
            grid_rows: 8,
            grid_columns: 8,
        })
    );
    assert_eq!(terminal_state, state_before_rejected_records);
}

#[test]
fn image_placement_count_limit_leaves_state_unchanged() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    for _ in 0..MAX_IMAGE_PLACEMENT_COUNT {
        terminal_state
            .apply_image_record(&image_record)
            .expect("the placement fits the count limit");
    }
    let state_before_placement_limit = terminal_state.clone();

    assert_eq!(
        terminal_state.apply_image_record(&image_record),
        Err(ImagePlacementError::TooManyPlacements {
            placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        })
    );
    assert_eq!(terminal_state, state_before_placement_limit);
}

#[test]
fn image_placement_storage_limit_leaves_state_unchanged() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_display = ImageDisplay {
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let full_size_image_record = build_image_record(image_display.clone(), (0, 0), 16_384, 1_024);
    terminal_state
        .apply_image_record(&full_size_image_record)
        .expect("the image fits the byte limit");
    let state_before_storage_limit = terminal_state.clone();
    let extra_image_record = build_image_record(image_display, (0, 0), 1, 1);

    assert_eq!(
        terminal_state.apply_image_record(&extra_image_record),
        Err(ImagePlacementError::StorageLimit {
            used_byte_count: MAX_IMAGE_STORAGE_BYTE_COUNT,
            requested_byte_count: 4,
            byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
        })
    );
    assert_eq!(terminal_state, state_before_storage_limit);
}

#[test]
fn same_kitty_identity_replacement_reuses_the_storage_budget() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let full_display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let full_size_image_record = build_image_record(full_display.clone(), (0, 0), 16_384, 1_024);
    terminal_state
        .apply_image_record(&full_size_image_record)
        .expect("the full image fits the byte limit");

    let replacement = build_image_record(full_display, (1, 1), 1, 1);
    terminal_state
        .apply_image_record(&replacement)
        .expect("the same identity may reuse the released bytes");

    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_record(),
        &replacement
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_placement_id(),
        1
    );
}

#[test]
fn image_dimensions_derive_one_kitty_axis_and_reject_pixel_only_sizes() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 12,
        row_count: 8,
    });
    terminal_state.set_cell_size(
        koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 10)
            .expect("nonzero cell size"),
    );
    let derived_image_record = build_image_record(
        ImageDisplay {
            image_id: Some(1),
            requested_column_count: Some(3),
            requested_width: Some(ImageDimension::Pixels(4)),
            requested_height: Some(ImageDimension::Pixels(6)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        4,
        6,
    );
    terminal_state
        .apply_image_record(&derived_image_record)
        .expect("the missing kitty row count is derived");
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_cell_dimensions(),
        (5, 3)
    );
    terminal_state.cell_size = None;

    let pixel_only_image_record = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Pixels(4)),
            requested_height: Some(ImageDimension::Pixels(6)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        4,
        6,
    );
    let state_before_missing_cell_dimensions = terminal_state.clone();
    assert_eq!(
        terminal_state.apply_image_record(&pixel_only_image_record),
        Err(ImagePlacementError::MissingCellDimensions {
            requested_width: Some(ImageDimension::Pixels(4)),
            requested_height: Some(ImageDimension::Pixels(6)),
        })
    );
    assert_eq!(terminal_state, state_before_missing_cell_dimensions);
}

#[test]
fn kitty_source_rectangle_is_validated_before_placement_mutation() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            requested_width: Some(ImageDimension::Pixels(2)),
            source_pixel_offset_x: Some(3),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        3,
        1,
    );
    let state_before_source_bounds_error = terminal_state.clone();

    assert_eq!(
        terminal_state.apply_image_record(&image_record),
        Err(ImagePlacementError::SourceOutOfBounds {
            source_x: 3,
            source_y: 0,
            source_pixel_width: 2,
            source_pixel_height: 1,
            image_pixel_width: 3,
            image_pixel_height: 1,
        })
    );
    assert_eq!(terminal_state, state_before_source_bounds_error);
}

#[test]
fn image_placement_rejects_mixed_iterm_cell_and_pixel_units_without_mutation() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let mut image_record = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Pixels(4)),
            requested_height: Some(ImageDimension::Cells(2)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        4,
        2,
    );
    image_record.protocol = GraphicsProtocol::Iterm2;
    let state_before_mixed_unit_error = terminal_state.clone();

    assert_eq!(
        terminal_state.apply_image_record(&image_record),
        Err(ImagePlacementError::UnsupportedCellDimensions {
            requested_width: Some(ImageDimension::Pixels(4)),
            requested_height: Some(ImageDimension::Cells(2)),
        })
    );
    assert_eq!(terminal_state, state_before_mixed_unit_error);
}

#[test]
fn kitty_transmit_removes_matching_images_from_both_screens() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let primary_image_record = build_image_record(image_display.clone(), (0, 0), 1, 1);
    terminal_state
        .apply_image_record(&primary_image_record)
        .expect("the primary image fits");

    switch_to_alternate_screen(&mut terminal_state);
    terminal_state
        .apply_image_record(&build_image_record(image_display, (1, 1), 1, 1))
        .expect("the alternate image fits");
    let transmit = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255; 4],
        })
        .into(),
        animation: None,
        action: ImageAction::Transmit,
        display: ImageDisplay {
            image_id: Some(7),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    terminal_state
        .apply_image_record(&transmit)
        .expect("transmit cleanup is not a placement");
    assert_eq!(terminal_state.list_image_placements(), &[]);
    terminal_state.active_screen = Screen::Primary;
    assert_eq!(terminal_state.list_image_placements(), &[]);
}

#[test]
fn kitty_transmit_does_not_remove_a_non_kitty_record_with_the_same_id() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let mut iterm = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            image_id: Some(7),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    iterm.protocol = GraphicsProtocol::Iterm2;
    terminal_state
        .apply_image_record(&iterm)
        .expect("the iTerm2 image fits");

    let transmit = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255; 4],
        })
        .into(),
        animation: None,
        action: ImageAction::Transmit,
        display: ImageDisplay {
            image_id: Some(7),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    terminal_state
        .apply_image_record(&transmit)
        .expect("the Kitty transmit cleanup succeeds");

    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_record(),
        &iterm
    );
}

#[test]
fn kitty_replacement_does_not_count_non_kitty_records_with_the_same_id() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let mut sixel = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            image_id: Some(7),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    sixel.protocol = GraphicsProtocol::Sixel;
    for _ in 0..MAX_IMAGE_PLACEMENT_COUNT {
        terminal_state
            .apply_image_record(&sixel)
            .expect("the non-Kitty placement fits");
    }

    let retransmit = ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255; 4],
        })
        .into(),
        animation: None,
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            image_id: Some(7),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    assert_eq!(
        terminal_state.apply_image_record(&retransmit),
        Err(ImagePlacementError::TooManyPlacements {
            placement_count: MAX_IMAGE_PLACEMENT_COUNT + 1,
            placement_limit: MAX_IMAGE_PLACEMENT_COUNT,
        })
    );
    assert_eq!(
        terminal_state.list_image_placements().len(),
        MAX_IMAGE_PLACEMENT_COUNT
    );
}

#[test]
fn image_placements_survive_serde_and_primary_reflow() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let restored: TerminalState =
        serde_json::from_value(serde_json::to_value(&terminal_state).expect("state serializes"))
            .expect("state deserializes");
    assert_eq!(restored, terminal_state);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 4,
    });
    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (1, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_record(),
        &image_record
    );
    assert_eq!(
        serde_json::from_value::<TerminalState>(
            serde_json::to_value(&terminal_state).expect("reflowed state serializes")
        )
        .expect("reflowed state deserializes"),
        terminal_state
    );
}

#[test]
fn primary_image_placement_follows_rows_into_history_and_scrolled_views() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(8, 1024),
    );
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        2,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    assert_eq!(terminal_state.list_image_placements(), &[]);
    let first_view = terminal_state.list_image_placements_for_view(1);
    assert_eq!(first_view.len(), 1);
    assert_eq!(first_view[0].get_image_anchor(), (0, 0));
    assert_eq!(first_view[0].get_image_cell_dimensions(), (2, 1));

    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 2);
    assert_eq!(terminal_state.list_image_placements(), &[]);
    let second_view = terminal_state.list_image_placements_for_view(2);
    assert_eq!(second_view.len(), 1);
    assert_eq!(second_view[0].get_image_anchor(), (0, 0));
    assert_eq!(second_view[0].get_image_record(), &image_record);
    assert!(terminal_state.list_image_placements_for_view(0).is_empty());

    let restored: TerminalState = serde_json::from_value(
        serde_json::to_value(&terminal_state).expect("history state serializes"),
    )
    .expect("history state deserializes");
    assert_eq!(restored, terminal_state);
    assert_eq!(restored.list_image_placements_for_view(2), second_view);
}

#[test]
fn serialized_primary_history_image_must_fit_the_primary_width() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(8, 1024),
    );
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");

    let mut serialized_state =
        serde_json::to_value(&terminal_state).expect("history state serializes");
    serialized_state["primary_image_history"][0]["anchor"] = serde_json::json!([0, 4]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a history image outside the primary width must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement at primary row 0, column 4 with 1 columns exceeds the 4-column primary grid"
    );
}

#[test]
fn serialized_primary_history_image_must_fit_the_retained_row_range() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(8, 1024),
    );
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");

    let mut serialized_state =
        serde_json::to_value(&terminal_state).expect("history state serializes");
    serialized_state["primary_image_history"][0]["anchor"] = serde_json::json!([2, 0]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a history image outside the retained rows must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement at primary row 2, column 0 with 1 columns by 1 rows exceeds retained primary rows 0 up to but not including 3"
    );
}

#[test]
fn serialized_primary_row_counter_must_leave_room_for_live_rows() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["scrollback"]["total_pushed_line_count"] = serde_json::json!(u64::MAX);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a live-row range that overflows u64 must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "primary image row range at 18446744073709551615 with 2 live rows overflows u64"
    );
}

#[test]
fn serialized_scrollback_cannot_exceed_its_absolute_row_count() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    terminal_state
        .scrollback
        .push_row_cells(&[Cell::build_blank()], RowMetadata::default());

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["scrollback"]["total_pushed_line_count"] = serde_json::json!(0);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("retained rows must have nonnegative absolute row numbers");
    assert_eq!(
        deserialize_error.to_string(),
        "primary scrollback row count 1 exceeds total pushed count 0"
    );
}

#[test]
fn primary_image_placement_is_removed_as_one_rectangle_when_history_evicts_it() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(1, 1024),
    );
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n\x1b[2;1H\n");
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    assert!(terminal_state.list_image_placements().is_empty());
    assert!(terminal_state.primary_image_history.is_empty());
    assert!(terminal_state.list_image_placements_for_view(1).is_empty());
}

#[test]
fn primary_image_placement_crossing_the_live_screen_is_cleared_by_ed_2() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(8, 1024),
    );
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        2,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");
    assert!(terminal_state.list_image_placements().is_empty());
    let crossing_view = terminal_state.list_image_placements_for_view(1);
    assert_eq!(crossing_view.len(), 1);
    assert_eq!(crossing_view[0].get_image_anchor(), (0, 0));

    process_terminal_bytes(&mut terminal_state, b"\x1b[2J");
    assert!(terminal_state.list_image_placements().is_empty());
    assert!(terminal_state.list_image_placements_for_view(1).is_empty());
}

#[test]
fn ed_3_removes_primary_image_placements_from_cleared_history() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(8, 1024),
    );
    let image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");
    assert_eq!(terminal_state.list_image_placements_for_view(1).len(), 1);

    process_terminal_bytes(&mut terminal_state, b"\x1b[3J");
    assert!(terminal_state.get_scrollback().is_empty());
    assert!(terminal_state.list_image_placements_for_view(1).is_empty());
}

#[test]
fn kitty_replacement_replaces_a_primary_history_placement() {
    let mut terminal_state = TerminalState::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(8, 1024),
    );
    let original_kitty_image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&original_kitty_image_record)
        .expect("the first image fits");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");
    assert_eq!(terminal_state.list_image_placements_for_view(1).len(), 1);

    let unrelated_image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 2),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&unrelated_image_record)
        .expect("the other image fits");

    let replacement = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 1),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&replacement)
        .expect("the replacement fits");

    assert_eq!(terminal_state.list_image_placements().len(), 2);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_placement_id(),
        1
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (0, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_record(),
        &replacement
    );
    assert_eq!(
        terminal_state.list_image_placements()[1].get_image_placement_id(),
        2
    );
    assert_eq!(
        terminal_state.list_image_placements()[1].get_image_anchor(),
        (1, 2)
    );
    let live_view = terminal_state.list_image_placements_for_view(0);
    assert_eq!(live_view.len(), 2);
    assert_eq!(live_view[0].get_image_anchor(), (0, 1));
    assert_eq!(live_view[1].get_image_anchor(), (1, 2));
    let scrolled_view = terminal_state.list_image_placements_for_view(1);
    assert_eq!(scrolled_view.len(), 1);
    assert_eq!(scrolled_view[0].get_image_anchor(), (1, 1));
}

#[test]
fn primary_image_drops_when_reflow_width_has_no_columns() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    terminal_state.resize_terminal_state(PtySize {
        column_count: 0,
        row_count: 2,
    });

    assert!(terminal_state.list_image_placements().is_empty());
    assert!(terminal_state.list_image_placements_for_view(0).is_empty());
}

#[test]
fn primary_image_anchor_follows_text_through_width_reflow() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"abcdef");
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 4),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    terminal_state.resize_terminal_state(PtySize {
        column_count: 3,
        row_count: 3,
    });

    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (1, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_record(),
        &image_record
    );
}

#[test]
fn primary_image_rectangle_is_clipped_when_reflow_narrows_the_grid() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"abc");
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(2),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 1),
        2,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    terminal_state.resize_terminal_state(PtySize {
        column_count: 2,
        row_count: 3,
    });

    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (0, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_cell_dimensions(),
        (1, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0]
            .get_image_geometry()
            .full_size,
        koshi_core::geometry::Size {
            column_count: 2,
            row_count: 1
        }
    );
    assert_eq!(
        terminal_state.list_image_placements_for_view(0),
        terminal_state.list_image_placements()
    );
}

#[test]
fn primary_multi_row_image_keeps_its_scale_when_text_reflows() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    for (column, character) in "abcd".chars().enumerate() {
        set_terminal_cell(&mut terminal_state, 0, column as u16, character, 1);
    }
    for (column, character) in "ef".chars().enumerate() {
        set_terminal_cell(&mut terminal_state, 1, column as u16, character, 1);
    }
    terminal_state
        .get_active_grid_mut()
        .set_row_end(0, RowEnd::Soft);

    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 1),
        1,
        2,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    terminal_state.resize_terminal_state(PtySize {
        column_count: 3,
        row_count: 3,
    });

    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (0, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_cell_dimensions(),
        (2, 1)
    );
    assert_eq!(
        terminal_state.list_image_placements_for_view(0),
        terminal_state.list_image_placements()
    );
}

#[test]
fn malformed_serialized_image_placement_is_rejected_before_use() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let placement = serialized_state
        .get_mut("primary_image_placements")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|placements| placements.first_mut())
        .expect("the serialized placement exists");
    placement["anchor"] = serde_json::json!([u16::MAX, 0]);
    placement["image_record"]["anchor"] = serde_json::json!([u16::MAX, 0]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("an overflowing placement must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement coordinate extent does not fit in u16"
    );
}

#[test]
fn serialized_image_placement_outside_grid_is_rejected() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(2),
            requested_row_count: Some(2),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (1, 1),
        2,
        2,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let placement = serialized_state
        .get_mut("primary_image_placements")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|placements| placements.first_mut())
        .expect("the serialized placement exists");
    placement["anchor"] = serde_json::json!([7, 7]);
    placement["image_record"]["anchor"] = serde_json::json!([7, 7]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("an out-of-grid placement must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement at row 7, column 7 with 2 columns by 2 rows exceeds the 8-row by 8-column grid"
    );
}

#[test]
fn duplicate_serialized_image_placement_identity_is_rejected() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            placement_id: Some(3),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let placement = serialized_state["primary_image_placements"][0].clone();
    serialized_state["primary_image_placements"] =
        serde_json::json!([placement.clone(), placement]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("duplicate placement state must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement identities must be unique per screen"
    );
}

#[test]
fn a_serialized_kitty_placement_without_its_upload_is_rejected() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let mut image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    image_record.action = ImageAction::TransmitAndDisplay;
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    assert_eq!(
        serialized_state["kitty_images"]
            .as_array()
            .expect("the uploads serialize as a list")
            .len(),
        1
    );
    serialized_state["kitty_images"] = serde_json::json!([]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a placement naming a missing upload must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "a Kitty image placement names an image id no retained upload holds"
    );
}

#[test]
fn serialized_image_placement_identity_cannot_repeat_across_screens() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");
    terminal_state.allocate_alternate_grid();

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let placement = serialized_state["primary_image_placements"][0].clone();
    serialized_state["alternate_image_placements"] = serde_json::json!([placement]);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a cross-screen identity collision must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "image placement identities must be unique across screens"
    );
}

#[test]
fn serialized_image_placement_count_is_bounded_before_state_use() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let placement = serialized_state["primary_image_placements"][0].clone();
    serialized_state["primary_image_placements"] = serde_json::Value::Array(
        (1..=MAX_IMAGE_PLACEMENT_COUNT + 1)
            .map(|placement_id| {
                let mut placement = placement.clone();
                placement["image_placement_id"] = serde_json::json!(placement_id);
                placement
            })
            .collect(),
    );

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a placement-count overflow must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        format!(
            "image placement count {} exceeds the limit of {}",
            MAX_IMAGE_PLACEMENT_COUNT + 1,
            MAX_IMAGE_PLACEMENT_COUNT
        )
    );
}

#[test]
fn serialized_image_placement_next_identity_must_be_nonzero() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["next_image_placement_id"] = serde_json::json!(0);

    let deserialize_error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a zero next identity must be rejected");
    assert_eq!(
        deserialize_error.to_string(),
        "next image placement identity must be nonzero"
    );
}

#[test]
fn image_placement_identity_exhaustion_leaves_state_unchanged() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    terminal_state.next_image_placement_id = ImagePlacementId::MAX;
    let image_record = build_image_record(
        ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    let state_before_identity_exhaustion = terminal_state.clone();

    assert_eq!(
        terminal_state.apply_image_record(&image_record),
        Err(ImagePlacementError::IdentityExhausted)
    );
    assert_eq!(terminal_state, state_before_identity_exhaustion);
}

#[test]
fn active_grid_follows_active_screen() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert!(std::ptr::eq(
        terminal_state.get_active_grid(),
        &terminal_state.primary
    ));
    switch_to_alternate_screen(&mut terminal_state);
    assert!(std::ptr::eq(
        terminal_state.get_active_grid(),
        get_alternate_grid(&terminal_state)
    ));
}

#[test]
fn active_grid_mut_follows_active_screen() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(
        terminal_state.get_active_grid_mut(),
        &Grid::build_blank(2, 4, Style::default())
    );
    switch_to_alternate_screen(&mut terminal_state);
    assert_eq!(
        terminal_state.get_active_grid_mut(),
        &Grid::build_blank(2, 4, Style::default())
    );
}

#[test]
fn resize_reallocs_the_primary_and_allocates_no_alternate_grid() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert_eq!(
        terminal_state.primary,
        Grid::build_blank(5, 10, Style::default())
    );
    assert_eq!(
        terminal_state.alternate,
        AlternateScreen::Blank {
            fill_style: Style::default()
        }
    );
}

#[test]
fn resize_reallocs_an_allocated_alternate_grid_to_new_size() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    terminal_state.allocate_alternate_grid();
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert_eq!(
        terminal_state.alternate,
        AlternateScreen::Allocated(Grid::build_blank(5, 10, Style::default()))
    );
}

#[test]
fn resize_pads_each_grid_with_its_own_screen_background() {
    // Padding a resize creates is filled with that screen's own render
    // background, never the other screen's. On the reflowed primary,
    // fully-default blanks count as padding, so they re-fill too — the same
    // background-color-erase fill every erase and scroll uses. Content cells
    // (anything non-default) keep their own styles.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    set_terminal_cell(&mut terminal_state, 0, 0, 'x', 1);
    terminal_state.allocate_alternate_grid();
    terminal_state
        .primary_render
        .style
        .set_background_color(Color::Indexed(4)); // primary: blue
    terminal_state
        .alternate_render
        .style
        .set_background_color(Color::Indexed(1)); // alternate: red
    terminal_state.resize_terminal_state(PtySize {
        column_count: 6,
        row_count: 3,
    });

    let mut blue_fill = Style::default();
    blue_fill.set_background_color(Color::Indexed(4)); // bg-only: fg + attrs stay default
    let mut red_fill = Style::default();
    red_fill.set_background_color(Color::Indexed(1));

    // Content keeps its own style.
    assert_eq!(
        terminal_state.primary.get_cell(0, 0),
        Some(&Cell::from_character('x', 1, Style::default()))
    );
    // Primary padding — re-created row tails and the new bottom row — takes
    // the primary fill.
    assert_eq!(
        terminal_state.primary.get_cell(0, 5),
        Some(&Cell::build_blank_with_style(blue_fill))
    );
    assert_eq!(
        terminal_state.primary.get_cell(2, 3),
        Some(&Cell::build_blank_with_style(blue_fill))
    );
    // The alternate crops in place: its untouched cells stay default and
    // only the grown region takes the alternate fill.
    assert_eq!(
        get_alternate_grid(&terminal_state).get_cell(0, 0),
        Some(&Cell::build_blank())
    );
    assert_eq!(
        get_alternate_grid(&terminal_state).get_cell(0, 5),
        Some(&Cell::build_blank_with_style(red_fill))
    );
    assert_eq!(
        get_alternate_grid(&terminal_state).get_cell(2, 3),
        Some(&Cell::build_blank_with_style(red_fill))
    );
}

#[test]
fn resize_clamps_out_of_bounds_cursor_to_last_cell() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    terminal_state.primary_cursor.row = 23;
    terminal_state.primary_cursor.column = 79;
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert_eq!(terminal_state.primary_cursor.row, 4);
    assert_eq!(terminal_state.primary_cursor.column, 9);
}

#[test]
fn resize_leaves_in_bounds_cursor_untouched() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    terminal_state.primary_cursor.row = 2;
    terminal_state.primary_cursor.column = 3;
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert_eq!(terminal_state.primary_cursor.row, 2);
    assert_eq!(terminal_state.primary_cursor.column, 3);
}

#[test]
fn resize_clears_a_pending_wrap_latched_to_the_old_edge() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    terminal_state.primary_cursor.is_wrap_pending = true;
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert!(!terminal_state.primary_cursor.is_wrap_pending);
}

#[test]
fn resize_preserves_cell_contents_across_width_and_height_changes() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 4,
    });
    set_terminal_cell(&mut terminal_state, 0, 0, 'h', 1);
    set_terminal_cell(&mut terminal_state, 0, 1, 'i', 1);
    set_terminal_cell(&mut terminal_state, 1, 0, '!', 1);
    terminal_state.primary_cursor.row = 1;

    // Shrink: trailing blank rows go first, the written rows stay put.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'h'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 1)
            .unwrap()
            .get_character(),
        'i'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        '!'
    );
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);

    // Grow back: the content is still where it was, new space is blank.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 6,
        row_count: 4,
    });
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'h'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        '!'
    );
    assert_eq!(
        terminal_state.primary.get_cell(3, 5),
        Some(&Cell::build_blank())
    );
}

#[test]
fn resize_shrink_pushes_top_rows_to_scrollback_and_grow_pulls_them_back() {
    // Every row written, cursor on the last row: nothing blank to trim, so a
    // 2-row shrink scrolls the top two rows into history.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 4,
    });
    for row_index in 0..4 {
        set_terminal_cell(
            &mut terminal_state,
            row_index,
            0,
            char::from(b'a' + row_index as u8),
            1,
        );
    }
    terminal_state.primary_cursor.row = 3;

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 2);
    assert_eq!(
        terminal_state.scrollback.list_retained_lines()[0].decode_text_cells()[0].get_character(),
        'a'
    );
    assert_eq!(
        terminal_state.scrollback.list_retained_lines()[1].decode_text_cells()[0].get_character(),
        'b'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'c'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        'd'
    );
    // The cursor followed its row up.
    assert_eq!(terminal_state.primary_cursor.row, 1);

    // Growing pulls the same rows back in at the top, newest first.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 4,
    });
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'a'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        'b'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(2, 0)
            .unwrap()
            .get_character(),
        'c'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(3, 0)
            .unwrap()
            .get_character(),
        'd'
    );
    assert_eq!(terminal_state.primary_cursor.row, 3);
}

#[test]
fn resize_width_shrink_wraps_a_wide_glyph_whole() {
    // 世 occupies cols 2–3; at width 3 its base would land in the last
    // column, so the reflow leaves a spacer there and wraps the glyph whole
    // onto the next row — never a dangling half.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 2,
    });
    set_terminal_cell(&mut terminal_state, 0, 0, 'a', 1);
    set_terminal_cell(&mut terminal_state, 0, 2, '世', 2);
    set_terminal_cell(&mut terminal_state, 0, 3, ' ', 0);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 3,
        row_count: 2,
    });
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'a'
    );
    assert_eq!(
        terminal_state.primary.get_cell(0, 2),
        Some(&Cell::build_blank())
    );
    assert_eq!(terminal_state.primary.get_row_end(0), RowEnd::SoftWide);
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        '世'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_display_width(),
        2
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 1)
            .unwrap()
            .get_display_width(),
        0
    );
}

#[test]
fn resize_to_zero_rows_pushes_all_content_into_scrollback_without_panicking() {
    // A pane driven to zero height (e.g. mid-drag in the layout) must not
    // panic; every row it held becomes history, and growing back pulls the
    // same rows back in, in order.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    set_terminal_cell(&mut terminal_state, 0, 0, 'a', 1);
    set_terminal_cell(&mut terminal_state, 1, 0, 'b', 1);
    set_terminal_cell(&mut terminal_state, 2, 0, 'c', 1);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 0,
    });
    // A zero-row grid reports zero columns too: `get_grid_dimensions` reads
    // the column count from the first row, and there is no first row (see the
    // grid-level `dimensions_of_grids_with_a_zero_axis` test for the same rule).
    assert_eq!(terminal_state.primary.get_grid_dimensions(), (0, 0));
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 3);
    assert_eq!(terminal_state.primary_cursor.row, 0); // clamped: no row to sit on
    assert_eq!(terminal_state.primary_cursor.column, 0);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 3,
    });
    assert_eq!(
        terminal_state
            .primary
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'a'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        'b'
    );
    assert_eq!(
        terminal_state
            .primary
            .get_cell(2, 0)
            .unwrap()
            .get_character(),
        'c'
    );
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
}

#[test]
fn empty_zero_row_resize_does_not_create_scrollback_history() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 0,
    });

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 0,
    });

    assert_eq!(terminal_state.primary.get_grid_dimensions(), (0, 0));
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 0);
}

#[test]
fn resize_to_zero_cols_yields_a_zero_width_grid_without_panicking() {
    // A zero-width grid has no cells to hold text; the erased content (there
    // is nowhere for it to live at width 0) does not resurface on regrow.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    set_terminal_cell(&mut terminal_state, 0, 0, 'h', 1);
    set_terminal_cell(&mut terminal_state, 0, 1, 'i', 1);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 0,
        row_count: 2,
    });
    assert_eq!(terminal_state.primary.get_grid_dimensions(), (2, 0));
    assert_eq!(terminal_state.primary_cursor.column, 0);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(terminal_state.primary.get_grid_dimensions(), (2, 4));
    assert_eq!(
        terminal_state.primary.get_cell(0, 0),
        Some(&Cell::build_blank())
    );
}

#[test]
fn resize_alternate_screen_crops_without_touching_scrollback() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    switch_to_alternate_screen(&mut terminal_state);
    for row_index in 0..3 {
        set_terminal_cell(
            &mut terminal_state,
            row_index,
            0,
            char::from(b'x' + row_index as u8),
            1,
        );
    }
    terminal_state
        .get_active_grid_mut()
        .set_prompt_mark(1, true);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    // The top row is cropped away — the alternate screen has no history.
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
    assert_eq!(
        get_alternate_grid(&terminal_state)
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'y'
    );
    assert!(get_alternate_grid(&terminal_state).has_prompt_mark(0));
    assert_eq!(
        get_alternate_grid(&terminal_state)
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        'z'
    );
    assert!(!get_alternate_grid(&terminal_state).has_prompt_mark(1));
}

#[test]
fn new_starts_with_an_empty_scrollback() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    assert!(terminal_state.get_scrollback().is_empty());
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 0);
}

/// The characters of `row_snapshot`'s cells as a string; blank cells read as
/// spaces.
fn get_row_snapshot_text(row_snapshot: &RowSnapshot) -> String {
    row_snapshot
        .decode_row_cells()
        .iter()
        .map(Cell::get_character)
        .collect()
}

/// Whether `row_snapshots` are the published snapshots of every row of
/// `screen_grid`, shared and not copied.
fn is_published_row_snapshot_list(row_snapshots: &[Arc<RowSnapshot>], screen_grid: &Grid) -> bool {
    let (row_count, _) = screen_grid.get_grid_dimensions();
    row_snapshots.len() == usize::from(row_count)
        && row_snapshots
            .iter()
            .zip(screen_grid.list_row_snapshots())
            .all(|(row_snapshot, published_row_snapshot)| {
                Arc::ptr_eq(row_snapshot, &published_row_snapshot)
            })
}

/// A 3-wide, 2-row primary screen with live rows `L0`/`L1` and three retained
/// history rows `h0`/`h1`/`h2` (oldest first).
fn build_state_with_history() -> TerminalState {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    for (column_index, character) in "L0.".chars().enumerate() {
        *terminal_state
            .get_active_grid_mut()
            .get_cell_mut(0, column_index as u16)
            .unwrap() = Cell::from_character(character, 1, Style::default());
    }
    for (column_index, character) in "L1.".chars().enumerate() {
        *terminal_state
            .get_active_grid_mut()
            .get_cell_mut(1, column_index as u16)
            .unwrap() = Cell::from_character(character, 1, Style::default());
    }
    terminal_state
        .scrollback
        .push_row_cells(&build_line_cells("h0."), RowMetadata::default());
    terminal_state
        .scrollback
        .push_row_cells(&build_line_cells("h1."), RowMetadata::default());
    terminal_state
        .scrollback
        .push_row_cells(&build_line_cells("h2."), RowMetadata::default());
    terminal_state
}

#[test]
fn scrolled_view_at_offset_zero_shares_the_live_buffer() {
    let terminal_state = build_state_with_history();
    // Offset 0 follows live: the live rows' own snapshots, no copy, and
    // effective 0.
    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(0);
    assert!(is_published_row_snapshot_list(
        &row_snapshots,
        terminal_state.get_active_grid()
    ));
    assert_eq!(clamped_scroll_offset, 0);
}

#[test]
fn scrolled_view_composes_history_above_the_live_screen() {
    let terminal_state = build_state_with_history();
    // Offset 1: newest history row on top, top live row below.
    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(1);
    assert_eq!(row_snapshots.len(), 2);
    assert_eq!(get_row_snapshot_text(&row_snapshots[0]), "h2.");
    assert_eq!(get_row_snapshot_text(&row_snapshots[1]), "L0.");
    assert_eq!(clamped_scroll_offset, 1);
}

#[test]
fn scrolled_view_keeps_a_history_row_prompt_mark() {
    let mut terminal_state = build_state_with_history();
    terminal_state.scrollback.push_row_cells(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
    );

    let (row_snapshots, _) = terminal_state.get_scrolled_view(1);

    assert!(row_snapshots[0].get_row_metadata().has_prompt_mark);
    assert!(!row_snapshots[1].get_row_metadata().has_prompt_mark);
}

#[test]
fn scrolled_view_at_the_screen_height_shows_only_history() {
    let terminal_state = build_state_with_history();
    // Offset 2 == the 2-row screen height: both rows come from history.
    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(2);
    assert_eq!(get_row_snapshot_text(&row_snapshots[0]), "h1.");
    assert_eq!(get_row_snapshot_text(&row_snapshots[1]), "h2.");
    assert_eq!(clamped_scroll_offset, 2);
}

#[test]
fn scrolled_view_clamps_an_over_scroll_to_the_oldest_line() {
    let terminal_state = build_state_with_history();
    // Three history rows, screen height 2: offset 3 shows the oldest window,
    // and any larger offset clamps — the rows and the clamped offset both — to
    // that same window rather than reading past.
    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(3);
    assert_eq!(get_row_snapshot_text(&row_snapshots[0]), "h0.");
    assert_eq!(get_row_snapshot_text(&row_snapshots[1]), "h1.");
    assert_eq!(clamped_scroll_offset, 3);

    let (over_scrolled_row_snapshots, over_scrolled_clamped_offset) =
        terminal_state.get_scrolled_view(99);
    assert_eq!(
        get_row_snapshot_text(&over_scrolled_row_snapshots[0]),
        "h0."
    );
    assert_eq!(
        get_row_snapshot_text(&over_scrolled_row_snapshots[1]),
        "h1."
    );
    assert_eq!(over_scrolled_clamped_offset, 3); // clamped to the retained count
}

#[test]
fn a_scrolled_view_at_offset_thirty_shares_every_history_line_and_screen_row() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 10,
        row_count: 50,
    });
    for line_index in 0..1_000 {
        terminal_state.scrollback.push_row_cells(
            &build_line_cells(&format!("line {line_index}")),
            RowMetadata::default(),
        );
    }

    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(30);

    let retained_lines = terminal_state.get_scrollback().list_retained_lines();
    let shared_row_snapshots: Vec<Arc<RowSnapshot>> = retained_lines
        .range(970..1_000)
        .cloned()
        .chain(
            terminal_state
                .get_active_grid()
                .list_row_snapshots()
                .take(20),
        )
        .collect();
    assert_eq!(clamped_scroll_offset, 30);
    assert_eq!(row_snapshots.len(), 50);
    for (view_row_index, (row_snapshot, shared_row_snapshot)) in
        row_snapshots.iter().zip(&shared_row_snapshots).enumerate()
    {
        assert!(
            Arc::ptr_eq(row_snapshot, shared_row_snapshot),
            "view row {view_row_index}"
        );
    }
}

#[test]
fn scrolled_view_on_the_alternate_screen_reports_a_live_zero_offset() {
    let mut terminal_state = build_state_with_history();
    switch_to_alternate_screen(&mut terminal_state); // full-screen apps keep no scrollback
    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(5);
    // The alternate screen always shows live: the live rows and a zero
    // effective offset.
    assert!(is_published_row_snapshot_list(
        &row_snapshots,
        get_alternate_grid(&terminal_state)
    ));
    assert_eq!(clamped_scroll_offset, 0);
}

#[test]
fn scrolled_view_with_empty_history_follows_live() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let (row_snapshots, clamped_scroll_offset) = terminal_state.get_scrolled_view(5);
    assert!(is_published_row_snapshot_list(
        &row_snapshots,
        terminal_state.get_active_grid()
    ));
    assert_eq!(clamped_scroll_offset, 0);
}

#[test]
fn scrolled_view_shows_a_history_rows_default_blanks_as_they_were() {
    // A history row holding `ab` and a default blank on a 3-wide screen. The
    // app has since set a background pen (SGR 48), which does not reach back
    // and repaint that column: scrolling back shows the line as it was.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    terminal_state
        .primary_render
        .style
        .set_background_color(Color::Indexed(4));
    let mut history_row_cells = build_line_cells("ab");
    history_row_cells.push(Cell::build_blank());
    terminal_state
        .scrollback
        .push_row_cells(&history_row_cells, RowMetadata::default());

    let (row_snapshots, _) = terminal_state.get_scrolled_view(1);
    assert_eq!(row_snapshots[0].decode_row_cells(), history_row_cells);
}

#[test]
fn scrolled_view_keeps_a_history_rows_own_background() {
    // The other half: color a program actually painted into a blank cell is
    // content, so it is stored and drawn — a full-width colored bar in history
    // still shows its color after scrolling back.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let mut red_background_style = Style::default();
    red_background_style.set_background_color(Color::Indexed(1));
    terminal_state.scrollback.push_row_cells(
        &vec![Cell::build_blank_with_style(red_background_style); 3],
        RowMetadata::default(),
    );

    let (row_snapshots, _) = terminal_state.get_scrolled_view(1);
    assert_eq!(
        row_snapshots[0].decode_row_cells(),
        vec![Cell::build_blank_with_style(red_background_style); 3]
    );
}

#[test]
fn text_view_on_the_alternate_screen_reads_its_grid_alone() {
    // The scrollback belongs to the primary and is still retained while the
    // alternate screen is up, so the alternate's view must hold its own grid
    // alone: its top row is the first readable row, and the primary's history
    // rows read as gone.
    let mut terminal_state = build_state_with_history();
    switch_to_alternate_screen(&mut terminal_state);

    // Three rows were pushed into history, so the live top row is absolute
    // row 3 and the screen's two rows are 3 and 4.
    let text_view = terminal_state.get_text_view();
    assert_eq!(text_view.get_first_row_index(), 3);
    assert_eq!(text_view.get_last_row_index(), 4);
    assert_eq!(text_view.get_row_end(2), None);

    // The primary's own view still reaches back over the same history.
    terminal_state.active_screen = Screen::Primary;
    assert_eq!(terminal_state.get_text_view().get_first_row_index(), 0);
}

#[test]
fn resize_blanks_a_wide_glyph_the_alternate_screen_cuts_in_half() {
    // The alternate screen crops instead of reflowing. 世 occupies cols 2-3;
    // cropping to 3 columns drops its right half, so the base left in the last
    // column is blanked rather than drawn as a half glyph.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 1,
    });
    switch_to_alternate_screen(&mut terminal_state);
    set_terminal_cell(&mut terminal_state, 0, 0, 'a', 1);
    set_terminal_cell(&mut terminal_state, 0, 2, '世', 2);
    set_terminal_cell(&mut terminal_state, 0, 3, ' ', 0);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 3,
        row_count: 1,
    });
    assert_eq!(
        get_alternate_grid(&terminal_state)
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'a'
    );
    assert_eq!(
        get_alternate_grid(&terminal_state).get_cell(0, 2),
        Some(&Cell::build_blank())
    );
}

#[test]
fn resize_moves_the_alternate_cursor_up_by_the_rows_cropped_off_the_top() {
    // Alternate height 4 -> 2 crops the two top rows away. The cursor sat on
    // row 2 holding `c`, so it lands on row 0 with `c` still under it — not on
    // the last row, which is where a bare clamp would leave it.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 2,
        row_count: 4,
    });
    switch_to_alternate_screen(&mut terminal_state);
    for (row, character) in "abcd".chars().enumerate() {
        set_terminal_cell(&mut terminal_state, row as u16, 0, character, 1);
    }
    terminal_state.alternate_cursor.row = 2;
    terminal_state.alternate_cursor.column = 1;

    terminal_state.resize_terminal_state(PtySize {
        column_count: 2,
        row_count: 2,
    });
    assert_eq!(
        get_alternate_grid(&terminal_state)
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'c'
    );
    assert_eq!(terminal_state.alternate_cursor.row, 0);
    assert_eq!(terminal_state.alternate_cursor.column, 1);
}

/// `terminal_state` written in the shape resume format 4 stores it: each
/// grid as `rows` of JSON cells beside a `row_metadata` list, the alternate
/// screen as its grid (an [`AlternateScreen::Blank`] one as its blank grid at
/// the primary screen's size), each history line as a pair of its JSON cells,
/// without the trailing default blanks of a hard-ended line, and its metadata,
/// and the modes without `is_insert_mode_enabled`.
fn write_as_resume_format_4(terminal_state: &TerminalState) -> serde_json::Value {
    let mut terminal_state_json =
        serde_json::to_value(terminal_state).expect("the terminal state serializes");
    let (row_count, column_count) = terminal_state.primary.get_grid_dimensions();
    let alternate_grid = match &terminal_state.alternate {
        AlternateScreen::Blank { fill_style } => {
            Grid::build_blank(row_count, column_count, *fill_style)
        }
        AlternateScreen::Allocated(alternate_grid) => alternate_grid.clone(),
    };
    terminal_state_json["alternate"] =
        serde_json::to_value(&alternate_grid).expect("the alternate grid serializes");
    terminal_state_json["modes"]
        .as_object_mut()
        .expect("the modes are a JSON object")
        .remove("is_insert_mode_enabled")
        .expect("the modes hold is_insert_mode_enabled");
    for grid_name in ["primary", "alternate"] {
        let stored_rows: Vec<RowSnapshot> =
            serde_json::from_value(terminal_state_json[grid_name]["rows"].take())
                .expect("read the stored grid rows");
        terminal_state_json[grid_name] = serde_json::json!({
            "rows": stored_rows
                .iter()
                .map(RowSnapshot::decode_row_cells)
                .collect::<Vec<Vec<Cell>>>(),
            "row_metadata": stored_rows
                .iter()
                .map(RowSnapshot::get_row_metadata)
                .collect::<Vec<RowMetadata>>(),
        });
    }
    let stored_lines: Vec<RowSnapshot> =
        serde_json::from_value(terminal_state_json["scrollback"]["retained_lines"].take())
            .expect("read the stored history lines");
    terminal_state_json["scrollback"]["retained_lines"] = serde_json::to_value(
        stored_lines
            .iter()
            .map(|row_snapshot| {
                (
                    row_snapshot.decode_text_cells(),
                    row_snapshot.get_row_metadata(),
                )
            })
            .collect::<Vec<(Vec<Cell>, RowMetadata)>>(),
    )
    .expect("encode the history lines");
    terminal_state_json
}

/// `terminal_state_json`, in the shape resume format 4 stores, read back as a
/// terminal state.
fn read_as_resume_format_4(terminal_state_json: serde_json::Value) -> TerminalState {
    serde_json::from_value::<ResumeFormat4TerminalState>(terminal_state_json)
        .expect("the format 4 state reads")
        .into_terminal_state()
}

#[test]
fn a_resume_format_4_terminal_state_reads_back_the_screens_and_history_it_was_written_from() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(
        &mut terminal_state,
        b"ab\r\n\x1b]133;A\x07wrapped-line\r\n\x1b[41mred\x1b[0m\r\nlast",
    );
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 3);
    let terminal_state_json = write_as_resume_format_4(&terminal_state);
    assert_eq!(
        terminal_state_json["scrollback"]["retained_lines"][0][0]
            .as_array()
            .map(Vec::len),
        Some(2),
        "the hard-ended line `ab` is stored without its padding"
    );

    assert_eq!(read_as_resume_format_4(terminal_state_json), terminal_state);
}

#[test]
fn a_resume_format_4_history_line_wider_than_the_screen_drops_the_image_fragments_it_loses() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    let image_record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 2,
            pixel_height: 1,
            rgba_bytes: vec![255; 2 * 4],
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_width: Some(ImageDimension::Cells(2)),
            requested_height: Some(ImageDimension::Cells(1)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 2),
    };
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H\n");
    let image_source_id = terminal_state.native_images[0]
        .placement
        .get_image_placement_id();
    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::from([(image_source_id, 2)])
    );
    let mut terminal_state_json = write_as_resume_format_4(&terminal_state);
    let history_line_cells = terminal_state_json["scrollback"]["retained_lines"][0][0]
        .as_array_mut()
        .expect("the history line holds a list of cells");
    let last_image_cell = history_line_cells[3].clone();
    history_line_cells.push(last_image_cell);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert_eq!(
        read_terminal_state.native_fragment_count_by_image_source_id,
        HashMap::from([(image_source_id, 2)])
    );
    assert_eq!(read_terminal_state, terminal_state);
}

#[test]
fn a_resume_format_4_terminal_state_reads_back_with_insert_mode_off() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(&mut terminal_state, b"ab\x1b[?7l\x1b[4h");
    let terminal_state_json = write_as_resume_format_4(&terminal_state);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert!(!read_terminal_state.modes.is_insert_mode_enabled);
    assert!(!read_terminal_state.modes.is_autowrap_enabled);
    let mut expected_terminal_state = terminal_state;
    expected_terminal_state.modes.is_insert_mode_enabled = false;
    assert_eq!(read_terminal_state, expected_terminal_state);
}

#[test]
fn a_terminal_state_in_insert_mode_round_trips_through_the_current_shape() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(&mut terminal_state, b"ab\x1b[4h");
    let terminal_state_json =
        serde_json::to_string(&terminal_state).expect("the terminal state serializes");

    let read_terminal_state: TerminalState =
        serde_json::from_str(&terminal_state_json).expect("the state reads back");

    assert!(read_terminal_state.modes.is_insert_mode_enabled);
    assert_eq!(read_terminal_state, terminal_state);
}

/// The blank fill style of a pen whose background is `Color::Indexed(color_index)`.
fn build_background_fill_style(color_index: u8) -> Style {
    let mut fill_style = Style::default();
    fill_style.set_background_color(Color::Indexed(color_index));
    fill_style
}

#[test]
fn a_pane_that_never_enters_the_alternate_screen_holds_no_alternate_grid() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 3,
    });
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[44mprompt\r\n\x1b]133;A\x07ls\r\nout\x1b[!p\x1b[2J",
    );
    for (column_count, row_count) in [(2, 1), (9, 5), (0, 0), (6, 3)] {
        terminal_state.resize_terminal_state(PtySize {
            column_count,
            row_count,
        });
    }

    assert_eq!(
        terminal_state.alternate,
        AlternateScreen::Blank {
            fill_style: Style::default()
        }
    );
}

#[test]
fn a_non_clearing_exit_keeps_the_alternate_cells_for_the_next_entry() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 2,
    });

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47habc\x1b[?47l\x1b[?47h");

    let first_row_characters: String = terminal_state
        .get_active_grid()
        .get_row_cells(0)
        .expect("the alternate screen has a first row")
        .iter()
        .map(Cell::get_character)
        .collect();
    assert_eq!(first_row_characters, "abc  ");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 3));
}

#[test]
fn a_clearing_exit_leaves_the_blanks_a_held_grid_cleared_in_the_pen_background() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[?1049h\x1b[44m\x1b]133;A\x07wrapped-text\x1b[2;2H\xe4\xb8\x96",
    );
    let mut cleared_grid = terminal_state.get_active_grid().clone();
    for row_index in 0..3 {
        cleared_grid.clear_line(row_index, 0, 4, build_background_fill_style(4));
        cleared_grid.set_prompt_mark(row_index, false);
    }

    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l");
    assert_eq!(
        terminal_state.alternate,
        AlternateScreen::Blank {
            fill_style: build_background_fill_style(4)
        }
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");

    assert_eq!(terminal_state.get_active_grid(), &cleared_grid);
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0));
}

#[test]
fn every_order_of_alternate_screen_switches_matches_an_alternate_grid_held_from_the_start() {
    const STEP_BYTES: [&[u8]; 21] = [
        b"\x1b[?47h",
        b"\x1b[?47l",
        b"\x1b[?1047h",
        b"\x1b[?1047l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?47;1049h",
        b"\x1b[?1049;1047l",
        b"\x1b[41m",
        b"\x1b[44m",
        b"\x1b[0m",
        b"abc",
        b"\x1b[2;3Hxyz",
        b"\r\n",
        b"\x1b[2J",
        b"\x1b]133;A\x07",
        b"\x1b[!p",
        b"\x1bc",
        b"\x1b7",
        b"\x1b8",
        b"\x1b[2;3r\x1b[3;1H\n\n",
    ];
    const STEP_COUNT_PER_SEED: usize = 400;

    for seed in 0..8 {
        let mut seeded_sequence =
            koshi_test_support::output_workloads::SeededSequence::from_seed(seed);
        let starting_pty_size = PtySize {
            column_count: 5,
            row_count: 3,
        };
        let mut lazy_terminal_state = TerminalState::from_pty_size(starting_pty_size);
        let mut held_terminal_state = TerminalState::from_pty_size(starting_pty_size);
        for step_index in 0..STEP_COUNT_PER_SEED {
            held_terminal_state.allocate_alternate_grid();
            let step_choice = seeded_sequence.compute_next_index(STEP_BYTES.len() + 1);
            let step_name = if step_choice == STEP_BYTES.len() {
                let pty_size = PtySize {
                    column_count: seeded_sequence.compute_next_index(8) as u16,
                    row_count: seeded_sequence.compute_next_index(6) as u16,
                };
                lazy_terminal_state.resize_terminal_state(pty_size);
                held_terminal_state.resize_terminal_state(pty_size);
                format!("resize to {pty_size:?}")
            } else {
                let step_bytes = STEP_BYTES[step_choice];
                process_terminal_bytes(&mut lazy_terminal_state, step_bytes);
                process_terminal_bytes(&mut held_terminal_state, step_bytes);
                format!("{:?}", String::from_utf8_lossy(step_bytes))
            };
            let mut compared_lazy_terminal_state = lazy_terminal_state.clone();
            compared_lazy_terminal_state.allocate_alternate_grid();
            let mut compared_held_terminal_state = held_terminal_state.clone();
            compared_held_terminal_state.allocate_alternate_grid();
            assert_eq!(
                compared_lazy_terminal_state, compared_held_terminal_state,
                "seed {seed}, step {step_index}: {step_name}"
            );
        }
    }
}

#[test]
fn a_resume_format_4_blank_alternate_grid_reads_back_as_no_grid() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    let terminal_state_json = write_as_resume_format_4(&terminal_state);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert_eq!(
        read_terminal_state.alternate,
        AlternateScreen::Blank {
            fill_style: Style::default()
        }
    );
}

#[test]
fn a_resume_format_4_alternate_grid_that_holds_text_keeps_its_grid() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47habc\x1b[?47l");
    let terminal_state_json = write_as_resume_format_4(&terminal_state);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert_eq!(read_terminal_state, terminal_state);
}

#[test]
fn a_resume_format_4_blank_alternate_grid_keeps_its_grid_while_it_is_active() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h");
    let terminal_state_json = write_as_resume_format_4(&terminal_state);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert_eq!(
        read_terminal_state.alternate,
        AlternateScreen::Allocated(Grid::build_blank(2, 6, Style::default()))
    );
}

#[test]
fn a_resume_format_4_blank_alternate_grid_off_the_alternate_pen_background_keeps_its_grid() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[41m\x1b[?47h\x1b[?47l");
    let terminal_state_json = write_as_resume_format_4(&terminal_state);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert_eq!(
        read_terminal_state.alternate,
        AlternateScreen::Allocated(Grid::build_blank(2, 6, Style::default()))
    );
}

#[test]
fn an_alternate_screen_with_no_grid_is_stored_as_its_blank_style() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });

    let terminal_state_json =
        serde_json::to_value(&terminal_state).expect("the terminal state serializes");

    assert_eq!(
        terminal_state_json["alternate"],
        serde_json::json!({
            "Blank": {
                "fill_style": serde_json::to_value(Style::default())
                    .expect("the style serializes")
            }
        })
    );
    let read_terminal_state: TerminalState =
        serde_json::from_value(terminal_state_json).expect("the state reads back");
    assert_eq!(read_terminal_state, terminal_state);
}

#[test]
fn a_stored_active_alternate_screen_with_no_grid_does_not_load() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    let mut terminal_state_json =
        serde_json::to_value(&terminal_state).expect("the terminal state serializes");
    terminal_state_json["active_screen"] =
        serde_json::to_value(Screen::Alternate).expect("the screen serializes");

    let read_error = serde_json::from_value::<TerminalState>(terminal_state_json)
        .expect_err("an active alternate screen needs a grid");

    assert_eq!(
        read_error.to_string(),
        "the active alternate screen holds no grid"
    );
}

#[test]
fn a_stored_alternate_screen_with_no_grid_and_an_image_placement_does_not_load() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    terminal_state
        .apply_image_record(&build_image_record(
            ImageDisplay {
                requested_column_count: Some(1),
                requested_row_count: Some(1),
                should_move_cursor: false,
                ..ImageDisplay::default()
            },
            (0, 0),
            1,
            1,
        ))
        .expect("the image fits");
    let mut terminal_state_json =
        serde_json::to_value(&terminal_state).expect("the terminal state serializes");
    let image_placement = terminal_state_json["primary_image_placements"][0].clone();
    terminal_state_json["primary_image_placements"] = serde_json::json!([]);
    terminal_state_json["alternate_image_placements"] = serde_json::json!([image_placement]);

    let read_error = serde_json::from_value::<TerminalState>(terminal_state_json)
        .expect_err("an alternate screen with no grid holds no image placement");

    assert_eq!(
        read_error.to_string(),
        "an alternate screen with no grid holds an image placement"
    );
}

#[test]
fn a_resume_format_4_blank_alternate_grid_resizes_as_its_grid_did() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 4,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h\x1b[3;2H\x1b[?47l");
    let terminal_state_json = write_as_resume_format_4(&terminal_state);
    let mut read_terminal_state = read_as_resume_format_4(terminal_state_json);
    assert_eq!(
        read_terminal_state.alternate,
        AlternateScreen::Blank {
            fill_style: Style::default()
        }
    );
    let mut held_terminal_state = read_terminal_state.clone();
    held_terminal_state.allocate_alternate_grid();

    let pty_size = PtySize {
        column_count: 5,
        row_count: 2,
    };
    read_terminal_state.resize_terminal_state(pty_size);
    held_terminal_state.resize_terminal_state(pty_size);

    assert_eq!(
        (
            read_terminal_state.alternate_cursor.row,
            read_terminal_state.alternate_cursor.column
        ),
        (0, 1)
    );
    read_terminal_state.allocate_alternate_grid();
    assert_eq!(read_terminal_state, held_terminal_state);
}

#[test]
fn a_clearing_exit_drops_the_native_images_the_alternate_cells_held() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h");
    let mut iterm_image_record = build_image_record(
        ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        (0, 0),
        1,
        1,
    );
    iterm_image_record.protocol = GraphicsProtocol::Iterm2;
    terminal_state
        .apply_image_record(&iterm_image_record)
        .expect("the iTerm2 image fits");
    assert_eq!(
        terminal_state
            .native_fragment_count_by_image_source_id
            .values()
            .copied()
            .collect::<Vec<usize>>(),
        vec![1]
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l");

    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::new()
    );
    assert_eq!(terminal_state.native_images, Vec::new());
}

#[test]
fn a_resume_format_4_blank_alternate_grid_with_an_image_placement_keeps_its_grid() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 2,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    terminal_state
        .apply_image_record(&build_image_record(
            ImageDisplay {
                requested_column_count: Some(1),
                requested_row_count: Some(1),
                should_move_cursor: false,
                ..ImageDisplay::default()
            },
            (0, 0),
            1,
            1,
        ))
        .expect("the alternate image fits");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert_eq!(
        terminal_state.alternate,
        AlternateScreen::Allocated(Grid::build_blank(2, 6, Style::default()))
    );
    let terminal_state_json = write_as_resume_format_4(&terminal_state);

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    assert_eq!(read_terminal_state, terminal_state);
}

/// A 5-column, 1-row state whose history holds the hard line `abc` encoded
/// 3 cells wide, then the soft line `abcdefg` encoded 7 cells wide.
fn build_state_with_history_of_other_widths() -> TerminalState {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 1,
    });
    terminal_state
        .scrollback
        .push_row_cells(&build_line_cells("abc"), RowMetadata::default());
    terminal_state.scrollback.push_row_cells(
        &build_line_cells("abcdefg"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );
    terminal_state
}

#[test]
fn a_stored_state_whose_history_line_holds_another_width_does_not_load() {
    let terminal_state_json = serde_json::to_value(build_state_with_history_of_other_widths())
        .expect("the terminal state serializes");

    let read_error = serde_json::from_value::<TerminalState>(terminal_state_json)
        .expect_err("every history line holds the screen's width");

    assert_eq!(
        read_error.to_string(),
        "a history line holds another column count than the primary screen"
    );
}

#[test]
fn a_resume_format_4_history_line_of_another_width_reads_back_at_the_screen_width() {
    let terminal_state_json = write_as_resume_format_4(&build_state_with_history_of_other_widths());

    let read_terminal_state = read_as_resume_format_4(terminal_state_json);

    let list_padded_line_texts_and_metadata =
        |terminal_state: &TerminalState| -> Vec<(String, RowMetadata)> {
            terminal_state
                .scrollback
                .list_retained_lines()
                .iter()
                .map(|row_snapshot| {
                    (
                        get_row_snapshot_text(row_snapshot),
                        row_snapshot.get_row_metadata(),
                    )
                })
                .collect()
        };
    assert_eq!(
        list_padded_line_texts_and_metadata(&read_terminal_state),
        vec![
            ("abc  ".to_string(), RowMetadata::default()),
            (
                "abcde".to_string(),
                RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: false,
                }
            ),
        ]
    );
    assert!(read_terminal_state
        .scrollback
        .has_only_lines_of_column_count(5));
}
