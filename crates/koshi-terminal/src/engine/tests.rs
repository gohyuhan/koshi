//! Tests for the per-pane terminal engine: construction, chunked byte
//! decoding across `advance` calls, device-reply return, resize delegation,
//! taking the state apart and rebuilding it, and carrying a half-received
//! escape sequence to the next parser.

use std::path::Path;
use std::time::{Duration, Instant};

use koshi_core::process::PtySize;

use crate::state::Screen;
use crate::style::{Color, Style};

use super::*;

/// The bytes the PTY reader hands the runtime in one go, so a scale test feeds
/// the engine the same chunk size a running pane does.
const READ_CHUNK: usize = 8192;

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

// --- Taking the state apart and rebuilding it ---

/// Everything the state holds must survive being written out and read back:
/// both screen buffers, both cursors and their saved snapshots, the pen, the
/// modes, the scrollback with its truncation tallies, the title, and the
/// grapheme cluster still open at the cursor.
#[test]
fn a_driven_engine_state_survives_a_serde_round_trip() {
    let mut engine = TerminalEngine::with_scrollback(
        PtySize { cols: 8, rows: 3 },
        ScrollbackLimit::new(4, 4096),
    );

    // Bold red pen, then ten characters on an eight-column row, so the row
    // soft-wraps onto the row below it.
    let _ = engine.advance(b"\x1b[1;31mabcdefghij");
    // A wide CJK glyph, which takes two columns.
    let _ = engine.advance("世".as_bytes());
    // DECSC saves the cursor and the pen, both change, DECRC restores them.
    let _ = engine.advance(b"\x1b7\x1b[4;32mZ\x1b8");
    // Paint the alternate screen, then return to the primary.
    let _ = engine.advance(b"\x1b[?1049hALT\x1b[?1049l");
    // Ten line feeds on a three-row screen hand more rows to history than the
    // four-line cap holds, so the oldest are dropped and tallied.
    let _ = engine.advance(b"\n\n\n\n\n\n\n\n\n\n");
    // A title, then a base character with a combining acute over it: the
    // cluster is still open when the state is taken apart.
    let _ = engine.advance("\x1b]0;koshi\x07e\u{0301}".as_bytes());

    let state = engine.into_state();

    // Landmarks, so the comparison below is not two blank states.
    assert_eq!(state.title(), Some("koshi"));
    assert_eq!(state.active_screen(), Screen::Primary);
    assert_eq!(state.scrollback().len(), 4);
    // The cursor sat on row 1, so the first feed descends and the other nine
    // each hand a row to history: nine pushed, four kept, five dropped.
    assert_eq!(state.scrollback().dropped_lines(), 5);

    let written = serde_json::to_string(&state).expect("the state writes out");
    let read_back: TerminalState = serde_json::from_str(&written).expect("the state reads back");
    assert_eq!(read_back, state);

    // An engine rebuilt from the recovered state holds exactly that state.
    let rebuilt = TerminalEngine::from_state(read_back, b"");
    assert_eq!(rebuilt.state(), &state);
}

// --- Carrying a half-received sequence to the next parser ---

/// Take `engine` apart the way a process-image swap does and build the next
/// engine from what crossed: the screen state and the bytes the parser held.
fn swapped(engine: TerminalEngine) -> TerminalEngine {
    let carried = engine.undecoded().to_vec();
    TerminalEngine::from_state(engine.into_state(), &carried)
}

/// A chunk that ends on a sequence boundary leaves the next parser nothing to
/// take over.
#[test]
fn a_finished_chunk_leaves_the_parser_holding_nothing() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b[31mab");

    assert_eq!(engine.undecoded(), b"");
}

/// A working-directory report (OSC 7) cut in half is carried whole, so the pane
/// keeps its old directory until the report finishes and no part of the URI
/// prints as text.
#[test]
fn a_split_working_directory_report_is_carried_whole() {
    let mut engine = engine();

    // The shell reports /Users/yuhan/Projects/koshi, and the chunk ends after
    // `/Proj`.
    let _ = engine.advance(b"\x1b]7;file://host/Users/yuhan/Proj");

    assert_eq!(engine.state().current_cwd(), None);
    assert_eq!(engine.undecoded(), b"\x1b]7;file://host/Users/yuhan/Proj");

    let mut next = swapped(engine);
    let _ = next.advance(b"ects/koshi\x07");

    let cwd = next.state().current_cwd().expect("the report finished");
    assert_eq!(cwd.host(), Some("host"));
    assert_eq!(cwd.path(), Path::new("/Users/yuhan/Projects/koshi"));
    // The tail joined the sequence instead of landing on the screen.
    assert_eq!(ch(&next, 0, 0), ' ');
    assert_eq!(next.state().active_cursor_position(), (0, 0));
}

/// A title report (OSC 0) cut in half is carried whole, so the title changes
/// once, to the whole payload.
#[test]
fn a_split_title_report_is_carried_whole() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]0;ti");

    assert_eq!(engine.state().title(), None);
    assert_eq!(engine.undecoded(), b"\x1b]0;ti");

    let mut next = swapped(engine);
    let _ = next.advance(b"tle\x07");

    assert_eq!(next.state().title(), Some("title"));
    assert_eq!(ch(&next, 0, 0), ' ');
}

/// A CSI cut in half is carried whole, so its final byte completes the sequence
/// in the next parser instead of printing as text.
#[test]
fn a_split_csi_is_carried_whole() {
    let mut engine = engine();

    // SGR 31 (red foreground) cut off before its final `m`.
    let _ = engine.advance(b"\x1b[31");

    assert_eq!(engine.undecoded(), b"\x1b[31");

    let mut next = swapped(engine);
    let _ = next.advance(b"mZ");

    let cell = next
        .state()
        .active_grid()
        .cell(0, 0)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    assert_eq!(cell.ch(), 'Z');
    assert_eq!(cell.style(), red);
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// A UTF-8 code point cut in half is carried whole, so the next parser prints
/// the glyph rather than two replacement characters.
#[test]
fn a_split_code_point_is_carried_whole() {
    let mut engine = engine();

    // 'é' is 0xC3 0xA9; only its first byte arrives.
    let _ = engine.advance(b"\xc3");

    assert_eq!(ch(&engine, 0, 0), ' ');
    assert_eq!(engine.undecoded(), b"\xc3");

    let mut next = swapped(engine);
    let _ = next.advance(b"\xa9");

    assert_eq!(ch(&next, 0, 0), 'é');
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// A sequence spread over three chunks with no escape byte in the last two is
/// still carried whole.
#[test]
fn a_sequence_spread_over_three_chunks_is_carried_whole() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]0;ko");
    let _ = engine.advance(b"s");
    let _ = engine.advance(b"hi");

    assert_eq!(engine.undecoded(), b"\x1b]0;koshi");

    let mut next = swapped(engine);
    let _ = next.advance(b"\x07");

    assert_eq!(next.state().title(), Some("koshi"));
}

/// Text after a finished sequence leaves the parser holding nothing, so the
/// carry never replays glyphs that already reached the screen.
#[test]
fn text_after_a_finished_sequence_is_not_carried() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]0;koshi\x07ab");

    assert_eq!(engine.undecoded(), b"");

    let next = swapped(engine);

    assert_eq!(ch(&next, 0, 0), 'a');
    assert_eq!(ch(&next, 0, 1), 'b');
    assert_eq!(ch(&next, 0, 2), ' ');
    assert_eq!(next.state().active_cursor_position(), (0, 2));
}

/// A CSI sequence holding a control character is carried whole. The control
/// character reached the screen as it arrived and does not land twice, and the
/// sequence still swallows its final byte after the swap.
#[test]
fn a_sequence_holding_a_control_character_is_carried_without_repeating_it() {
    let mut engine = engine();

    // A line feed between the parameter and the rest of the sequence.
    let _ = engine.advance(b"\x1b[1\n3");

    assert_eq!(engine.undecoded(), b"\x1b[1\n3");
    assert_eq!(engine.state().active_cursor_position(), (1, 0));

    let mut next = swapped(engine);

    // The replayed line feed moved no cursor a second time.
    assert_eq!(next.state().active_cursor_position(), (1, 0));

    // `m` finishes the sequence as SGR 13, which koshi ignores. It reaches the
    // grid as neither a glyph nor a cursor step.
    let _ = next.advance(b"m");

    assert_eq!(ch(&next, 1, 0), ' ');
    assert_eq!(next.state().active_cursor_position(), (1, 0));
}

/// A device control string cut in the middle of its body carries its opening
/// bytes, so the rest of the body is swallowed after the swap instead of
/// printing as text.
#[test]
fn a_split_device_control_string_carries_its_opening_bytes() {
    let mut engine = engine();

    // A sixel image: `ESC P q` opens the string and the payload follows.
    let _ = engine.advance(b"\x1bPq#0;2;0;0;0");

    assert_eq!(engine.undecoded(), b"\x1bPq");
    assert_eq!(ch(&engine, 0, 0), ' ');
    assert_eq!(engine.state().active_cursor_position(), (0, 0));

    // More payload, with no escape byte in the chunk: what is carried stays the
    // opening bytes and does not grow with the image.
    let _ = engine.advance(b"#0~~@@");

    assert_eq!(engine.undecoded(), b"\x1bPq");

    let mut next = swapped(engine);

    // The rest of the payload is swallowed by the resumed string; only the `Z`
    // after the terminator reaches the grid.
    let _ = next.advance(b"vv@@~~$\x1b\\Z");

    assert_eq!(ch(&next, 0, 0), 'Z');
    assert_eq!(ch(&next, 0, 1), ' ');
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// A device control string closed by the 8-bit terminator `0x9c` — the one
/// ending that reaches no escape byte — leaves the parser on a sequence
/// boundary, so nothing is carried and the text after it prints as text.
#[test]
fn a_device_control_string_closed_by_the_eight_bit_terminator_is_not_carried() {
    let mut engine = engine();

    // `ESC P q` opens the string, `0x9c` closes it, and `Z` follows it.
    let _ = engine.advance(b"\x1bPq#0~~\x9cZ");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));

    let mut next = swapped(engine);
    let _ = next.advance(b"Y");

    assert_eq!(ch(&next, 0, 1), 'Y');
    assert_eq!(next.state().active_cursor_position(), (0, 2));
}

/// `CAN` (`0x18`) abandons the sequence it lands in without dispatching it, so
/// the parser is back on a sequence boundary and nothing is carried.
#[test]
fn a_cancelled_sequence_is_not_carried() {
    let mut engine = engine();

    // SGR 31 abandoned mid-parameter by `CAN`.
    let _ = engine.advance(b"\x1b[31\x18");

    assert_eq!(engine.undecoded(), b"");

    // The next chunk starts a fresh text run, and none of it is carried.
    let _ = engine.advance(b"Z");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

/// Chunks that hold no escape byte and open no sequence leave nothing to carry,
/// however many of them arrive: text and control characters both decode whole.
#[test]
fn plain_chunks_on_a_sequence_boundary_carry_nothing() {
    let mut engine = TerminalEngine::new(PtySize { cols: 80, rows: 24 });

    for _ in 0..64 {
        let _ = engine.advance(&[b'a'; READ_CHUNK]);
        assert_eq!(engine.undecoded(), b"");

        let _ = engine.advance(&[b'\n'; READ_CHUNK]);
        assert_eq!(engine.undecoded(), b"");
    }
}

/// A clipboard write (OSC 52) whose payload outruns many reads gives every
/// chunk after the first a body with no escape byte in it. Each chunk must cost
/// one pass over that chunk: reading the whole held sequence again per chunk
/// makes the cost grow with the square of the payload, and the pane's
/// dispatcher turn is the thread that repaints every client. The carry holds
/// the payload up to `MAX_UNDECODED` and drops it past that, so the pane's
/// memory does not grow with the payload.
#[test]
fn a_clipboard_write_spread_over_many_chunks_stays_linear() {
    const PAYLOAD: usize = 8 * 1024 * 1024;
    let opening = b"\x1b]52;c;";
    let payload = vec![b'A'; PAYLOAD];

    let mut engine = engine();
    let started = Instant::now();

    let _ = engine.advance(opening);
    assert_eq!(engine.undecoded(), opening);

    for (round, chunk) in payload.chunks(READ_CHUNK).enumerate() {
        let _ = engine.advance(chunk);
        let held = opening.len() + (round + 1) * READ_CHUNK;
        if held <= MAX_UNDECODED {
            assert_eq!(engine.undecoded().len(), held);
        } else {
            assert_eq!(engine.undecoded(), b"");
        }
    }
    let elapsed = started.elapsed();

    // One pass per chunk lands in the hundreds of milliseconds. Reading the
    // held sequence again per chunk reads four gigabytes and takes tens of
    // seconds.
    assert!(
        elapsed < Duration::from_secs(3),
        "the payload took {elapsed:?}",
    );

    // The payload passed `MAX_UNDECODED`, so the carry is empty. The real
    // parser still swallows the body: no part of it printed.
    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), ' ');
    assert_eq!(engine.state().active_cursor_position(), (0, 0));

    // koshi handles no clipboard write, so the terminator only closes the
    // sequence and the `Z` after it prints.
    let _ = engine.advance(b"\x07Z");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

/// A device control string that ends inside the chunk that opened it leaves the
/// parser on a sequence boundary, so nothing is carried.
#[test]
fn a_finished_device_control_string_is_not_carried() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1bPq#0~~\x1b\\");

    assert_eq!(engine.undecoded(), b"");

    let mut next = swapped(engine);
    let _ = next.advance(b"Z");

    assert_eq!(ch(&next, 0, 0), 'Z');
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// The parser drops the body of a start of string, a privacy message and an
/// application program command, so each one carries its two opening bytes and
/// no more, however long the body runs.
#[test]
fn a_string_whose_body_the_parser_drops_carries_only_its_opening_bytes() {
    for opening in [b"\x1bX", b"\x1b^", b"\x1b_"] {
        let mut engine = engine();

        let _ = engine.advance(opening);
        assert_eq!(engine.undecoded(), opening);

        // A megabyte of body arriving one read at a time adds nothing.
        let body = vec![b'y'; 1024 * 1024];
        for chunk in body.chunks(READ_CHUNK) {
            let _ = engine.advance(chunk);
            assert_eq!(engine.undecoded(), opening);
        }

        assert_eq!(ch(&engine, 0, 0), ' ');
        assert_eq!(engine.state().active_cursor_position(), (0, 0));

        // The two carried bytes put the next parser back inside the body: the
        // rest of it is swallowed and only the `Z` after `ESC \` prints.
        let mut next = swapped(engine);
        let _ = next.advance(b"yyy\x1b\\Z");

        assert_eq!(next.undecoded(), b"");
        assert_eq!(ch(&next, 0, 0), 'Z');
        assert_eq!(next.state().active_cursor_position(), (0, 1));
    }
}

/// An application program command whose two opening bytes are split across
/// chunks — the kitty graphics protocol's `ESC _` — still carries exactly those
/// two bytes once the second one arrives.
#[test]
fn a_split_application_program_command_opening_carries_both_of_its_bytes() {
    let mut engine = engine();

    // The chunk ends on the escape byte alone.
    let _ = engine.advance(b"\x1b");
    assert_eq!(engine.undecoded(), b"\x1b");

    // The `_` and the start of a kitty graphics payload arrive next.
    let _ = engine.advance(b"_Ga=T,f=100;iVBORw0KGgo");
    assert_eq!(engine.undecoded(), b"\x1b_");

    let mut next = swapped(engine);
    let _ = next.advance(b"AAANSUhEUg\x1b\\Z");

    assert_eq!(ch(&next, 0, 0), 'Z');
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// An operating system command longer than `MAX_UNDECODED` stops being held, so
/// one pane cannot grow the engine's memory without a bound. The real parser
/// still swallows the body, and the sequence's end returns the carry to empty.
#[test]
fn an_operating_system_command_past_the_limit_is_not_held() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]52;c;");
    assert_eq!(engine.undecoded(), b"\x1b]52;c;");

    // One chunk short of the limit, the whole sequence is still held.
    let under = vec![b'A'; MAX_UNDECODED - READ_CHUNK];
    for chunk in under.chunks(READ_CHUNK) {
        let _ = engine.advance(chunk);
    }
    assert_eq!(engine.undecoded().len(), 7 + MAX_UNDECODED - READ_CHUNK);

    // The next chunk passes the limit, and the carry drops to empty. The
    // buffer is released, not cleared, so the pane keeps no room for it.
    let _ = engine.advance(&[b'A'; READ_CHUNK]);
    assert_eq!(engine.undecoded(), b"");
    assert_eq!(engine.undecoded.capacity(), 0);

    // More body changes nothing, and none of it prints.
    let _ = engine.advance(&[b'A'; READ_CHUNK]);
    assert_eq!(engine.undecoded(), b"");
    assert_eq!(engine.undecoded.capacity(), 0);
    assert_eq!(ch(&engine, 0, 0), ' ');
    assert_eq!(engine.state().active_cursor_position(), (0, 0));

    // The terminator closes the sequence and the `Z` after it prints.
    let _ = engine.advance(b"\x07Z");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

/// The limit guards every sequence kind, not only strings: a control sequence
/// whose parameter digits never end holds no more than `MAX_UNDECODED` either.
#[test]
fn a_control_sequence_with_endless_parameters_is_not_held_past_the_limit() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b[");
    assert_eq!(engine.undecoded(), b"\x1b[");

    let digits = vec![b'1'; 2 * MAX_UNDECODED];
    for chunk in digits.chunks(READ_CHUNK) {
        let _ = engine.advance(chunk);
        assert!(
            engine.undecoded().len() <= MAX_UNDECODED,
            "the carry grew to {} bytes",
            engine.undecoded().len(),
        );
    }

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), ' ');

    // `m` finishes it as an SGR koshi ignores; the `Z` after it prints.
    let _ = engine.advance(b"mZ");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}
