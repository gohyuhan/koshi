//! Unit tests for per-pane terminal state.

use super::*;
use crate::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
};
use crate::grid::state::{Cell, Grid, RowEnd, RowMetadata};
use crate::scrollback::ScrollbackLimit;
use crate::state::images::{MAX_IMAGE_PLACEMENT_COUNT, MAX_IMAGE_STORAGE_BYTE_COUNT};
use crate::style::{Color, Style};

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
        .active_grid_mut()
        .get_cell_mut(row_index, column_index)
        .unwrap() = Cell::from_character(character, cell_width, Style::default());
}

fn build_image_record(
    display: ImageDisplay,
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
        display,
        anchor: image_anchor,
    }
}

fn process_terminal_bytes(terminal_state: &mut TerminalState, input_bytes: &[u8]) {
    let mut parser = vte::Parser::<{ crate::engine::OSC_BUFFER_BYTE_CAPACITY }>::new_with_size();
    parser.advance(terminal_state, input_bytes);
}

#[test]
fn new_initializes_both_screens_to_blank_of_size() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    assert_eq!(*terminal_state.primary, Grid::blank(3, 5, Style::default()));
    assert_eq!(
        *terminal_state.alternate,
        Grid::blank(3, 5, Style::default())
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
        pending_wrap: false,
        origin: false,
        saved: None,
    };
    assert_eq!(terminal_state.primary_cursor, expected_cursor);
    assert_eq!(terminal_state.alternate_cursor, expected_cursor);
    assert_eq!(
        terminal_state.active_render().charsets,
        [Charset::default(); 4]
    );
    assert_eq!(terminal_state.active_render().gl, 0);
    assert_eq!(terminal_state.active_render().style, Style::default());
    assert_eq!(
        terminal_state.primary_render,
        terminal_state.alternate_render
    );
    assert_eq!(terminal_state.modes, TerminalModes::default());
    assert_eq!(terminal_state.title, None);
}

#[test]
fn state_without_shell_metadata_deserializes_as_prompt() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state
        .as_object_mut()
        .expect("state is an object")
        .remove("shell_integration_state");

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("legacy state deserializes");

    assert_eq!(
        restored.shell_integration_state,
        ShellIntegrationState::Prompt
    );
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
    assert!(restored.modes.declrmm);
    assert!(restored.primary_cursor.origin);
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
fn state_without_keyboard_stacks_deserializes_with_no_flags() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[>8u");
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let serialized_fields = serialized_state
        .as_object_mut()
        .expect("state is an object");
    serialized_fields.remove("primary_keyboard_stack");
    serialized_fields.remove("alternate_keyboard_stack");

    let mut restored: TerminalState =
        serde_json::from_value(serialized_state).expect("legacy state deserializes");

    assert_eq!(restored.get_keyboard_flags(), 0);
    process_terminal_bytes(&mut restored, b"\x1b[?1049h");
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
    serialized_state["modes"]["declrmm"] = serde_json::json!(true);
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
    serialized_state["modes"]["declrmm"] = serde_json::json!(true);
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
    assert!(!restored.modes.declrmm);
}

#[test]
fn state_deserialization_normalizes_full_width_horizontal_margins() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["modes"]["declrmm"] = serde_json::json!(true);
    serialized_state["primary_horizontal_margins"] = serde_json::json!([0, 4]);
    serialized_state["alternate_horizontal_margins"] = serde_json::json!([1, 3]);

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("full-width margins are valid");

    assert_eq!(restored.primary_horizontal_margins, None);
    assert_eq!(restored.alternate_horizontal_margins, Some((1, 3)));
    assert!(restored.modes.declrmm);
}

#[test]
fn state_without_origin_and_horizontal_margin_fields_deserializes() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[?6h\x1b7");
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let object = serialized_state
        .as_object_mut()
        .expect("state is an object");
    object.remove("primary_horizontal_margins");
    object.remove("alternate_horizontal_margins");
    object
        .get_mut("modes")
        .and_then(serde_json::Value::as_object_mut)
        .expect("modes is an object")
        .remove("declrmm");
    for cursor_field_name in ["primary_cursor", "alternate_cursor"] {
        let cursor_object = object
            .get_mut(cursor_field_name)
            .and_then(serde_json::Value::as_object_mut)
            .expect("cursor is an object");
        cursor_object.remove("origin");
        if let Some(saved_cursor_object) = cursor_object
            .get_mut("saved")
            .and_then(serde_json::Value::as_object_mut)
        {
            saved_cursor_object.remove("origin");
        }
    }

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("legacy state deserializes");

    assert_eq!(restored.primary_horizontal_margins, None);
    assert_eq!(restored.alternate_horizontal_margins, None);
    assert!(!restored.modes.declrmm);
    assert!(!restored.primary_cursor.origin);
    assert!(!restored.alternate_cursor.origin);
    assert!(
        !restored
            .primary_cursor
            .saved
            .expect("saved cursor is retained")
            .origin
    );
}

#[test]
fn state_without_image_fields_deserializes_with_empty_image_state() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let object = serialized_state
        .as_object_mut()
        .expect("state is an object");
    object.remove("primary_image_placements");
    object.remove("primary_image_history");
    object.remove("alternate_image_placements");
    object.remove("next_image_placement_id");

    let restored: TerminalState =
        serde_json::from_value(serialized_state).expect("legacy state deserializes");

    assert_eq!(restored.list_image_placements(), &[]);
    assert_eq!(restored.next_image_placement_id, 1);
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
    let display = ImageDisplay {
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
        display: display.clone(),
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
    let display = ImageDisplay {
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
        display: display.clone(),
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
        assert!(placement["record"].get("image").is_none());
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
fn serialized_content_table_rejects_duplicate_dangling_extra_and_inline_entries() {
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
    let base = serde_json::to_value(&terminal_state).expect("state serializes");
    let content = base["image_contents"][0].clone();

    let mut duplicate = base.clone();
    duplicate["image_contents"] = serde_json::json!([content.clone(), content.clone()]);
    let error = serde_json::from_value::<TerminalState>(duplicate)
        .expect_err("duplicate content identities must be rejected");
    assert_eq!(error.to_string(), "image content identities must be unique");

    let mut dangling = base.clone();
    dangling["primary_image_placements"][0]["content_id"] = serde_json::json!(999);
    let error = serde_json::from_value::<TerminalState>(dangling)
        .expect_err("a missing content identity must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement refers to missing content"
    );

    let mut unreferenced_content_state = base.clone();
    let mut unreferenced_image_content = content.clone();
    unreferenced_image_content["id"] = serde_json::json!(999);
    unreferenced_content_state["image_contents"] =
        serde_json::json!([content.clone(), unreferenced_image_content]);
    let error = serde_json::from_value::<TerminalState>(unreferenced_content_state)
        .expect_err("unreferenced content must be rejected");
    assert_eq!(
        error.to_string(),
        "image content table contains unreferenced entries"
    );

    let mut inline = base.clone();
    inline["primary_image_placements"][0]["record"]["image"] = content["image"].clone();
    let error = serde_json::from_value::<TerminalState>(inline)
        .expect_err("content-table records must not carry inline pixels");
    assert_eq!(
        error.to_string(),
        "content-table image records cannot carry inline pixels"
    );

    let mut collision = base;
    collision["next_image_content_id"] = content["id"].clone();
    let error = serde_json::from_value::<TerminalState>(collision)
        .expect_err("the next content identity cannot collide");
    assert_eq!(
        error.to_string(),
        "next image content identity collides with retained content"
    );
}

#[test]
fn serialized_content_table_rejects_legacy_raster_fields_and_combined_count_overflow() {
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
    let base = serde_json::to_value(&terminal_state).expect("state serializes");
    let content = base["image_contents"][0].clone();

    let mut legacy_plan = base.clone();
    legacy_plan["primary_image_placements"][0]["plan"] = serde_json::Value::Null;
    let error = serde_json::from_value::<TerminalState>(legacy_plan)
        .expect_err("new-format placements must carry their validated plan");
    assert_eq!(
        error.to_string(),
        "content-table image placements require a raster plan"
    );

    let mut legacy_raster = base.clone();
    legacy_raster["primary_image_placements"][0]["raster"] = content["image"].clone();
    let error = serde_json::from_value::<TerminalState>(legacy_raster)
        .expect_err("new-format placements must not carry legacy raster data");
    assert_eq!(
        error.to_string(),
        "content-table image placements cannot carry legacy raster fields"
    );

    let placement = base["primary_image_placements"][0].clone();
    let mut placements = Vec::with_capacity(MAX_IMAGE_PLACEMENT_COUNT);
    for placement_id in 1..=MAX_IMAGE_PLACEMENT_COUNT {
        let mut placement = placement.clone();
        placement["id"] = serde_json::json!(placement_id);
        placements.push(placement);
    }
    let mut count_overflow = base;
    count_overflow["primary_image_placements"] = serde_json::Value::Array(placements);
    count_overflow["alternate_image_placements"] = serde_json::json!([placement]);
    let error = serde_json::from_value::<TerminalState>(count_overflow)
        .expect_err("combined image placement lists must be bounded");
    assert_eq!(
        error.to_string(),
        format!(
            "image placement count {} exceeds the limit of {}",
            MAX_IMAGE_PLACEMENT_COUNT + 1,
            MAX_IMAGE_PLACEMENT_COUNT
        )
    );
}

#[test]
fn raw_image_state_deserialization_shares_one_byte_budget_across_all_fields() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    let image_record = build_image_record(
        ImageDisplay {
            image_id: Some(7),
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
    let serialized_image_bytes = serialized_state["image_contents"][0]["image"].clone();
    let mut placement = serialized_state["primary_image_placements"][0].clone();
    placement
        .as_object_mut()
        .expect("placement is an object")
        .remove("content_id");
    placement["record"]["image"] = serialized_image_bytes.clone();
    serialized_state["primary_image_placements"] = serde_json::json!([placement.clone()]);
    serialized_state["primary_image_history"] = serde_json::json!([placement.clone()]);
    serialized_state["alternate_image_placements"] = serde_json::json!([placement]);
    let mut kitty = serialized_state["primary_image_placements"][0]["record"].clone();
    kitty["action"] = serde_json::json!("Transmit");
    serialized_state["kitty_images"] = serde_json::json!([kitty]);

    let mut budget = images::ImageStateBudget::with_byte_limit(16);
    use serde::de::IntoDeserializer;

    let deserializer = serialized_state.into_deserializer();
    let error = match RawTerminalStateFields::deserialize_with_budget(deserializer, &mut budget) {
        Ok(_) => panic!("the fifth image payload must exceed the shared budget"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
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
    let display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    terminal_state
        .apply_image_record(&build_image_record(display.clone(), (0, 0), 1, 1))
        .expect("the primary image fits");
    terminal_state.active_screen = Screen::Alternate;
    terminal_state
        .apply_image_record(&build_image_record(display, (1, 1), 1, 1))
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
    let display = ImageDisplay {
        image_id: Some(0),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    terminal_state
        .apply_image_record(&build_image_record(display.clone(), (0, 0), 1, 1))
        .expect("the first anonymous image fits");
    terminal_state
        .apply_image_record(&build_image_record(display, (1, 1), 1, 1))
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
    let display = ImageDisplay {
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let full_size_image_record = build_image_record(display.clone(), (0, 0), 16_384, 1_024);
    terminal_state
        .apply_image_record(&full_size_image_record)
        .expect("the image fits the byte limit");
    let state_before_storage_limit = terminal_state.clone();
    let extra_image_record = build_image_record(display, (0, 0), 1, 1);

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
    let display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        requested_column_count: Some(1),
        requested_row_count: Some(1),
        should_move_cursor: false,
        ..ImageDisplay::default()
    };
    let primary_image_record = build_image_record(display.clone(), (0, 0), 1, 1);
    terminal_state
        .apply_image_record(&primary_image_record)
        .expect("the primary image fits");

    terminal_state.active_screen = Screen::Alternate;
    terminal_state
        .apply_image_record(&build_image_record(display, (1, 1), 1, 1))
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

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a history image outside the primary width must be rejected");
    assert_eq!(
        error.to_string(),
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

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a history image outside the retained rows must be rejected");
    assert_eq!(
        error.to_string(),
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
    serialized_state["scrollback"]["total_pushed"] = serde_json::json!(u64::MAX);

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a live-row range that overflows u64 must be rejected");
    assert_eq!(
        error.to_string(),
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
        .push_row(&[Cell::blank()], RowMetadata::default());

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    serialized_state["scrollback"]["total_pushed"] = serde_json::json!(0);

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("retained rows must have nonnegative absolute row numbers");
    assert_eq!(
        error.to_string(),
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
    for (column, ch) in "abcd".chars().enumerate() {
        set_terminal_cell(&mut terminal_state, 0, column as u16, ch, 1);
    }
    for (column, ch) in "ef".chars().enumerate() {
        set_terminal_cell(&mut terminal_state, 1, column as u16, ch, 1);
    }
    terminal_state
        .active_grid_mut()
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
    placement["record"]["anchor"] = serde_json::json!([u16::MAX, 0]);

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("an overflowing placement must be rejected");
    assert_eq!(
        error.to_string(),
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
    placement["record"]["anchor"] = serde_json::json!([7, 7]);

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("an out-of-grid placement must be rejected");
    assert_eq!(
        error.to_string(),
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

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("duplicate placement state must be rejected");
    assert_eq!(
        error.to_string(),
        "image placement identities must be unique per screen"
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

    let mut serialized_state = serde_json::to_value(&terminal_state).expect("state serializes");
    let placement = serialized_state["primary_image_placements"][0].clone();
    serialized_state["alternate_image_placements"] = serde_json::json!([placement]);

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a cross-screen identity collision must be rejected");
    assert_eq!(
        error.to_string(),
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
                placement["id"] = serde_json::json!(placement_id);
                placement
            })
            .collect(),
    );

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a placement-count overflow must be rejected");
    assert_eq!(
        error.to_string(),
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

    let error = serde_json::from_value::<TerminalState>(serialized_state)
        .expect_err("a zero next identity must be rejected");
    assert_eq!(
        error.to_string(),
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
        terminal_state.primary.as_ref()
    ));
    terminal_state.active_screen = Screen::Alternate;
    assert!(std::ptr::eq(
        terminal_state.get_active_grid(),
        terminal_state.alternate.as_ref()
    ));
}

#[test]
fn active_grid_mut_follows_active_screen() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(
        terminal_state.active_grid_mut(),
        &Grid::blank(2, 4, Style::default())
    );
    terminal_state.active_screen = Screen::Alternate;
    assert_eq!(
        terminal_state.active_grid_mut(),
        &Grid::blank(2, 4, Style::default())
    );
}

#[test]
fn resize_reallocs_both_grids_to_new_size() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert_eq!(
        *terminal_state.primary,
        Grid::blank(5, 10, Style::default())
    );
    assert_eq!(
        *terminal_state.alternate,
        Grid::blank(5, 10, Style::default())
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
        Some(&Cell::blank_with(blue_fill))
    );
    assert_eq!(
        terminal_state.primary.get_cell(2, 3),
        Some(&Cell::blank_with(blue_fill))
    );
    // The alternate crops in place: its untouched cells stay default and
    // only the grown region takes the alternate fill.
    assert_eq!(
        terminal_state.alternate.get_cell(0, 0),
        Some(&Cell::blank())
    );
    assert_eq!(
        terminal_state.alternate.get_cell(0, 5),
        Some(&Cell::blank_with(red_fill))
    );
    assert_eq!(
        terminal_state.alternate.get_cell(2, 3),
        Some(&Cell::blank_with(red_fill))
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
    terminal_state.primary_cursor.pending_wrap = true;
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 5,
    });
    assert!(!terminal_state.primary_cursor.pending_wrap);
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
    assert_eq!(terminal_state.primary.get_cell(3, 5), Some(&Cell::blank()));
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
        terminal_state.scrollback.list_retained_lines()[0].0[0].get_character(),
        'a'
    );
    assert_eq!(
        terminal_state.scrollback.list_retained_lines()[1].0[0].get_character(),
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
    assert_eq!(terminal_state.primary.get_cell(0, 2), Some(&Cell::blank()));
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
    // A zero-row grid reports zero columns too: `dimensions()` derives cols
    // from the first row, and there is no first row (see the grid-level
    // `dimensions_of_grids_with_a_zero_axis` test for the same rule).
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
    assert_eq!(terminal_state.primary.get_cell(0, 0), Some(&Cell::blank()));
}

#[test]
fn resize_alternate_screen_crops_without_touching_scrollback() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    terminal_state.active_screen = Screen::Alternate;
    for row_index in 0..3 {
        set_terminal_cell(
            &mut terminal_state,
            row_index,
            0,
            char::from(b'x' + row_index as u8),
            1,
        );
    }
    terminal_state.active_grid_mut().set_prompt_mark(1, true);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    // The top row is cropped away — the alternate screen has no history.
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
    assert_eq!(
        terminal_state
            .alternate
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'y'
    );
    assert!(terminal_state.alternate.has_prompt_mark(0));
    assert_eq!(
        terminal_state
            .alternate
            .get_cell(1, 0)
            .unwrap()
            .get_character(),
        'z'
    );
    assert!(!terminal_state.alternate.has_prompt_mark(1));
}

#[test]
fn new_starts_with_an_empty_scrollback() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 5,
        row_count: 3,
    });
    assert!(terminal_state.get_scrollback().is_empty());
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 0);
    assert_eq!(terminal_state.get_scrollback().get_dropped_line_count(), 0);
    assert_eq!(terminal_state.get_scrollback().get_dropped_byte_count(), 0);
}

/// A row of `line_text`, one default-styled cell per character — a scrollback line fixture.
fn build_line_cells(line_text: &str) -> Vec<Cell> {
    line_text
        .chars()
        .map(|character| Cell::from_character(character, 1, Style::default()))
        .collect()
}

/// Read `row_index` of `grid` as a string; blank cells read as spaces.
fn get_grid_row_text(grid: &Grid, row_index: u16) -> String {
    let (_, column_count) = grid.get_grid_dimensions();
    (0..column_count)
        .map(|column_index| {
            grid.get_cell(row_index, column_index)
                .map(Cell::get_character)
                .unwrap_or(' ')
        })
        .collect()
}

/// A 3-wide, 2-row primary screen with live rows `L0`/`L1` and three retained
/// history rows `h0`/`h1`/`h2` (oldest first).
fn state_with_history() -> TerminalState {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    for (col, ch) in "L0.".chars().enumerate() {
        *terminal_state
            .active_grid_mut()
            .get_cell_mut(0, col as u16)
            .unwrap() = Cell::from_character(ch, 1, Style::default());
    }
    for (col, ch) in "L1.".chars().enumerate() {
        *terminal_state
            .active_grid_mut()
            .get_cell_mut(1, col as u16)
            .unwrap() = Cell::from_character(ch, 1, Style::default());
    }
    terminal_state
        .scrollback
        .push_row(&build_line_cells("h0."), RowMetadata::default());
    terminal_state
        .scrollback
        .push_row(&build_line_cells("h1."), RowMetadata::default());
    terminal_state
        .scrollback
        .push_row(&build_line_cells("h2."), RowMetadata::default());
    terminal_state
}

#[test]
fn scrolled_view_at_offset_zero_shares_the_live_buffer() {
    let terminal_state = state_with_history();
    // Offset 0 follows live: the same Arc (no compose, no copy) and effective 0.
    let (grid, effective) = terminal_state.scrolled_view(0);
    assert!(Arc::ptr_eq(&grid, &terminal_state.get_active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_composes_history_above_the_live_screen() {
    let terminal_state = state_with_history();
    // Offset 1: newest history row on top, top live row below.
    let (grid, effective) = terminal_state.scrolled_view(1);
    assert_eq!(grid.get_grid_dimensions(), (2, 3));
    assert_eq!(get_grid_row_text(&grid, 0), "h2.");
    assert_eq!(get_grid_row_text(&grid, 1), "L0.");
    assert_eq!(effective, 1);
}

#[test]
fn scrolled_view_keeps_a_history_row_prompt_mark() {
    let mut terminal_state = state_with_history();
    terminal_state.scrollback.push_row(
        &build_line_cells("prompt"),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: true,
        },
    );

    let (grid, _) = terminal_state.scrolled_view(1);

    assert!(grid.has_prompt_mark(0));
    assert!(!grid.has_prompt_mark(1));
}

#[test]
fn scrolled_view_at_the_screen_height_shows_only_history() {
    let terminal_state = state_with_history();
    // Offset 2 == the 2-row screen pixel_height: both rows come from history.
    let (grid, effective) = terminal_state.scrolled_view(2);
    assert_eq!(get_grid_row_text(&grid, 0), "h1.");
    assert_eq!(get_grid_row_text(&grid, 1), "h2.");
    assert_eq!(effective, 2);
}

#[test]
fn scrolled_view_clamps_an_over_scroll_to_the_oldest_line() {
    let terminal_state = state_with_history();
    // Three history rows, screen height 2: offset 3 shows the oldest window,
    // and any larger offset clamps — grid and effective offset both — to that
    // same window rather than reading past.
    let (grid, effective) = terminal_state.scrolled_view(3);
    assert_eq!(get_grid_row_text(&grid, 0), "h0.");
    assert_eq!(get_grid_row_text(&grid, 1), "h1.");
    assert_eq!(effective, 3);

    let (over, over_effective) = terminal_state.scrolled_view(99);
    assert_eq!(get_grid_row_text(&over, 0), "h0.");
    assert_eq!(get_grid_row_text(&over, 1), "h1.");
    assert_eq!(over_effective, 3); // clamped to the retained count
}

#[test]
fn scrolled_view_on_the_alternate_screen_reports_a_live_zero_offset() {
    let mut terminal_state = state_with_history();
    terminal_state.active_screen = Screen::Alternate; // full-screen apps keep no scrollback
    let (grid, effective) = terminal_state.scrolled_view(5);
    // The alternate screen always shows live: the live Arc and a zero effective
    // offset, so the indicator and cursor never treat it as scrolled.
    assert!(Arc::ptr_eq(&grid, &terminal_state.get_active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_with_empty_history_follows_live() {
    let terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let (grid, effective) = terminal_state.scrolled_view(5);
    assert!(Arc::ptr_eq(&grid, &terminal_state.get_active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_pads_history_rows_with_the_blanks_that_were_trimmed() {
    // A history row holding `ab` on a 3-wide screen: its third cell was a
    // default blank and is not stored. The app has since set a background pen
    // (SGR 48), which must not reach back and repaint that column — the cell
    // was default when the line scrolled off, and scrolling back shows the
    // line as it was, not as the running program currently paints.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    terminal_state
        .primary_render
        .style
        .set_background_color(Color::Indexed(4));
    terminal_state
        .scrollback
        .push_row(&build_line_cells("ab"), RowMetadata::default());

    let (grid, _) = terminal_state.scrolled_view(1);
    let padded = grid.get_cell(0, 2).unwrap();
    assert_eq!(padded.get_character(), ' ');
    assert_eq!(padded.get_style(), Style::default());
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
    let mut red = Style::default();
    red.set_background_color(Color::Indexed(1));
    terminal_state
        .scrollback
        .push_row(&vec![Cell::blank_with(red); 3], RowMetadata::default());

    let (grid, _) = terminal_state.scrolled_view(1);
    for column_index in 0..3 {
        assert_eq!(
            grid.get_cell(0, column_index)
                .unwrap()
                .get_style()
                .get_background_color(),
            Color::Indexed(1)
        );
    }
}

#[test]
fn text_view_on_the_alternate_screen_reads_its_grid_alone() {
    // The scrollback belongs to the primary and is still retained while the
    // alternate screen is up, so the alternate's view must hold its own grid
    // alone: its top row is the first readable row, and the primary's history
    // rows read as gone.
    let mut terminal_state = state_with_history();
    terminal_state.active_screen = Screen::Alternate;

    // Three rows were pushed into history, so the live top row is absolute
    // row 3 and the screen's two rows are 3 and 4.
    let view = terminal_state.get_text_view();
    assert_eq!(view.get_first_row_index(), 3);
    assert_eq!(view.get_last_row_index(), 4);
    assert_eq!(view.get_row(2).map(|(cells, _)| cells.len()), None);

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
    terminal_state.active_screen = Screen::Alternate;
    set_terminal_cell(&mut terminal_state, 0, 0, 'a', 1);
    set_terminal_cell(&mut terminal_state, 0, 2, '世', 2);
    set_terminal_cell(&mut terminal_state, 0, 3, ' ', 0);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 3,
        row_count: 1,
    });
    assert_eq!(
        terminal_state
            .alternate
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'a'
    );
    assert_eq!(
        terminal_state.alternate.get_cell(0, 2),
        Some(&Cell::blank())
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
    terminal_state.active_screen = Screen::Alternate;
    for (row, ch) in "abcd".chars().enumerate() {
        set_terminal_cell(&mut terminal_state, row as u16, 0, ch, 1);
    }
    terminal_state.alternate_cursor.row = 2;
    terminal_state.alternate_cursor.column = 1;

    terminal_state.resize_terminal_state(PtySize {
        column_count: 2,
        row_count: 2,
    });
    assert_eq!(
        terminal_state
            .alternate
            .get_cell(0, 0)
            .unwrap()
            .get_character(),
        'c'
    );
    assert_eq!(terminal_state.alternate_cursor.row, 0);
    assert_eq!(terminal_state.alternate_cursor.column, 1);
}
