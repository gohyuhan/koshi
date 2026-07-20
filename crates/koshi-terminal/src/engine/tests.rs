//! Tests for the per-pane terminal engine: construction, chunked byte
//! decoding across `advance` calls, device-reply return, and resize
//! delegation.

use koshi_core::process::PtySize;

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
fn advance_prints_text_into_the_grid_and_returns_no_replies() {
    let mut engine = engine();

    assert_eq!(engine.advance(b"hi"), b"");

    assert_eq!(ch(&engine, 0, 0), 'h');
    assert_eq!(ch(&engine, 0, 1), 'i');
    assert_eq!(engine.state().active_cursor_position(), (0, 2));
}

#[test]
fn an_escape_sequence_split_across_chunks_decodes_once() {
    let mut engine = engine();

    // SGR 31 (red foreground) split mid-sequence across two chunks.
    assert_eq!(engine.advance(b"\x1b[3"), b"");
    assert_eq!(engine.advance(b"1mx"), b"");

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
    assert_eq!(engine.advance(b"\xc3"), b"");
    assert_eq!(engine.advance(b"\xa9"), b"");

    assert_eq!(ch(&engine, 0, 0), 'é');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

#[test]
fn advance_returns_a_querys_reply_bytes() {
    let mut engine = engine();

    assert_eq!(engine.advance(b"\x1b[5n"), b"\x1b[0n");
}

#[test]
fn a_query_split_across_chunks_replies_on_the_completing_chunk() {
    let mut engine = engine();

    assert_eq!(engine.advance(b"\x1b[6"), b"");
    assert_eq!(engine.advance(b"n"), b"\x1b[1;1R");
}

#[test]
fn advance_drains_the_reply_queue_each_call() {
    let mut engine = engine();

    assert_eq!(engine.advance(b"\x1b[5n"), b"\x1b[0n");
    // The reply was handed out above; the next chunk starts empty.
    assert_eq!(engine.advance(b"x"), b"");
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
    assert_eq!(engine.advance(b"\x1b[3"), b"");
    engine.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(engine.advance(b"1mx"), b"");

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

// --- Adversarial: chunk-split torture and scale ---

/// A mixed run of SGR, cursor moves, an erase, line feeds, and text. Fed both
/// whole and one byte at a time, the parser must reach byte-identical state:
/// splitting a sequence at any boundary may never change the outcome.
#[test]
fn a_sequence_split_at_every_byte_boundary_matches_the_whole_feed() {
    let seq = b"\x1b[1;31mAB\x1b[2;3HCD\r\n\x1b[Kxy";

    let mut whole = engine();
    let _ = whole.advance(seq);

    let mut split = engine();
    for byte in seq {
        let _ = split.advance(&[*byte]);
    }

    // Concrete landmarks so the comparison is not vacuously two blank grids.
    assert_eq!(ch(&whole, 0, 0), 'A');
    assert_eq!(ch(&whole, 1, 2), 'C');
    assert_eq!(ch(&whole, 2, 0), 'x');
    assert_eq!(whole.state().active_cursor_position(), (2, 2));

    // The one-byte-at-a-time feed lands on exactly the same grid and cursor.
    assert_eq!(whole.state().active_grid(), split.state().active_grid());
    assert_eq!(
        whole.state().active_cursor_position(),
        split.state().active_cursor_position(),
    );
}

#[test]
fn a_three_byte_wide_char_split_across_chunks_decodes_once() {
    let mut engine = engine();

    // '世' is 0xE4 0xB8 0x96 — a wide CJK glyph split after its first byte.
    assert_eq!(engine.advance(b"\xe4"), b"");
    assert_eq!(engine.advance(b"\xb8\x96"), b"");

    let cell = engine
        .state()
        .active_grid()
        .cell(0, 0)
        .expect("cell in bounds");
    assert_eq!(cell.ch(), '世');
    assert_eq!(cell.width(), 2);
    assert_eq!(engine.state().active_cursor_position(), (0, 2));
}

#[test]
fn a_truncated_csi_resumes_and_applies_on_the_next_chunk() {
    let mut engine = engine();

    let _ = engine.advance(b"abc"); // fill row 0
    let _ = engine.advance(b"\x1b["); // CSI opened but not completed — held
    let _ = engine.advance(b"2J"); // completes ED 2 across the chunk boundary

    // The held CSI resumed and cleared the whole screen.
    assert_eq!(ch(&engine, 0, 0), ' ');
    assert_eq!(ch(&engine, 0, 1), ' ');
    assert_eq!(ch(&engine, 0, 2), ' ');
}

#[test]
fn an_escape_split_from_its_bracket_still_forms_a_csi() {
    let mut engine = engine();

    let _ = engine.advance(b"abc"); // fill row 0
    let _ = engine.advance(b"\x1b"); // lone ESC at a chunk end — held in Escape
    let _ = engine.advance(b"[2J"); // the bracket + ED 2 arrive next

    assert_eq!(ch(&engine, 0, 0), ' ');
    assert_eq!(ch(&engine, 0, 1), ' ');
    assert_eq!(ch(&engine, 0, 2), ' ');
}

#[test]
fn a_ten_thousand_column_line_wraps_without_panicking() {
    let mut engine = TerminalEngine::new(PtySize { cols: 80, rows: 24 });

    let flood = vec![b'a'; 10_000];
    let _ = engine.advance(&flood);

    // 10000 / 80 = 125 logical rows; the last parks unscrolled, so the bottom
    // row holds the final run and the cursor rests on the last column.
    assert_eq!(engine.state().active_cursor_position(), (23, 79));
    assert_eq!(ch(&engine, 23, 0), 'a');
    assert_eq!(ch(&engine, 23, 79), 'a');
    // 125 rows produced, 24 on screen (the last unscrolled) → 101 in history.
    assert_eq!(engine.state().scrollback().len(), 101);
}

#[test]
fn many_line_feeds_cap_the_scrollback_and_tally_the_drops() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    // 12000 line feeds on a 2-row screen: the first descends without scrolling,
    // the remaining 11999 each push one row into history.
    let feeds = vec![b'\n'; 12_000];
    let _ = engine.advance(&feeds);

    // The default 10 000-line cap holds; the overflow is dropped and tallied.
    assert_eq!(engine.state().scrollback().len(), 10_000);
    assert_eq!(engine.state().scrollback().dropped_lines(), 1_999);
    assert_eq!(engine.state().active_cursor_position(), (1, 0));
}
