//! Exact pixel padding, protocol sizing, clipping, and restored image storage.

use super::*;
use crate::engine::TerminalEngine;
use crate::graphics::ImageDisplay;
use koshi_core::process::PtySize;

fn engine() -> TerminalEngine {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 8 });
    engine.set_cell_size(PixelCellSize::new(2, 3).expect("nonzero cell"));
    engine
}

#[test]
fn subcell_offsets_are_part_of_the_requested_cell_rectangle() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,X=1,Y=2,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(placement.dimensions(), (1, 1));
    let rendered = placement.render_record_arc();
    assert_eq!(
        rendered.image.as_ref(),
        &DecodedImage {
            width: 2,
            height: 3,
            rgba: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 255]
        }
    );
    assert_eq!(
        (
            rendered.display.cell_offset_x,
            rendered.display.cell_offset_y
        ),
        (None, None)
    );
}

#[test]
fn offsets_outside_a_cell_reject_even_explicit_cell_sizes() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,X=2,C=1,i=7;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;EINVAL:invalid image placement\x1b\\"
    );
    assert_eq!(engine.state().image_placements(), []);
    assert_eq!(engine.state().kitty_images, []);
}

#[test]
fn a_single_pixel_is_padded_instead_of_stretched() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,C=1,q=2,i=7;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(placement.dimensions(), (1, 1));
    assert_eq!(placement.record().image.rgba, [255, 0, 0, 255]);
    assert_eq!(
        placement.render_record_arc().image.as_ref(),
        &DecodedImage {
            width: 2,
            height: 3,
            rgba: vec![255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        }
    );
    let state: TerminalState =
        serde_json::from_slice(&serde_json::to_vec(engine.state()).expect("serialize"))
            .expect("restore");
    assert_eq!(state.image_placements(), engine.state().image_placements());
    assert_eq!(state.cell_size(), PixelCellSize::new(2, 3));
}

#[test]
fn a_single_requested_axis_uses_the_shared_cell_proportions() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=3,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(placement.dimensions(), (2, 3));
    assert_eq!(
        placement.render_record_arc().image.as_ref(),
        &DecodedImage {
            width: 6,
            height: 6,
            rgba: [255, 0, 0, 255].repeat(36)
        }
    );
}

#[test]
fn explicit_cell_geometry_keeps_the_programs_stretching_request() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=3,r=4,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(placement.dimensions(), (4, 3));
    assert_eq!(placement.raster, None);
    assert_eq!(
        placement.render_record_arc().image.as_ref(),
        &DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255]
        }
    );
}

#[test]
fn explicit_iterm_rectangle_keeps_all_requested_rows_with_tall_cells() {
    let record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            width: 2,
            height: 2,
            rgba: [255, 0, 0, 255].repeat(4),
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            width: Some(ImageDimension::Cells(4)),
            height: Some(ImageDimension::Cells(4)),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    };

    let prepared = prepare_with_plan(&record, PixelCellSize::new(10, 20), (10, 10))
        .expect("the requested rectangle fits");

    assert_eq!(prepared.columns, 4);
    assert_eq!(prepared.rows, 4);
    assert_eq!(prepared.plan.geometry.full_size, Size { cols: 4, rows: 4 });
    assert_eq!(
        prepared
            .raster
            .as_deref()
            .map(|image| (image.width, image.height)),
        Some((40, 80))
    );
}

#[test]
fn sixel_pixel_data_uses_the_same_padding_and_cell_measurement() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1bP0;1q\"1;1;1;1#1;2;100;0;0#1@\x1b\\"),
        b""
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(placement.dimensions(), (1, 1));
    assert_eq!(
        placement.render_record_arc().image.as_ref(),
        &DecodedImage {
            width: 2,
            height: 3,
            rgba: vec![255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        }
    );
}

#[test]
fn sixel_indexed_sources_survive_state_restore_and_palette_updates() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b[?1070l\x1bPq#1;2;100;0;0#1@\x1b\\"),
        b""
    );
    let encoded = serde_json::to_value(engine.state()).expect("state serializes");
    assert!(encoded["image_contents"][0]["sixel"].is_object());

    let restored: TerminalState = serde_json::from_value(encoded).expect("state restores");
    assert_eq!(
        restored.image_placements(),
        engine.state().image_placements()
    );

    let mut restored_engine = TerminalEngine::from_state(restored, &[]);
    assert_eq!(restored_engine.advance(b"\x1bPq#1;2;0;100;0\x1b\\"), b"");
    assert_eq!(
        &restored_engine.state().image_placements()[0]
            .record()
            .image
            .rgba[..4],
        [0, 255, 0, 255]
    );
}

#[test]
fn percent_and_mixed_iterm_dimensions_use_the_shared_grid() {
    let mut record = ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
        }),
        animation: None,
        action: ImageAction::Display,
        display: crate::graphics::ImageDisplay {
            width: Some(ImageDimension::Percent(50)),
            height: Some(ImageDimension::Cells(1)),
            preserve_aspect_ratio: false,
            ..crate::graphics::ImageDisplay::default()
        },
        anchor: (0, 0),
    };
    let (columns, rows, raster) =
        prepare(&record, PixelCellSize::new(2, 3), (8, 8)).expect("pixel dimensions");
    assert_eq!((columns, rows), (4, 1));
    assert_eq!(
        raster.as_deref(),
        Some(&DecodedImage {
            width: 8,
            height: 3,
            rgba: [255, 0, 0, 255].repeat(24)
        })
    );
    record.display.width = Some(ImageDimension::Pixels(4));
    let (columns, rows, raster) =
        prepare(&record, PixelCellSize::new(2, 3), (8, 8)).expect("mixed dimensions");
    assert_eq!((columns, rows), (2, 1));
    assert_eq!(
        raster.as_deref(),
        Some(&DecodedImage {
            width: 4,
            height: 3,
            rgba: [255, 0, 0, 255].repeat(12)
        })
    );
}

#[test]
fn measurements_change_new_images_but_do_not_resize_existing_placements() {
    let mut engine = engine();
    let image = b"\x1b_Ga=T,f=32,s=1,v=1,c=3,C=1,q=2;/wAA/w==\x1b\\";
    assert_eq!(engine.advance(image), b"");
    engine.set_cell_size(PixelCellSize::new(2, 2).expect("nonzero cell"));
    assert_eq!(engine.advance(image), b"");
    assert_eq!(
        engine
            .state()
            .image_placements()
            .iter()
            .map(ImagePlacement::dimensions)
            .collect::<Vec<_>>(),
        [(2, 3), (3, 3)]
    );
    assert_eq!(
        engine.advance(b"\x1b[16t\x1b[14t\x1b[18t"),
        b"\x1b[6;2;2t\x1b[4;16;16t\x1b[8;8;8t"
    );
}
