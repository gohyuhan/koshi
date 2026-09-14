//! Exact pixel padding, protocol sizing, clipping, and restored image storage.

use super::*;
use crate::engine::TerminalEngine;
use crate::graphics::ImageDisplay;
use koshi_core::process::PtySize;

fn build_terminal_engine() -> TerminalEngine {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 8,
    });
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(2, 3).expect("nonzero cell"));
    engine
}

#[test]
fn subcell_offsets_are_part_of_the_requested_cell_rectangle() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,X=1,Y=2,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(placement.get_image_cell_dimensions(), (1, 1));
    let rendered_image_record = placement.create_render_image_record();
    assert_eq!(
        rendered_image_record.image.as_ref(),
        &DecodedImage {
            pixel_width: 2,
            pixel_height: 3,
            rgba_bytes: vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 255
            ]
        }
    );
    assert_eq!(
        (
            rendered_image_record.display.cell_pixel_offset_x,
            rendered_image_record.display.cell_pixel_offset_y
        ),
        (None, None)
    );
}

#[test]
fn offsets_outside_a_cell_reject_even_explicit_cell_sizes() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,X=2,C=1,i=7;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;EINVAL:invalid image placement\x1b\\"
    );
    assert_eq!(engine.get_terminal_state().list_image_placements(), []);
    assert_eq!(engine.get_terminal_state().kitty_images, []);
}

#[test]
fn a_single_pixel_is_padded_instead_of_stretched() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,C=1,q=2,i=7;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(placement.get_image_cell_dimensions(), (1, 1));
    assert_eq!(
        placement.get_image_record().image.rgba_bytes,
        [255, 0, 0, 255]
    );
    assert_eq!(
        placement.create_render_image_record().image.as_ref(),
        &DecodedImage {
            pixel_width: 2,
            pixel_height: 3,
            rgba_bytes: vec![
                255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        }
    );
    let restored_terminal_state: TerminalState = serde_json::from_slice(
        &serde_json::to_vec(engine.get_terminal_state()).expect("serialize"),
    )
    .expect("restore");
    assert_eq!(
        restored_terminal_state.list_image_placements(),
        engine.get_terminal_state().list_image_placements()
    );
    assert_eq!(
        restored_terminal_state.cell_size,
        PixelCellSize::from_pixel_dimensions(2, 3)
    );
}

#[test]
fn a_single_requested_axis_uses_the_shared_cell_proportions() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=3,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(placement.get_image_cell_dimensions(), (2, 3));
    assert_eq!(
        placement.create_render_image_record().image.as_ref(),
        &DecodedImage {
            pixel_width: 6,
            pixel_height: 6,
            rgba_bytes: [255, 0, 0, 255].repeat(36)
        }
    );
}

#[test]
fn explicit_cell_geometry_keeps_the_programs_stretching_request() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=3,r=4,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(placement.get_image_cell_dimensions(), (4, 3));
    assert_eq!(placement.raster, None);
    assert_eq!(
        placement.create_render_image_record().image.as_ref(),
        &DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255]
        }
    );
}

#[test]
fn explicit_iterm_rectangle_keeps_all_requested_rows_with_tall_cells() {
    let record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 2,
            pixel_height: 2,
            rgba_bytes: [255, 0, 0, 255].repeat(4),
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_width: Some(ImageDimension::Cells(4)),
            requested_height: Some(ImageDimension::Cells(4)),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    let prepared = prepare_image_with_raster_plan(
        &record,
        PixelCellSize::from_pixel_dimensions(10, 20),
        (10, 10),
    )
    .expect("the requested rectangle fits");

    assert_eq!(prepared.column_count, 4);
    assert_eq!(prepared.row_count, 4);
    assert_eq!(
        prepared.plan.geometry.full_size,
        Size {
            column_count: 4,
            row_count: 4
        }
    );
    assert_eq!(
        prepared
            .raster
            .as_deref()
            .map(|image| (image.pixel_width, image.pixel_height)),
        Some((40, 80))
    );
}

#[test]
fn sixel_pixel_data_uses_the_same_padding_and_cell_measurement() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1bP0;1q\"1;1;1;1#1;2;100;0;0#1@\x1b\\"),
        b""
    );
    let placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(placement.get_image_cell_dimensions(), (1, 1));
    assert_eq!(
        placement.create_render_image_record().image.as_ref(),
        &DecodedImage {
            pixel_width: 2,
            pixel_height: 3,
            rgba_bytes: vec![
                255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        }
    );
}

#[test]
fn sixel_indexed_sources_survive_state_restore_and_palette_updates() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b[?1070l\x1bPq#1;2;100;0;0#1@\x1b\\"),
        b""
    );
    let serialized_terminal_state =
        serde_json::to_value(engine.get_terminal_state()).expect("state serializes");
    assert!(serialized_terminal_state["image_contents"][0]["sixel"].is_object());

    let restored_terminal_state: TerminalState =
        serde_json::from_value(serialized_terminal_state).expect("state restores");
    assert_eq!(
        restored_terminal_state.list_image_placements(),
        engine.get_terminal_state().list_image_placements()
    );

    let mut restored_engine = TerminalEngine::from_terminal_state(restored_terminal_state, &[]);
    assert_eq!(
        restored_engine.process_pty_output(b"\x1bPq#1;2;0;100;0\x1b\\"),
        b""
    );
    assert_eq!(
        &restored_engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes[..4],
        [0, 255, 0, 255]
    );
}

#[test]
fn percent_and_mixed_iterm_dimensions_use_the_shared_grid() {
    let mut record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255],
        }),
        animation: None,
        action: ImageAction::Display,
        display: crate::graphics::ImageDisplay {
            requested_width: Some(ImageDimension::Percent(50)),
            requested_height: Some(ImageDimension::Cells(1)),
            is_aspect_ratio_preserved: false,
            ..crate::graphics::ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let (column_count, row_count, raster) =
        prepare_image_raster(&record, PixelCellSize::from_pixel_dimensions(2, 3), (8, 8))
            .expect("pixel dimensions");
    assert_eq!((column_count, row_count), (4, 1));
    assert_eq!(
        raster.as_deref(),
        Some(&DecodedImage {
            pixel_width: 8,
            pixel_height: 3,
            rgba_bytes: [255, 0, 0, 255].repeat(24)
        })
    );
    record.display.requested_width = Some(ImageDimension::Pixels(4));
    let (column_count, row_count, raster) =
        prepare_image_raster(&record, PixelCellSize::from_pixel_dimensions(2, 3), (8, 8))
            .expect("mixed dimensions");
    assert_eq!((column_count, row_count), (2, 1));
    assert_eq!(
        raster.as_deref(),
        Some(&DecodedImage {
            pixel_width: 4,
            pixel_height: 3,
            rgba_bytes: [255, 0, 0, 255].repeat(12)
        })
    );
}

#[test]
fn iterm_percent_dimensions_round_up_and_fit_the_right_edge() {
    let mut record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 4,
            pixel_height: 4,
            rgba_bytes: [255, 0, 0, 255].repeat(16),
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_width: Some(ImageDimension::Percent(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            is_aspect_ratio_preserved: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(10, 10);

    let one_percent_raster = prepare_image_with_raster_plan(&record, pixel_cell_size, (8, 80))
        .expect("one percent fits");
    assert_eq!(
        (
            one_percent_raster.column_count,
            one_percent_raster.row_count
        ),
        (1, 1)
    );
    assert_eq!(one_percent_raster.plan.canvas_size, (10, 10));

    record.display.requested_width = Some(ImageDimension::Percent(100));
    record.display.requested_height = Some(ImageDimension::Cells(4));
    record.display.is_aspect_ratio_preserved = true;
    record.anchor.1 = 79;
    let aspect_preserved_raster = prepare_image_with_raster_plan(&record, pixel_cell_size, (8, 80))
        .expect("the edge cell fits");
    assert_eq!(
        (
            aspect_preserved_raster.column_count,
            aspect_preserved_raster.row_count
        ),
        (1, 1)
    );
    assert_eq!(aspect_preserved_raster.plan.target_size, (10, 10));

    record.display.is_aspect_ratio_preserved = false;
    let stretched_raster = prepare_image_with_raster_plan(&record, pixel_cell_size, (8, 80))
        .expect("the width is constrained");
    assert_eq!(
        (stretched_raster.column_count, stretched_raster.row_count),
        (1, 4)
    );
    assert_eq!(stretched_raster.plan.target_size, (10, 40));
}

#[test]
fn iterm_height_is_capped_at_255_rows_with_exact_aspect_behavior() {
    let mut record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 10,
            pixel_height: 300,
            rgba_bytes: [255, 0, 0, 255].repeat(3_000),
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_width: Some(ImageDimension::Cells(10)),
            requested_height: Some(ImageDimension::Cells(300)),
            is_aspect_ratio_preserved: true,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(1, 1);

    let aspect_preserved_raster =
        prepare_image_with_raster_plan(&record, pixel_cell_size, (400, 20))
            .expect("the tall image fits");
    assert_eq!(
        (
            aspect_preserved_raster.column_count,
            aspect_preserved_raster.row_count
        ),
        (8, 255)
    );
    assert_eq!(aspect_preserved_raster.plan.canvas_size, (8, 255));
    assert_eq!(aspect_preserved_raster.plan.target_size, (8, 240));

    record.display.is_aspect_ratio_preserved = false;
    let stretched_raster = prepare_image_with_raster_plan(&record, pixel_cell_size, (400, 20))
        .expect("the tall image fits");
    assert_eq!(
        (stretched_raster.column_count, stretched_raster.row_count),
        (10, 255)
    );
    assert_eq!(stretched_raster.plan.canvas_size, (10, 255));
    assert_eq!(stretched_raster.plan.target_size, (10, 255));
}

#[test]
fn canonical_one_cell_iterm_dimensions_produce_one_cell_geometry() {
    let base_image_record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255],
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_width: Some(ImageDimension::Cells(1)),
            requested_height: Some(ImageDimension::Cells(1)),
            is_aspect_ratio_preserved: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(10, 20);

    for requested_dimension in [ImageDimension::Cells(1), ImageDimension::Pixels(1)] {
        let mut image_record = base_image_record.clone();
        image_record.display.requested_width = Some(requested_dimension);
        image_record.display.requested_height = Some(requested_dimension);
        let prepared = prepare_image_with_raster_plan(&image_record, pixel_cell_size, (8, 8))
            .expect("one cell is renderable");
        assert_eq!(
            (prepared.column_count, prepared.row_count),
            (1, 1),
            "{requested_dimension:?}"
        );
    }
}

#[test]
fn measurements_change_new_images_but_do_not_resize_existing_placements() {
    let mut engine = build_terminal_engine();
    let image_command_bytes = b"\x1b_Ga=T,f=32,s=1,v=1,c=3,C=1,q=2;/wAA/w==\x1b\\";
    assert_eq!(engine.process_pty_output(image_command_bytes), b"");
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(2, 2).expect("nonzero cell"));
    assert_eq!(engine.process_pty_output(image_command_bytes), b"");
    assert_eq!(
        engine
            .get_terminal_state()
            .list_image_placements()
            .iter()
            .map(ImagePlacement::get_image_cell_dimensions)
            .collect::<Vec<_>>(),
        [(2, 3), (3, 3)]
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b[16t\x1b[14t\x1b[18t"),
        b"\x1b[6;2;2t\x1b[4;16;16t\x1b[8;8;8t"
    );
}
