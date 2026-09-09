//! Tests for the per-pane terminal engine: construction, chunked byte
//! decoding across `advance` calls, device-reply return, resize delegation,
//! taking the state apart and rebuilding it, and carrying a half-received
//! escape sequence to the next parser.

use std::path::Path;
use std::time::Instant;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use koshi_core::geometry::PixelCellSize;
use koshi_core::process::PtySize;

use crate::graphics::{DecodedGraphics, DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay};
use crate::state::{ImagePlacementError, Screen, ShellIntegrationFact, TerminalState};
use crate::style::{Color, Style};

use super::*;

/// The bytes the PTY reader hands the runtime in one go, so a scale test feeds
/// the engine the same chunk size a running pane does.
const READ_CHUNK: usize = 8192;

fn engine() -> TerminalEngine {
    TerminalEngine::new(PtySize { cols: 8, rows: 3 })
}

#[test]
fn terminal_inert_compaction_keeps_all_other_bytes_and_exact_offsets() {
    let bytes = b"abCDEFghIJkl";
    let payloads = [2..6, 8..10];

    assert_eq!(without_terminal_inert(bytes, &payloads).as_ref(), b"abghkl");
    assert_eq!(without_terminal_inert_offset(0, &payloads), 0);
    assert_eq!(without_terminal_inert_offset(2, &payloads), 2);
    assert_eq!(without_terminal_inert_offset(4, &payloads), 2);
    assert_eq!(without_terminal_inert_offset(6, &payloads), 2);
    assert_eq!(without_terminal_inert_offset(8, &payloads), 4);
    assert_eq!(without_terminal_inert_offset(10, &payloads), 4);
    assert_eq!(without_terminal_inert_offset(12, &payloads), 6);
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
fn split_osc133_is_completed_by_the_next_chunk() {
    let mut engine = engine();

    assert_eq!(engine.advance(b"\x1b]133;"), b"");
    assert!(!engine.state().active_grid().prompt_mark(0));
    assert_eq!(engine.advance(b"A\x07"), b"");

    assert!(engine.state().active_grid().prompt_mark(0));
}

#[test]
fn advance_with_shell_integration_returns_command_facts_in_order() {
    let mut engine = engine();

    let (replies, facts) =
        engine.advance_with_shell_integration(b"\x1b]133;C\x07\x1b]133;D;137\x07");

    assert_eq!(replies, b"");
    assert_eq!(
        facts,
        vec![
            ShellIntegrationFact::CommandStarted,
            ShellIntegrationFact::CommandFinished {
                exit_code: Some(137),
            },
        ]
    );
}

#[test]
fn bare_shell_integration_finish_returns_no_exit_code() {
    let mut engine = engine();

    let (_, facts) = engine.advance_with_shell_integration(b"\x1b]133;C\x07\x1b]133;D\x07");

    assert_eq!(
        facts,
        vec![
            ShellIntegrationFact::CommandStarted,
            ShellIntegrationFact::CommandFinished { exit_code: None },
        ]
    );
}

#[test]
fn unmatched_shell_integration_finish_returns_no_fact() {
    let mut engine = engine();

    let (_, facts) = engine.advance_with_shell_integration(b"\x1b]133;D;1\x07");

    assert_eq!(facts, Vec::<ShellIntegrationFact>::new());
}

#[test]
fn advance_with_shell_integration_returns_replies_and_facts_from_one_chunk() {
    let mut engine = engine();

    let (replies, facts) = engine.advance_with_shell_integration(b"\x1b[5n\x1b]133;C\x07");

    assert_eq!(replies, b"\x1b[0n");
    assert_eq!(facts, vec![ShellIntegrationFact::CommandStarted]);
}

#[test]
fn advance_drains_shell_facts_without_returning_them() {
    let mut engine = engine();

    assert_eq!(engine.advance(b"\x1b]133;C\x07"), b"");

    let (replies, facts) = engine.advance_with_shell_integration(b"");

    assert_eq!(replies, b"");
    assert_eq!(facts, Vec::<ShellIntegrationFact>::new());
}

#[test]
fn split_shell_integration_command_fact_waits_for_the_terminator() {
    let mut engine = engine();

    let (_, facts) = engine.advance_with_shell_integration(b"\x1b]133;C\x07\x1b]133;D;");
    assert_eq!(facts, vec![ShellIntegrationFact::CommandStarted]);

    let (_, facts) = engine.advance_with_shell_integration(b"137\x07");
    assert_eq!(
        facts,
        vec![ShellIntegrationFact::CommandFinished {
            exit_code: Some(137),
        }]
    );
}

#[test]
fn pending_shell_facts_survive_a_state_round_trip() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 3 });
    let mut parser = vte::Parser::<{ crate::engine::OSC_CAPACITY }>::new_with_size();
    parser.advance(&mut state, b"\x1b]133;C\x07");

    let encoded = serde_json::to_string(&state).expect("state serializes");
    let mut restored: TerminalState = serde_json::from_str(&encoded).expect("state deserializes");

    assert_eq!(
        restored.take_shell_integration_facts(),
        vec![ShellIntegrationFact::CommandStarted]
    );
}

#[test]
fn prompt_marks_survive_scrollback_and_eviction() {
    let mut engine = TerminalEngine::with_scrollback(
        PtySize { cols: 4, rows: 2 },
        crate::scrollback::ScrollbackLimit::new(1, 1_000),
    );
    let _ = engine.advance(b"\x1b]133;A\x07\r\nx\r\n");

    assert!(engine.state().scrollback().lines()[0].1.prompt);

    let _ = engine.advance(b"y\r\n");

    assert_eq!(engine.state().scrollback().lines().len(), 1);
    assert!(!engine.state().scrollback().lines()[0].1.prompt);
}

#[test]
fn prompt_marks_follow_their_logical_row_through_reflow() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 5 });
    let _ = engine.advance(b"abcdefghij\x1b]133;A\x07kl");

    engine.resize(PtySize { cols: 4, rows: 5 });
    assert!(engine.state().active_grid().prompt_mark(2));
    assert!(!engine.state().active_grid().prompt_mark(1));

    engine.resize(PtySize { cols: 8, rows: 5 });
    assert!(engine.state().active_grid().prompt_mark(1));
    assert!(!engine.state().active_grid().prompt_mark(0));
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
fn a_graphics_image_byte_limit_drops_the_next_image() {
    let mut engine = engine();
    engine.queue_graphics_event(Ok(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            width: 16_384,
            height: 1_024,
            rgba: vec![0; MAX_IMAGE_BYTES],
        })
        .into(),
        animation: None,
        action: ImageAction::Transmit,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    }));

    let _ = engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let events = engine.take_graphics();

    assert_eq!(events.len(), 2);
    match &events[0] {
        Ok(record) => {
            assert_eq!(record.image.width, 16_384);
            assert_eq!(record.image.height, 1_024);
            assert_eq!(record.image.rgba.len(), MAX_IMAGE_BYTES);
        }
        Err(error) => panic!("the full image must stay queued, got {error:?}"),
    }
    assert_eq!(events[1], Err(GraphicsError::QueueFull { dropped: 1 }));
}

#[test]
fn queued_graphics_events_are_readable_without_draining() {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 4 });
    assert_eq!(engine.advance(&sixel_register_image(100, 0, 0)), b"");

    let expected = Err(GraphicsError::PlacementRejected {
        protocol: GraphicsProtocol::Sixel,
        reason: ImagePlacementError::MissingCellDimensions {
            width: None,
            height: None,
        },
    });
    assert_eq!(
        engine.graphics_events().cloned().collect::<Vec<_>>(),
        vec![expected.clone()]
    );
    assert_eq!(engine.graphics_events().count(), 1);
    assert_eq!(engine.take_graphics(), vec![expected]);
    assert_eq!(engine.graphics_events().count(), 0);
}

#[test]
fn dropped_graphics_errors_are_counted_separately_from_dropped_images() {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 4 });
    let image = sixel_register_image(100, 0, 0);

    for _ in 0..=MAX_GRAPHICS_EVENTS {
        assert_eq!(engine.advance(&image), b"");
    }

    assert_eq!(engine.graphics_events().count(), MAX_GRAPHICS_EVENTS);
    assert_eq!(engine.graphics_errors_dropped(), 1);
    let events = engine.take_graphics();
    assert_eq!(events.len(), MAX_GRAPHICS_EVENT_BATCH);
    assert_eq!(
        events.last(),
        Some(&Err(GraphicsError::QueueFull { dropped: 1 }))
    );
    assert_eq!(engine.graphics_errors_dropped(), 0);
}

#[test]
fn rejected_graphics_placements_do_not_consume_the_image_byte_budget() {
    let mut engine = engine();
    let rejected = || DecodedGraphics {
        query: false,
        protocol: GraphicsProtocol::Sixel,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![0; MAX_IMAGE_BYTES],
        },
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
    };

    engine.queue_graphics(
        Ok(crate::graphics::GraphicsOperation::Image(rejected())),
        (0, 0),
    );
    engine.queue_graphics(
        Ok(crate::graphics::GraphicsOperation::Image(rejected())),
        (0, 0),
    );

    assert_eq!(
        engine.take_graphics(),
        vec![
            Err(GraphicsError::PlacementRejected {
                protocol: GraphicsProtocol::Sixel,
                reason: ImagePlacementError::MissingCellDimensions {
                    width: None,
                    height: None,
                },
            }),
            Err(GraphicsError::PlacementRejected {
                protocol: GraphicsProtocol::Sixel,
                reason: ImagePlacementError::MissingCellDimensions {
                    width: None,
                    height: None,
                },
            }),
        ]
    );
}

fn sixel_register_image(red: u16, green: u16, blue: u16) -> Vec<u8> {
    format!("\x1bPq#1;2;{red};{green};{blue}#1@\x1b\\").into_bytes()
}

fn sixel_palette_only(red: u16, green: u16, blue: u16) -> Vec<u8> {
    format!("\x1bPq#1;2;{red};{green};{blue}\x1b\\").into_bytes()
}

fn sixel_engine() -> TerminalEngine {
    let mut engine = TerminalEngine::new(PtySize { cols: 4, rows: 4 });
    engine.set_cell_size(PixelCellSize::new(1, 6).expect("nonzero cell size"));
    engine
}

#[test]
fn indexed_sixel_operations_resolve_at_their_terminal_byte_offsets() {
    let mut engine = sixel_engine();
    let mut bytes = b"\x1b[?1070l".to_vec();
    bytes.extend(sixel_register_image(100, 0, 0));
    bytes.extend_from_slice(b"\x1b[?1070h");
    bytes.extend(sixel_register_image(0, 0, 100));

    assert_eq!(engine.advance(&bytes), b"");
    let placements = engine.state().image_placements();
    assert_eq!(placements.len(), 2);
    assert_eq!(&placements[0].record().image.rgba[..4], [255, 0, 0, 255]);
    assert_eq!(&placements[1].record().image.rgba[..4], [0, 0, 255, 255]);
}

#[test]
fn shared_sixel_palette_edits_repaint_existing_images_in_the_same_chunk() {
    let mut engine = sixel_engine();
    let mut bytes = b"\x1b[?1070l".to_vec();
    bytes.extend(sixel_register_image(100, 0, 0));
    bytes.extend(sixel_register_image(0, 100, 0));

    assert_eq!(engine.advance(&bytes), b"");
    let placements = engine.state().image_placements();
    assert_eq!(placements.len(), 2);
    assert_eq!(&placements[0].record().image.rgba[..4], [0, 255, 0, 255]);
    assert_eq!(&placements[1].record().image.rgba[..4], [0, 255, 0, 255]);
}

#[test]
fn a_shared_sixel_palette_only_operation_repaints_retained_images() {
    let mut engine = sixel_engine();
    let mut bytes = b"\x1b[?1070l".to_vec();
    bytes.extend(sixel_register_image(100, 0, 0));
    bytes.extend(sixel_palette_only(0, 100, 0));

    assert_eq!(engine.advance(&bytes), b"");
    assert_eq!(engine.state().image_placements().len(), 1);
    assert_eq!(
        &engine.state().image_placements()[0].record().image.rgba[..4],
        [0, 255, 0, 255]
    );
}

#[test]
fn c1_graphics_bytes_do_not_change_terminal_state() {
    let mut engine = engine();
    let before = engine.state().clone();
    let mut bytes = vec![0x9f];
    bytes.extend_from_slice(b"Gf=32,s=1,v=1;/wAA/w==");
    bytes.push(0x9c);

    assert_eq!(engine.advance(&bytes), b"");
    assert_eq!(engine.state(), &before);
    assert_eq!(
        engine.take_graphics(),
        vec![Ok(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: (DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![255, 0, 0, 255],
            })
            .into(),
            animation: None,
            action: ImageAction::Transmit,
            display: ImageDisplay::default(),
            anchor: (0, 0),
        })]
    );
}

#[test]
fn c1_osc_shell_marker_is_not_printed() {
    let mut engine = engine();
    let before = engine.state().clone();
    let mut bytes = vec![0x9d];
    bytes.extend_from_slice(b"133;C");
    bytes.push(0x07);

    let (_, facts) = engine.advance_with_shell_integration(&bytes);

    assert_eq!(facts, vec![ShellIntegrationFact::CommandStarted]);
    assert_eq!(engine.state().active_grid(), before.active_grid());
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
}

#[test]
fn c1_string_openers_inside_osc_remain_osc_data() {
    let mut engine = engine();

    let mut bytes = b"\x1b]2;before".to_vec();
    bytes.push(0x9f);
    bytes.extend_from_slice(b"after\x07");

    let _ = engine.advance(&bytes);

    assert_eq!(engine.state().title(), Some("before�after"));
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
}

fn red_png() -> Vec<u8> {
    use image::ImageEncoder;

    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel image encodes");
    bytes
}

fn graphics_control(protocol: GraphicsProtocol, c1: bool) -> Vec<u8> {
    match protocol {
        GraphicsProtocol::Kitty => {
            let body = b"Gf=32,s=1,v=1;/wAA/w==";
            if c1 {
                let mut bytes = vec![0x9f];
                bytes.extend_from_slice(body);
                bytes.push(0x9c);
                bytes
            } else {
                let mut bytes = b"\x1b_".to_vec();
                bytes.extend_from_slice(body);
                bytes.extend_from_slice(b"\x1b\\");
                bytes
            }
        }
        GraphicsProtocol::Sixel => {
            let body = b"q#1;2;100;0;0#1@";
            if c1 {
                let mut bytes = vec![0x90];
                bytes.extend_from_slice(body);
                bytes.push(0x9c);
                bytes
            } else {
                let mut bytes = b"\x1bP".to_vec();
                bytes.extend_from_slice(body);
                bytes.extend_from_slice(b"\x1b\\");
                bytes
            }
        }
        GraphicsProtocol::Iterm2 => {
            let encoded = STANDARD.encode(red_png());
            let body = format!(
                "1337;File=inline=1;size=67;width=1px;height=1px;preserveAspectRatio=0:{encoded}"
            );
            if c1 {
                let mut bytes = vec![0x9d];
                bytes.extend_from_slice(body.as_bytes());
                bytes.push(0x9c);
                bytes
            } else {
                let mut bytes = b"\x1b]".to_vec();
                bytes.extend_from_slice(body.as_bytes());
                bytes.push(0x07);
                bytes
            }
        }
    }
}

fn visible_graphics_control(protocol: GraphicsProtocol) -> Vec<u8> {
    match protocol {
        GraphicsProtocol::Kitty => b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
        GraphicsProtocol::Sixel => sixel_register_image(100, 0, 0),
        GraphicsProtocol::Iterm2 => graphics_control(GraphicsProtocol::Iterm2, false),
    }
}

#[test]
fn synchronized_graphics_commit_only_after_the_complete_end_sequence() {
    let now = Instant::now();
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        let mut engine = engine();
        engine.set_cell_size(PixelCellSize::new(1, 6).expect("nonzero cell size"));
        assert_eq!(engine.advance(b"old"), b"");
        let committed = engine.state().clone();
        let mut update = BEGIN_SYNCHRONIZED_OUTPUT.to_vec();
        update.extend_from_slice(b"\x1b[2J\x1b[HN");
        update.extend(visible_graphics_control(protocol));
        update.extend_from_slice(END_SYNCHRONIZED_OUTPUT);

        for (index, byte) in update.iter().enumerate() {
            let (_, _, _) = engine.advance_with_shell_integration_at(&[*byte], now);
            if index + 1 < update.len() {
                assert_eq!(engine.state(), &committed, "{protocol:?}, byte {index}");
                assert_eq!(engine.take_graphics(), [], "{protocol:?}, byte {index}");
            }
        }

        assert_eq!(ch(&engine, 0, 0), 'N', "{protocol:?}");
        assert_eq!(engine.state().image_placements().len(), 1, "{protocol:?}");
        let events = engine.take_graphics();
        assert_eq!(events.len(), 1, "{protocol:?}");
        assert_eq!(
            events[0].as_ref().map(|record| record.protocol),
            Ok(protocol)
        );
    }
}

#[test]
fn synchronized_control_lookalikes_inside_strings_do_not_release() {
    let now = Instant::now();
    let strings: [&[u8]; 7] = [
        b"\x1b_Gbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1bPqbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1b]1337;File=inline=1:broken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x07",
        b"\x1bPtmux;broken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1bP\x1bPbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\\x1b\\",
        b"\x1bXbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1b^broken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
    ];

    for string in strings {
        let mut engine = engine();
        let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
        let mut body = string.to_vec();
        body.push(b'X');
        let (replies, facts, advanced) = engine.advance_with_shell_integration_at(&body, now);

        assert_eq!((replies, facts, advanced), (vec![], vec![], false));
        assert_eq!(ch(&engine, 0, 0), ' ');
        assert!(engine.synchronized_output_transport(now).is_some());

        let (_, _, advanced) =
            engine.advance_with_shell_integration_at(END_SYNCHRONIZED_OUTPUT, now);
        assert!(advanced);
        assert!(engine.synchronized_output_transport(now).is_none());
    }
}

#[test]
fn c1_synchronized_controls_hold_and_release_at_every_byte_split() {
    const C1_BEGIN: &[u8] = b"\x9b?2026h";
    const C1_END: &[u8] = b"\x9b?2026l";
    let now = Instant::now();

    for begin_split in 0..=C1_BEGIN.len() {
        for end_split in 0..=C1_END.len() {
            let mut engine = engine();
            let _ = engine.advance(b"old");
            let committed = engine.state().clone();

            let _ = engine.advance_with_shell_integration_at(&C1_BEGIN[..begin_split], now);
            let _ = engine.advance_with_shell_integration_at(&C1_BEGIN[begin_split..], now);
            let _ = engine.advance_with_shell_integration_at(b"\x1b[2J\x1b[HN", now);
            assert_eq!(
                engine.state(),
                &committed,
                "begin={begin_split}, end={end_split}"
            );

            let (_, _, first_advanced) =
                engine.advance_with_shell_integration_at(&C1_END[..end_split], now);
            if end_split < C1_END.len() {
                assert_eq!(
                    engine.state(),
                    &committed,
                    "begin={begin_split}, end={end_split}"
                );
            }
            let (_, _, second_advanced) =
                engine.advance_with_shell_integration_at(&C1_END[end_split..], now);

            assert!(
                first_advanced || second_advanced,
                "begin={begin_split}, end={end_split}"
            );
            assert_eq!(
                ch(&engine, 0, 0),
                'N',
                "begin={begin_split}, end={end_split}"
            );
            assert!(engine.synchronized_output_transport(now).is_none());
        }
    }
}

#[test]
fn c1_synchronized_control_state_survives_process_swaps() {
    let now = Instant::now();
    let wall_now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000);
    let mut engine = engine();
    let _ = engine.advance(b"old");
    let _ = engine.advance(b"\x9b?20");
    let first_transport = engine
        .synchronized_output_transport_at(now, wall_now)
        .expect("the split C1 begin has transport state");
    let first_undecoded = engine.undecoded().to_vec();
    let first_graphics_undecoded = engine.graphics_undecoded().to_vec();
    let first_graphics_transport = engine.graphics_transport_state().unwrap_or_default();
    let first_events = engine.take_graphics();
    let first_state = engine.into_state();
    let mut restored =
        TerminalEngine::from_state_with_graphics_events_wrappers_and_synchronized_output(
            first_state,
            &first_undecoded,
            &first_graphics_undecoded,
            &first_events,
            first_graphics_transport,
            Some(first_transport),
            now,
            wall_now,
        );

    let _ = restored.advance(b"26h\x1b[2J\x1b[HN\x9b?20");
    let committed = restored.state().clone();
    assert_eq!(ch(&restored, 0, 0), 'o');
    let second_transport = restored
        .synchronized_output_transport_at(now, wall_now)
        .expect("the split C1 end has transport state");
    let second_undecoded = restored.undecoded().to_vec();
    let second_graphics_undecoded = restored.graphics_undecoded().to_vec();
    let second_graphics_transport = restored.graphics_transport_state().unwrap_or_default();
    let second_events = restored.take_graphics();
    let second_state = restored.into_state();
    let mut restored =
        TerminalEngine::from_state_with_graphics_events_wrappers_and_synchronized_output(
            second_state,
            &second_undecoded,
            &second_graphics_undecoded,
            &second_events,
            second_graphics_transport,
            Some(second_transport),
            now,
            wall_now,
        );

    assert_eq!(restored.state(), &committed);
    let (_, _, advanced) = restored.advance_with_shell_integration_at(b"26l", now);

    assert!(advanced);
    assert_eq!(ch(&restored, 0, 0), 'N');
    assert!(restored.synchronized_output_transport(now).is_none());
}

#[test]
fn c1_csi_bytes_inside_utf8_and_strings_are_not_synchronized_controls() {
    let now = Instant::now();
    let mut engine = engine();
    let mut bytes = vec![0xe2, 0x9b, 0xa0];
    bytes.extend_from_slice(b"\x1b]2;\x9b?2026h\x9b?2026l\x07");
    bytes.extend_from_slice(b"\x1bPq\x9b?2026h\x9b?2026l\x1b\\");
    bytes.extend_from_slice(b"\x1b_data\x9b?2026h\x9b?2026l\x1b\\");
    bytes.extend_from_slice(b"\x1bXdata\x9b?2026h\x9b?2026l\x1b\\");
    bytes.extend_from_slice(b"\x1b^data\x9b?2026h\x9b?2026l\x1b\\X");

    let (_, _, advanced) = engine.advance_with_shell_integration_at(&bytes, now);

    assert!(advanced);
    assert_eq!(engine.next_synchronized_output_delay(now), None);
    assert_eq!(ch(&engine, 0, 1), 'X');
    assert!(engine.synchronized_output_transport(now).is_none());
}

#[test]
fn end_then_begin_in_one_chunk_commits_one_group_and_keeps_the_next() {
    let now = Instant::now();
    let mut engine = engine();
    let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
    let mut bytes = b"A".to_vec();
    bytes.extend_from_slice(END_SYNCHRONIZED_OUTPUT);
    bytes.extend_from_slice(BEGIN_SYNCHRONIZED_OUTPUT);
    bytes.push(b'B');

    let (_, _, advanced) = engine.advance_with_shell_integration_at(&bytes, now);

    assert!(advanced);
    assert_eq!(ch(&engine, 0, 0), 'A');
    assert_eq!(ch(&engine, 0, 1), ' ');
    assert!(engine.synchronized_output_transport(now).is_some());

    let (_, _, advanced) = engine.advance_with_shell_integration_at(END_SYNCHRONIZED_OUTPUT, now);
    assert!(advanced);
    assert_eq!(ch(&engine, 0, 1), 'B');
}

#[test]
fn overdue_synchronized_bytes_are_released_before_new_input() {
    let now = Instant::now();
    let mut engine = engine();
    let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
    let (_, _, advanced) = engine.advance_with_shell_integration_at(b"A", now);
    assert!(!advanced);

    let (_, _, advanced) =
        engine.advance_with_shell_integration_at(b"B", now + SYNCHRONIZED_OUTPUT_TIMEOUT);

    assert!(advanced);
    assert_eq!(ch(&engine, 0, 0), 'A');
    assert_eq!(ch(&engine, 0, 1), 'B');
    assert!(engine
        .synchronized_output_transport(now + SYNCHRONIZED_OUTPUT_TIMEOUT)
        .is_none());
}

#[test]
fn synchronized_output_releases_when_the_byte_bound_is_reached() {
    let now = Instant::now();
    let mut engine = engine();
    let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);

    let (_, _, advanced) =
        engine.advance_with_shell_integration_at(&vec![b'x'; MAX_SYNCHRONIZED_OUTPUT_BYTES], now);

    assert!(advanced);
    assert!(engine.synchronized_output_transport(now).is_none());
    assert_eq!(ch(&engine, 2, 7), 'x');
}

#[test]
fn synchronized_output_deadline_releases_replies_shell_facts_and_graphics() {
    let now = Instant::now();
    let mut engine = engine();
    engine.set_cell_size(PixelCellSize::new(1, 6).expect("nonzero cell size"));
    let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
    let mut body = b"Z\x1b[5n\x1b]133;C\x07".to_vec();
    body.extend(visible_graphics_control(GraphicsProtocol::Kitty));
    let (_, _, advanced) = engine.advance_with_shell_integration_at(&body, now);
    assert!(!advanced);

    assert_eq!(
        engine.expire_synchronized_output(now + Duration::from_millis(149)),
        None
    );
    assert_eq!(ch(&engine, 0, 0), ' ');
    let (replies, facts) = engine
        .expire_synchronized_output(now + SYNCHRONIZED_OUTPUT_TIMEOUT)
        .expect("the deadline releases the update");

    assert_eq!(replies, b"\x1b[0n");
    assert_eq!(facts, [ShellIntegrationFact::CommandStarted]);
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().image_placements().len(), 1);
    assert_eq!(engine.take_graphics().len(), 1);
}

#[test]
fn finish_releases_synchronized_text_and_reports_an_inner_truncated_image() {
    let now = Instant::now();
    let mut engine = engine();
    let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
    let (_, _, advanced) =
        engine.advance_with_shell_integration_at(b"Q\x1b_Gf=32,s=1,v=1;AAAA", now);
    assert!(!advanced);

    assert_eq!(
        engine.finish(),
        [Err(GraphicsError::Truncated {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
    assert_eq!(ch(&engine, 0, 0), 'Q');
    assert!(engine.synchronized_output_transport(now).is_none());
}

#[test]
fn synchronized_output_deadline_keeps_elapsed_process_swap_time() {
    let now = Instant::now();
    let wall_now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = engine();
    let (_, _, _) = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
    let (_, _, _) = engine.advance_with_shell_integration_at(b"R", now);
    let transport = engine
        .synchronized_output_transport_at(now + Duration::from_millis(40), wall_now)
        .expect("the open update has transport state");
    assert_eq!(
        transport.deadline(),
        Some(wall_now + Duration::from_millis(110))
    );
    let undecoded = engine.undecoded().to_vec();
    let graphics_undecoded = engine.graphics_undecoded().to_vec();
    let graphics_transport = engine.graphics_transport_state().unwrap_or_default();
    let state = engine.into_state();
    let restored_at = now + Duration::from_secs(1);
    let mut restored =
        TerminalEngine::from_state_with_graphics_events_wrappers_and_synchronized_output(
            state,
            &undecoded,
            &graphics_undecoded,
            &[],
            graphics_transport,
            Some(transport),
            restored_at,
            wall_now + Duration::from_millis(100),
        );

    assert_eq!(
        restored.next_synchronized_output_delay(restored_at),
        Some(Duration::from_millis(10))
    );
    assert_eq!(
        restored.expire_synchronized_output(restored_at + Duration::from_millis(9)),
        None
    );
    assert!(restored
        .expire_synchronized_output(restored_at + Duration::from_millis(10))
        .is_some());
    assert_eq!(ch(&restored, 0, 0), 'R');
}

#[test]
fn synchronized_output_transport_rejects_impossible_scanner_state() {
    let now = Instant::now();
    let wall_now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = engine();
    let _ = engine.advance_with_shell_integration_at(BEGIN_SYNCHRONIZED_OUTPUT, now);
    let mut encoded = serde_json::to_value(
        engine
            .synchronized_output_transport_at(now, wall_now)
            .expect("the open update has transport state"),
    )
    .expect("the transport serializes");

    encoded["terminal_input"]["utf8_continuations"] = serde_json::json!(4);
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(encoded.clone())
            .unwrap_err()
            .to_string(),
        "terminal-input UTF-8 continuation count is invalid"
    );

    encoded["terminal_input"]["utf8_continuations"] = serde_json::json!(0);
    encoded["terminal_input"]["tail_len"] = serde_json::json!(0);
    encoded["terminal_input"]["tail_next"] = serde_json::json!(7);
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(encoded.clone())
            .unwrap_err()
            .to_string(),
        "terminal-input scanner tail is invalid"
    );

    encoded["terminal_input"]["tail_next"] = serde_json::json!(0);
    encoded["bytes"] = serde_json::json!([]);
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(encoded)
            .unwrap_err()
            .to_string(),
        "synchronized-output deadline has no bytes"
    );
}

#[test]
fn overlong_open_string_keeps_its_scanner_state_across_process_swap() {
    let now = Instant::now();
    let wall_now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
    let mut opening = b"\x1b]2;".to_vec();
    opening.extend(std::iter::repeat_n(b'A', MAX_UNDECODED + 1));
    let suffix = b"inside\x1b[?2026hdata\x1b[?2026l\x07";

    let mut uninterrupted = engine();
    let _ = uninterrupted.advance(&opening);
    assert!(uninterrupted.undecoded().is_empty());

    let mut carried = engine();
    let _ = carried.advance(&opening);
    assert!(carried.undecoded().is_empty());
    let synchronized_output = carried
        .synchronized_output_transport_at(now, wall_now)
        .expect("the open string has scanner transport state");
    assert_eq!(synchronized_output.deadline(), None);
    let graphics_undecoded = carried.graphics_undecoded().to_vec();
    let graphics_transport = carried.graphics_transport_state().unwrap_or_default();
    let state = carried.into_state();
    let mut restored =
        TerminalEngine::from_state_with_graphics_events_wrappers_and_synchronized_output(
            state,
            &[],
            &graphics_undecoded,
            &[],
            graphics_transport,
            Some(synchronized_output),
            now,
            wall_now,
        );

    let _ = uninterrupted.advance(suffix);
    let (_, _, advanced) = restored.advance_with_shell_integration_at(suffix, now);

    assert!(advanced);
    assert_eq!(restored.next_synchronized_output_delay(now), None);
    assert_eq!(restored.take_graphics(), uninterrupted.take_graphics());
    assert!(restored.synchronized_output_transport(now).is_none());
}

#[test]
fn c1_string_end_and_split_utf8_stay_exact_across_two_process_swaps() {
    let now = Instant::now();
    let wall_now = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000);
    let mut opening = b"\x1b_Gf=32,s=1,v=1;".to_vec();
    opening.extend(std::iter::repeat_n(b'A', MAX_UNDECODED + 1));

    let mut uninterrupted = engine();
    let _ = uninterrupted.advance(&opening);
    let mut carried = engine();
    let _ = carried.advance(&opening);
    let first_sync = carried
        .synchronized_output_transport_at(now, wall_now)
        .expect("the open APC has terminal-input transport state");
    let first_graphics_undecoded = carried.graphics_undecoded().to_vec();
    let first_graphics_transport = carried
        .graphics_transport_state()
        .expect("the open APC has graphics transport state");
    let first_state = carried.into_state();
    let mut restored =
        TerminalEngine::from_state_with_graphics_events_wrappers_and_synchronized_output(
            first_state,
            &[],
            &first_graphics_undecoded,
            &[],
            first_graphics_transport,
            Some(first_sync),
            now,
            wall_now,
        );

    let c1_string_end = [0x9c];
    let _ = uninterrupted.advance(&c1_string_end);
    let _ = restored.advance(&c1_string_end);
    let _ = uninterrupted.advance(BEGIN_SYNCHRONIZED_OUTPUT);
    let _ = restored.advance(BEGIN_SYNCHRONIZED_OUTPUT);
    let split_utf8 = [0xe2, 0x94];
    let _ = uninterrupted.advance(&split_utf8);
    let _ = restored.advance(&split_utf8);

    let second_sync = restored
        .synchronized_output_transport_at(now, wall_now)
        .expect("the held UTF-8 prefix has transport state");
    let second_undecoded = restored.undecoded().to_vec();
    let second_graphics_undecoded = restored.graphics_undecoded().to_vec();
    let second_graphics_transport = restored.graphics_transport_state().unwrap_or_default();
    let second_events = restored.take_graphics();
    let second_state = restored.into_state();
    let mut restored =
        TerminalEngine::from_state_with_graphics_events_wrappers_and_synchronized_output(
            second_state,
            &second_undecoded,
            &second_graphics_undecoded,
            &second_events,
            second_graphics_transport,
            Some(second_sync),
            now,
            wall_now,
        );

    let final_utf8 = [0x90];
    let _ = uninterrupted.advance(&final_utf8);
    let _ = restored.advance(&final_utf8);
    let _ = uninterrupted.advance(END_SYNCHRONIZED_OUTPUT);
    let _ = restored.advance(END_SYNCHRONIZED_OUTPUT);

    assert_eq!(ch(&restored, 0, 0), '┐');
    assert_eq!(restored.next_synchronized_output_delay(now), None);
    assert_eq!(restored.take_graphics(), uninterrupted.take_graphics());
    assert_eq!(restored.state(), uninterrupted.state());
}

#[test]
fn malformed_synchronized_modes_remain_ordinary_terminal_input() {
    for bytes in [
        &b"\x1b[?2026;1hX"[..],
        &b"\x1b[?1;2026hX"[..],
        &b"\x1b[?2026:1hX"[..],
        &b"\x1b[?02026hX"[..],
    ] {
        let now = Instant::now();
        let mut engine = engine();
        let (_, _, advanced) = engine.advance_with_shell_integration_at(bytes, now);

        assert!(advanced);
        assert_eq!(ch(&engine, 0, 0), 'X');
        assert!(engine.synchronized_output_transport(now).is_none());
    }
}

fn assert_graphics_event_after_utf8_prefix(prefix: &[u8], protocol: GraphicsProtocol, c1: bool) {
    let mut engine = engine();
    engine.set_cell_size(PixelCellSize::new(1, 1).expect("nonzero cell size"));
    let mut bytes = prefix.to_vec();
    bytes.extend(graphics_control(protocol, c1));

    let _ = engine.advance(&bytes);
    let events = engine.take_graphics();
    assert_eq!(events.len(), 1, "{protocol:?}, C1={c1}");
    let record = events[0]
        .as_ref()
        .unwrap_or_else(|error| panic!("{protocol:?}, C1={c1} was rejected: {error:?}"));
    assert_eq!(record.protocol, protocol);
}

#[test]
fn every_utf8_continuation_before_each_graphics_protocol_is_text() {
    for continuation in 0x80..=0x9f {
        let prefix = [0xe0, 0xa0, continuation];
        for protocol in [
            GraphicsProtocol::Kitty,
            GraphicsProtocol::Sixel,
            GraphicsProtocol::Iterm2,
        ] {
            for c1 in [false, true] {
                assert_graphics_event_after_utf8_prefix(&prefix, protocol, c1);
            }
        }
    }
}

#[test]
fn a_split_utf8_code_point_before_each_graphics_protocol_stays_text() {
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        for c1 in [false, true] {
            let mut engine = engine();
            engine.set_cell_size(PixelCellSize::new(1, 1).expect("nonzero cell size"));
            assert_eq!(engine.advance(b"\xe0\xa0"), b"");

            let mut rest = vec![0x90];
            rest.extend(graphics_control(protocol, c1));
            let _ = engine.advance(&rest);
            let events = engine.take_graphics();
            assert_eq!(events.len(), 1, "{protocol:?}, C1={c1}");
            assert_eq!(events[0].as_ref().unwrap().protocol, protocol);
        }
    }
}

#[test]
fn a_split_utf8_prefix_survives_an_engine_replacement_before_each_protocol() {
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        for c1 in [false, true] {
            let mut engine = engine();
            engine.set_cell_size(PixelCellSize::new(1, 1).expect("nonzero cell size"));
            assert_eq!(engine.advance(b"\xe2\x94"), b"");
            assert_eq!(engine.undecoded(), b"\xe2\x94");
            assert_eq!(engine.graphics_undecoded(), b"\xe2\x94");

            let state = engine.into_state();
            let mut rebuilt =
                TerminalEngine::from_state_with_graphics(state, b"\xe2\x94", b"\xe2\x94");
            assert_eq!(rebuilt.advance(&[0x90]), b"");
            assert_eq!(ch(&rebuilt, 0, 0), '┐');
            assert_eq!(rebuilt.state().active_cursor_position(), (0, 1));

            let _ = rebuilt.advance(&graphics_control(protocol, c1));
            let events = rebuilt.take_graphics();
            assert_eq!(events.len(), 1, "{protocol:?}, C1={c1}");
            assert_eq!(events[0].as_ref().unwrap().protocol, protocol);
        }
    }
}

#[test]
fn an_invalid_utf8_continuation_resets_before_a_standalone_c1_graphics_string() {
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        let mut engine = engine();
        engine.set_cell_size(PixelCellSize::new(1, 1).expect("nonzero cell size"));
        assert_eq!(engine.advance(&[0xe2, b'A']), b"");

        assert_eq!(ch(&engine, 0, 0), '\u{fffd}');
        assert_eq!(ch(&engine, 0, 1), 'A');
        assert_eq!(engine.state().active_cursor_position(), (0, 2));

        let _ = engine.advance(&graphics_control(protocol, true));
        let events = engine.take_graphics();
        assert_eq!(events.len(), 1, "{protocol:?}");
        assert_eq!(events[0].as_ref().unwrap().protocol, protocol);
    }
}

#[test]
fn utf8_c1_st_inside_osc_remains_osc_data() {
    let mut engine = engine();

    let mut bytes = b"\x1b]2;before\xc2".to_vec();
    bytes.extend_from_slice(b"\x9cafter\x07");

    let _ = engine.advance(&bytes);

    assert_eq!(engine.state().title(), Some("beforeafter"));
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
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

/// A clipboard write (OSC 52) can span many reads. The carry holds the payload
/// up to `MAX_UNDECODED` and drops it past that, so the pane's memory does not
/// grow with the payload.
#[test]
fn a_large_clipboard_write_keeps_bounded_carry_and_terminal_state() {
    const PAYLOAD: usize = 8 * 1024 * 1024;
    let opening = b"\x1b]52;c;";
    let payload = vec![b'A'; PAYLOAD];

    let mut engine = engine();

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

#[test]
#[ignore = "release performance benchmark"]
fn benchmark_chunked_clipboard_write() {
    const PAYLOAD: usize = 8 * 1024 * 1024;
    const RUNS: usize = 6;

    let payload = vec![b'A'; PAYLOAD];
    let mut totals = Vec::with_capacity(RUNS - 1);

    for run in 0..RUNS {
        let mut engine = engine();
        let started = Instant::now();
        let _ = engine.advance(b"\x1b]52;c;");
        for chunk in payload.chunks(READ_CHUNK) {
            let _ = engine.advance(chunk);
        }
        let _ = engine.advance(b"\x07Z");
        if run != 0 {
            totals.push(started.elapsed());
        }
        assert_eq!(ch(&engine, 0, 0), 'Z');
        assert_eq!(engine.state().active_cursor_position(), (0, 1));
    }

    totals.sort_unstable();
    println!(
        "8 MiB clipboard write: median total {:?}",
        totals[totals.len() / 2]
    );
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
    for (round, chunk) in digits.chunks(READ_CHUNK).enumerate() {
        let _ = engine.advance(chunk);
        let held = 2 + (round + 1) * READ_CHUNK;
        let expected = if held <= MAX_UNDECODED { held } else { 0 };
        assert_eq!(engine.undecoded().len(), expected, "after chunk {round}");
    }

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), ' ');

    // `m` finishes it as an SGR koshi ignores; the `Z` after it prints.
    let _ = engine.advance(b"mZ");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

/// A sequence that passed the limit in the engine that was swapped out leaves
/// the next parser on a sequence boundary: the rest of the body prints as
/// text.
#[test]
fn a_swap_inside_a_sequence_past_the_limit_prints_the_rest_of_the_body() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]52;c;");
    let _ = engine.advance(&vec![b'A'; MAX_UNDECODED + 1]);
    assert_eq!(engine.undecoded(), b"");

    let mut next = swapped(engine);
    let _ = next.advance(b"BC\x07Z");

    assert_eq!(next.undecoded(), b"");
    assert_eq!(ch(&next, 0, 0), 'B');
    assert_eq!(ch(&next, 0, 1), 'C');
    assert_eq!(ch(&next, 0, 2), 'Z');
    assert_eq!(next.state().active_cursor_position(), (0, 3));
}

/// An empty chunk decodes nothing and leaves what the parser held in place:
/// the first byte of a code point, or an open control sequence.
#[test]
fn an_empty_chunk_keeps_the_held_bytes() {
    let mut engine = engine();

    assert_eq!(engine.advance(b""), b"");
    assert_eq!(engine.undecoded(), b"");

    let _ = engine.advance(b"a\xc3");
    assert_eq!(engine.advance(b""), b"");
    assert_eq!(engine.undecoded(), b"\xc3");
    assert_eq!(ch(&engine, 0, 0), 'a');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));

    let _ = engine.advance(b"\xa9\x1b[3");
    assert_eq!(engine.advance(b""), b"");
    assert_eq!(engine.undecoded(), b"\x1b[3");
    assert_eq!(ch(&engine, 0, 1), 'é');
    assert_eq!(engine.state().active_cursor_position(), (0, 2));
}

/// A four-byte code point spread over three chunks is carried whole at each
/// cut and prints once, as one wide glyph, when its last byte arrives.
#[test]
fn a_four_byte_code_point_split_over_three_chunks_is_carried_whole() {
    let mut engine = engine();

    // U+1F600 is 0xF0 0x9F 0x98 0x80.
    let _ = engine.advance(b"\xf0\x9f");
    assert_eq!(engine.undecoded(), b"\xf0\x9f");

    let _ = engine.advance(b"\x98");
    assert_eq!(engine.undecoded(), b"\xf0\x9f\x98");
    assert_eq!(ch(&engine, 0, 0), ' ');

    let mut next = swapped(engine);
    let _ = next.advance(b"\x80");

    assert_eq!(next.undecoded(), b"");
    assert_eq!(ch(&next, 0, 0), '\u{1f600}');
    assert_eq!(next.state().active_cursor_position(), (0, 2));
}

/// An escape byte that cuts a code point short prints one replacement
/// character for the cut bytes, and the sequence it opens is held.
#[test]
fn a_code_point_cut_short_by_an_escape_prints_a_replacement_and_holds_the_sequence() {
    let mut engine = engine();

    let _ = engine.advance(b"\xc3");
    assert_eq!(engine.undecoded(), b"\xc3");

    let _ = engine.advance(b"\x1b[31");
    assert_eq!(engine.undecoded(), b"\x1b[31");
    assert_eq!(ch(&engine, 0, 0), '\u{fffd}');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));

    let _ = engine.advance(b"mZ");
    assert_eq!(engine.undecoded(), b"");

    let cell = engine
        .state()
        .active_grid()
        .cell(0, 1)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    assert_eq!(cell.ch(), 'Z');
    assert_eq!(cell.style(), red);
}

/// `SUB` (`0x1a`) abandons the sequence it lands in, the same as `CAN`: the
/// parser is back on a sequence boundary and nothing is carried.
#[test]
fn a_substituted_sequence_is_not_carried() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b[31\x1a");
    assert_eq!(engine.undecoded(), b"");

    let _ = engine.advance(b"Z");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

/// A second escape byte restarts the sequence: one escape is held, and the
/// bytes after it form the sequence.
#[test]
fn a_repeated_escape_byte_holds_one_escape() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b");
    let _ = engine.advance(b"\x1b");
    assert_eq!(engine.undecoded(), b"\x1b");

    let _ = engine.advance(b"[31mZ");
    assert_eq!(engine.undecoded(), b"");

    let cell = engine
        .state()
        .active_grid()
        .cell(0, 0)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    assert_eq!(cell.ch(), 'Z');
    assert_eq!(cell.style(), red);
}

/// An operating system command closed by `ESC \` ends on a sequence boundary:
/// the terminator's escape byte restarts the scan and its `\` finishes it.
#[test]
fn an_operating_system_command_closed_by_the_string_terminator_is_not_carried() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]0;hi");
    assert_eq!(engine.undecoded(), b"\x1b]0;hi");

    let _ = engine.advance(b"\x1b\\Z");

    assert_eq!(engine.undecoded(), b"");
    assert_eq!(engine.state().title(), Some("hi"));
    assert_eq!(ch(&engine, 0, 0), 'Z');
    assert_eq!(engine.state().active_cursor_position(), (0, 1));
}

/// A chunk that ends on the escape byte of `ESC \` has already dispatched the
/// operating system command; only that escape byte is carried, and the `\`
/// after the swap closes it without printing.
#[test]
fn an_operating_system_command_cut_at_its_terminator_carries_only_the_escape() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b]0;hi\x1b");
    assert_eq!(engine.state().title(), Some("hi"));
    assert_eq!(engine.undecoded(), b"\x1b");

    let mut next = swapped(engine);
    let _ = next.advance(b"\\Z");

    assert_eq!(next.undecoded(), b"");
    assert_eq!(next.state().title(), Some("hi"));
    assert_eq!(ch(&next, 0, 0), 'Z');
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// A device control string cut before its final byte carries `ESC P`; once
/// the final byte arrives, the carry is the three opening bytes and no more.
#[test]
fn a_device_control_string_cut_before_its_final_byte_carries_its_opening() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1bP");
    assert_eq!(engine.undecoded(), b"\x1bP");

    let _ = engine.advance(b"q#0~~");
    assert_eq!(engine.undecoded(), b"\x1bPq");

    let mut next = swapped(engine);
    let _ = next.advance(b"@@\x1b\\Z");

    assert_eq!(next.undecoded(), b"");
    assert_eq!(ch(&next, 0, 0), 'Z');
    assert_eq!(next.state().active_cursor_position(), (0, 1));
}

/// A resize leaves the held bytes in place.
#[test]
fn a_resize_keeps_the_held_bytes() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b[3");
    engine.resize(PtySize { cols: 4, rows: 2 });

    assert_eq!(engine.undecoded(), b"\x1b[3");
}

/// A control sequence the parser ignores dispatches nothing: the scan holds
/// it, and every control byte after it, until the next escape byte or printed
/// character. Replaying it leaves the parser on a sequence boundary.
#[test]
fn an_ignored_control_sequence_is_held_until_the_next_escape_or_print() {
    let mut engine = engine();

    let _ = engine.advance(b"\x1b[3?m");
    assert_eq!(engine.undecoded(), b"\x1b[3?m");

    let _ = engine.advance(b"\n");
    assert_eq!(engine.undecoded(), b"\x1b[3?m\n");
    assert_eq!(engine.state().active_cursor_position(), (1, 0));

    let mut next = swapped(engine);
    assert_eq!(next.undecoded(), b"\x1b[3?m\n");
    assert_eq!(next.state().active_cursor_position(), (1, 0));

    let _ = next.advance(b"Z");
    assert_eq!(next.undecoded(), b"");
    assert_eq!(ch(&next, 1, 0), 'Z');
    assert_eq!(next.state().active_cursor_position(), (1, 1));

    let _ = next.advance(b"\x1b[3?m\x1b[3");
    assert_eq!(next.undecoded(), b"\x1b[3");
}

/// The engine holds its OSC buffer inline, so its own size bounds how much one
/// unterminated sequence can accumulate.
#[test]
fn the_engine_carries_a_bounded_osc_buffer() {
    // One engine exists per pane, so its size is a per-pane cost. The bound is
    // an absolute figure: expressing it against `OSC_CAPACITY` would rise with
    // the capacity it is meant to bound.
    const PER_PANE_LIMIT: usize = 64 * 1024;
    let size = std::mem::size_of::<TerminalEngine>();
    assert!(
        size < PER_PANE_LIMIT,
        "TerminalEngine is {size} bytes, over the {PER_PANE_LIMIT} byte per-pane limit"
    );
}

#[test]
fn an_unterminated_osc_leaves_the_parser_usable() {
    let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
    let _ = engine.advance(b"\x1b]0;");
    let chunk = vec![b'A'; 1 << 20];
    for _ in 0..64 {
        let _ = engine.advance(&chunk);
    }
    // The sequence is still open, so no title has been set.
    assert_eq!(engine.state().title(), None);

    // Terminating it yields a title cut to the reported-text limit, and the
    // parser takes the next sequence normally.
    let _ = engine.advance(b"\x07");
    assert_eq!(
        engine.state().title().map(str::len),
        Some(koshi_core::text::MAX_REPORTED_TEXT_BYTES)
    );
    let _ = engine.advance(b"\x1b]2;ok\x07");
    assert_eq!(engine.state().title(), Some("ok"));
}

#[test]
fn a_title_split_across_chunks_is_still_bounded() {
    // vte holds the open sequence between calls, so the cap must apply to the
    // assembled payload rather than to one chunk.
    let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
    let _ = engine.advance(b"\x1b]2;");
    for _ in 0..100 {
        let _ = engine.advance(&[b'A'; 100]);
    }
    let _ = engine.advance(b"\x07");
    assert_eq!(
        engine.state().title().map(str::len),
        Some(koshi_core::text::MAX_REPORTED_TEXT_BYTES)
    );
}

#[test]
fn a_refused_character_split_across_chunks_is_still_removed() {
    // A multi-byte character delivered one byte at a time must be filtered as
    // the character it forms, not passed through as bytes.
    let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
    let _ = engine.advance(b"\x1b]2;a");
    for byte in "\u{202e}".as_bytes() {
        let _ = engine.advance(&[*byte]);
    }
    let _ = engine.advance(b"b\x07");
    assert_eq!(engine.state().title(), Some("ab"));
}

#[test]
fn an_osc_7_uri_split_across_chunks_is_still_refused_past_the_limit() {
    let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
    let _ = engine.advance(b"\x1b]7;file://localhost/tmp\x07");
    let _ = engine.advance(b"\x1b]7;file://localhost/");
    for _ in 0..100 {
        let _ = engine.advance(&[b'a'; 100]);
    }
    let _ = engine.advance(b"\x07");
    assert_eq!(
        engine
            .state()
            .current_cwd()
            .map(|cwd| cwd.path().to_path_buf()),
        Some(std::path::PathBuf::from("/tmp")),
        "an over-long URI replaced the working directory"
    );
}

#[test]
fn a_title_survives_a_reset_and_can_be_set_again() {
    let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
    let _ = engine.advance(b"\x1b]2;first\x07");
    assert_eq!(engine.state().title(), Some("first"));
    let _ = engine.advance(b"\x1bc");
    assert_eq!(engine.state().title(), None);
    let _ = engine.advance("\x1b]2;sec\u{7f}ond\x07".as_bytes());
    assert_eq!(engine.state().title(), Some("second"));
}

#[test]
fn a_title_past_the_parser_capacity_is_identical_to_one_within_it() {
    // The parser stops taking bytes at `OSC_CAPACITY`, so a longer sequence
    // reaches `osc_dispatch` short. For a title that changes nothing: the cut
    // to `MAX_REPORTED_TEXT_BYTES` happens well below the capacity, so both
    // lengths yield the same bytes.
    let within = {
        let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
        let mut seq = Vec::from(&b"\x1b]2;"[..]);
        seq.extend(std::iter::repeat_n(b'A', 4_000));
        seq.push(0x07);
        let _ = engine.advance(&seq);
        engine.state().title().map(str::to_owned)
    };
    let past = {
        let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
        let mut seq = Vec::from(&b"\x1b]2;"[..]);
        seq.extend(std::iter::repeat_n(b'A', 200_000));
        seq.push(0x07);
        let _ = engine.advance(&seq);
        engine.state().title().map(str::to_owned)
    };
    assert_eq!(within, past);
    assert_eq!(
        within.map(|t| t.len()),
        Some(koshi_core::text::MAX_REPORTED_TEXT_BYTES)
    );
}

#[test]
fn a_sequence_past_the_parser_capacity_does_not_disturb_the_next_one() {
    // Bytes are dropped from the oversized sequence alone. The parser still
    // terminates it and reads what follows normally.
    let mut engine = TerminalEngine::new(PtySize { rows: 24, cols: 80 });
    let mut seq = Vec::from(&b"\x1b]2;"[..]);
    seq.extend(std::iter::repeat_n(b'A', 200_000));
    seq.push(0x07);
    let _ = engine.advance(&seq);

    let _ = engine.advance(b"\x1b]7;file://localhost/tmp\x07");
    assert_eq!(
        engine
            .state()
            .current_cwd()
            .map(|cwd| cwd.path().to_path_buf()),
        Some(std::path::PathBuf::from("/tmp"))
    );
    let _ = engine.advance(b"\x1b]2;after\x07");
    assert_eq!(engine.state().title(), Some("after"));
    // A printable glyph still lands on the grid.
    let _ = engine.advance(b"z");
    assert_eq!(ch(&engine, 0, 0), 'z');
}

#[test]
fn a_repeated_transfer_with_the_same_pixels_shares_one_image() {
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        let mut engine = sixel_engine();
        let first = match protocol {
            GraphicsProtocol::Kitty => {
                b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec()
            }
            _ => visible_graphics_control(protocol),
        };
        let second = match protocol {
            GraphicsProtocol::Kitty => {
                b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec()
            }
            _ => visible_graphics_control(protocol),
        };
        let _ = engine.advance(&first);
        let _ = engine.advance(b"\r\n");
        let _ = engine.advance(&second);
        let events: Vec<_> = engine
            .take_graphics()
            .into_iter()
            .map(|event| event.expect("both transfers succeed"))
            .collect();

        assert_eq!(events.len(), 2, "{protocol:?}");
        assert!(
            std::sync::Arc::ptr_eq(&events[0].image, &events[1].image),
            "{protocol:?} transfers with equal pixels share one image"
        );
        let placements = engine.state().image_placements();
        assert_eq!(placements.len(), 2, "{protocol:?}");
        assert!(
            std::sync::Arc::ptr_eq(&placements[0].record().image, &placements[1].record().image),
            "{protocol:?} placements share one image"
        );
    }
}

#[test]
fn a_repeated_transfer_with_different_pixels_keeps_its_own_image() {
    let mut engine = sixel_engine();
    let _ = engine.advance(b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let _ = engine.advance(b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;AP8A/w==\x1b\\");
    let events: Vec<_> = engine
        .take_graphics()
        .into_iter()
        .map(|event| event.expect("both transfers succeed"))
        .collect();

    assert_eq!(events.len(), 2);
    assert!(!std::sync::Arc::ptr_eq(&events[0].image, &events[1].image));
    assert_eq!(events[0].image.rgba, vec![255, 0, 0, 255]);
    assert_eq!(events[1].image.rgba, vec![0, 255, 0, 255]);
}

#[test]
fn a_transfer_with_the_same_bytes_but_swapped_dimensions_keeps_its_own_image() {
    let mut engine = sixel_engine();
    let _ = engine.advance(b"\x1b_Ga=T,i=1,f=32,s=2,v=1,c=1,r=1,C=1;/wAA/wD/AP8=\x1b\\");
    let _ = engine.advance(b"\x1b_Ga=T,i=2,f=32,s=1,v=2,c=1,r=1,C=1;/wAA/wD/AP8=\x1b\\");
    let events: Vec<_> = engine
        .take_graphics()
        .into_iter()
        .map(|event| event.expect("both transfers succeed"))
        .collect();

    assert_eq!(events.len(), 2);
    assert_eq!((events[0].image.width, events[0].image.height), (2, 1));
    assert_eq!((events[1].image.width, events[1].image.height), (1, 2));
    assert_eq!(events[0].image.rgba, events[1].image.rgba);
    assert!(!std::sync::Arc::ptr_eq(&events[0].image, &events[1].image));
}

#[test]
fn a_kitty_upload_without_a_placement_shares_its_pixels_with_a_later_transfer() {
    let mut engine = sixel_engine();
    let _ = engine.advance(b"\x1b_Ga=t,i=1,f=32,s=1,v=1;/wAA/w==\x1b\\");
    let _ = engine.advance(b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let events: Vec<_> = engine
        .take_graphics()
        .into_iter()
        .map(|event| event.expect("both transfers succeed"))
        .collect();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].action, ImageAction::Transmit);
    assert!(std::sync::Arc::ptr_eq(&events[0].image, &events[1].image));
}

#[test]
fn an_image_scrolled_into_history_shares_its_pixels_with_a_later_transfer() {
    let mut engine = sixel_engine();
    let _ = engine.advance(b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let _ = engine.advance(b"\r\n\r\n\r\n\r\n\r\n\r\n");
    assert_eq!(
        engine.state().image_placements().len(),
        0,
        "the first image left the visible screen"
    );
    let _ = engine.advance(b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let events: Vec<_> = engine
        .take_graphics()
        .into_iter()
        .map(|event| event.expect("both transfers succeed"))
        .collect();

    assert_eq!(events.len(), 2);
    assert!(std::sync::Arc::ptr_eq(&events[0].image, &events[1].image));
}
