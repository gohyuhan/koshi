//! Tests for the per-pane terminal engine: construction, chunked byte
//! decoding across `advance` calls, and resize delegation.

use tile_core::process::PtySize;

use crate::style::{Color, Style};

use super::*;

fn engine() -> TerminalEngine {
    TerminalEngine::new(PtySize { cols: 8, rows: 3 })
}

/// The character at (`row`, `col`) on the engine's active grid.
fn ch(engine: &TerminalEngine, row: u16, col: u16) -> char {
    engine
        .state()
        .active_grid()
        .cell(row, col)
        .expect("cell in bounds")
        .ch()
}

#[test]
fn new_engine_is_blank_at_the_given_size() {
    let engine = engine();

    assert_eq!(engine.state().active_grid().dimensions(), (3, 8));
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
    assert_eq!(ch(&engine, 0, 0), ' ');
}

#[test]
fn advance_prints_text_into_the_grid() {
    let mut engine = engine();

    engine.advance(b"hi");

    assert_eq!(ch(&engine, 0, 0), 'h');
    assert_eq!(ch(&engine, 0, 1), 'i');
    assert_eq!(engine.state().active_cursor_position(), (0, 2));
}

#[test]
fn an_escape_sequence_split_across_chunks_decodes_once() {
    let mut engine = engine();

    // SGR 31 (red foreground) split mid-sequence across two chunks.
    engine.advance(b"\x1b[3");
    engine.advance(b"1mx");

    let cell = engine
        .state()
        .active_grid()
        .cell(0, 0)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    assert_eq!(cell.ch(), 'x');
    assert_eq!(cell.style(), red);
}

#[test]
fn a_utf8_code_point_split_across_chunks_decodes_once() {
    let mut engine = engine();

    // 'é' (0xC3 0xA9) split between its two bytes.
    engine.advance(b"\xc3");
    engine.advance(b"\xa9");

    assert_eq!(ch(&engine, 0, 0), 'é');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

#[test]
fn resize_resizes_the_state() {
    let mut engine = engine();

    engine.resize(PtySize { cols: 4, rows: 2 });

    assert_eq!(engine.state().active_grid().dimensions(), (2, 4));
}

#[test]
fn a_partial_decode_survives_a_resize() {
    let mut engine = engine();

    // The sequence opens before the resize and completes after it: the pen
    // still turns red and the glyph lands styled.
    engine.advance(b"\x1b[3");
    engine.resize(PtySize { cols: 4, rows: 2 });
    engine.advance(b"1mx");

    let cell = engine
        .state()
        .active_grid()
        .cell(0, 0)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    assert_eq!(cell.ch(), 'x');
    assert_eq!(cell.style(), red);
}
