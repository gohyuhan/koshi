//! Scrolling and resizing keep the image scale while clipping its visible cells.

use super::*;

type PlacementGeometry = ((u16, u16), (u16, u16), ImageCellGeometry);
use crate::engine::TerminalEngine;
use crate::graphics::{GraphicsError, GraphicsProtocol};
use crate::state::ImagePlacementError;
use koshi_core::geometry::PixelCellSize;
use koshi_core::process::PtySize;

const IMAGE: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=2,c=2,r=2,C=1,i=7,q=2;/wAA/wD/AP8=\x1b\\";
const SIXEL_TWO_ROWS: &[u8] = b"\x1bPq\"1;1;1;1#1;2;100;0;0#1@-@\x1b\\";
const SIXEL_ONE_ROW: &[u8] = b"\x1bPq\"1;1;1;1#1;2;100;0;0#1@\x1b\\";

fn engine() -> TerminalEngine {
    TerminalEngine::new(PtySize { cols: 6, rows: 4 })
}

fn view(engine: &TerminalEngine, offset: usize) -> Vec<PlacementGeometry> {
    engine
        .state()
        .image_placements_for_view(offset)
        .iter()
        .map(|placement| {
            (
                placement.anchor(),
                placement.dimensions(),
                placement.geometry(),
            )
        })
        .collect()
}

fn geometry(x: u16, y: u16) -> ImageCellGeometry {
    ImageCellGeometry {
        full_size: Size { cols: 2, rows: 2 },
        offset: Point { x, y },
    }
}

#[test]
fn scrolling_one_row_clips_the_image_and_scrollback_restores_the_full_view() {
    let mut engine = engine();
    assert_eq!(engine.advance(IMAGE), b"");
    assert_eq!(engine.advance(b"\x1b[S"), b"");
    assert_eq!(view(&engine, 0), [((0, 0), (1, 2), geometry(0, 1))]);
    assert_eq!(view(&engine, 1), [((0, 0), (2, 2), geometry(0, 0))]);
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
    let state: TerminalState =
        serde_json::from_slice(&serde_json::to_vec(engine.state()).expect("serialize"))
            .expect("restore");
    let restored = TerminalEngine::from_state(state, &[]);
    assert_eq!(view(&restored, 0), view(&engine, 0));
    assert_eq!(view(&restored, 1), view(&engine, 1));
}

#[test]
fn images_crossing_a_margin_stay_in_place() {
    for screen in [b"".as_slice(), b"\x1b[?1049h".as_slice()] {
        let mut engine = engine();
        assert_eq!(engine.advance(screen), b"");
        assert_eq!(engine.advance(IMAGE), b"");
        assert_eq!(engine.advance(b"\x1b[2;4r\x1b[S"), b"");
        assert_eq!(view(&engine, 0), [((0, 0), (2, 2), geometry(0, 0))]);
    }
}

#[test]
fn images_inside_a_margin_are_clipped_when_scrolled_up() {
    for screen in [b"".as_slice(), b"\x1b[?1049h".as_slice()] {
        let mut engine = engine();
        assert_eq!(engine.advance(screen), b"");
        assert_eq!(engine.advance(b"\x1b[2;1H"), b"");
        assert_eq!(engine.advance(IMAGE), b"");
        assert_eq!(engine.advance(b"\x1b[2;4r\x1b[S"), b"");
        assert_eq!(view(&engine, 0), [((1, 0), (1, 2), geometry(0, 1))]);
    }
}

#[test]
fn a_margin_starting_at_the_top_clips_images_without_leaking_them_into_history() {
    let mut engine = engine();
    assert_eq!(engine.advance(IMAGE), b"");
    assert_eq!(engine.advance(b"\x1b[1;3r\x1b[S"), b"");
    assert_eq!(view(&engine, 0), [((0, 0), (1, 2), geometry(0, 1))]);
    assert_eq!(view(&engine, 1), [((1, 0), (1, 2), geometry(0, 1))]);
}

#[test]
fn scrolling_down_clips_the_bottom_without_changing_the_top_source_offset() {
    let mut engine = engine();
    assert_eq!(engine.advance(b"\x1b[3;1H"), b"");
    assert_eq!(engine.advance(IMAGE), b"");
    assert_eq!(engine.advance(b"\x1b[T"), b"");
    assert_eq!(view(&engine, 0), [((3, 0), (1, 2), geometry(0, 0))]);
}

#[test]
fn alternate_resize_clips_top_and_right_without_erasing_the_image() {
    let mut engine = engine();
    assert_eq!(engine.advance(b"\x1b[?1049h"), b"");
    assert_eq!(engine.advance(IMAGE), b"");
    engine.resize(PtySize { cols: 1, rows: 3 });
    assert_eq!(view(&engine, 0), [((0, 0), (1, 1), geometry(0, 1))]);
    engine.resize(PtySize { cols: 6, rows: 4 });
    assert_eq!(view(&engine, 0), [((0, 0), (1, 1), geometry(0, 1))]);
}

#[test]
fn a_source_rectangle_is_intersected_with_the_image() {
    let mut engine = engine();
    assert_eq!(engine.advance(IMAGE), b"");
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,y=1,h=4294967295,w=4294967295,c=2,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.state().image_placements()[1].record().source_rect(),
        Ok((0, 1, 1, 1))
    );
}

#[test]
fn sixel_scrolls_only_after_the_image_passes_admission() {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 3 });
    engine.set_cell_size(PixelCellSize::new(2, 6).expect("nonzero cell size"));
    assert_eq!(engine.advance(b"\x1b[3;1H"), b"");
    assert_eq!(engine.advance(SIXEL_TWO_ROWS), b"");

    assert_eq!(engine.state().scrollback().lines().len(), 1);
    assert_eq!(engine.state().image_placements()[0].anchor(), (1, 0));
    assert_eq!(engine.state().image_placements()[0].dimensions(), (2, 1));
    assert_eq!(engine.state().active_cursor_position(), (2, 0));
}

#[test]
fn a_rejected_sixel_does_not_scroll_or_move_the_cursor() {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 3 });
    assert_eq!(engine.advance(b"\x1b[3;1H"), b"");
    assert_eq!(engine.advance(SIXEL_TWO_ROWS), b"");

    assert_eq!(engine.state().scrollback().lines().len(), 0);
    assert_eq!(engine.state().active_cursor_position(), (2, 0));
    assert!(engine.state().image_placements().is_empty());
    assert!(matches!(
        engine.take_graphics().as_slice(),
        [Err(GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            reason: ImagePlacementError::MissingCellDimensions { .. },
        })]
    ));
}

#[test]
fn fixed_sixel_output_uses_the_home_anchor_without_moving_the_cursor() {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 3 });
    engine.set_cell_size(PixelCellSize::new(2, 6).expect("nonzero cell size"));
    assert_eq!(engine.advance(b"\x1b[?80h\x1b[3;4H"), b"");
    assert_eq!(engine.advance(SIXEL_ONE_ROW), b"");

    assert_eq!(engine.state().image_placements()[0].anchor(), (0, 0));
    assert_eq!(engine.state().active_cursor_position(), (2, 3));
    assert_eq!(engine.state().scrollback().lines().len(), 0);
}

#[test]
fn sixel_cursor_right_wraps_and_scrolls_at_the_right_edge() {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 3 });
    engine.set_cell_size(PixelCellSize::new(2, 6).expect("nonzero cell size"));
    assert_eq!(engine.advance(b"\x1b[?8452h\x1b[3;4H"), b"");
    assert_eq!(engine.advance(SIXEL_ONE_ROW), b"");

    assert_eq!(engine.state().image_placements()[0].anchor(), (1, 3));
    assert_eq!(engine.state().active_cursor_position(), (2, 0));
    assert_eq!(engine.state().scrollback().lines().len(), 1);
}
