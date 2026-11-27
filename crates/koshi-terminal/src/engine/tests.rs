//! Tests for the per-pane terminal engine: construction, chunked byte
//! decoding across `advance` calls, device replies and shell-integration
//! facts, eight-bit controls, image events and their queue limits,
//! synchronized output, resize delegation, taking the state apart and
//! rebuilding it, carrying a half-received escape sequence to the next
//! parser, clipboard copies, printed runs that leave the same state as
//! single-character prints, and the start of the `plain` output workload.

use std::path::Path;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use koshi_core::geometry::PixelCellSize;
use koshi_core::process::PtySize;
use koshi_core::program_status::{BlockedKind, ProgramStatusState};
use koshi_test_support::output_workloads::{
    build_workload_bytes, OutputWorkload, DEFAULT_WORKLOAD_SEED,
};

use crate::graphics::{DecodedGraphics, DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay};
use crate::grid::state::{Cell, RowEnd};
use crate::state::{ImagePlacementError, ProgramFact, ProgramStatusFact, Screen, TerminalState};
use crate::style::{Color, Style};

use super::*;
use crate::graphics::tests::build_red_png_bytes;
use crate::state::tests::process_terminal_bytes;

impl SynchronizedOutputTransport {
    /// The wall-clock deadline for releasing the open update.
    fn get_deadline(&self) -> Option<SystemTime> {
        self.deadline
    }
}

/// The bytes the PTY reader hands the runtime in one go, so a scale test feeds
/// the engine the same chunk size a running pane does.
const READ_CHUNK_BYTE_COUNT: usize = 8192;

fn build_test_terminal_engine() -> TerminalEngine {
    TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 3,
    })
}

#[test]
fn terminal_inert_compaction_keeps_all_other_bytes_and_exact_offsets() {
    let terminal_input_bytes = b"abCDEFghIJkl";
    let inert_byte_ranges = [2..6, 8..10];

    assert_eq!(
        remove_terminal_inert_bytes(terminal_input_bytes, &inert_byte_ranges).as_ref(),
        b"abghkl"
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(0, &inert_byte_ranges),
        0
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(2, &inert_byte_ranges),
        2
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(4, &inert_byte_ranges),
        2
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(6, &inert_byte_ranges),
        2
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(8, &inert_byte_ranges),
        4
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(10, &inert_byte_ranges),
        4
    );
    assert_eq!(
        compute_terminal_byte_offset_without_inert_ranges(12, &inert_byte_ranges),
        6
    );
}

/// The character at (`row_index`, `column_index`) on the engine's active grid.
fn get_terminal_cell_character(
    terminal_engine: &TerminalEngine,
    row_index: u16,
    column_index: u16,
) -> char {
    terminal_engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(row_index, column_index)
        .expect("cell in bounds")
        .get_character()
}

#[test]
fn new_engine_is_blank_at_the_given_size() {
    let engine = build_test_terminal_engine();

    assert_eq!(
        engine
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (3, 8)
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
}

#[test]
fn process_pty_output_prints_text_into_the_grid_and_returns_no_replies() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"hi"), b"");

    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'h');
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'i');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 2)
    );
}

#[test]
fn split_osc133_is_completed_by_the_next_chunk() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"\x1b]133;"), b"");
    assert!(!engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(0));
    assert_eq!(engine.process_pty_output(b"A\x07"), b"");

    assert!(engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(0));
}

#[test]
fn process_pty_output_at_returns_command_facts_with_their_exit_code_in_order() {
    let mut engine = build_test_terminal_engine();
    let read_instant = Instant::now();

    assert_eq!(
        engine.process_pty_output_at(b"\x1b]133;C\x07\x1b]133;D;137\x07", read_instant),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: build_timed_program_facts(
                vec![
                    ProgramFact::CommandStarted,
                    ProgramFact::CommandFinished {
                        exit_code: Some(137)
                    },
                ],
                read_instant
            ),
        })
    );
}

#[test]
fn bare_shell_integration_finish_returns_a_finish_with_no_exit_code() {
    let mut engine = build_test_terminal_engine();
    let read_instant = Instant::now();

    assert_eq!(
        engine.process_pty_output_at(b"\x1b]133;C\x07\x1b]133;D\x07", read_instant),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: build_timed_program_facts(
                vec![
                    ProgramFact::CommandStarted,
                    ProgramFact::CommandFinished { exit_code: None },
                ],
                read_instant
            ),
        })
    );
}

#[test]
fn unmatched_shell_integration_finish_returns_no_fact() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        engine.process_pty_output_at(b"\x1b]133;D;1\x07", Instant::now()),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: Vec::new(),
        })
    );
}

#[test]
fn process_pty_output_at_returns_replies_and_facts_from_one_chunk() {
    let mut engine = build_test_terminal_engine();
    let read_instant = Instant::now();

    assert_eq!(
        engine.process_pty_output_at(b"\x1b[5n\x1b]133;C\x07", read_instant),
        Some(TerminalAdvance {
            device_query_replies: b"\x1b[0n".to_vec(),
            program_facts: build_timed_program_facts(
                vec![ProgramFact::CommandStarted],
                read_instant
            ),
        })
    );
}

#[test]
fn process_pty_output_drops_the_program_facts_it_reads() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"\x1b]133;C\x07"), b"");

    assert_eq!(
        engine.process_pty_output_at(b"x", Instant::now()),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: Vec::new(),
        })
    );
}

#[test]
fn an_empty_chunk_reaches_no_parser_and_returns_none() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output_at(b"", Instant::now()), None);
}

#[test]
fn split_shell_integration_command_fact_waits_for_the_terminator() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b]133;C\x07\x1b]133;D;"),
        vec![ProgramFact::CommandStarted]
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"137\x07"),
        vec![ProgramFact::CommandFinished {
            exit_code: Some(137)
        }]
    );
}

#[test]
fn a_state_read_back_holds_no_program_fact_and_no_device_reply() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;C\x07\x1b[5n");

    let serialized_terminal_state =
        serde_json::to_value(&terminal_state).expect("state serializes");
    let mut restored_terminal_state: TerminalState =
        serde_json::from_value(serialized_terminal_state.clone()).expect("state deserializes");

    assert_eq!(serialized_terminal_state.get("program_facts"), None);
    assert_eq!(
        serialized_terminal_state.get("shell_integration_facts"),
        None
    );
    assert_eq!(serialized_terminal_state.get("device_query_replies"), None);
    assert_eq!(restored_terminal_state.take_program_facts(), Vec::new());
    assert_eq!(
        restored_terminal_state.take_device_query_replies(),
        Vec::<u8>::new()
    );
}

#[test]
fn prompt_marks_survive_scrollback_and_eviction() {
    let mut engine = TerminalEngine::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        ScrollbackLimit::from_line_and_byte_limits(1, 1_000),
    );
    let _ = engine.process_pty_output(b"\x1b]133;A\x07\r\nx\r\n");

    assert!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()[0]
            .get_row_metadata()
            .has_prompt_mark
    );

    let _ = engine.process_pty_output(b"y\r\n");

    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()
            .len(),
        1
    );
    assert!(
        !engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()[0]
            .get_row_metadata()
            .has_prompt_mark
    );
}

#[test]
fn prompt_marks_follow_their_logical_row_through_reflow() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 5,
    });
    let _ = engine.process_pty_output(b"abcdefghij\x1b]133;A\x07kl");

    engine.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 5,
    });
    assert!(engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(2));
    assert!(!engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(1));

    engine.resize_terminal_state(PtySize {
        column_count: 8,
        row_count: 5,
    });
    assert!(engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(1));
    assert!(!engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(0));
}

#[test]
fn an_escape_sequence_split_across_chunks_decodes_once() {
    let mut engine = build_test_terminal_engine();

    // SGR 31 (red foreground) split mid-sequence across two chunks.
    assert_eq!(engine.process_pty_output(b"\x1b[3"), b"");
    assert_eq!(engine.process_pty_output(b"1mx"), b"");

    let cell = engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    assert_eq!(cell.get_character(), 'x');
    assert_eq!(cell.get_style(), red_style);
}

#[test]
fn a_utf8_code_point_split_across_chunks_decodes_once() {
    let mut engine = build_test_terminal_engine();

    // 'é' (0xC3 0xA9) split between its two bytes.
    assert_eq!(engine.process_pty_output(b"\xc3"), b"");
    assert_eq!(engine.process_pty_output(b"\xa9"), b"");

    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'é');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

#[test]
fn process_pty_output_returns_a_querys_reply_bytes() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"\x1b[5n"), b"\x1b[0n");
}

#[test]
fn a_query_split_across_chunks_replies_on_the_completing_chunk() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"\x1b[6"), b"");
    assert_eq!(engine.process_pty_output(b"n"), b"\x1b[1;1R");
}

#[test]
fn process_pty_output_drains_the_reply_queue_each_call() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"\x1b[5n"), b"\x1b[0n");
    // The reply was handed out above; the next chunk starts empty.
    assert_eq!(engine.process_pty_output(b"x"), b"");
}

#[test]
fn resize_terminal_state_changes_the_grid_dimensions() {
    let mut engine = build_test_terminal_engine();

    engine.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });

    assert_eq!(
        engine
            .get_terminal_state()
            .get_active_grid()
            .get_grid_dimensions(),
        (2, 4)
    );
}

#[test]
fn a_graphics_image_byte_limit_drops_the_next_image() {
    let mut engine = build_test_terminal_engine();
    engine.enqueue_graphics_event(Ok(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 16_384,
            pixel_height: 1_024,
            rgba_bytes: vec![0; MAX_IMAGE_BYTE_COUNT],
        })
        .into(),
        animation: None,
        action: ImageAction::Transmit,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    }));

    let _ = engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let graphics_events = engine.take_graphics_events();

    assert_eq!(graphics_events.len(), 2);
    match &graphics_events[0] {
        Ok(image_record) => {
            assert_eq!(image_record.image.pixel_width, 16_384);
            assert_eq!(image_record.image.pixel_height, 1_024);
            assert_eq!(image_record.image.rgba_bytes.len(), MAX_IMAGE_BYTE_COUNT);
        }
        Err(graphics_error) => panic!("the full image must stay queued, got {graphics_error:?}"),
    }
    assert_eq!(
        graphics_events[1],
        Err(GraphicsError::QueueFull {
            dropped_event_count: 1,
        })
    );
}

#[test]
fn queued_graphics_events_are_readable_without_draining() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 4,
    });
    assert_eq!(
        engine.process_pty_output(&build_sixel_image_registration(100, 0, 0)),
        b""
    );

    let expected_graphics_error = Err(GraphicsError::PlacementRejected {
        protocol: GraphicsProtocol::Sixel,
        placement_error: ImagePlacementError::MissingCellDimensions {
            requested_width: None,
            requested_height: None,
        },
    });
    assert_eq!(
        engine.list_graphics_events().cloned().collect::<Vec<_>>(),
        vec![expected_graphics_error.clone()]
    );
    assert_eq!(engine.list_graphics_events().count(), 1);
    assert_eq!(engine.take_graphics_events(), vec![expected_graphics_error]);
    assert_eq!(engine.list_graphics_events().count(), 0);
}

#[test]
fn dropped_graphics_errors_are_counted_separately_from_dropped_images() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 4,
    });
    let sixel_image_bytes = build_sixel_image_registration(100, 0, 0);

    for _ in 0..=MAX_GRAPHICS_EVENT_COUNT {
        assert_eq!(engine.process_pty_output(&sixel_image_bytes), b"");
    }

    assert_eq!(
        engine.list_graphics_events().count(),
        MAX_GRAPHICS_EVENT_COUNT
    );
    assert_eq!(engine.get_dropped_graphics_error_count(), 1);
    let graphics_events = engine.take_graphics_events();
    assert_eq!(graphics_events.len(), MAX_GRAPHICS_EVENT_BATCH_COUNT);
    assert_eq!(
        graphics_events.last(),
        Some(&Err(GraphicsError::QueueFull {
            dropped_event_count: 1,
        }))
    );
    assert_eq!(engine.get_dropped_graphics_error_count(), 0);
}

#[test]
fn rejected_graphics_placements_do_not_consume_the_image_byte_budget() {
    let mut engine = build_test_terminal_engine();
    let create_rejected_graphics = || DecodedGraphics {
        is_query: false,
        protocol: GraphicsProtocol::Sixel,
        image: DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![0; MAX_IMAGE_BYTE_COUNT],
        },
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
    };

    engine.process_graphics_operation(
        Ok(GraphicsOperation::Image(create_rejected_graphics())),
        (0, 0),
    );
    engine.process_graphics_operation(
        Ok(GraphicsOperation::Image(create_rejected_graphics())),
        (0, 0),
    );

    assert_eq!(
        engine.take_graphics_events(),
        vec![
            Err(GraphicsError::PlacementRejected {
                protocol: GraphicsProtocol::Sixel,
                placement_error: ImagePlacementError::MissingCellDimensions {
                    requested_width: None,
                    requested_height: None,
                },
            }),
            Err(GraphicsError::PlacementRejected {
                protocol: GraphicsProtocol::Sixel,
                placement_error: ImagePlacementError::MissingCellDimensions {
                    requested_width: None,
                    requested_height: None,
                },
            }),
        ]
    );
}

fn build_sixel_image_registration(
    red_channel: u16,
    green_channel: u16,
    blue_channel: u16,
) -> Vec<u8> {
    format!("\x1bPq#1;2;{red_channel};{green_channel};{blue_channel}#1@\x1b\\").into_bytes()
}

fn build_sixel_palette_update(red_channel: u16, green_channel: u16, blue_channel: u16) -> Vec<u8> {
    format!("\x1bPq#1;2;{red_channel};{green_channel};{blue_channel}\x1b\\").into_bytes()
}

fn build_graphics_terminal_engine() -> TerminalEngine {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 4,
        row_count: 4,
    });
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 6).expect("nonzero cell size"));
    engine
}

#[test]
fn indexed_sixel_operations_resolve_at_their_terminal_byte_offsets() {
    let mut engine = build_graphics_terminal_engine();
    let mut graphics_input_bytes = b"\x1b[?1070l".to_vec();
    graphics_input_bytes.extend(build_sixel_image_registration(100, 0, 0));
    graphics_input_bytes.extend_from_slice(b"\x1b[?1070h");
    graphics_input_bytes.extend(build_sixel_image_registration(0, 0, 100));

    assert_eq!(engine.process_pty_output(&graphics_input_bytes), b"");
    let image_placements = engine.get_terminal_state().list_image_placements();
    assert_eq!(image_placements.len(), 2);
    assert_eq!(
        &image_placements[0].get_image_record().image.rgba_bytes[..4],
        [255, 0, 0, 255]
    );
    assert_eq!(
        &image_placements[1].get_image_record().image.rgba_bytes[..4],
        [0, 0, 255, 255]
    );
}

#[test]
fn shared_sixel_palette_edits_repaint_existing_images_in_the_same_chunk() {
    let mut engine = build_graphics_terminal_engine();
    let mut graphics_input_bytes = b"\x1b[?1070l".to_vec();
    graphics_input_bytes.extend(build_sixel_image_registration(100, 0, 0));
    graphics_input_bytes.extend(build_sixel_image_registration(0, 100, 0));

    assert_eq!(engine.process_pty_output(&graphics_input_bytes), b"");
    let image_placements = engine.get_terminal_state().list_image_placements();
    assert_eq!(image_placements.len(), 2);
    assert_eq!(
        &image_placements[0].get_image_record().image.rgba_bytes[..4],
        [0, 255, 0, 255]
    );
    assert_eq!(
        &image_placements[1].get_image_record().image.rgba_bytes[..4],
        [0, 255, 0, 255]
    );
}

#[test]
fn a_shared_sixel_palette_only_operation_repaints_retained_images() {
    let mut engine = build_graphics_terminal_engine();
    let mut graphics_input_bytes = b"\x1b[?1070l".to_vec();
    graphics_input_bytes.extend(build_sixel_image_registration(100, 0, 0));
    graphics_input_bytes.extend(build_sixel_palette_update(0, 100, 0));

    assert_eq!(engine.process_pty_output(&graphics_input_bytes), b"");
    assert_eq!(engine.get_terminal_state().list_image_placements().len(), 1);
    assert_eq!(
        &engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes[..4],
        [0, 255, 0, 255]
    );
}

#[test]
fn c1_graphics_bytes_do_not_change_terminal_state() {
    let mut engine = build_test_terminal_engine();
    let initial_terminal_state = engine.get_terminal_state().clone();
    let mut terminal_input_bytes = vec![0x9f];
    terminal_input_bytes.extend_from_slice(b"Gf=32,s=1,v=1;/wAA/w==");
    terminal_input_bytes.push(0x9c);

    assert_eq!(engine.process_pty_output(&terminal_input_bytes), b"");
    assert_eq!(engine.get_terminal_state(), &initial_terminal_state);
    assert_eq!(
        engine.take_graphics_events(),
        vec![Ok(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: (DecodedImage {
                pixel_width: 1,
                pixel_height: 1,
                rgba_bytes: vec![255, 0, 0, 255],
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
    let mut engine = build_test_terminal_engine();
    let initial_terminal_state = engine.get_terminal_state().clone();
    let mut terminal_input_bytes = vec![0x9d];
    terminal_input_bytes.extend_from_slice(b"133;C");
    terminal_input_bytes.push(0x07);

    let program_facts = take_chunk_program_facts(&mut engine, &terminal_input_bytes);

    assert_eq!(program_facts, vec![ProgramFact::CommandStarted]);
    assert_eq!(
        engine.get_terminal_state().get_active_grid(),
        initial_terminal_state.get_active_grid()
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
}

#[test]
fn c1_string_openers_inside_osc_remain_osc_data() {
    let mut engine = build_test_terminal_engine();

    let mut osc_input_bytes = b"\x1b]2;before".to_vec();
    osc_input_bytes.push(0x9f);
    osc_input_bytes.extend_from_slice(b"after\x07");

    let _ = engine.process_pty_output(&osc_input_bytes);

    assert_eq!(
        engine.get_terminal_state().get_title(),
        Some("before�after")
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
}

fn build_graphics_control(protocol: GraphicsProtocol, is_c1_encoded: bool) -> Vec<u8> {
    match protocol {
        GraphicsProtocol::Kitty => {
            let control_payload = b"Gf=32,s=1,v=1;/wAA/w==";
            if is_c1_encoded {
                let mut control_bytes = vec![0x9f];
                control_bytes.extend_from_slice(control_payload);
                control_bytes.push(0x9c);
                control_bytes
            } else {
                let mut control_bytes = b"\x1b_".to_vec();
                control_bytes.extend_from_slice(control_payload);
                control_bytes.extend_from_slice(b"\x1b\\");
                control_bytes
            }
        }
        GraphicsProtocol::Sixel => {
            let control_payload = b"q#1;2;100;0;0#1@";
            if is_c1_encoded {
                let mut control_bytes = vec![0x90];
                control_bytes.extend_from_slice(control_payload);
                control_bytes.push(0x9c);
                control_bytes
            } else {
                let mut control_bytes = b"\x1bP".to_vec();
                control_bytes.extend_from_slice(control_payload);
                control_bytes.extend_from_slice(b"\x1b\\");
                control_bytes
            }
        }
        GraphicsProtocol::Iterm2 => {
            let base64_image_bytes = STANDARD.encode(build_red_png_bytes());
            let control_payload = format!(
                "1337;File=inline=1;size=67;width=1px;height=1px;preserveAspectRatio=0:{base64_image_bytes}"
            );
            if is_c1_encoded {
                let mut control_bytes = vec![0x9d];
                control_bytes.extend_from_slice(control_payload.as_bytes());
                control_bytes.push(0x9c);
                control_bytes
            } else {
                let mut control_bytes = b"\x1b]".to_vec();
                control_bytes.extend_from_slice(control_payload.as_bytes());
                control_bytes.push(0x07);
                control_bytes
            }
        }
    }
}

fn build_visible_graphics_control(protocol: GraphicsProtocol) -> Vec<u8> {
    match protocol {
        GraphicsProtocol::Kitty => b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec(),
        GraphicsProtocol::Sixel => build_sixel_image_registration(100, 0, 0),
        GraphicsProtocol::Iterm2 => build_graphics_control(GraphicsProtocol::Iterm2, false),
    }
}

#[test]
fn synchronized_graphics_commit_only_after_the_complete_end_sequence() {
    let test_timestamp = Instant::now();
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        let mut engine = build_test_terminal_engine();
        engine
            .set_cell_size(PixelCellSize::from_pixel_dimensions(1, 6).expect("nonzero cell size"));
        assert_eq!(engine.process_pty_output(b"old"), b"");
        let committed_terminal_state = engine.get_terminal_state().clone();
        let mut synchronized_input_bytes = BEGIN_SYNCHRONIZED_OUTPUT_BYTES.to_vec();
        synchronized_input_bytes.extend_from_slice(b"\x1b[2J\x1b[HN");
        synchronized_input_bytes.extend(build_visible_graphics_control(protocol));
        synchronized_input_bytes.extend_from_slice(END_SYNCHRONIZED_OUTPUT_BYTES);

        for (synchronized_byte_index, synchronized_input_byte) in
            synchronized_input_bytes.iter().enumerate()
        {
            let _ = engine.process_pty_output_at(&[*synchronized_input_byte], test_timestamp);
            if synchronized_byte_index + 1 < synchronized_input_bytes.len() {
                assert_eq!(
                    engine.get_terminal_state(),
                    &committed_terminal_state,
                    "{protocol:?}, byte {synchronized_byte_index}"
                );
                assert_eq!(
                    engine.take_graphics_events(),
                    [],
                    "{protocol:?}, byte {synchronized_byte_index}"
                );
            }
        }

        assert_eq!(
            get_terminal_cell_character(&engine, 0, 0),
            'N',
            "{protocol:?}"
        );
        assert_eq!(
            engine.get_terminal_state().list_image_placements().len(),
            1,
            "{protocol:?}"
        );
        let graphics_events = engine.take_graphics_events();
        assert_eq!(graphics_events.len(), 1, "{protocol:?}");
        assert_eq!(
            graphics_events[0]
                .as_ref()
                .map(|image_record| image_record.protocol),
            Ok(protocol)
        );
    }
}

#[test]
fn synchronized_control_lookalikes_inside_strings_do_not_release() {
    let test_timestamp = Instant::now();
    let control_lookalike_sequences: [&[u8]; 7] = [
        b"\x1b_Gbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1bPqbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1b]1337;File=inline=1:broken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x07",
        b"\x1bPtmux;broken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1bP\x1bPbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\\x1b\\",
        b"\x1bXbroken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
        b"\x1b^broken\x1b[?2026l\x9b?2026h\x9b?2026lbytes\x1b\\",
    ];

    for control_lookalike_sequence in control_lookalike_sequences {
        let mut engine = build_test_terminal_engine();
        let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);
        let mut synchronized_input_bytes = control_lookalike_sequence.to_vec();
        synchronized_input_bytes.push(b'X');
        assert_eq!(
            engine.process_pty_output_at(&synchronized_input_bytes, test_timestamp),
            None
        );
        assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
        assert_eq!(
            engine.get_next_synchronized_output_delay(test_timestamp),
            Some(SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION)
        );

        let has_advanced_terminal_state = engine
            .process_pty_output_at(END_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp)
            .is_some();
        assert!(has_advanced_terminal_state);
        assert!(engine
            .get_synchronized_output_transport(test_timestamp)
            .is_none());
    }
}

#[test]
fn c1_synchronized_controls_hold_and_release_at_every_byte_split() {
    const C1_SYNCHRONIZED_OUTPUT_BEGIN_BYTES: &[u8] = b"\x9b?2026h";
    const C1_SYNCHRONIZED_OUTPUT_END_BYTES: &[u8] = b"\x9b?2026l";
    let test_timestamp = Instant::now();

    for begin_byte_count in 0..=C1_SYNCHRONIZED_OUTPUT_BEGIN_BYTES.len() {
        for end_byte_count in 0..=C1_SYNCHRONIZED_OUTPUT_END_BYTES.len() {
            let mut engine = build_test_terminal_engine();
            let _ = engine.process_pty_output(b"old");
            let committed_terminal_state = engine.get_terminal_state().clone();

            let _ = engine.process_pty_output_at(
                &C1_SYNCHRONIZED_OUTPUT_BEGIN_BYTES[..begin_byte_count],
                test_timestamp,
            );
            let _ = engine.process_pty_output_at(
                &C1_SYNCHRONIZED_OUTPUT_BEGIN_BYTES[begin_byte_count..],
                test_timestamp,
            );
            let _ = engine.process_pty_output_at(b"\x1b[2J\x1b[HN", test_timestamp);
            assert_eq!(
                engine.get_terminal_state(),
                &committed_terminal_state,
                "begin_byte_count={begin_byte_count}, end_byte_count={end_byte_count}"
            );

            let has_advanced_before_end = engine
                .process_pty_output_at(
                    &C1_SYNCHRONIZED_OUTPUT_END_BYTES[..end_byte_count],
                    test_timestamp,
                )
                .is_some();
            if end_byte_count < C1_SYNCHRONIZED_OUTPUT_END_BYTES.len() {
                assert_eq!(
                    engine.get_terminal_state(),
                    &committed_terminal_state,
                    "begin_byte_count={begin_byte_count}, end_byte_count={end_byte_count}"
                );
            }
            let has_advanced_after_end = engine
                .process_pty_output_at(
                    &C1_SYNCHRONIZED_OUTPUT_END_BYTES[end_byte_count..],
                    test_timestamp,
                )
                .is_some();

            assert!(
                has_advanced_before_end || has_advanced_after_end,
                "begin_byte_count={begin_byte_count}, end_byte_count={end_byte_count}"
            );
            assert_eq!(
                get_terminal_cell_character(&engine, 0, 0),
                'N',
                "begin_byte_count={begin_byte_count}, end_byte_count={end_byte_count}"
            );
            assert!(engine
                .get_synchronized_output_transport(test_timestamp)
                .is_none());
        }
    }
}

#[test]
fn c1_synchronized_control_state_survives_process_swaps() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output(b"old");
    let _ = engine.process_pty_output(b"\x9b?20");
    let first_synchronized_output_transport = engine
        .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
        .expect("the split C1 begin has transport state");
    let first_terminal_undecoded_bytes = engine.get_undecoded_terminal_bytes().to_vec();
    let first_graphics_transport_state = engine.get_graphics_transport_state().unwrap_or_default();
    let first_graphics_events = engine.take_graphics_events();
    let first_terminal_state = engine.into_terminal_state();
    let mut restored_engine = TerminalEngine::from_carried_state(
        first_terminal_state,
        &first_terminal_undecoded_bytes,
        &first_graphics_events,
        first_graphics_transport_state,
        Some(first_synchronized_output_transport),
        test_timestamp,
        wall_clock_timestamp,
    );

    let _ = restored_engine.process_pty_output(b"26h\x1b[2J\x1b[HN\x9b?20");
    let committed_terminal_state = restored_engine.get_terminal_state().clone();
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'o');
    let second_synchronized_output_transport = restored_engine
        .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
        .expect("the split C1 end has transport state");
    let second_terminal_undecoded_bytes = restored_engine.get_undecoded_terminal_bytes().to_vec();
    let second_graphics_transport_state = restored_engine
        .get_graphics_transport_state()
        .unwrap_or_default();
    let second_graphics_events = restored_engine.take_graphics_events();
    let second_terminal_state = restored_engine.into_terminal_state();
    let mut restored_engine = TerminalEngine::from_carried_state(
        second_terminal_state,
        &second_terminal_undecoded_bytes,
        &second_graphics_events,
        second_graphics_transport_state,
        Some(second_synchronized_output_transport),
        test_timestamp,
        wall_clock_timestamp,
    );

    assert_eq!(
        restored_engine.get_terminal_state(),
        &committed_terminal_state
    );
    let has_advanced_terminal_state = restored_engine
        .process_pty_output_at(b"26l", test_timestamp)
        .is_some();

    assert!(has_advanced_terminal_state);
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'N');
    assert!(restored_engine
        .get_synchronized_output_transport(test_timestamp)
        .is_none());
}

#[test]
fn c1_csi_bytes_inside_utf8_and_strings_are_not_synchronized_controls() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    let mut terminal_input_bytes = vec![0xe2, 0x9b, 0xa0];
    terminal_input_bytes.extend_from_slice(b"\x1b]2;\x9b?2026h\x9b?2026l\x07");
    terminal_input_bytes.extend_from_slice(b"\x1bPq\x9b?2026h\x9b?2026l\x1b\\");
    terminal_input_bytes.extend_from_slice(b"\x1b_data\x9b?2026h\x9b?2026l\x1b\\");
    terminal_input_bytes.extend_from_slice(b"\x1bXdata\x9b?2026h\x9b?2026l\x1b\\");
    terminal_input_bytes.extend_from_slice(b"\x1b^data\x9b?2026h\x9b?2026l\x1b\\X");

    let has_advanced_terminal_state = engine
        .process_pty_output_at(&terminal_input_bytes, test_timestamp)
        .is_some();

    assert!(has_advanced_terminal_state);
    assert_eq!(
        engine.get_next_synchronized_output_delay(test_timestamp),
        None
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'X');
    assert!(engine
        .get_synchronized_output_transport(test_timestamp)
        .is_none());
}

#[test]
fn end_then_begin_in_one_chunk_commits_one_group_and_keeps_the_next() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);
    let mut synchronized_input_bytes = b"A".to_vec();
    synchronized_input_bytes.extend_from_slice(END_SYNCHRONIZED_OUTPUT_BYTES);
    synchronized_input_bytes.extend_from_slice(BEGIN_SYNCHRONIZED_OUTPUT_BYTES);
    synchronized_input_bytes.push(b'B');

    let has_advanced_terminal_state = engine
        .process_pty_output_at(&synchronized_input_bytes, test_timestamp)
        .is_some();

    assert!(has_advanced_terminal_state);
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'A');
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), ' ');
    assert_eq!(
        engine.get_next_synchronized_output_delay(test_timestamp),
        Some(SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION)
    );

    let has_advanced_terminal_state = engine
        .process_pty_output_at(END_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp)
        .is_some();
    assert!(has_advanced_terminal_state);
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'B');
}

#[test]
fn overdue_synchronized_bytes_are_released_before_new_input() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);
    let has_advanced_terminal_state = engine.process_pty_output_at(b"A", test_timestamp).is_some();
    assert!(!has_advanced_terminal_state);

    let has_advanced_terminal_state = engine
        .process_pty_output_at(b"B", test_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION)
        .is_some();

    assert!(has_advanced_terminal_state);
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'A');
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'B');
    assert!(engine
        .get_synchronized_output_transport(test_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION)
        .is_none());
}

#[test]
fn synchronized_output_releases_when_the_byte_bound_is_reached() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);

    let has_advanced_terminal_state = engine
        .process_pty_output_at(
            &vec![b'x'; MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT],
            test_timestamp,
        )
        .is_some();

    assert!(has_advanced_terminal_state);
    assert!(engine
        .get_synchronized_output_transport(test_timestamp)
        .is_none());
    assert_eq!(get_terminal_cell_character(&engine, 2, 7), 'x');
}

#[test]
fn synchronized_output_deadline_releases_replies_shell_facts_and_graphics() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 6).expect("nonzero cell size"));
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);
    let mut synchronized_input_bytes = b"Z\x1b[5n\x1b]133;C\x07".to_vec();
    synchronized_input_bytes.extend(build_visible_graphics_control(GraphicsProtocol::Kitty));
    let has_advanced_terminal_state = engine
        .process_pty_output_at(&synchronized_input_bytes, test_timestamp)
        .is_some();
    assert!(!has_advanced_terminal_state);

    assert_eq!(
        engine.expire_synchronized_output(test_timestamp + Duration::from_millis(149)),
        None
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.expire_synchronized_output(test_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION),
        Some(TerminalAdvance {
            device_query_replies: b"\x1b[0n".to_vec(),
            program_facts: build_timed_program_facts(
                vec![ProgramFact::CommandStarted],
                test_timestamp
            ),
        })
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(engine.get_terminal_state().list_image_placements().len(), 1);
    assert_eq!(engine.take_graphics_events().len(), 1);
}

#[test]
fn synchronized_output_deadline_keeps_elapsed_process_swap_time() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);
    let _ = engine.process_pty_output_at(b"R", test_timestamp);
    let synchronized_output_transport = engine
        .get_synchronized_output_transport_at(
            test_timestamp + Duration::from_millis(40),
            wall_clock_timestamp,
        )
        .expect("the open update has transport state");
    assert_eq!(
        synchronized_output_transport.get_deadline(),
        Some(wall_clock_timestamp + Duration::from_millis(110))
    );
    let terminal_undecoded_bytes = engine.get_undecoded_terminal_bytes().to_vec();
    let graphics_transport_state = engine.get_graphics_transport_state().unwrap_or_default();
    let terminal_state = engine.into_terminal_state();
    let restored_timestamp = test_timestamp + Duration::from_secs(1);
    let mut restored_engine = TerminalEngine::from_carried_state(
        terminal_state,
        &terminal_undecoded_bytes,
        &[],
        graphics_transport_state,
        Some(synchronized_output_transport),
        restored_timestamp,
        wall_clock_timestamp + Duration::from_millis(100),
    );

    assert_eq!(
        restored_engine.get_next_synchronized_output_delay(restored_timestamp),
        Some(Duration::from_millis(10))
    );
    assert_eq!(
        restored_engine.expire_synchronized_output(restored_timestamp + Duration::from_millis(9)),
        None
    );
    assert_eq!(
        restored_engine.expire_synchronized_output(restored_timestamp + Duration::from_millis(10)),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: Vec::new(),
        })
    );
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'R');
}

#[test]
fn synchronized_output_transport_rejects_impossible_scanner_state() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, test_timestamp);
    let mut serialized_transport = serde_json::to_value(
        engine
            .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
            .expect("the open update has transport state"),
    )
    .expect("the transport serializes");

    serialized_transport["terminal_input"]["remaining_utf8_continuation_count"] =
        serde_json::json!(4);
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(serialized_transport.clone())
            .unwrap_err()
            .to_string(),
        "terminal-input UTF-8 continuation count is invalid"
    );

    serialized_transport["terminal_input"]["remaining_utf8_continuation_count"] =
        serde_json::json!(0);
    serialized_transport["terminal_input"]["trailing_byte_count"] = serde_json::json!(0);
    serialized_transport["terminal_input"]["trailing_start_index"] = serde_json::json!(7);
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(serialized_transport.clone())
            .unwrap_err()
            .to_string(),
        "terminal-input scanner tail is invalid"
    );

    serialized_transport["terminal_input"]["trailing_start_index"] = serde_json::json!(0);
    serialized_transport["normalized_bytes"] = serde_json::json!([]);
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(serialized_transport)
            .unwrap_err()
            .to_string(),
        "synchronized-output deadline has no bytes"
    );
}

#[test]
fn held_facts_keep_the_read_instant_of_the_chunk_that_brought_them() {
    let begin_instant = Instant::now();
    let started_instant = begin_instant + Duration::from_millis(10);
    let finished_instant = begin_instant + Duration::from_millis(20);
    let mut engine = build_test_terminal_engine();
    assert_eq!(
        engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, begin_instant),
        None
    );
    assert_eq!(
        engine.process_pty_output_at(b"\x1b]133;C\x07", started_instant),
        None
    );
    assert_eq!(
        engine.process_pty_output_at(b"\x1b]133;D;0\x07", finished_instant),
        None
    );

    assert_eq!(
        engine.process_pty_output_at(
            END_SYNCHRONIZED_OUTPUT_BYTES,
            begin_instant + Duration::from_millis(30)
        ),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: vec![
                TimedProgramFact {
                    program_fact: ProgramFact::CommandStarted,
                    read_instant: started_instant,
                },
                TimedProgramFact {
                    program_fact: ProgramFact::CommandFinished { exit_code: Some(0) },
                    read_instant: finished_instant,
                },
            ],
        })
    );
}

#[test]
fn an_update_that_ends_and_opens_again_in_one_chunk_keeps_each_fact_at_its_read_instant() {
    // The second chunk ends the open update and opens a new one. The bytes
    // before the new `CSI ? 2026 h` release; the start after it waits for the
    // deadline of the new update, and keeps the second chunk's instant.
    let first_read_instant = Instant::now();
    let second_read_instant = first_read_instant + Duration::from_millis(20);
    let mut engine = build_test_terminal_engine();
    let mut first_chunk_bytes = BEGIN_SYNCHRONIZED_OUTPUT_BYTES.to_vec();
    first_chunk_bytes.extend_from_slice(b"\x1b]133;C\x07");
    assert_eq!(
        engine.process_pty_output_at(&first_chunk_bytes, first_read_instant),
        None
    );
    let mut second_chunk_bytes = b"\x1b]133;D;0\x07".to_vec();
    second_chunk_bytes.extend_from_slice(END_SYNCHRONIZED_OUTPUT_BYTES);
    second_chunk_bytes.extend_from_slice(BEGIN_SYNCHRONIZED_OUTPUT_BYTES);
    second_chunk_bytes.extend_from_slice(b"\x1b]133;C\x07");

    assert_eq!(
        engine.process_pty_output_at(&second_chunk_bytes, second_read_instant),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: vec![
                TimedProgramFact {
                    program_fact: ProgramFact::CommandStarted,
                    read_instant: first_read_instant,
                },
                TimedProgramFact {
                    program_fact: ProgramFact::CommandFinished { exit_code: Some(0) },
                    read_instant: second_read_instant,
                },
            ],
        })
    );
    assert_eq!(
        engine
            .expire_synchronized_output(second_read_instant + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: build_timed_program_facts(
                vec![ProgramFact::CommandStarted],
                second_read_instant
            ),
        })
    );
}

#[test]
fn splitting_held_chunk_starts_gives_each_part_the_starts_of_its_own_bytes() {
    let first_read_instant = Instant::now();
    let second_read_instant = first_read_instant + Duration::from_millis(20);
    let build_synchronized_output = || SynchronizedOutput {
        buffered_bytes: vec![b'x'; 14],
        held_chunk_starts: vec![
            HeldChunkStart {
                first_byte_index: 0,
                read_instant: first_read_instant,
            },
            HeldChunkStart {
                first_byte_index: 10,
                read_instant: second_read_instant,
            },
        ],
        deadline: None,
    };

    let mut split_inside_first_chunk = build_synchronized_output();
    assert_eq!(
        split_inside_first_chunk.split_held_chunk_starts(4),
        [HeldChunkStart {
            first_byte_index: 0,
            read_instant: first_read_instant,
        }]
    );
    assert_eq!(
        split_inside_first_chunk.held_chunk_starts,
        [
            HeldChunkStart {
                first_byte_index: 0,
                read_instant: first_read_instant,
            },
            HeldChunkStart {
                first_byte_index: 6,
                read_instant: second_read_instant,
            },
        ]
    );

    let mut split_at_second_chunk = build_synchronized_output();
    assert_eq!(
        split_at_second_chunk.split_held_chunk_starts(10),
        [HeldChunkStart {
            first_byte_index: 0,
            read_instant: first_read_instant,
        }]
    );
    assert_eq!(
        split_at_second_chunk.held_chunk_starts,
        [HeldChunkStart {
            first_byte_index: 0,
            read_instant: second_read_instant,
        }]
    );
}

#[test]
fn held_chunk_read_times_survive_a_process_swap() {
    // Read ages at the swap: 40 ms and 30 ms. The restored engine starts
    // 100 ms of wall time later, so the ages become 140 ms and 130 ms.
    let begin_instant = Instant::now();
    let started_instant = begin_instant + Duration::from_millis(10);
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, begin_instant);
    let _ = engine.process_pty_output_at(b"\x1b]133;C\x07", started_instant);
    let synchronized_output_transport = engine
        .get_synchronized_output_transport_at(
            begin_instant + Duration::from_millis(40),
            wall_clock_timestamp,
        )
        .expect("the open update has transport state");
    assert_eq!(
        synchronized_output_transport.held_chunk_read_times,
        [
            HeldChunkReadTime {
                first_byte_index: 0,
                read_at: wall_clock_timestamp - Duration::from_millis(40),
            },
            HeldChunkReadTime {
                first_byte_index: BEGIN_SYNCHRONIZED_OUTPUT_BYTES.len(),
                read_at: wall_clock_timestamp - Duration::from_millis(30),
            },
        ]
    );
    let serialized_transport =
        serde_json::to_value(&synchronized_output_transport).expect("the transport serializes");
    assert_eq!(
        serde_json::from_value::<SynchronizedOutputTransport>(serialized_transport)
            .expect("the transport deserializes"),
        synchronized_output_transport
    );
    let terminal_state = engine.into_terminal_state();
    let restored_timestamp = begin_instant + Duration::from_secs(1);
    let mut restored_engine = TerminalEngine::from_carried_state(
        terminal_state,
        &[],
        &[],
        GraphicsTransportState::default(),
        Some(synchronized_output_transport),
        restored_timestamp,
        wall_clock_timestamp + Duration::from_millis(100),
    );

    assert_eq!(
        restored_engine.expire_synchronized_output(restored_timestamp + Duration::from_millis(10)),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: build_timed_program_facts(
                vec![ProgramFact::CommandStarted],
                restored_timestamp - Duration::from_millis(130)
            ),
        })
    );
}

#[test]
fn a_resume_format_5_transport_reads_every_held_byte_at_its_deadline_less_150_ms() {
    let begin_instant = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, begin_instant);
    let synchronized_output_transport = engine
        .get_synchronized_output_transport_at(begin_instant, wall_clock_timestamp)
        .expect("the open update has transport state");
    let mut format_5_transport_json =
        serde_json::to_value(&synchronized_output_transport).expect("the transport serializes");
    format_5_transport_json
        .as_object_mut()
        .expect("the transport is an object")
        .remove("held_chunk_read_times");

    let read_format_5_transport = |transport_json: serde_json::Value| {
        serde_json::from_value::<ResumeFormat5SynchronizedOutputTransport>(transport_json)
            .expect("the format 5 transport deserializes")
            .into_synchronized_output_transport()
    };

    assert_eq!(
        read_format_5_transport(format_5_transport_json.clone()),
        synchronized_output_transport
    );

    format_5_transport_json["normalized_bytes"] = serde_json::json!([]);
    format_5_transport_json["deadline"] = serde_json::Value::Null;
    assert_eq!(
        read_format_5_transport(format_5_transport_json).held_chunk_read_times,
        []
    );
}

#[test]
fn synchronized_output_transport_rejects_read_times_that_do_not_fit_its_bytes() {
    let begin_instant = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_at(BEGIN_SYNCHRONIZED_OUTPUT_BYTES, begin_instant);
    let _ =
        engine.process_pty_output_at(b"\x1b]133;C\x07", begin_instant + Duration::from_millis(10));
    let serialized_transport = serde_json::to_value(
        engine
            .get_synchronized_output_transport_at(begin_instant, wall_clock_timestamp)
            .expect("the open update has transport state"),
    )
    .expect("the transport serializes");
    let held_byte_count = serialized_transport["normalized_bytes"]
        .as_array()
        .expect("the bytes are an array")
        .len();
    let read_problem = |transport_json: serde_json::Value| {
        serde_json::from_value::<SynchronizedOutputTransport>(transport_json)
            .unwrap_err()
            .to_string()
    };

    let mut no_read_times = serialized_transport.clone();
    no_read_times["held_chunk_read_times"] = serde_json::json!([]);
    assert_eq!(
        read_problem(no_read_times),
        "synchronized-output bytes have no chunk read time at index 0"
    );

    let mut first_read_time_after_index_0 = serialized_transport.clone();
    first_read_time_after_index_0["held_chunk_read_times"][0]["first_byte_index"] =
        serde_json::json!(1);
    assert_eq!(
        read_problem(first_read_time_after_index_0),
        "synchronized-output bytes have no chunk read time at index 0"
    );

    let mut repeated_index = serialized_transport.clone();
    repeated_index["held_chunk_read_times"][1]["first_byte_index"] = serde_json::json!(0);
    assert_eq!(
        read_problem(repeated_index),
        "synchronized-output chunk read times are out of order"
    );

    let mut index_past_the_bytes = serialized_transport.clone();
    index_past_the_bytes["held_chunk_read_times"][1]["first_byte_index"] =
        serde_json::json!(held_byte_count);
    assert_eq!(
        read_problem(index_past_the_bytes),
        "synchronized-output chunk read times are out of order"
    );

    let mut read_times_with_no_bytes = serialized_transport;
    read_times_with_no_bytes["normalized_bytes"] = serde_json::json!([]);
    read_times_with_no_bytes["deadline"] = serde_json::Value::Null;
    assert_eq!(
        read_problem(read_times_with_no_bytes),
        "synchronized-output chunk read times have no bytes"
    );
}

#[test]
fn overlong_open_string_keeps_its_scanner_state_across_process_swap() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
    let mut opening_string_bytes = b"\x1b]2;".to_vec();
    opening_string_bytes.extend(std::iter::repeat_n(b'A', MAX_UNDECODED_BYTE_COUNT + 1));
    let string_suffix_bytes = b"inside\x1b[?2026hdata\x1b[?2026l\x07";

    let mut uninterrupted_engine = build_test_terminal_engine();
    let _ = uninterrupted_engine.process_pty_output(&opening_string_bytes);
    assert!(uninterrupted_engine
        .get_undecoded_terminal_bytes()
        .is_empty());

    let mut carried_engine = build_test_terminal_engine();
    let _ = carried_engine.process_pty_output(&opening_string_bytes);
    assert!(carried_engine.get_undecoded_terminal_bytes().is_empty());
    let synchronized_output_transport = carried_engine
        .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
        .expect("the open string has scanner transport state");
    assert_eq!(synchronized_output_transport.get_deadline(), None);
    let graphics_transport_state = carried_engine
        .get_graphics_transport_state()
        .unwrap_or_default();
    let terminal_state = carried_engine.into_terminal_state();
    let mut restored_engine = TerminalEngine::from_carried_state(
        terminal_state,
        &[],
        &[],
        graphics_transport_state,
        Some(synchronized_output_transport),
        test_timestamp,
        wall_clock_timestamp,
    );

    let _ = uninterrupted_engine.process_pty_output(string_suffix_bytes);
    let has_advanced_terminal_state = restored_engine
        .process_pty_output_at(string_suffix_bytes, test_timestamp)
        .is_some();

    assert!(has_advanced_terminal_state);
    assert_eq!(
        restored_engine.get_next_synchronized_output_delay(test_timestamp),
        None
    );
    assert_eq!(
        restored_engine.take_graphics_events(),
        uninterrupted_engine.take_graphics_events()
    );
    assert!(restored_engine
        .get_synchronized_output_transport(test_timestamp)
        .is_none());
}

#[test]
fn c1_string_end_and_split_utf8_stay_exact_across_two_process_swaps() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000);
    let mut opening_graphics_string_bytes = b"\x1b_Gf=32,s=1,v=1;".to_vec();
    opening_graphics_string_bytes.extend(std::iter::repeat_n(b'A', MAX_UNDECODED_BYTE_COUNT + 1));

    let mut uninterrupted_engine = build_test_terminal_engine();
    let _ = uninterrupted_engine.process_pty_output(&opening_graphics_string_bytes);
    let mut carried_engine = build_test_terminal_engine();
    let _ = carried_engine.process_pty_output(&opening_graphics_string_bytes);
    let first_synchronized_output_transport = carried_engine
        .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
        .expect("the open APC has terminal-input transport state");
    let first_graphics_transport_state = carried_engine
        .get_graphics_transport_state()
        .expect("the open APC has graphics transport state");
    let first_terminal_state = carried_engine.into_terminal_state();
    let mut restored_engine = TerminalEngine::from_carried_state(
        first_terminal_state,
        &[],
        &[],
        first_graphics_transport_state,
        Some(first_synchronized_output_transport),
        test_timestamp,
        wall_clock_timestamp,
    );

    let c1_string_terminator = [0x9c];
    let _ = uninterrupted_engine.process_pty_output(&c1_string_terminator);
    let _ = restored_engine.process_pty_output(&c1_string_terminator);
    let _ = uninterrupted_engine.process_pty_output(BEGIN_SYNCHRONIZED_OUTPUT_BYTES);
    let _ = restored_engine.process_pty_output(BEGIN_SYNCHRONIZED_OUTPUT_BYTES);
    let split_utf8_bytes = [0xe2, 0x94];
    let _ = uninterrupted_engine.process_pty_output(&split_utf8_bytes);
    let _ = restored_engine.process_pty_output(&split_utf8_bytes);

    let second_synchronized_output_transport = restored_engine
        .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
        .expect("the held UTF-8 prefix has transport state");
    let second_terminal_undecoded_bytes = restored_engine.get_undecoded_terminal_bytes().to_vec();
    let second_graphics_transport_state = restored_engine
        .get_graphics_transport_state()
        .unwrap_or_default();
    let second_graphics_events = restored_engine.take_graphics_events();
    let second_terminal_state = restored_engine.into_terminal_state();
    let mut restored_engine = TerminalEngine::from_carried_state(
        second_terminal_state,
        &second_terminal_undecoded_bytes,
        &second_graphics_events,
        second_graphics_transport_state,
        Some(second_synchronized_output_transport),
        test_timestamp,
        wall_clock_timestamp,
    );

    let final_utf8_byte = [0x90];
    let _ = uninterrupted_engine.process_pty_output(&final_utf8_byte);
    let _ = restored_engine.process_pty_output(&final_utf8_byte);
    let _ = uninterrupted_engine.process_pty_output(END_SYNCHRONIZED_OUTPUT_BYTES);
    let _ = restored_engine.process_pty_output(END_SYNCHRONIZED_OUTPUT_BYTES);

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), '┐');
    assert_eq!(
        restored_engine.get_next_synchronized_output_delay(test_timestamp),
        None
    );
    assert_eq!(
        restored_engine.take_graphics_events(),
        uninterrupted_engine.take_graphics_events()
    );
    assert_eq!(
        restored_engine.get_terminal_state(),
        uninterrupted_engine.get_terminal_state()
    );
}

#[test]
fn malformed_synchronized_modes_remain_ordinary_terminal_input() {
    for malformed_terminal_input_bytes in [
        &b"\x1b[?2026;1hX"[..],
        &b"\x1b[?1;2026hX"[..],
        &b"\x1b[?2026:1hX"[..],
        &b"\x1b[?02026hX"[..],
    ] {
        let test_timestamp = Instant::now();
        let mut engine = build_test_terminal_engine();
        let has_advanced_terminal_state = engine
            .process_pty_output_at(malformed_terminal_input_bytes, test_timestamp)
            .is_some();

        assert!(has_advanced_terminal_state);
        assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'X');
        assert!(engine
            .get_synchronized_output_transport(test_timestamp)
            .is_none());
    }
}

fn assert_graphics_event_after_utf8_prefix(
    utf8_prefix_bytes: &[u8],
    protocol: GraphicsProtocol,
    is_c1_encoded: bool,
) {
    let mut engine = build_test_terminal_engine();
    engine.set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).expect("nonzero cell size"));
    let mut terminal_input_bytes = utf8_prefix_bytes.to_vec();
    terminal_input_bytes.extend(build_graphics_control(protocol, is_c1_encoded));

    let _ = engine.process_pty_output(&terminal_input_bytes);
    let graphics_events = engine.take_graphics_events();
    assert_eq!(graphics_events.len(), 1, "{protocol:?}, C1={is_c1_encoded}");
    let image_record = graphics_events[0]
        .as_ref()
        .unwrap_or_else(|graphics_error| {
            panic!("{protocol:?}, C1={is_c1_encoded} was rejected: {graphics_error:?}")
        });
    assert_eq!(image_record.protocol, protocol);
}

#[test]
fn every_utf8_continuation_before_each_graphics_protocol_is_text() {
    for continuation_byte in 0x80..=0x9f {
        let utf8_prefix_bytes = [0xe0, 0xa0, continuation_byte];
        for protocol in [
            GraphicsProtocol::Kitty,
            GraphicsProtocol::Sixel,
            GraphicsProtocol::Iterm2,
        ] {
            for is_c1_encoded in [false, true] {
                assert_graphics_event_after_utf8_prefix(
                    &utf8_prefix_bytes,
                    protocol,
                    is_c1_encoded,
                );
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
        for is_c1_encoded in [false, true] {
            let mut engine = build_test_terminal_engine();
            engine.set_cell_size(
                PixelCellSize::from_pixel_dimensions(1, 1).expect("nonzero cell size"),
            );
            assert_eq!(engine.process_pty_output(b"\xe0\xa0"), b"");

            let mut remaining_terminal_input_bytes = vec![0x90];
            remaining_terminal_input_bytes.extend(build_graphics_control(protocol, is_c1_encoded));
            let _ = engine.process_pty_output(&remaining_terminal_input_bytes);
            let graphics_events = engine.take_graphics_events();
            assert_eq!(graphics_events.len(), 1, "{protocol:?}, C1={is_c1_encoded}");
            assert_eq!(graphics_events[0].as_ref().unwrap().protocol, protocol);
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
        for is_c1_encoded in [false, true] {
            let mut engine = build_test_terminal_engine();
            engine.set_cell_size(
                PixelCellSize::from_pixel_dimensions(1, 1).expect("nonzero cell size"),
            );
            assert_eq!(engine.process_pty_output(b"\xe2\x94"), b"");
            assert_eq!(engine.get_undecoded_terminal_bytes(), b"\xe2\x94");
            let graphics_transport_state = engine
                .get_graphics_transport_state()
                .expect("the split UTF-8 prefix has graphics transport state");
            assert_eq!(graphics_transport_state.carry_bytes, b"\xe2\x94");

            let terminal_state = engine.into_terminal_state();
            let mut rebuilt_engine = TerminalEngine::from_carried_state(
                terminal_state,
                b"\xe2\x94",
                &[],
                graphics_transport_state,
                None,
                Instant::now(),
                SystemTime::now(),
            );
            assert_eq!(rebuilt_engine.process_pty_output(&[0x90]), b"");
            assert_eq!(get_terminal_cell_character(&rebuilt_engine, 0, 0), '┐');
            assert_eq!(
                rebuilt_engine
                    .get_terminal_state()
                    .get_active_cursor_position(),
                (0, 1)
            );

            let _ =
                rebuilt_engine.process_pty_output(&build_graphics_control(protocol, is_c1_encoded));
            let graphics_events = rebuilt_engine.take_graphics_events();
            assert_eq!(graphics_events.len(), 1, "{protocol:?}, C1={is_c1_encoded}");
            assert_eq!(graphics_events[0].as_ref().unwrap().protocol, protocol);
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
        let mut engine = build_test_terminal_engine();
        engine
            .set_cell_size(PixelCellSize::from_pixel_dimensions(1, 1).expect("nonzero cell size"));
        assert_eq!(engine.process_pty_output(&[0xe2, b'A']), b"");

        assert_eq!(get_terminal_cell_character(&engine, 0, 0), '\u{fffd}');
        assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'A');
        assert_eq!(
            engine.get_terminal_state().get_active_cursor_position(),
            (0, 2)
        );

        let _ = engine.process_pty_output(&build_graphics_control(protocol, true));
        let graphics_events = engine.take_graphics_events();
        assert_eq!(graphics_events.len(), 1, "{protocol:?}");
        assert_eq!(graphics_events[0].as_ref().unwrap().protocol, protocol);
    }
}

#[test]
fn utf8_c1_st_inside_osc_remains_osc_data() {
    let mut engine = build_test_terminal_engine();

    let mut osc_input_bytes = b"\x1b]2;before\xc2".to_vec();
    osc_input_bytes.extend_from_slice(b"\x9cafter\x07");

    let _ = engine.process_pty_output(&osc_input_bytes);

    assert_eq!(engine.get_terminal_state().get_title(), Some("beforeafter"));
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
}

#[test]
fn a_partial_decode_survives_a_resize() {
    let mut engine = build_test_terminal_engine();

    // The sequence opens before the resize and completes after it: the pen
    // still turns red and the glyph lands styled.
    assert_eq!(engine.process_pty_output(b"\x1b[3"), b"");
    engine.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(engine.process_pty_output(b"1mx"), b"");

    let cell = engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    assert_eq!(cell.get_character(), 'x');
    assert_eq!(cell.get_style(), red_style);
}

// --- Adversarial: chunk-split torture and scale ---

/// A mixed run of SGR, cursor moves, an erase, line feeds, and text. Fed both
/// whole and one byte at a time, the parser reaches byte-identical state:
/// splitting a sequence at any boundary does not change the outcome.
#[test]
fn a_sequence_split_at_every_byte_boundary_matches_the_whole_feed() {
    let terminal_input_sequence = b"\x1b[1;31mAB\x1b[2;3HCD\r\n\x1b[Kxy";

    let mut whole_input_engine = build_test_terminal_engine();
    let _ = whole_input_engine.process_pty_output(terminal_input_sequence);

    let mut split_input_engine = build_test_terminal_engine();
    for terminal_input_byte in terminal_input_sequence {
        let _ = split_input_engine.process_pty_output(&[*terminal_input_byte]);
    }

    // Concrete landmarks: the comparison below is between two non-blank grids.
    assert_eq!(get_terminal_cell_character(&whole_input_engine, 0, 0), 'A');
    assert_eq!(get_terminal_cell_character(&whole_input_engine, 1, 2), 'C');
    assert_eq!(get_terminal_cell_character(&whole_input_engine, 2, 0), 'x');
    assert_eq!(
        whole_input_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (2, 2)
    );

    // The one-byte-at-a-time feed lands on exactly the same grid and cursor.
    assert_eq!(
        whole_input_engine.get_terminal_state().get_active_grid(),
        split_input_engine.get_terminal_state().get_active_grid()
    );
    assert_eq!(
        whole_input_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        split_input_engine
            .get_terminal_state()
            .get_active_cursor_position(),
    );
}

/// Streams whose control sequences meet an open code point, an eight-bit
/// control, a CAN or a second `ESC` give the same screen, title and images fed
/// whole as fed one byte at a time.
#[test]
fn control_sequences_beside_open_code_points_and_eight_bit_controls_match_one_byte_feeds() {
    let terminal_input_streams: [&[u8]; 5] = [
        b"\xe2\x1b[\x9fGf=32,s=1,v=1;/wAA/w==\x9c",
        b"\xe2\x1b[m\x9d0;title\x07",
        b"\x1b[1\x1b[?2026hX\x1b[?2026lY",
        b"\x9d0;t\x07\x1b[1mX",
        b"\x1b[1\x18X\x1b[?2026h",
    ];
    for terminal_input_stream in terminal_input_streams {
        let mut whole_input_engine = build_test_terminal_engine();
        let _ = whole_input_engine.process_pty_output(terminal_input_stream);
        let mut split_input_engine = build_test_terminal_engine();
        for terminal_input_byte in terminal_input_stream {
            let _ = split_input_engine.process_pty_output(&[*terminal_input_byte]);
        }

        assert!(
            whole_input_engine.get_terminal_state() == split_input_engine.get_terminal_state(),
            "{terminal_input_stream:?}"
        );
        assert_eq!(
            whole_input_engine.take_graphics_events(),
            split_input_engine.take_graphics_events(),
            "{terminal_input_stream:?}"
        );
    }
}

#[test]
fn an_eight_bit_control_after_a_control_sequence_that_cut_a_code_point_is_a_control() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\xe2\x1b[m\x9d0;title\x07");

    assert_eq!(engine.get_terminal_state().get_title(), Some("title"));
}

#[test]
fn a_control_sequence_after_an_eight_bit_control_in_one_chunk_applies() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x9d0;t\x07\x1b[1mX");

    let mut bold_style = Style::default();
    bold_style.set_bold(true);
    let cell = engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    assert_eq!(cell.get_character(), 'X');
    assert_eq!(cell.get_style(), bold_style);
}

#[test]
fn a_synchronized_update_that_interrupts_a_control_sequence_holds_the_text_after_it() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[1\x1b[?2026hX");

    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    let _ = engine.process_pty_output(b"\x1b[?2026l");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'X');
}

#[test]
fn a_three_byte_wide_char_split_across_chunks_decodes_once() {
    let mut engine = build_test_terminal_engine();

    // '世' is 0xE4 0xB8 0x96 — a wide CJK glyph split after its first byte.
    assert_eq!(engine.process_pty_output(b"\xe4"), b"");
    assert_eq!(engine.process_pty_output(b"\xb8\x96"), b"");

    let cell = engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    assert_eq!(cell.get_character(), '世');
    assert_eq!(cell.get_display_width(), 2);
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 2)
    );
}

#[test]
fn a_truncated_csi_resumes_and_applies_on_the_next_chunk() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"abc"); // fill row 0
    let _ = engine.process_pty_output(b"\x1b["); // CSI opened but not completed — held
    let _ = engine.process_pty_output(b"2J"); // completes ED 2 across the chunk boundary

    // The held CSI resumed and cleared the whole screen.
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), ' ');
    assert_eq!(get_terminal_cell_character(&engine, 0, 2), ' ');
}

#[test]
fn an_escape_split_from_its_bracket_still_forms_a_csi() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"abc"); // fill row 0
    let _ = engine.process_pty_output(b"\x1b"); // lone ESC at a chunk end — held in Escape
    let _ = engine.process_pty_output(b"[2J"); // the bracket + ED 2 arrive next

    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), ' ');
    assert_eq!(get_terminal_cell_character(&engine, 0, 2), ' ');
}

#[test]
fn a_ten_thousand_column_line_wraps_without_panicking() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });

    let terminal_input_bytes = vec![b'a'; 10_000];
    let _ = engine.process_pty_output(&terminal_input_bytes);

    // 10000 / 80 = 125 logical rows; the last stays on screen unscrolled: the
    // bottom row holds the final run and the cursor rests on the last column.
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (23, 79)
    );
    assert_eq!(get_terminal_cell_character(&engine, 23, 0), 'a');
    assert_eq!(get_terminal_cell_character(&engine, 23, 79), 'a');
    // 125 rows produced, 24 on screen (the last unscrolled) → 101 in history.
    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        101
    );
}

#[test]
fn many_line_feeds_cap_the_scrollback_at_its_line_limit() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    // 12000 line feeds on a 2-row screen: the first descends without scrolling,
    // the remaining 11999 each push one row into history.
    let line_feed_bytes = vec![b'\n'; 12_000];
    let _ = engine.process_pty_output(&line_feed_bytes);

    // The default 10 000-line cap holds; the overflow is dropped.
    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        10_000
    );
    assert_eq!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .get_total_pushed_line_count(),
        11_999
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (1, 0)
    );
}

// --- Taking the state apart and rebuilding it ---

/// Everything the state holds survives being written out and read back:
/// both screen buffers, both cursors and their saved snapshots, the pen, the
/// modes, the scrollback with its line counts, the title, and the
/// grapheme cluster still open at the cursor.
#[test]
fn a_driven_engine_state_survives_a_serde_round_trip() {
    let mut engine = TerminalEngine::with_scrollback(
        PtySize {
            column_count: 8,
            row_count: 3,
        },
        ScrollbackLimit::from_line_and_byte_limits(4, 4096),
    );

    // Bold red pen, then ten characters on an eight-column row: the row
    // soft-wraps onto the row below it.
    let _ = engine.process_pty_output(b"\x1b[1;31mabcdefghij");
    // A wide CJK glyph, which takes two columns.
    let _ = engine.process_pty_output("世".as_bytes());
    // DECSC saves the cursor and the pen, both change, DECRC restores them.
    let _ = engine.process_pty_output(b"\x1b7\x1b[4;32mZ\x1b8");
    // Paint the alternate screen, then return to the primary.
    let _ = engine.process_pty_output(b"\x1b[?1049hALT\x1b[?1049l");
    // Ten line feeds on a three-row screen hand more rows to history than the
    // four-line cap holds: the oldest are dropped.
    let _ = engine.process_pty_output(b"\n\n\n\n\n\n\n\n\n\n");
    // A title, then a base character with a combining acute over it: the
    // cluster is still open when the state is taken apart.
    let _ = engine.process_pty_output("\x1b]0;koshi\x07e\u{0301}".as_bytes());

    let terminal_state = engine.into_terminal_state();

    // Landmarks: the comparison below is between two non-blank states.
    assert_eq!(terminal_state.get_title(), Some("koshi"));
    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 4);
    // The cursor sat on row 1, so the first feed descends and the other nine
    // each hand a row to history: nine pushed, four kept, five dropped.
    assert_eq!(
        terminal_state
            .get_scrollback()
            .get_total_pushed_line_count(),
        9
    );

    let serialized_terminal_state =
        serde_json::to_string(&terminal_state).expect("the state writes out");
    let deserialized_terminal_state: TerminalState =
        serde_json::from_str(&serialized_terminal_state).expect("the state reads back");
    assert_eq!(deserialized_terminal_state, terminal_state);

    // An engine rebuilt from the recovered state holds exactly that state.
    let rebuilt_engine = TerminalEngine::from_terminal_state(deserialized_terminal_state, b"");
    assert_eq!(rebuilt_engine.get_terminal_state(), &terminal_state);
}

// --- Carrying a half-received sequence to the next parser ---

/// Take a terminal engine apart the way a process-image swap does and build the next
/// engine from what crossed: the screen state and the bytes the parser held.
fn rebuild_terminal_engine(terminal_engine: TerminalEngine) -> TerminalEngine {
    let carried_undecoded_bytes = terminal_engine.get_undecoded_terminal_bytes().to_vec();
    TerminalEngine::from_terminal_state(
        terminal_engine.into_terminal_state(),
        &carried_undecoded_bytes,
    )
}

/// A chunk that ends on a sequence boundary leaves the next parser nothing to
/// take over.
#[test]
fn a_finished_chunk_leaves_the_parser_holding_nothing() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[31mab");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
}

/// A working-directory report (OSC 7) cut in half is carried whole: the pane
/// keeps its old directory until the report finishes, and no part of the URI
/// prints as text.
#[test]
fn a_split_working_directory_report_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // The shell reports /home/user/Projects/koshi, and the chunk ends after
    // `/Proj`.
    let _ = engine.process_pty_output(b"\x1b]7;file://host/home/user/Proj");

    assert_eq!(
        engine.get_terminal_state().get_current_working_directory(),
        None
    );
    assert_eq!(
        engine.get_undecoded_terminal_bytes(),
        b"\x1b]7;file://host/home/user/Proj"
    );

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"ects/koshi\x07");

    let reported_working_directory = restored_engine
        .get_terminal_state()
        .get_current_working_directory()
        .expect("the report finished");
    assert_eq!(reported_working_directory.get_host(), Some("host"));
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/home/user/Projects/koshi")
    );
    // The tail joined the sequence instead of landing on the screen.
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), ' ');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 0)
    );
}

/// A title report (OSC 0) cut in half is carried whole: the title changes
/// once, to the whole payload.
#[test]
fn a_split_title_report_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;ti");

    assert_eq!(engine.get_terminal_state().get_title(), None);
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b]0;ti");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"tle\x07");

    assert_eq!(
        restored_engine.get_terminal_state().get_title(),
        Some("title")
    );
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), ' ');
}

/// Each chunk adds `1` to the title change count when it names another title
/// or clears one, and `1` to the working directory change count when it names
/// another directory. A chunk that repeats the current value, or that sends an
/// OSC 7 that does not parse, adds `0`.
#[test]
fn a_chunk_counts_one_title_or_directory_change_only_when_the_value_changes() {
    let mut engine = build_test_terminal_engine();
    let chunk_change_counts: [(&[u8], (u64, u64)); 9] = [
        (b"\x1b]2;htop\x07", (1, 0)),
        (b"\x1b]2;htop\x07", (1, 0)),
        (b"\x1b]0;vim\x07\x1b]2;less\x07", (2, 0)),
        (b"\x1b]7;file://host/home/user/project\x07", (2, 1)),
        (b"\x1b]7;file://host/home/user/project\x07", (2, 1)),
        (b"\x1b]7;http://host/home/user\x07", (2, 1)),
        (b"\x1b]7;file://host/home/user\x07", (2, 2)),
        (b"\x1bc", (3, 2)),
        (b"\x1bc", (3, 2)),
    ];

    for (chunk_bytes, expected_change_counts) in chunk_change_counts {
        let _ = engine.process_pty_output(chunk_bytes);

        assert_eq!(
            (
                engine.get_title_change_count(),
                engine.get_working_directory_change_count()
            ),
            expected_change_counts,
            "{chunk_bytes:?}"
        );
    }
    assert_eq!(engine.get_terminal_state().get_title(), None);
    assert_eq!(
        engine
            .get_terminal_state()
            .get_current_working_directory()
            .map(|reported_working_directory| reported_working_directory
                .get_working_directory_path()),
        Some(Path::new("/home/user"))
    );
}

/// A CSI cut in half is carried whole: its final byte completes the sequence
/// in the next parser instead of printing as text.
#[test]
fn a_split_csi_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // SGR 31 (red foreground) cut off before its final `m`.
    let _ = engine.process_pty_output(b"\x1b[31");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[31");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"mZ");

    let cell = restored_engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    assert_eq!(cell.get_character(), 'Z');
    assert_eq!(cell.get_style(), red_style);
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// A UTF-8 code point cut in half is carried whole: the next parser prints the
/// glyph, not two replacement characters.
#[test]
fn a_split_code_point_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // 'é' is 0xC3 0xA9; only its first byte arrives.
    let _ = engine.process_pty_output(b"\xc3");

    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\xc3");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\xa9");

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'é');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// A sequence spread over three chunks with no escape byte in the last two is
/// still carried whole.
#[test]
fn a_sequence_spread_over_three_chunks_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;ko");
    let _ = engine.process_pty_output(b"s");
    let _ = engine.process_pty_output(b"hi");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b]0;koshi");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\x07");

    assert_eq!(
        restored_engine.get_terminal_state().get_title(),
        Some("koshi")
    );
}

/// Text after a finished sequence leaves the parser holding nothing: the carry
/// never replays glyphs that already reached the screen.
#[test]
fn text_after_a_finished_sequence_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;koshi\x07ab");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let restored_engine = rebuild_terminal_engine(engine);

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'a');
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 1), 'b');
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 2), ' ');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 2)
    );
}

/// A CSI sequence holding a control character is carried whole. The control
/// character reached the screen as it arrived and does not land twice, and the
/// sequence still swallows its final byte after the swap.
#[test]
fn a_sequence_holding_a_control_character_is_carried_without_repeating_it() {
    let mut engine = build_test_terminal_engine();

    // A line feed between the parameter and the rest of the sequence.
    let _ = engine.process_pty_output(b"\x1b[1\n3");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[1\n3");
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (1, 0)
    );

    let mut restored_engine = rebuild_terminal_engine(engine);

    // The replayed line feed moved no cursor a second time.
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (1, 0)
    );

    // `m` finishes the sequence as SGR 13, which koshi ignores. It reaches the
    // grid as neither a glyph nor a cursor step.
    let _ = restored_engine.process_pty_output(b"m");

    assert_eq!(get_terminal_cell_character(&restored_engine, 1, 0), ' ');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (1, 0)
    );
}

/// A device control string cut in the middle of its body carries its opening
/// bytes: the rest of the body is swallowed after the swap instead of printing
/// as text.
#[test]
fn a_split_device_control_string_carries_its_opening_bytes() {
    let mut engine = build_test_terminal_engine();

    // A sixel image: `ESC P q` opens the string and the payload follows.
    let _ = engine.process_pty_output(b"\x1bPq#0;2;0;0;0");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1bPq");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );

    // More payload, with no escape byte in the chunk: what is carried stays the
    // opening bytes and does not grow with the image.
    let _ = engine.process_pty_output(b"#0~~@@");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1bPq");

    let mut restored_engine = rebuild_terminal_engine(engine);

    // The rest of the payload is swallowed by the resumed string; only the `Z`
    // after the terminator reaches the grid.
    let _ = restored_engine.process_pty_output(b"vv@@~~$\x1b\\Z");

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'Z');
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 1), ' ');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// A device control string closed by the 8-bit terminator `0x9c` — the one
/// ending that reaches no escape byte — leaves the parser on a sequence
/// boundary: nothing is carried, and the text after it prints as text.
#[test]
fn a_device_control_string_closed_by_the_eight_bit_terminator_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    // `ESC P q` opens the string, `0x9c` closes it, and `Z` follows it.
    let _ = engine.process_pty_output(b"\x1bPq#0~~\x9cZ");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"Y");

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 1), 'Y');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 2)
    );
}

/// `CAN` (`0x18`) abandons the sequence it lands in without dispatching it:
/// the parser is back on a sequence boundary and nothing is carried.
#[test]
fn a_cancelled_sequence_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    // SGR 31 abandoned mid-parameter by `CAN`.
    let _ = engine.process_pty_output(b"\x1b[31\x18");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    // The next chunk starts a fresh text run, and none of it is carried.
    let _ = engine.process_pty_output(b"Z");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

/// Chunks that hold no escape byte and open no sequence leave nothing to carry,
/// however many of them arrive: text and control characters both decode whole.
#[test]
fn plain_chunks_on_a_sequence_boundary_carry_nothing() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });

    for _ in 0..64 {
        let _ = engine.process_pty_output(&[b'a'; READ_CHUNK_BYTE_COUNT]);
        assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

        let _ = engine.process_pty_output(&[b'\n'; READ_CHUNK_BYTE_COUNT]);
        assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    }
}

/// A clipboard write (OSC 52) can span many reads. The carry holds the payload
/// up to `MAX_UNDECODED_BYTE_COUNT` and drops it past that: the pane's memory
/// does not grow with the payload.
#[test]
fn a_large_clipboard_write_keeps_bounded_carry_and_terminal_state() {
    const CLIPBOARD_PAYLOAD_BYTE_COUNT: usize = 8 * 1024 * 1024;
    let clipboard_sequence_opening_bytes = b"\x1b]52;c;";
    let clipboard_payload_bytes = vec![b'A'; CLIPBOARD_PAYLOAD_BYTE_COUNT];

    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(clipboard_sequence_opening_bytes);
    assert_eq!(
        engine.get_undecoded_terminal_bytes(),
        clipboard_sequence_opening_bytes
    );

    for (chunk_round, payload_chunk) in clipboard_payload_bytes
        .chunks(READ_CHUNK_BYTE_COUNT)
        .enumerate()
    {
        let _ = engine.process_pty_output(payload_chunk);
        let held_byte_count =
            clipboard_sequence_opening_bytes.len() + (chunk_round + 1) * READ_CHUNK_BYTE_COUNT;
        if held_byte_count <= MAX_UNDECODED_BYTE_COUNT {
            assert_eq!(engine.get_undecoded_terminal_bytes().len(), held_byte_count);
        } else {
            assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
        }
    }

    // The payload passed `MAX_UNDECODED_BYTE_COUNT`: the carry is empty. The
    // real parser still swallows the body: no part of it printed.
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );

    // This engine reads no clipboard copy: the terminator only closes the
    // sequence, and the `Z` after it prints.
    let _ = engine.process_pty_output(b"\x07Z");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

#[test]
#[ignore = "release performance benchmark"]
fn benchmark_chunked_clipboard_write() {
    const CLIPBOARD_PAYLOAD_BYTE_COUNT: usize = 8 * 1024 * 1024;
    const BENCHMARK_RUN_COUNT: usize = 6;

    let clipboard_payload_bytes = vec![b'A'; CLIPBOARD_PAYLOAD_BYTE_COUNT];
    let mut elapsed_durations = Vec::with_capacity(BENCHMARK_RUN_COUNT - 1);

    for benchmark_run_index in 0..BENCHMARK_RUN_COUNT {
        let mut engine = build_test_terminal_engine();
        let benchmark_start_timestamp = Instant::now();
        let _ = engine.process_pty_output(b"\x1b]52;c;");
        for payload_chunk in clipboard_payload_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
            let _ = engine.process_pty_output(payload_chunk);
        }
        let _ = engine.process_pty_output(b"\x07Z");
        if benchmark_run_index != 0 {
            elapsed_durations.push(benchmark_start_timestamp.elapsed());
        }
        assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
        assert_eq!(
            engine.get_terminal_state().get_active_cursor_position(),
            (0, 1)
        );
    }

    elapsed_durations.sort_unstable();
    println!(
        "8 MiB clipboard write: median total {:?}",
        elapsed_durations[elapsed_durations.len() / 2]
    );
}

/// A device control string that ends inside the chunk that opened it leaves the
/// parser on a sequence boundary: nothing is carried.
#[test]
fn a_finished_device_control_string_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1bPq#0~~\x1b\\");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"Z");

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// The parser drops the body of a start of string, a privacy message and an
/// application program command: each one carries its two opening bytes and no
/// more, however long the body runs.
#[test]
fn a_string_whose_body_the_parser_drops_carries_only_its_opening_bytes() {
    for string_opening_bytes in [b"\x1bX", b"\x1b^", b"\x1b_"] {
        let mut engine = build_test_terminal_engine();

        let _ = engine.process_pty_output(string_opening_bytes);
        assert_eq!(engine.get_undecoded_terminal_bytes(), string_opening_bytes);

        // A megabyte of body arriving one read at a time adds nothing.
        let string_body_bytes = vec![b'y'; 1024 * 1024];
        for body_chunk in string_body_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
            let _ = engine.process_pty_output(body_chunk);
            assert_eq!(engine.get_undecoded_terminal_bytes(), string_opening_bytes);
        }

        assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
        assert_eq!(
            engine.get_terminal_state().get_active_cursor_position(),
            (0, 0)
        );

        // The two carried bytes put the next parser back inside the body: the
        // rest of it is swallowed and only the `Z` after `ESC \` prints.
        let mut restored_engine = rebuild_terminal_engine(engine);
        let _ = restored_engine.process_pty_output(b"yyy\x1b\\Z");

        assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"");
        assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'Z');
        assert_eq!(
            restored_engine
                .get_terminal_state()
                .get_active_cursor_position(),
            (0, 1)
        );
    }
}

/// An application program command whose two opening bytes are split across
/// chunks — the kitty graphics protocol's `ESC _` — still carries exactly those
/// two bytes once the second one arrives.
#[test]
fn a_split_application_program_command_opening_carries_both_of_its_bytes() {
    let mut engine = build_test_terminal_engine();

    // The chunk ends on the escape byte alone.
    let _ = engine.process_pty_output(b"\x1b");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b");

    // The `_` and the start of a kitty graphics payload arrive next.
    let _ = engine.process_pty_output(b"_Ga=T,f=100;iVBORw0KGgo");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b_");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"AAANSUhEUg\x1b\\Z");

    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// An operating system command longer than `MAX_UNDECODED_BYTE_COUNT` stops
/// being held. The real parser still swallows the body, and the sequence's end
/// returns the carry to empty.
#[test]
fn an_operating_system_command_past_the_limit_is_not_held() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]52;c;");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b]52;c;");

    // One chunk short of the limit, the whole sequence is still held.
    let under_limit_input_bytes = vec![b'A'; MAX_UNDECODED_BYTE_COUNT - READ_CHUNK_BYTE_COUNT];
    for terminal_input_chunk in under_limit_input_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
        let _ = engine.process_pty_output(terminal_input_chunk);
    }
    assert_eq!(
        engine.get_undecoded_terminal_bytes().len(),
        7 + MAX_UNDECODED_BYTE_COUNT - READ_CHUNK_BYTE_COUNT
    );

    // The next chunk passes the limit, and the carry drops to empty. The
    // buffer is released, not cleared: its capacity drops to 0.
    let _ = engine.process_pty_output(&[b'A'; READ_CHUNK_BYTE_COUNT]);
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(engine.undecoded_terminal_bytes.capacity(), 0);

    // More body changes nothing, and none of it prints.
    let _ = engine.process_pty_output(&[b'A'; READ_CHUNK_BYTE_COUNT]);
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(engine.undecoded_terminal_bytes.capacity(), 0);
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );

    // The terminator closes the sequence and the `Z` after it prints.
    let _ = engine.process_pty_output(b"\x07Z");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

/// The limit guards every sequence kind, not only strings: a control sequence
/// whose parameter digits never end holds no more than `MAX_UNDECODED_BYTE_COUNT` either.
#[test]
fn a_control_sequence_with_endless_parameters_is_not_held_past_the_limit() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[");

    let parameter_digit_bytes = vec![b'1'; 2 * MAX_UNDECODED_BYTE_COUNT];
    for (chunk_round, terminal_input_chunk) in parameter_digit_bytes
        .chunks(READ_CHUNK_BYTE_COUNT)
        .enumerate()
    {
        let _ = engine.process_pty_output(terminal_input_chunk);
        let held_byte_count = 2 + (chunk_round + 1) * READ_CHUNK_BYTE_COUNT;
        let expected_held_byte_count = if held_byte_count <= MAX_UNDECODED_BYTE_COUNT {
            held_byte_count
        } else {
            0
        };
        assert_eq!(
            engine.get_undecoded_terminal_bytes().len(),
            expected_held_byte_count,
            "after chunk {chunk_round}"
        );
    }

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');

    // `m` finishes it as an SGR koshi ignores; the `Z` after it prints.
    let _ = engine.process_pty_output(b"mZ");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

/// A sequence that passed the limit in the engine that was swapped out leaves
/// the next parser on a sequence boundary: the rest of the body prints as
/// text.
#[test]
fn a_swap_inside_a_sequence_past_the_limit_prints_the_rest_of_the_body() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]52;c;");
    let _ = engine.process_pty_output(&vec![b'A'; MAX_UNDECODED_BYTE_COUNT + 1]);
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"BC\x07Z");

    assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'B');
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 1), 'C');
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 2), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 3)
    );
}

/// An empty chunk decodes nothing and leaves what the parser held in place:
/// the first byte of a code point, or an open control sequence.
#[test]
fn an_empty_chunk_keeps_the_held_bytes() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b""), b"");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let _ = engine.process_pty_output(b"a\xc3");
    assert_eq!(engine.process_pty_output(b""), b"");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\xc3");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'a');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );

    let _ = engine.process_pty_output(b"\xa9\x1b[3");
    assert_eq!(engine.process_pty_output(b""), b"");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[3");
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'é');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 2)
    );
}

/// A four-byte code point spread over three chunks is carried whole at each
/// cut and prints once, as one wide glyph, when its last byte arrives.
#[test]
fn a_four_byte_code_point_split_over_three_chunks_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // U+1F600 is 0xF0 0x9F 0x98 0x80.
    let _ = engine.process_pty_output(b"\xf0\x9f");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\xf0\x9f");

    let _ = engine.process_pty_output(b"\x98");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\xf0\x9f\x98");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\x80");

    assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(
        get_terminal_cell_character(&restored_engine, 0, 0),
        '\u{1f600}'
    );
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 2)
    );
}

/// An escape byte that cuts a code point short prints one replacement
/// character for the cut bytes, and the sequence it opens is held.
#[test]
fn a_code_point_cut_short_by_an_escape_prints_a_replacement_and_holds_the_sequence() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\xc3");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\xc3");

    let _ = engine.process_pty_output(b"\x1b[31");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[31");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), '\u{fffd}');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );

    let _ = engine.process_pty_output(b"mZ");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let cell = engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 1)
        .expect("cell in bounds");
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    assert_eq!(cell.get_character(), 'Z');
    assert_eq!(cell.get_style(), red_style);
}

/// `SUB` (`0x1a`) abandons the sequence it lands in, the same as `CAN`: the
/// parser is back on a sequence boundary and nothing is carried.
#[test]
fn a_substituted_sequence_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[31\x1a");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let _ = engine.process_pty_output(b"Z");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

/// A second escape byte restarts the sequence: one escape is held, and the
/// bytes after it form the sequence.
#[test]
fn a_repeated_escape_byte_holds_one_escape() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b");
    let _ = engine.process_pty_output(b"\x1b");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b");

    let _ = engine.process_pty_output(b"[31mZ");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");

    let cell = engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(0, 0)
        .expect("cell in bounds");
    let mut red_style = Style::default();
    red_style.set_foreground_color(Color::Indexed(1));
    assert_eq!(cell.get_character(), 'Z');
    assert_eq!(cell.get_style(), red_style);
}

/// An operating system command closed by `ESC \` ends on a sequence boundary:
/// the terminator's escape byte restarts the scan and its `\` finishes it.
#[test]
fn an_operating_system_command_closed_by_the_string_terminator_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;hi");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b]0;hi");

    let _ = engine.process_pty_output(b"\x1b\\Z");

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(engine.get_terminal_state().get_title(), Some("hi"));
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );
}

/// A chunk that ends on the escape byte of `ESC \` has already dispatched the
/// operating system command; only that escape byte is carried, and the `\`
/// after the swap closes it without printing.
#[test]
fn an_operating_system_command_cut_at_its_terminator_carries_only_the_escape() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;hi\x1b");
    assert_eq!(engine.get_terminal_state().get_title(), Some("hi"));
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\\Z");

    assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(restored_engine.get_terminal_state().get_title(), Some("hi"));
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// A device control string cut before its final byte carries `ESC P`; once
/// the final byte arrives, the carry is the three opening bytes and no more.
#[test]
fn a_device_control_string_cut_before_its_final_byte_carries_its_opening() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1bP");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1bP");

    let _ = engine.process_pty_output(b"q#0~~");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1bPq");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"@@\x1b\\Z");

    assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 1)
    );
}

/// A resize leaves the held bytes in place.
#[test]
fn a_resize_keeps_the_held_bytes() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[3");
    engine.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });

    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[3");
}

/// A control sequence the parser ignores dispatches nothing: the scan holds
/// it, and every control byte after it, until the next escape byte or printed
/// character. Replaying it leaves the parser on a sequence boundary.
#[test]
fn an_ignored_control_sequence_is_held_until_the_next_escape_or_print() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[3?m");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[3?m");

    let _ = engine.process_pty_output(b"\n");
    assert_eq!(engine.get_undecoded_terminal_bytes(), b"\x1b[3?m\n");
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (1, 0)
    );

    let mut restored_engine = rebuild_terminal_engine(engine);
    assert_eq!(
        restored_engine.get_undecoded_terminal_bytes(),
        b"\x1b[3?m\n"
    );
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (1, 0)
    );

    let _ = restored_engine.process_pty_output(b"Z");
    assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&restored_engine, 1, 0), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (1, 1)
    );

    let _ = restored_engine.process_pty_output(b"\x1b[3?m\x1b[3");
    assert_eq!(restored_engine.get_undecoded_terminal_bytes(), b"\x1b[3");
}

/// The engine holds its OSC buffer inline: its own size bounds how much one
/// unterminated sequence can accumulate.
#[test]
fn the_engine_carries_a_bounded_osc_buffer() {
    // One engine exists per pane. The bound is an absolute figure,
    // independent of `OSC_BUFFER_BYTE_CAPACITY`.
    const PER_PANE_BYTE_COUNT_LIMIT: usize = 64 * 1024;
    let terminal_engine_size_bytes = std::mem::size_of::<TerminalEngine>();
    assert!(
        terminal_engine_size_bytes < PER_PANE_BYTE_COUNT_LIMIT,
        "TerminalEngine is {terminal_engine_size_bytes} bytes, over the {PER_PANE_BYTE_COUNT_LIMIT} byte per-pane limit"
    );
}

#[test]
fn an_unterminated_osc_leaves_the_parser_usable() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        row_count: 24,
        column_count: 80,
    });
    let _ = engine.process_pty_output(b"\x1b]0;");
    let terminal_input_chunk = vec![b'A'; 1 << 20];
    for _ in 0..64 {
        let _ = engine.process_pty_output(&terminal_input_chunk);
    }
    // The sequence is still open, so no title has been set.
    assert_eq!(engine.get_terminal_state().get_title(), None);

    // Terminating it yields a title cut to the reported-text limit, and the
    // parser takes the next sequence normally.
    let _ = engine.process_pty_output(b"\x07");
    assert_eq!(
        engine.get_terminal_state().get_title().map(str::len),
        Some(koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT)
    );
    let _ = engine.process_pty_output(b"\x1b]2;ok\x07");
    assert_eq!(engine.get_terminal_state().get_title(), Some("ok"));
}

#[test]
fn a_title_split_across_chunks_is_still_bounded() {
    // vte holds the open sequence between calls: the cap applies to the
    // assembled payload, not to one chunk.
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        row_count: 24,
        column_count: 80,
    });
    let _ = engine.process_pty_output(b"\x1b]2;");
    for _ in 0..100 {
        let _ = engine.process_pty_output(&[b'A'; 100]);
    }
    let _ = engine.process_pty_output(b"\x07");
    assert_eq!(
        engine.get_terminal_state().get_title().map(str::len),
        Some(koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT)
    );
}

#[test]
fn a_refused_character_split_across_chunks_is_still_removed() {
    // A multi-byte character delivered one byte at a time is filtered as the
    // character it forms.
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        row_count: 24,
        column_count: 80,
    });
    let _ = engine.process_pty_output(b"\x1b]2;a");
    for terminal_input_byte in "\u{202e}".as_bytes() {
        let _ = engine.process_pty_output(&[*terminal_input_byte]);
    }
    let _ = engine.process_pty_output(b"b\x07");
    assert_eq!(engine.get_terminal_state().get_title(), Some("ab"));
}

#[test]
fn an_osc_7_uri_split_across_chunks_is_still_refused_past_the_limit() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        row_count: 24,
        column_count: 80,
    });
    let _ = engine.process_pty_output(b"\x1b]7;file://localhost/tmp\x07");
    let _ = engine.process_pty_output(b"\x1b]7;file://localhost/");
    for _ in 0..100 {
        let _ = engine.process_pty_output(&[b'a'; 100]);
    }
    let _ = engine.process_pty_output(b"\x07");
    assert_eq!(
        engine
            .get_terminal_state()
            .get_current_working_directory()
            .map(|reported_working_directory| reported_working_directory
                .get_working_directory_path()
                .to_path_buf()),
        Some(std::path::PathBuf::from("/tmp")),
        "an over-long URI replaced the working directory"
    );
}

#[test]
fn a_title_survives_a_reset_and_can_be_set_again() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        row_count: 24,
        column_count: 80,
    });
    let _ = engine.process_pty_output(b"\x1b]2;first\x07");
    assert_eq!(engine.get_terminal_state().get_title(), Some("first"));
    let _ = engine.process_pty_output(b"\x1bc");
    assert_eq!(engine.get_terminal_state().get_title(), None);
    let _ = engine.process_pty_output("\x1b]2;sec\u{7f}ond\x07".as_bytes());
    assert_eq!(engine.get_terminal_state().get_title(), Some("second"));
}

#[test]
fn a_title_past_the_parser_capacity_is_identical_to_one_within_it() {
    // The parser stops taking bytes at `OSC_BUFFER_BYTE_CAPACITY`: a longer
    // sequence reaches `osc_dispatch` short. The title cut to
    // `MAX_REPORTED_TEXT_BYTE_COUNT` is below the capacity: both lengths yield
    // the same bytes.
    let title_within_parser_capacity = {
        let mut engine = TerminalEngine::from_pty_size(PtySize {
            row_count: 24,
            column_count: 80,
        });
        let mut title_input_bytes = Vec::from(&b"\x1b]2;"[..]);
        title_input_bytes.extend(std::iter::repeat_n(b'A', 4_000));
        title_input_bytes.push(0x07);
        let _ = engine.process_pty_output(&title_input_bytes);
        engine.get_terminal_state().get_title().map(str::to_owned)
    };
    let title_past_parser_capacity = {
        let mut engine = TerminalEngine::from_pty_size(PtySize {
            row_count: 24,
            column_count: 80,
        });
        let mut title_input_bytes = Vec::from(&b"\x1b]2;"[..]);
        title_input_bytes.extend(std::iter::repeat_n(b'A', 200_000));
        title_input_bytes.push(0x07);
        let _ = engine.process_pty_output(&title_input_bytes);
        engine.get_terminal_state().get_title().map(str::to_owned)
    };
    assert_eq!(title_within_parser_capacity, title_past_parser_capacity);
    assert_eq!(
        title_within_parser_capacity.map(|reported_text| reported_text.len()),
        Some(koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT)
    );
}

#[test]
fn a_sequence_past_the_parser_capacity_does_not_disturb_the_next_one() {
    // Bytes are dropped from the oversized sequence alone. The parser still
    // terminates it and reads what follows normally.
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        row_count: 24,
        column_count: 80,
    });
    let mut title_input_bytes = Vec::from(&b"\x1b]2;"[..]);
    title_input_bytes.extend(std::iter::repeat_n(b'A', 200_000));
    title_input_bytes.push(0x07);
    let _ = engine.process_pty_output(&title_input_bytes);

    let _ = engine.process_pty_output(b"\x1b]7;file://localhost/tmp\x07");
    assert_eq!(
        engine
            .get_terminal_state()
            .get_current_working_directory()
            .map(|reported_working_directory| reported_working_directory
                .get_working_directory_path()
                .to_path_buf()),
        Some(std::path::PathBuf::from("/tmp"))
    );
    let _ = engine.process_pty_output(b"\x1b]2;after\x07");
    assert_eq!(engine.get_terminal_state().get_title(), Some("after"));
    // A printable glyph still lands on the grid.
    let _ = engine.process_pty_output(b"z");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'z');
}

#[test]
fn a_repeated_transfer_with_the_same_pixels_shares_one_image() {
    for protocol in [
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Iterm2,
    ] {
        let mut engine = build_graphics_terminal_engine();
        let first_image_transfer_bytes = match protocol {
            GraphicsProtocol::Kitty => {
                b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec()
            }
            _ => build_visible_graphics_control(protocol),
        };
        let second_image_transfer_bytes = match protocol {
            GraphicsProtocol::Kitty => {
                b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\".to_vec()
            }
            _ => build_visible_graphics_control(protocol),
        };
        let _ = engine.process_pty_output(&first_image_transfer_bytes);
        let _ = engine.process_pty_output(b"\r\n");
        let _ = engine.process_pty_output(&second_image_transfer_bytes);
        let image_records: Vec<_> = engine
            .take_graphics_events()
            .into_iter()
            .map(|graphics_event_result| graphics_event_result.expect("both transfers succeed"))
            .collect();

        assert_eq!(image_records.len(), 2, "{protocol:?}");
        assert!(
            std::sync::Arc::ptr_eq(&image_records[0].image, &image_records[1].image),
            "{protocol:?} transfers with equal pixels share one image"
        );
        let image_placements = engine.get_terminal_state().list_image_placements();
        assert_eq!(image_placements.len(), 2, "{protocol:?}");
        assert!(
            std::sync::Arc::ptr_eq(
                &image_placements[0].get_image_record().image,
                &image_placements[1].get_image_record().image
            ),
            "{protocol:?} placements share one image"
        );
    }
}

#[test]
fn a_repeated_transfer_with_different_pixels_keeps_its_own_image() {
    let mut engine = build_graphics_terminal_engine();
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;AP8A/w==\x1b\\");
    let image_records: Vec<_> = engine
        .take_graphics_events()
        .into_iter()
        .map(|graphics_event_result| graphics_event_result.expect("both transfers succeed"))
        .collect();

    assert_eq!(image_records.len(), 2);
    assert!(!std::sync::Arc::ptr_eq(
        &image_records[0].image,
        &image_records[1].image
    ));
    assert_eq!(image_records[0].image.rgba_bytes, vec![255, 0, 0, 255]);
    assert_eq!(image_records[1].image.rgba_bytes, vec![0, 255, 0, 255]);
}

#[test]
fn a_transfer_with_the_same_bytes_but_swapped_dimensions_keeps_its_own_image() {
    let mut engine = build_graphics_terminal_engine();
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=1,f=32,s=2,v=1,c=1,r=1,C=1;/wAA/wD/AP8=\x1b\\");
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=2,f=32,s=1,v=2,c=1,r=1,C=1;/wAA/wD/AP8=\x1b\\");
    let image_records: Vec<_> = engine
        .take_graphics_events()
        .into_iter()
        .map(|graphics_event_result| graphics_event_result.expect("both transfers succeed"))
        .collect();

    assert_eq!(image_records.len(), 2);
    assert_eq!(
        (
            image_records[0].image.pixel_width,
            image_records[0].image.pixel_height
        ),
        (2, 1)
    );
    assert_eq!(
        (
            image_records[1].image.pixel_width,
            image_records[1].image.pixel_height
        ),
        (1, 2)
    );
    assert_eq!(
        image_records[0].image.rgba_bytes,
        image_records[1].image.rgba_bytes
    );
    assert!(!std::sync::Arc::ptr_eq(
        &image_records[0].image,
        &image_records[1].image
    ));
}

#[test]
fn a_kitty_upload_without_a_placement_shares_its_pixels_with_a_subsequent_transfer() {
    let mut engine = build_graphics_terminal_engine();
    let _ = engine.process_pty_output(b"\x1b_Ga=t,i=1,f=32,s=1,v=1;/wAA/w==\x1b\\");
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let image_records: Vec<_> = engine
        .take_graphics_events()
        .into_iter()
        .map(|graphics_event_result| graphics_event_result.expect("both transfers succeed"))
        .collect();

    assert_eq!(image_records.len(), 2);
    assert_eq!(image_records[0].action, ImageAction::Transmit);
    assert!(std::sync::Arc::ptr_eq(
        &image_records[0].image,
        &image_records[1].image
    ));
}

#[test]
fn an_image_scrolled_into_history_shares_its_pixels_with_a_subsequent_transfer() {
    let mut engine = build_graphics_terminal_engine();
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=1,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let _ = engine.process_pty_output(b"\r\n\r\n\r\n\r\n\r\n\r\n");
    assert_eq!(
        engine.get_terminal_state().list_image_placements().len(),
        0,
        "the first image left the visible screen"
    );
    let _ = engine.process_pty_output(b"\x1b_Ga=T,i=2,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let image_records: Vec<_> = engine
        .take_graphics_events()
        .into_iter()
        .map(|graphics_event_result| graphics_event_result.expect("both transfers succeed"))
        .collect();

    assert_eq!(image_records.len(), 2);
    assert!(std::sync::Arc::ptr_eq(
        &image_records[0].image,
        &image_records[1].image
    ));
}

/// A copy of `copied_bytes` for `clipboard_targets`.
pub(crate) fn build_clipboard_copy(clipboard_targets: &str, copied_bytes: &[u8]) -> ClipboardCopy {
    ClipboardCopy {
        clipboard_targets: clipboard_targets.to_string(),
        copied_bytes: copied_bytes.to_vec(),
    }
}

#[test]
fn an_engine_that_does_not_read_clipboard_copies_hands_back_none() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]52;c;aGk=\x07");

    assert_eq!(engine.take_clipboard_copies(), Vec::new());
}

#[test]
fn an_engine_reading_clipboard_copies_hands_back_each_copy_once() {
    let mut engine = build_test_terminal_engine();
    engine.start_reading_clipboard_copies(1024);

    let _ = engine.process_pty_output(b"ab\x1b]52;c;aGk=\x07cd\x1b]52;p;aGVsbG8=\x1b\\");

    assert_eq!(
        engine.take_clipboard_copies(),
        vec![
            build_clipboard_copy("c", b"hi"),
            build_clipboard_copy("p", b"hello"),
        ]
    );
    assert_eq!(engine.take_clipboard_copies(), Vec::new());
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'a');
    assert_eq!(get_terminal_cell_character(&engine, 0, 2), 'c');
}

#[test]
fn a_clipboard_copy_cut_before_the_reader_started_completes_in_the_next_chunk() {
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output(b"\x1b]52;c;aG");

    engine.start_reading_clipboard_copies(1024);
    let _ = engine.process_pty_output(b"k=\x07");

    assert_eq!(
        engine.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn a_carried_engine_completes_a_clipboard_copy_the_swap_cut_in_half() {
    let mut engine = TerminalEngine::from_terminal_state(
        TerminalState::from_pty_size(PtySize {
            column_count: 8,
            row_count: 3,
        }),
        b"\x1b]52;c;aG",
    );

    engine.start_reading_clipboard_copies(1024);
    let _ = engine.process_pty_output(b"k=\x07");

    assert_eq!(
        engine.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn a_clipboard_copy_inside_synchronized_output_arrives_when_the_update_ends() {
    let mut engine = build_test_terminal_engine();
    engine.start_reading_clipboard_copies(1024);

    let _ = engine.process_pty_output(b"\x1b[?2026h\x1b]52;c;aGk=\x07");
    assert_eq!(engine.take_clipboard_copies(), Vec::new());

    let _ = engine.process_pty_output(b"\x1b[?2026l");
    assert_eq!(
        engine.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn an_eight_bit_clipboard_copy_reads_as_its_seven_bit_form() {
    let mut engine = build_test_terminal_engine();
    engine.start_reading_clipboard_copies(1024);

    let _ = engine.process_pty_output(b"\x9d52;c;aGk=\x9c");

    assert_eq!(
        engine.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn the_first_three_plain_workload_lines_parse_to_three_hard_rows_of_their_text() {
    let expected_line_texts = [
        concat!(
            "G!F 2/bc=Zo)/+,:p{6fv@6VU?4p]GBp`bPCg=T\\znhC5)TT!swHDfYJ!Gb(&Vo \\AqR{&^b s^b(Yjux-rC$?1Q5[P?G^v",
            "{aS$ftkPZsX1Uy0UQQNnn+/76qFFoM*U>]R'`Tc{w-)t1_f]lr^Awn$b|Zf$y_<JkQ]T tRCb;9nEtrPW5AKBaHXg VYpux"
        ),
        concat!(
            "If1$B8f,<|5R3ezR(ws=]V5h_]g;Q,K\"EX<ON) uHqPyVvM\"dM8J .o$;w! ?tg9~&B07^{m}I5m!A_5>G@4uwQ+E<#m$ek",
            ":rG).+[+<IUtUq\\'wC|Bpy0.Xsy&'{$@1hXl{\\2)^CiPBY[7%kbUhbyAGSFCXSCH,]Yh;l/{7e2=\\Srl2*Wqd[\"F1ELARP~"
        ),
        concat!(
            "b.q> R@I)a>(I+enh#I*rJ`!LafHN1Cm{:16_mPj!qn_`KomM%X~X;EYP3ff7cA;BLvZt@+x{uR-,.7#;md}7p#1HUWXX%X",
            "'5<SG,zOWm1'Dx_x!VOT[4)[QLPbS)d~h//w~sPYK#Duc1!];<hID|Vyu|IXFjlE(kuPd|z216_7lB1-q3qT:8/0G e?0R{"
        ),
    ];
    let workload_bytes = build_workload_bytes(OutputWorkload::Plain, 576, DEFAULT_WORKLOAD_SEED);
    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 200,
        row_count: 4,
    });

    let _ = terminal_engine.process_pty_output(&workload_bytes);

    let terminal_state = terminal_engine.get_terminal_state();
    let active_grid = terminal_state.get_active_grid();
    assert_eq!(workload_bytes.len(), 576);
    for (row_index, expected_line_text) in expected_line_texts.iter().enumerate() {
        let mut expected_row_cells: Vec<Cell> = expected_line_text
            .chars()
            .map(|line_character| Cell::from_character(line_character, 1, Style::default()))
            .collect();
        expected_row_cells.resize(200, Cell::build_blank());
        let row_index = row_index as u16;
        assert_eq!(
            active_grid.get_row_cells(row_index),
            Some(expected_row_cells.as_slice()),
            "row {row_index}"
        );
        assert_eq!(
            active_grid.get_row_end(row_index),
            RowEnd::Hard,
            "row {row_index}"
        );
    }
    assert_eq!(
        active_grid.get_row_cells(3),
        Some(vec![Cell::build_blank(); 200].as_slice())
    );
    assert_eq!(terminal_state.get_active_cursor_position(), (3, 0));
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 0);
}

/// The terminal state a parser leaves when it hands each printed character
/// to `TerminalState::print` on its own, with no engine around the state.
fn print_one_character_at_a_time(pty_size: PtySize, terminal_bytes: &[u8]) -> TerminalState {
    let mut terminal_state = TerminalState::from_pty_size(pty_size);
    process_terminal_bytes(&mut terminal_state, terminal_bytes);
    terminal_state
}

/// Assert that a new engine of `pty_size` fed `terminal_bytes` in one chunk
/// holds the terminal state [`print_one_character_at_a_time`] leaves.
fn assert_printed_runs_match_single_character_prints(pty_size: PtySize, terminal_bytes: &[u8]) {
    let mut terminal_engine = TerminalEngine::from_pty_size(pty_size);
    let _ = terminal_engine.process_pty_output(terminal_bytes);
    assert!(
        terminal_engine.get_terminal_state()
            == &print_one_character_at_a_time(pty_size, terminal_bytes),
        "the engine's state differs for {:?}",
        String::from_utf8_lossy(terminal_bytes)
    );
}

#[test]
fn printed_runs_leave_the_state_single_character_prints_leave() {
    let small_pty_size = PtySize {
        column_count: 10,
        row_count: 4,
    };
    let scrolled_lines = "abcdefghij\r\n".repeat(12) + "tail";
    let printed_streams: [&[u8]; 19] = [
        scrolled_lines.as_bytes(),
        b"abcdefghijklmnopqrstuvwxy",
        b"\x1b[?7labcdefghijklmno",
        b"abcdefghij",
        b"abcdefghijk",
        "中文字\ra\x1b[1;4Hbc".as_bytes(),
        "abcdefghi中".as_bytes(),
        b"\x1b[?69h\x1b[3;7s\x1b[1;3Habcdefghijkl\x1b[1;9Hxyz",
        b"\x1b[?69h\x1b[5;5s\x1b[1;5Habc",
        b"\x1b(0lqqk\x1b(Blqk\x1b)0\x0elqk\x0fabc",
        "abc\u{301}de\u{301}\u{302}f".as_bytes(),
        "\u{600}ab\u{1f44d}\u{1f3fd}xa\u{200d}b".as_bytes(),
        b"ab\x1b[31mcd\x1b[44mef\x1b[0m\x1b[K\r\n",
        b"a\tb\x08c\r\nde",
        b"\x1b[2;3r\x1b[3;1Hline1\r\nline2\r\nline3\r\n",
        b"abcdef\x1b[1;2H\x1b[4hXYZ\x1b[4lQ",
        b"\x1b[?1049habc\x1b[2;2Hde\x1b[?1049lfg",
        "e\u{301}\u{301}ab".as_bytes(),
        b"abcdefghijklm\x1b[1;5H123456",
    ];
    for printed_stream in printed_streams {
        assert_printed_runs_match_single_character_prints(small_pty_size, printed_stream);
    }
}

#[test]
fn each_output_workload_leaves_the_state_single_character_prints_leave() {
    let workload_pty_size = PtySize {
        column_count: 200,
        row_count: 50,
    };
    for output_workload in OutputWorkload::ALL {
        let workload_bytes =
            build_workload_bytes(output_workload, 64 * 1024, DEFAULT_WORKLOAD_SEED);
        assert_printed_runs_match_single_character_prints(workload_pty_size, &workload_bytes);
    }
}

/// Text with a control sequence, two- to four-byte UTF-8 code points and a
/// visible kitty graphics transfer between its printed runs.
fn build_mixed_terminal_bytes() -> Vec<u8> {
    let mut mixed_terminal_bytes = "ab\x1b[1;31mcd\u{e9}\u{4e2d}\u{1f600}\x1b[0m\r\n"
        .as_bytes()
        .to_vec();
    mixed_terminal_bytes
        .extend_from_slice(&build_visible_graphics_control(GraphicsProtocol::Kitty));
    mixed_terminal_bytes.extend_from_slice("xy\u{301}z\x1b]2;title\x07end".as_bytes());
    mixed_terminal_bytes
}

#[test]
fn a_chunk_split_at_every_byte_offset_leaves_the_state_of_the_whole_chunk() {
    let pty_size = PtySize {
        column_count: 10,
        row_count: 4,
    };
    let mixed_terminal_bytes = build_mixed_terminal_bytes();
    let mut whole_chunk_engine = TerminalEngine::from_pty_size(pty_size);
    let _ = whole_chunk_engine.process_pty_output(&mixed_terminal_bytes);
    assert_eq!(whole_chunk_engine.list_graphics_events().count(), 1);
    assert_eq!(
        whole_chunk_engine.get_terminal_state().get_title(),
        Some("title")
    );

    for split_byte_index in 0..=mixed_terminal_bytes.len() {
        let mut split_chunk_engine = TerminalEngine::from_pty_size(pty_size);
        let _ = split_chunk_engine.process_pty_output(&mixed_terminal_bytes[..split_byte_index]);
        let _ = split_chunk_engine.process_pty_output(&mixed_terminal_bytes[split_byte_index..]);

        assert!(
            split_chunk_engine.get_terminal_state() == whole_chunk_engine.get_terminal_state(),
            "split at byte {split_byte_index}"
        );
        assert_eq!(
            split_chunk_engine.take_graphics_events(),
            whole_chunk_engine
                .list_graphics_events()
                .cloned()
                .collect::<Vec<_>>(),
            "split at byte {split_byte_index}"
        );
    }
}

#[test]
fn a_chunk_ending_inside_a_code_point_joins_the_next_chunk() {
    let pty_size = PtySize {
        column_count: 10,
        row_count: 2,
    };
    let mut terminal_engine = TerminalEngine::from_pty_size(pty_size);

    let _ = terminal_engine.process_pty_output(b"ab\xe2\x82");
    let _ = terminal_engine.process_pty_output(b"\xaccd");

    assert_eq!(
        get_terminal_cell_character(&terminal_engine, 0, 2),
        '\u{20ac}'
    );
    assert_eq!(get_terminal_cell_character(&terminal_engine, 0, 3), 'c');
    assert_eq!(get_terminal_cell_character(&terminal_engine, 0, 4), 'd');
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (0, 5)
    );
}

#[test]
fn a_chunk_with_no_escape_byte_inside_an_open_kitty_transfer_still_reaches_the_graphics_scan() {
    let pty_size = PtySize {
        column_count: 10,
        row_count: 2,
    };
    let kitty_transfer_bytes = build_visible_graphics_control(GraphicsProtocol::Kitty);
    let payload_start_index = kitty_transfer_bytes
        .iter()
        .position(|transfer_byte| *transfer_byte == b';')
        .expect("the transfer has a payload")
        + 1;
    let terminator_start_index = kitty_transfer_bytes.len() - 2;
    let mut whole_transfer_engine = TerminalEngine::from_pty_size(pty_size);
    let _ = whole_transfer_engine.process_pty_output(&kitty_transfer_bytes);

    let mut split_transfer_engine = TerminalEngine::from_pty_size(pty_size);
    let _ = split_transfer_engine.process_pty_output(&kitty_transfer_bytes[..payload_start_index]);
    let payload_bytes = &kitty_transfer_bytes[payload_start_index..terminator_start_index];
    assert!(!payload_bytes.contains(&ESCAPE_BYTE));
    let _ = split_transfer_engine.process_pty_output(payload_bytes);
    let _ =
        split_transfer_engine.process_pty_output(&kitty_transfer_bytes[terminator_start_index..]);

    assert_eq!(split_transfer_engine.list_graphics_events().count(), 1);
    assert_eq!(
        split_transfer_engine.take_graphics_events(),
        whole_transfer_engine.take_graphics_events()
    );
    assert!(
        split_transfer_engine.get_terminal_state() == whole_transfer_engine.get_terminal_state()
    );
    assert_eq!(
        split_transfer_engine
            .get_terminal_state()
            .get_active_grid()
            .get_row_cells(0),
        Some(vec![Cell::build_blank(); 10].as_slice())
    );
}

#[test]
fn every_control_sequence_final_byte_lets_a_following_eight_bit_csi_apply() {
    let pty_size = PtySize {
        column_count: 10,
        row_count: 3,
    };
    for final_byte in 0x40..=0x7e_u8 {
        let mut eight_bit_terminal_bytes = vec![ESCAPE_BYTE, b'[', final_byte, b'1'];
        let mut seven_bit_terminal_bytes = eight_bit_terminal_bytes.clone();
        eight_bit_terminal_bytes.extend_from_slice(b"\x9b2Jy");
        seven_bit_terminal_bytes.extend_from_slice(b"\x1b[2Jy");
        let mut eight_bit_engine = TerminalEngine::from_pty_size(pty_size);
        let mut seven_bit_engine = TerminalEngine::from_pty_size(pty_size);

        assert_eq!(
            eight_bit_engine.process_pty_output(&eight_bit_terminal_bytes),
            seven_bit_engine.process_pty_output(&seven_bit_terminal_bytes),
            "final byte {final_byte:#04x}"
        );
        assert!(
            eight_bit_engine.get_terminal_state() == seven_bit_engine.get_terminal_state(),
            "final byte {final_byte:#04x}"
        );
    }
}

/// The program facts `pty_output_bytes` gave `engine`, empty when no byte
/// reached the parsers.
fn take_chunk_program_facts(
    engine: &mut TerminalEngine,
    pty_output_bytes: &[u8],
) -> Vec<ProgramFact> {
    match engine.process_pty_output_at(pty_output_bytes, Instant::now()) {
        Some(terminal_advance) => terminal_advance
            .program_facts
            .into_iter()
            .map(|timed_program_fact| timed_program_fact.program_fact)
            .collect(),
        None => Vec::new(),
    }
}

/// Each of `program_facts`, read at `read_instant`.
fn build_timed_program_facts(
    program_facts: Vec<ProgramFact>,
    read_instant: Instant,
) -> Vec<TimedProgramFact> {
    program_facts
        .into_iter()
        .map(|program_fact| TimedProgramFact {
            program_fact,
            read_instant,
        })
        .collect()
}

/// The fact of `ESC ] 7501 ; state=<state> ST`: a report for the root record
/// that names `state` and no other key.
fn build_root_program_status_report(state: ProgramStatusState) -> ProgramFact {
    ProgramFact::ProgramStatus(ProgramStatusFact::Reported {
        record_id: String::new(),
        state,
        blocked_kind: None,
        progress_percent: None,
        app_name: None,
        title: None,
        message: None,
    })
}

/// Each sequence that gives program facts, byte-exact, with the facts it gives
/// a fresh engine.
fn build_program_fact_sequence_cases() -> Vec<(Vec<u8>, Vec<ProgramFact>)> {
    let hello_notification = ProgramFact::Notify {
        title: None,
        body: "hello".to_string(),
        notification_id: None,
    };
    let build_notification = ProgramFact::Notify {
        title: Some("Build".to_string()),
        body: "done".to_string(),
        notification_id: None,
    };
    let kitty_notification = ProgramFact::Notify {
        title: Some("Hi".to_string()),
        body: String::new(),
        notification_id: Some("1".to_string()),
    };
    let kitty_withdrawal = ProgramFact::NotificationWithdrawn {
        notification_id: "1".to_string(),
    };
    let command_facts = vec![
        ProgramFact::CommandStarted,
        ProgramFact::CommandFinished {
            exit_code: Some(101),
        },
    ];
    let terraform_report = ProgramFact::ProgramStatus(ProgramStatusFact::Reported {
        record_id: String::new(),
        state: ProgramStatusState::Blocked,
        blocked_kind: Some(BlockedKind::Permission),
        progress_percent: None,
        app_name: Some("terraform".to_string()),
        title: None,
        message: Some("Apply?".to_string()),
    });
    let deploy_clear = ProgramFact::ProgramStatus(ProgramStatusFact::Cleared {
        record_id: "deploy".to_string(),
    });
    let working_report_then_prompt = vec![
        build_root_program_status_report(ProgramStatusState::Working),
        ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted),
    ];
    vec![
        (b"\x07".to_vec(), vec![ProgramFact::Bell]),
        (
            b"\x1b]9;hello\x1b\\".to_vec(),
            vec![hello_notification.clone()],
        ),
        (b"\x1b]9;hello\x07".to_vec(), vec![hello_notification]),
        (
            b"\x1b]777;notify;Build;done\x1b\\".to_vec(),
            vec![build_notification.clone()],
        ),
        (
            b"\x1b]777;notify;Build;done\x07".to_vec(),
            vec![build_notification],
        ),
        (
            b"\x1b]99;i=1;Hi\x1b\\".to_vec(),
            vec![kitty_notification.clone()],
        ),
        (b"\x1b]99;i=1;Hi\x07".to_vec(), vec![kitty_notification]),
        (
            b"\x1b]99;i=1:p=close;\x1b\\".to_vec(),
            vec![kitty_withdrawal.clone()],
        ),
        (b"\x1b]99;i=1:p=close;\x07".to_vec(), vec![kitty_withdrawal]),
        (
            b"\x1b]133;C\x1b\\\x1b]133;D;101\x1b\\".to_vec(),
            command_facts.clone(),
        ),
        (b"\x1b]133;C\x07\x1b]133;D;101\x07".to_vec(), command_facts),
        (
            b"\x1b]7501;state=blocked:kind=permission:app=terraform:msg=QXBwbHk/\x1b\\".to_vec(),
            vec![terraform_report.clone()],
        ),
        (
            b"\x1b]7501;state=blocked:kind=permission:app=terraform:msg=QXBwbHk/\x07".to_vec(),
            vec![terraform_report],
        ),
        (
            b"\x1b]7501;state=clear:id=deploy\x1b\\".to_vec(),
            vec![deploy_clear.clone()],
        ),
        (
            b"\x1b]7501;state=clear:id=deploy\x07".to_vec(),
            vec![deploy_clear],
        ),
        (
            b"\x1b]7501;state=working\x1b\\\x1b]133;A\x1b\\".to_vec(),
            working_report_then_prompt.clone(),
        ),
        (
            b"\x1b]7501;state=working\x07\x1b]133;A\x07".to_vec(),
            working_report_then_prompt,
        ),
        (
            b"\x1bc".to_vec(),
            vec![ProgramFact::ProgramStatus(ProgramStatusFact::TerminalReset)],
        ),
    ]
}

#[test]
fn each_program_fact_sequence_gives_its_facts_with_either_terminator() {
    for (sequence_bytes, expected_program_facts) in build_program_fact_sequence_cases() {
        let mut engine = build_test_terminal_engine();
        assert_eq!(
            take_chunk_program_facts(&mut engine, &sequence_bytes),
            expected_program_facts,
            "sequence {sequence_bytes:?}"
        );
    }
}

#[test]
fn a_program_fact_sequence_split_at_any_byte_gives_its_facts_once() {
    for (sequence_bytes, expected_program_facts) in build_program_fact_sequence_cases() {
        for split_byte_index in 1..sequence_bytes.len() {
            let mut engine = build_test_terminal_engine();
            let mut program_facts =
                take_chunk_program_facts(&mut engine, &sequence_bytes[..split_byte_index]);
            program_facts.extend(take_chunk_program_facts(
                &mut engine,
                &sequence_bytes[split_byte_index..],
            ));
            assert_eq!(
                program_facts, expected_program_facts,
                "sequence {sequence_bytes:?} split at {split_byte_index}"
            );
        }
    }
}

#[test]
fn a_bel_that_ends_or_sits_inside_a_string_is_not_a_bell() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b]0;title\x07"),
        Vec::new()
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b]7;file://host/home/user\x07"),
        Vec::new()
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1bPzz\x07zz\x1b\\"),
        Vec::new()
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x9d9;hi\x07"),
        vec![ProgramFact::Notify {
            title: None,
            body: "hi".to_string(),
            notification_id: None,
        }]
    );
}

#[test]
fn bells_in_one_read_fold_into_one_bell_until_another_fact() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(
        take_chunk_program_facts(&mut engine, &[0x07; 500]),
        vec![ProgramFact::Bell]
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x07\x1b]9;x\x07\x07\x07"),
        vec![
            ProgramFact::Bell,
            ProgramFact::Notify {
                title: None,
                body: "x".to_string(),
                notification_id: None,
            },
            ProgramFact::Bell,
        ]
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x07"),
        vec![ProgramFact::Bell]
    );
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x07"),
        vec![ProgramFact::Bell]
    );
}

#[test]
fn a_long_osc777_body_keeps_its_first_512_bytes() {
    let mut engine = build_test_terminal_engine();
    let mut sequence_bytes = "\x1b]777;notify;Build\u{202E}gpj;".as_bytes().to_vec();
    sequence_bytes.extend(vec![b'b'; 5000]);
    sequence_bytes.push(0x07);
    assert_eq!(
        take_chunk_program_facts(&mut engine, &sequence_bytes),
        vec![ProgramFact::Notify {
            title: Some("Buildgpj".to_string()),
            body: "b".repeat(512),
            notification_id: None,
        }]
    );
}

#[test]
fn an_osc9_body_keeps_every_field_past_the_sixteenth_separator() {
    let mut engine = build_test_terminal_engine();
    let field_letters: Vec<String> = ('a'..='t').map(String::from).collect();
    let sequence_bytes = format!("\x1b]9;{}\x07", field_letters.join(";"));
    assert_eq!(
        take_chunk_program_facts(&mut engine, sequence_bytes.as_bytes()),
        vec![ProgramFact::Notify {
            title: None,
            body: field_letters.join(";"),
            notification_id: None,
        }]
    );
}

#[test]
fn a_title_keeps_every_field_past_the_sixteenth_separator() {
    let mut engine = build_test_terminal_engine();
    let field_letters: Vec<String> = ('a'..='t').map(String::from).collect();
    let sequence_bytes = format!("\x1b]2;{}\x1b\\", field_letters.join(";"));
    let _ = engine.process_pty_output(sequence_bytes.as_bytes());
    assert_eq!(
        engine.get_terminal_state().get_title(),
        Some(field_letters.join(";").as_str())
    );
}

#[test]
fn an_osc9_with_no_separator_makes_no_notification() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b]9\x07\x1b]777\x07\x1b]99\x07"),
        Vec::new()
    );
}

#[test]
fn an_osc_text_cut_by_a_process_swap_completes_in_the_next_engine() {
    // The order a restored pane runs in: the engine is built from the carried
    // bytes, then reads copies, then reads the next chunk.
    for should_read_clipboard_copies in [false, true] {
        let mut engine = build_test_terminal_engine();
        assert_eq!(
            take_chunk_program_facts(&mut engine, b"\x1b]9;a;b;"),
            Vec::new()
        );
        let terminal_undecoded_bytes = engine.get_undecoded_terminal_bytes().to_vec();
        let mut restored_engine = TerminalEngine::from_terminal_state(
            engine.into_terminal_state(),
            &terminal_undecoded_bytes,
        );
        if should_read_clipboard_copies {
            restored_engine.start_reading_clipboard_copies(1024);
        }

        assert_eq!(
            take_chunk_program_facts(&mut restored_engine, b"c\x07"),
            vec![ProgramFact::Notify {
                title: None,
                body: "a;b;c".to_string(),
                notification_id: None,
            }],
            "reads copies: {should_read_clipboard_copies}"
        );
    }
}

#[test]
fn an_open_osc99_notification_does_not_ride_a_state_round_trip() {
    let mut engine = build_test_terminal_engine();
    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b]99;i=1:d=0;Old\x07"),
        Vec::new()
    );
    let terminal_state_json =
        serde_json::to_value(engine.get_terminal_state()).expect("state serializes");
    let restored_terminal_state: TerminalState =
        serde_json::from_value(terminal_state_json).expect("state deserializes");
    let mut restored_engine = TerminalEngine::from_carried_state(
        restored_terminal_state,
        &[],
        &[],
        GraphicsTransportState::default(),
        None,
        Instant::now(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        take_chunk_program_facts(&mut restored_engine, b"\x1b]99;i=1:p=body;New\x07"),
        vec![ProgramFact::Notify {
            title: None,
            body: "New".to_string(),
            notification_id: Some("1".to_string()),
        }]
    );
}

#[test]
fn the_program_status_query_is_answered_with_the_terminator_it_used() {
    let mut engine = build_test_terminal_engine();
    let read_instant = Instant::now();

    assert_eq!(
        engine.process_pty_output_at(b"\x1b]7501;?\x07", read_instant),
        Some(TerminalAdvance {
            device_query_replies: b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x07".to_vec(),
            program_facts: Vec::new(),
        })
    );
    assert_eq!(
        engine.process_pty_output_at(b"\x1b]7501;?\x1b\\", read_instant),
        Some(TerminalAdvance {
            device_query_replies: b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\".to_vec(),
            program_facts: Vec::new(),
        })
    );
}

#[test]
fn a_program_status_query_split_at_any_byte_is_answered_once() {
    let query_bytes = b"\x1b]7501;?\x1b\\";
    for split_byte_index in 1..query_bytes.len() {
        let mut engine = build_test_terminal_engine();
        let mut reply_bytes = engine.process_pty_output(&query_bytes[..split_byte_index]);
        reply_bytes.extend(engine.process_pty_output(&query_bytes[split_byte_index..]));
        assert_eq!(
            reply_bytes,
            b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\",
            "split at {split_byte_index}"
        );
    }
}

#[test]
fn no_program_status_sequence_but_the_query_writes_a_byte_back() {
    for sequence_bytes in [
        &b"\x1b]7501;state=working:app=cargo\x07"[..],
        b"\x1b]7501;state=clear\x1b\\",
        b"\x1b]7501;state=sleeping\x07",
        b"\x1b]7501;? \x07",
        b"\x1b]7501;?x\x07",
        b"\x1b]7501;\x07",
        b"\x1b]7501\x07",
    ] {
        let mut engine = build_test_terminal_engine();
        assert_eq!(
            engine.process_pty_output(sequence_bytes),
            Vec::<u8>::new(),
            "{sequence_bytes:?}"
        );
    }
}

#[test]
fn a_program_status_report_over_the_sequence_limit_gives_no_fact() {
    let build_sequence_bytes = |report_byte_count: usize, terminator_bytes: &[u8]| {
        let mut sequence_bytes = b"\x1b]7501;state=idle:".to_vec();
        sequence_bytes.resize(b"\x1b]7501;".len() + report_byte_count, b'x');
        sequence_bytes.extend_from_slice(terminator_bytes);
        sequence_bytes
    };
    let idle_report = vec![build_root_program_status_report(ProgramStatusState::Idle)];

    for (report_byte_count, terminator_bytes, expected_program_facts) in [
        (4087, &b"\x1b\\"[..], idle_report.clone()),
        (4088, b"\x1b\\", Vec::new()),
        (4088, b"\x07", idle_report.clone()),
        (4089, b"\x07", Vec::new()),
        (OSC_BUFFER_BYTE_CAPACITY + 100, b"\x07", Vec::new()),
    ] {
        let sequence_bytes = build_sequence_bytes(report_byte_count, terminator_bytes);
        assert_eq!(
            sequence_bytes.len(),
            7 + report_byte_count + terminator_bytes.len()
        );
        let mut engine = build_test_terminal_engine();
        assert_eq!(
            take_chunk_program_facts(&mut engine, &sequence_bytes),
            expected_program_facts,
            "{report_byte_count} bytes ended by {terminator_bytes:?}"
        );
    }
}

#[test]
fn an_eight_bit_program_status_report_gives_its_fact() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x9d7501;state=idle\x9c"),
        vec![build_root_program_status_report(ProgramStatusState::Idle)]
    );
}

#[test]
fn a_soft_reset_gives_no_program_status_fact() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b[!p"),
        Vec::new()
    );
}

/// A program status report cut in half by a process-image swap gives its fact
/// once the next engine reads the rest.
#[test]
fn a_split_program_status_report_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b]7501;state=work"),
        Vec::new()
    );
    assert_eq!(
        engine.get_undecoded_terminal_bytes(),
        b"\x1b]7501;state=work"
    );

    let mut restored_engine = rebuild_terminal_engine(engine);
    assert_eq!(
        take_chunk_program_facts(&mut restored_engine, b"ing\x07"),
        vec![build_root_program_status_report(
            ProgramStatusState::Working
        )]
    );
}

#[test]
fn a_program_status_report_reads_every_byte_past_the_sixteenth_semicolon() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(
            &mut engine,
            b"\x1b]7501;state=idle;;;;;;;;;;;;;;;;;;;;:state=error\x07"
        ),
        vec![build_root_program_status_report(ProgramStatusState::Error)]
    );
}

#[test]
fn a_program_status_sequence_ended_by_can_sub_or_another_escape_counts_two_terminator_bytes() {
    for ending_bytes in [&b"\x18"[..], b"\x1a", b"\x1b[0m"] {
        let mut report_bytes = b"\x1b]7501;state=idle".to_vec();
        report_bytes.extend_from_slice(ending_bytes);
        let mut engine = build_test_terminal_engine();
        assert_eq!(
            take_chunk_program_facts(&mut engine, &report_bytes),
            vec![build_root_program_status_report(ProgramStatusState::Idle)],
            "{ending_bytes:?}"
        );

        let mut query_bytes = b"\x1b]7501;?".to_vec();
        query_bytes.extend_from_slice(ending_bytes);
        let mut engine = build_test_terminal_engine();
        assert_eq!(
            engine.process_pty_output(&query_bytes),
            b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\",
            "{ending_bytes:?}"
        );
    }

    let mut sequence_bytes = b"\x1b]7501;state=idle:".to_vec();
    sequence_bytes.resize(b"\x1b]7501;".len() + 4088, b'x');
    sequence_bytes.push(0x18);
    let mut engine = build_test_terminal_engine();
    assert_eq!(
        take_chunk_program_facts(&mut engine, &sequence_bytes),
        Vec::new()
    );
}

#[test]
fn program_status_replies_and_facts_keep_the_order_the_program_wrote() {
    let mut engine = build_test_terminal_engine();
    let read_instant = Instant::now();

    assert_eq!(
        engine.process_pty_output_at(
            b"\x1b[5n\x1b]7501;?\x07\x1b]7501;state=idle\x07\x1b]7501;?\x1b\\\x1b]7501;state=done\x07",
            read_instant
        ),
        Some(TerminalAdvance {
            device_query_replies: [
                &b"\x1b[0n"[..],
                b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x07",
                b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\",
            ]
            .concat(),
            program_facts: build_timed_program_facts(
                vec![
                    build_root_program_status_report(ProgramStatusState::Idle),
                    build_root_program_status_report(ProgramStatusState::Done),
                ],
                read_instant
            ),
        })
    );
}

#[test]
fn each_repeated_report_and_prompt_start_gives_its_own_fact() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(
            &mut engine,
            b"\x1b]7501;state=idle\x07\x1b]7501;state=idle\x07\x1b]133;A\x07\x1b]133;A;cl=m;aid=1\x07"
        ),
        vec![
            build_root_program_status_report(ProgramStatusState::Idle),
            build_root_program_status_report(ProgramStatusState::Idle),
            ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted),
            ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted),
        ]
    );
}

#[test]
fn a_program_status_report_on_the_alternate_screen_gives_its_fact() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(
        take_chunk_program_facts(&mut engine, b"\x1b[?1049h\x1b]7501;state=working\x07"),
        vec![build_root_program_status_report(
            ProgramStatusState::Working
        )]
    );
}

#[test]
fn a_program_status_report_that_synchronized_output_holds_keeps_the_read_instant_of_its_chunk() {
    let mut engine = build_test_terminal_engine();
    let held_read_instant = Instant::now();
    let release_read_instant = held_read_instant + Duration::from_millis(5);

    assert_eq!(
        engine.process_pty_output_at(b"\x1b[?2026h\x1b]7501;state=working\x07", held_read_instant),
        None
    );
    assert_eq!(
        engine.process_pty_output_at(b"\x1b[?2026l", release_read_instant),
        Some(TerminalAdvance {
            device_query_replies: Vec::new(),
            program_facts: build_timed_program_facts(
                vec![build_root_program_status_report(
                    ProgramStatusState::Working
                )],
                held_read_instant
            ),
        })
    );
}
