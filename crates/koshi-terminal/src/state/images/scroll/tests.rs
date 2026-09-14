//! Scrolling and resizing keep the image scale while clipping its visible cells.

use super::*;

type ImagePlacementGeometry = ((u16, u16), (u16, u16), ImageCellGeometry);
use crate::engine::TerminalEngine;
use crate::graphics::{GraphicsError, GraphicsProtocol};
use crate::state::ImagePlacementError;
use koshi_core::geometry::PixelCellSize;
use koshi_core::process::PtySize;

const KITTY_IMAGE_COMMAND: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=2,c=2,r=2,C=1,i=7,q=2;/wAA/wD/AP8=\x1b\\";
const SIXEL_TWO_ROW_COMMAND: &[u8] = b"\x1bPq\"1;1;1;1#1;2;100;0;0#1@-@\x1b\\";
const SIXEL_ONE_ROW_COMMAND: &[u8] = b"\x1bPq\"1;1;1;1#1;2;100;0;0#1@\x1b\\";

fn build_test_terminal_engine() -> TerminalEngine {
    TerminalEngine::from_pty_size(PtySize {
        column_count: 6,
        row_count: 4,
    })
}

fn list_image_placement_geometries_for_view(
    engine: &TerminalEngine,
    scrollback_offset: usize,
) -> Vec<ImagePlacementGeometry> {
    engine
        .get_terminal_state()
        .list_image_placements_for_view(scrollback_offset)
        .iter()
        .map(|image_placement| {
            (
                image_placement.get_image_anchor(),
                image_placement.get_image_cell_dimensions(),
                image_placement.get_image_geometry(),
            )
        })
        .collect()
}

fn build_image_cell_geometry(column_offset: u16, row_offset: u16) -> ImageCellGeometry {
    ImageCellGeometry {
        full_size: Size {
            column_count: 2,
            row_count: 2,
        },
        cell_offset: Point {
            column: column_offset,
            row: row_offset,
        },
    }
}

#[test]
fn scrolling_one_row_clips_the_image_and_scrollback_restores_the_full_view() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
    assert_eq!(engine.process_pty_output(b"\x1b[S"), b"");
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 0),
        [((0, 0), (1, 2), build_image_cell_geometry(0, 1))]
    );
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 1),
        [((0, 0), (2, 2), build_image_cell_geometry(0, 0))]
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
    let terminal_state: TerminalState = serde_json::from_slice(
        &serde_json::to_vec(engine.get_terminal_state()).expect("serialize"),
    )
    .expect("restore");
    let restored_terminal_engine = TerminalEngine::from_terminal_state(terminal_state, &[]);
    assert_eq!(
        list_image_placement_geometries_for_view(&restored_terminal_engine, 0),
        list_image_placement_geometries_for_view(&engine, 0)
    );
    assert_eq!(
        list_image_placement_geometries_for_view(&restored_terminal_engine, 1),
        list_image_placement_geometries_for_view(&engine, 1)
    );
}

#[test]
fn images_crossing_a_margin_stay_in_place() {
    for screen_command_bytes in [b"".as_slice(), b"\x1b[?1049h".as_slice()] {
        let mut engine = build_test_terminal_engine();
        assert_eq!(engine.process_pty_output(screen_command_bytes), b"");
        assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
        assert_eq!(engine.process_pty_output(b"\x1b[2;4r\x1b[S"), b"");
        assert_eq!(
            list_image_placement_geometries_for_view(&engine, 0),
            [((0, 0), (2, 2), build_image_cell_geometry(0, 0))]
        );
    }
}

#[test]
fn images_inside_a_margin_are_clipped_when_scrolled_up() {
    for screen_command_bytes in [b"".as_slice(), b"\x1b[?1049h".as_slice()] {
        let mut engine = build_test_terminal_engine();
        assert_eq!(engine.process_pty_output(screen_command_bytes), b"");
        assert_eq!(engine.process_pty_output(b"\x1b[2;1H"), b"");
        assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
        assert_eq!(engine.process_pty_output(b"\x1b[2;4r\x1b[S"), b"");
        assert_eq!(
            list_image_placement_geometries_for_view(&engine, 0),
            [((1, 0), (1, 2), build_image_cell_geometry(0, 1))]
        );
    }
}

#[test]
fn a_margin_starting_at_the_top_clips_images_without_leaking_them_into_history() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
    assert_eq!(engine.process_pty_output(b"\x1b[1;3r\x1b[S"), b"");
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 0),
        [((0, 0), (1, 2), build_image_cell_geometry(0, 1))]
    );
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 1),
        [((1, 0), (1, 2), build_image_cell_geometry(0, 1))]
    );
}

#[test]
fn scrolling_down_clips_the_bottom_without_changing_the_top_source_offset() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(engine.process_pty_output(b"\x1b[3;1H"), b"");
    assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
    assert_eq!(engine.process_pty_output(b"\x1b[T"), b"");
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 0),
        [((3, 0), (1, 2), build_image_cell_geometry(0, 0))]
    );
}

#[test]
fn alternate_resize_clips_top_and_right_without_erasing_the_image() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(engine.process_pty_output(b"\x1b[?1049h"), b"");
    assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
    engine.resize_terminal_state(PtySize {
        column_count: 1,
        row_count: 3,
    });
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 0),
        [((0, 0), (1, 1), build_image_cell_geometry(0, 1))]
    );
    engine.resize_terminal_state(PtySize {
        column_count: 6,
        row_count: 4,
    });
    assert_eq!(
        list_image_placement_geometries_for_view(&engine, 0),
        [((0, 0), (1, 1), build_image_cell_geometry(0, 1))]
    );
}

#[test]
fn a_source_rectangle_is_intersected_with_the_image() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_IMAGE_COMMAND), b"");
    assert_eq!(
        engine.process_pty_output(
            b"\x1b_Ga=p,i=7,y=1,h=4294967295,w=4294967295,c=2,r=1,C=1,q=2\x1b\\"
        ),
        b""
    );
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[1]
            .get_image_record()
            .compute_source_rect(),
        Ok((0, 1, 1, 1))
    );
}

#[test]
fn sixel_scrolls_only_after_the_image_passes_admission() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(2, 6).expect("nonzero cell size"));
    assert_eq!(engine.process_pty_output(b"\x1b[3;1H"), b"");
    assert_eq!(engine.process_pty_output(SIXEL_TWO_ROW_COMMAND), b"");

    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()
            .len(),
        1
    );
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0].get_image_anchor(),
        (1, 0)
    );
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0].get_image_cell_dimensions(),
        (2, 1)
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (2, 0)
    );
}

#[test]
fn a_rejected_sixel_does_not_scroll_or_move_the_cursor() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    assert_eq!(engine.process_pty_output(b"\x1b[3;1H"), b"");
    assert_eq!(engine.process_pty_output(SIXEL_TWO_ROW_COMMAND), b"");

    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()
            .len(),
        0
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (2, 0)
    );
    assert!(engine
        .get_terminal_state()
        .list_image_placements()
        .is_empty());
    assert!(matches!(
        engine.take_graphics_events().as_slice(),
        [Err(GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            placement_error: ImagePlacementError::MissingCellDimensions { .. },
        })]
    ));
}

#[test]
fn fixed_sixel_output_uses_the_home_anchor_without_moving_the_cursor() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(2, 6).expect("nonzero cell size"));
    assert_eq!(engine.process_pty_output(b"\x1b[?80h\x1b[3;4H"), b"");
    assert_eq!(engine.process_pty_output(SIXEL_ONE_ROW_COMMAND), b"");

    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0].get_image_anchor(),
        (0, 0)
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (2, 3)
    );
    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()
            .len(),
        0
    );
}

#[test]
fn sixel_cursor_right_wraps_and_scrolls_at_the_right_edge() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 3,
    });
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(2, 6).expect("nonzero cell size"));
    assert_eq!(engine.process_pty_output(b"\x1b[?8452h\x1b[3;4H"), b"");
    assert_eq!(engine.process_pty_output(SIXEL_ONE_ROW_COMMAND), b"");

    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0].get_image_anchor(),
        (1, 3)
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (2, 0)
    );
    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()
            .len(),
        1
    );
}
