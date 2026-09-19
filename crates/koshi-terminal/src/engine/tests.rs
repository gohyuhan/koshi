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
fn process_pty_output_with_shell_integration_returns_command_facts_in_order() {
    let mut engine = build_test_terminal_engine();

    let (terminal_replies, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(b"\x1b]133;C\x07\x1b]133;D;137\x07");

    assert_eq!(terminal_replies, b"");
    assert_eq!(
        shell_integration_facts,
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
    let mut engine = build_test_terminal_engine();

    let (_, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(b"\x1b]133;C\x07\x1b]133;D\x07");

    assert_eq!(
        shell_integration_facts,
        vec![
            ShellIntegrationFact::CommandStarted,
            ShellIntegrationFact::CommandFinished { exit_code: None },
        ]
    );
}

#[test]
fn unmatched_shell_integration_finish_returns_no_fact() {
    let mut engine = build_test_terminal_engine();

    let (_, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(b"\x1b]133;D;1\x07");

    assert_eq!(shell_integration_facts, Vec::<ShellIntegrationFact>::new());
}

#[test]
fn process_pty_output_with_shell_integration_returns_replies_and_facts_from_one_chunk() {
    let mut engine = build_test_terminal_engine();

    let (terminal_replies, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(b"\x1b[5n\x1b]133;C\x07");

    assert_eq!(terminal_replies, b"\x1b[0n");
    assert_eq!(
        shell_integration_facts,
        vec![ShellIntegrationFact::CommandStarted]
    );
}

#[test]
fn process_pty_output_drains_shell_facts_without_returning_them() {
    let mut engine = build_test_terminal_engine();

    assert_eq!(engine.process_pty_output(b"\x1b]133;C\x07"), b"");

    let (terminal_replies, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(b"");

    assert_eq!(terminal_replies, b"");
    assert_eq!(shell_integration_facts, Vec::<ShellIntegrationFact>::new());
}

#[test]
fn split_shell_integration_command_fact_waits_for_the_terminator() {
    let mut engine = build_test_terminal_engine();

    let (_, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(b"\x1b]133;C\x07\x1b]133;D;");
    assert_eq!(
        shell_integration_facts,
        vec![ShellIntegrationFact::CommandStarted]
    );

    let (_, shell_integration_facts) = engine.process_pty_output_with_shell_integration(b"137\x07");
    assert_eq!(
        shell_integration_facts,
        vec![ShellIntegrationFact::CommandFinished {
            exit_code: Some(137),
        }]
    );
}

#[test]
fn pending_shell_facts_survive_a_state_round_trip() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 3,
    });
    let mut parser = vte::Parser::<{ crate::engine::OSC_BUFFER_BYTE_CAPACITY }>::new_with_size();
    parser.advance(&mut terminal_state, b"\x1b]133;C\x07");

    let serialized_terminal_state =
        serde_json::to_string(&terminal_state).expect("state serializes");
    let mut restored_terminal_state: TerminalState =
        serde_json::from_str(&serialized_terminal_state).expect("state deserializes");

    assert_eq!(
        restored_terminal_state.take_shell_integration_facts(),
        vec![ShellIntegrationFact::CommandStarted]
    );
}

#[test]
fn prompt_marks_survive_scrollback_and_eviction() {
    let mut engine = TerminalEngine::with_scrollback(
        PtySize {
            column_count: 4,
            row_count: 2,
        },
        crate::scrollback::ScrollbackLimit::from_line_and_byte_limits(1, 1_000),
    );
    let _ = engine.process_pty_output(b"\x1b]133;A\x07\r\nx\r\n");

    assert!(
        engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()[0]
            .1
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
            .1
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
fn resize_resizes_the_state() {
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
        Ok(crate::graphics::GraphicsOperation::Image(
            create_rejected_graphics(),
        )),
        (0, 0),
    );
    engine.process_graphics_operation(
        Ok(crate::graphics::GraphicsOperation::Image(
            create_rejected_graphics(),
        )),
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

    let (_, shell_integration_facts) =
        engine.process_pty_output_with_shell_integration(&terminal_input_bytes);

    assert_eq!(
        shell_integration_facts,
        vec![ShellIntegrationFact::CommandStarted]
    );
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

fn build_red_png_bytes() -> Vec<u8> {
    use image::ImageEncoder;

    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel image encodes");
    png_bytes
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
            let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
                &[*synchronized_input_byte],
                test_timestamp,
            );
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
        let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
            BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
            test_timestamp,
        );
        let mut synchronized_input_bytes = control_lookalike_sequence.to_vec();
        synchronized_input_bytes.push(b'X');
        let (terminal_replies, shell_integration_facts, has_advanced_terminal_state) = engine
            .process_pty_output_with_shell_integration_at(
                &synchronized_input_bytes,
                test_timestamp,
            );

        assert_eq!(
            (
                terminal_replies,
                shell_integration_facts,
                has_advanced_terminal_state
            ),
            (vec![], vec![], false)
        );
        assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
        assert!(engine
            .get_synchronized_output_transport(test_timestamp)
            .is_some());

        let (_, _, has_advanced_terminal_state) = engine
            .process_pty_output_with_shell_integration_at(
                END_SYNCHRONIZED_OUTPUT_BYTES,
                test_timestamp,
            );
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

            let _ = engine.process_pty_output_with_shell_integration_at(
                &C1_SYNCHRONIZED_OUTPUT_BEGIN_BYTES[..begin_byte_count],
                test_timestamp,
            );
            let _ = engine.process_pty_output_with_shell_integration_at(
                &C1_SYNCHRONIZED_OUTPUT_BEGIN_BYTES[begin_byte_count..],
                test_timestamp,
            );
            let _ = engine
                .process_pty_output_with_shell_integration_at(b"\x1b[2J\x1b[HN", test_timestamp);
            assert_eq!(
                engine.get_terminal_state(),
                &committed_terminal_state,
                "begin_byte_count={begin_byte_count}, end_byte_count={end_byte_count}"
            );

            let (_, _, has_advanced_before_end) = engine
                .process_pty_output_with_shell_integration_at(
                    &C1_SYNCHRONIZED_OUTPUT_END_BYTES[..end_byte_count],
                    test_timestamp,
                );
            if end_byte_count < C1_SYNCHRONIZED_OUTPUT_END_BYTES.len() {
                assert_eq!(
                    engine.get_terminal_state(),
                    &committed_terminal_state,
                    "begin_byte_count={begin_byte_count}, end_byte_count={end_byte_count}"
                );
            }
            let (_, _, has_advanced_after_end) = engine
                .process_pty_output_with_shell_integration_at(
                    &C1_SYNCHRONIZED_OUTPUT_END_BYTES[end_byte_count..],
                    test_timestamp,
                );

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
    let first_terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let first_graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let first_graphics_transport_state = engine.get_graphics_transport_state().unwrap_or_default();
    let first_graphics_events = engine.take_graphics_events();
    let first_terminal_state = engine.into_terminal_state();
    let mut restored_engine =
        TerminalEngine::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            first_terminal_state,
            &first_terminal_undecoded_bytes,
            &first_graphics_undecoded_bytes,
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
    let second_terminal_undecoded_bytes = restored_engine.undecoded_terminal_bytes().to_vec();
    let second_graphics_undecoded_bytes = restored_engine.undecoded_graphics_bytes().to_vec();
    let second_graphics_transport_state = restored_engine
        .get_graphics_transport_state()
        .unwrap_or_default();
    let second_graphics_events = restored_engine.take_graphics_events();
    let second_terminal_state = restored_engine.into_terminal_state();
    let mut restored_engine =
        TerminalEngine::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            second_terminal_state,
            &second_terminal_undecoded_bytes,
            &second_graphics_undecoded_bytes,
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
    let (_, _, has_advanced_terminal_state) =
        restored_engine.process_pty_output_with_shell_integration_at(b"26l", test_timestamp);

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

    let (_, _, has_advanced_terminal_state) =
        engine.process_pty_output_with_shell_integration_at(&terminal_input_bytes, test_timestamp);

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
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
    let mut synchronized_input_bytes = b"A".to_vec();
    synchronized_input_bytes.extend_from_slice(END_SYNCHRONIZED_OUTPUT_BYTES);
    synchronized_input_bytes.extend_from_slice(BEGIN_SYNCHRONIZED_OUTPUT_BYTES);
    synchronized_input_bytes.push(b'B');

    let (_, _, has_advanced_terminal_state) = engine
        .process_pty_output_with_shell_integration_at(&synchronized_input_bytes, test_timestamp);

    assert!(has_advanced_terminal_state);
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'A');
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), ' ');
    assert!(engine
        .get_synchronized_output_transport(test_timestamp)
        .is_some());

    let (_, _, has_advanced_terminal_state) = engine.process_pty_output_with_shell_integration_at(
        END_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
    assert!(has_advanced_terminal_state);
    assert_eq!(get_terminal_cell_character(&engine, 0, 1), 'B');
}

#[test]
fn overdue_synchronized_bytes_are_released_before_new_input() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
    let (_, _, has_advanced_terminal_state) =
        engine.process_pty_output_with_shell_integration_at(b"A", test_timestamp);
    assert!(!has_advanced_terminal_state);

    let (_, _, has_advanced_terminal_state) = engine.process_pty_output_with_shell_integration_at(
        b"B",
        test_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION,
    );

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
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );

    let (_, _, has_advanced_terminal_state) = engine.process_pty_output_with_shell_integration_at(
        &vec![b'x'; MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT],
        test_timestamp,
    );

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
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
    let mut synchronized_input_bytes = b"Z\x1b[5n\x1b]133;C\x07".to_vec();
    synchronized_input_bytes.extend(build_visible_graphics_control(GraphicsProtocol::Kitty));
    let (_, _, has_advanced_terminal_state) = engine
        .process_pty_output_with_shell_integration_at(&synchronized_input_bytes, test_timestamp);
    assert!(!has_advanced_terminal_state);

    assert_eq!(
        engine.expire_synchronized_output(test_timestamp + Duration::from_millis(149)),
        None
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    let (terminal_replies, shell_integration_facts) = engine
        .expire_synchronized_output(test_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION)
        .expect("the deadline releases the update");

    assert_eq!(terminal_replies, b"\x1b[0n");
    assert_eq!(
        shell_integration_facts,
        [ShellIntegrationFact::CommandStarted]
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Z');
    assert_eq!(engine.get_terminal_state().list_image_placements().len(), 1);
    assert_eq!(engine.take_graphics_events().len(), 1);
}

#[test]
fn finish_releases_synchronized_text_and_reports_an_inner_truncated_image() {
    let test_timestamp = Instant::now();
    let mut engine = build_test_terminal_engine();
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
    let (_, _, has_advanced_terminal_state) = engine
        .process_pty_output_with_shell_integration_at(b"Q\x1b_Gf=32,s=1,v=1;AAAA", test_timestamp);
    assert!(!has_advanced_terminal_state);

    assert_eq!(
        engine.finish_graphics_stream(),
        [Err(GraphicsError::Truncated {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'Q');
    assert!(engine
        .get_synchronized_output_transport(test_timestamp)
        .is_none());
}

#[test]
fn synchronized_output_deadline_keeps_elapsed_process_swap_time() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
    let (_, _, _) = engine.process_pty_output_with_shell_integration_at(b"R", test_timestamp);
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
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let graphics_transport_state = engine.get_graphics_transport_state().unwrap_or_default();
    let terminal_state = engine.into_terminal_state();
    let restored_timestamp = test_timestamp + Duration::from_secs(1);
    let mut restored_engine =
        TerminalEngine::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            terminal_state,
            &terminal_undecoded_bytes,
            &graphics_undecoded_bytes,
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
    assert!(restored_engine
        .expire_synchronized_output(restored_timestamp + Duration::from_millis(10))
        .is_some());
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), 'R');
}

#[test]
fn synchronized_output_transport_rejects_impossible_scanner_state() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let mut engine = build_test_terminal_engine();
    let _ = engine.process_pty_output_with_shell_integration_at(
        BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
        test_timestamp,
    );
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
fn overlong_open_string_keeps_its_scanner_state_across_process_swap() {
    let test_timestamp = Instant::now();
    let wall_clock_timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
    let mut opening_string_bytes = b"\x1b]2;".to_vec();
    opening_string_bytes.extend(std::iter::repeat_n(b'A', MAX_UNDECODED_BYTE_COUNT + 1));
    let string_suffix_bytes = b"inside\x1b[?2026hdata\x1b[?2026l\x07";

    let mut uninterrupted_engine = build_test_terminal_engine();
    let _ = uninterrupted_engine.process_pty_output(&opening_string_bytes);
    assert!(uninterrupted_engine.undecoded_terminal_bytes().is_empty());

    let mut carried_engine = build_test_terminal_engine();
    let _ = carried_engine.process_pty_output(&opening_string_bytes);
    assert!(carried_engine.undecoded_terminal_bytes().is_empty());
    let synchronized_output_transport = carried_engine
        .get_synchronized_output_transport_at(test_timestamp, wall_clock_timestamp)
        .expect("the open string has scanner transport state");
    assert_eq!(synchronized_output_transport.get_deadline(), None);
    let graphics_undecoded_bytes = carried_engine.undecoded_graphics_bytes().to_vec();
    let graphics_transport_state = carried_engine
        .get_graphics_transport_state()
        .unwrap_or_default();
    let terminal_state = carried_engine.into_terminal_state();
    let mut restored_engine =
        TerminalEngine::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            terminal_state,
            &[],
            &graphics_undecoded_bytes,
            &[],
            graphics_transport_state,
            Some(synchronized_output_transport),
            test_timestamp,
            wall_clock_timestamp,
        );

    let _ = uninterrupted_engine.process_pty_output(string_suffix_bytes);
    let (_, _, has_advanced_terminal_state) = restored_engine
        .process_pty_output_with_shell_integration_at(string_suffix_bytes, test_timestamp);

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
    let first_graphics_undecoded_bytes = carried_engine.undecoded_graphics_bytes().to_vec();
    let first_graphics_transport_state = carried_engine
        .get_graphics_transport_state()
        .expect("the open APC has graphics transport state");
    let first_terminal_state = carried_engine.into_terminal_state();
    let mut restored_engine =
        TerminalEngine::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            first_terminal_state,
            &[],
            &first_graphics_undecoded_bytes,
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
    let second_terminal_undecoded_bytes = restored_engine.undecoded_terminal_bytes().to_vec();
    let second_graphics_undecoded_bytes = restored_engine.undecoded_graphics_bytes().to_vec();
    let second_graphics_transport_state = restored_engine
        .get_graphics_transport_state()
        .unwrap_or_default();
    let second_graphics_events = restored_engine.take_graphics_events();
    let second_terminal_state = restored_engine.into_terminal_state();
    let mut restored_engine =
        TerminalEngine::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            second_terminal_state,
            &second_terminal_undecoded_bytes,
            &second_graphics_undecoded_bytes,
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
        let (_, _, has_advanced_terminal_state) = engine
            .process_pty_output_with_shell_integration_at(
                malformed_terminal_input_bytes,
                test_timestamp,
            );

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
            assert_eq!(engine.undecoded_terminal_bytes(), b"\xe2\x94");
            assert_eq!(engine.undecoded_graphics_bytes(), b"\xe2\x94");

            let terminal_state = engine.into_terminal_state();
            let mut rebuilt_engine = TerminalEngine::from_terminal_state_with_graphics(
                terminal_state,
                b"\xe2\x94",
                b"\xe2\x94",
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
/// whole and one byte at a time, the parser must reach byte-identical state:
/// splitting a sequence at any boundary may never change the outcome.
#[test]
fn a_sequence_split_at_every_byte_boundary_matches_the_whole_feed() {
    let terminal_input_sequence = b"\x1b[1;31mAB\x1b[2;3HCD\r\n\x1b[Kxy";

    let mut whole_input_engine = build_test_terminal_engine();
    let _ = whole_input_engine.process_pty_output(terminal_input_sequence);

    let mut split_input_engine = build_test_terminal_engine();
    for terminal_input_byte in terminal_input_sequence {
        let _ = split_input_engine.process_pty_output(&[*terminal_input_byte]);
    }

    // Concrete landmarks so the comparison is not vacuously two blank grids.
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

    // 10000 / 80 = 125 logical rows; the last parks unscrolled, so the bottom
    // row holds the final run and the cursor rests on the last column.
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
fn many_line_feeds_cap_the_scrollback_and_tally_the_drops() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    // 12000 line feeds on a 2-row screen: the first descends without scrolling,
    // the remaining 11999 each push one row into history.
    let line_feed_bytes = vec![b'\n'; 12_000];
    let _ = engine.process_pty_output(&line_feed_bytes);

    // The default 10 000-line cap holds; the overflow is dropped and tallied.
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
            .get_dropped_line_count(),
        1_999
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (1, 0)
    );
}

// --- Taking the state apart and rebuilding it ---

/// Everything the state holds must survive being written out and read back:
/// both screen buffers, both cursors and their saved snapshots, the pen, the
/// modes, the scrollback with its truncation tallies, the title, and the
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

    // Bold red pen, then ten characters on an eight-column row, so the row
    // soft-wraps onto the row below it.
    let _ = engine.process_pty_output(b"\x1b[1;31mabcdefghij");
    // A wide CJK glyph, which takes two columns.
    let _ = engine.process_pty_output("世".as_bytes());
    // DECSC saves the cursor and the pen, both change, DECRC restores them.
    let _ = engine.process_pty_output(b"\x1b7\x1b[4;32mZ\x1b8");
    // Paint the alternate screen, then return to the primary.
    let _ = engine.process_pty_output(b"\x1b[?1049hALT\x1b[?1049l");
    // Ten line feeds on a three-row screen hand more rows to history than the
    // four-line cap holds, so the oldest are dropped and tallied.
    let _ = engine.process_pty_output(b"\n\n\n\n\n\n\n\n\n\n");
    // A title, then a base character with a combining acute over it: the
    // cluster is still open when the state is taken apart.
    let _ = engine.process_pty_output("\x1b]0;koshi\x07e\u{0301}".as_bytes());

    let terminal_state = engine.into_terminal_state();

    // Landmarks, so the comparison below is not two blank states.
    assert_eq!(terminal_state.get_title(), Some("koshi"));
    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 4);
    // The cursor sat on row 1, so the first feed descends and the other nine
    // each hand a row to history: nine pushed, four kept, five dropped.
    assert_eq!(terminal_state.get_scrollback().get_dropped_line_count(), 5);

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
    let carried_undecoded_bytes = terminal_engine.undecoded_terminal_bytes().to_vec();
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

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
}

/// A working-directory report (OSC 7) cut in half is carried whole, so the pane
/// keeps its old directory until the report finishes and no part of the URI
/// prints as text.
#[test]
fn a_split_working_directory_report_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // The shell reports /Users/yuhan/Projects/koshi, and the chunk ends after
    // `/Proj`.
    let _ = engine.process_pty_output(b"\x1b]7;file://host/Users/yuhan/Proj");

    assert_eq!(
        engine.get_terminal_state().get_current_working_directory(),
        None
    );
    assert_eq!(
        engine.undecoded_terminal_bytes(),
        b"\x1b]7;file://host/Users/yuhan/Proj"
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
        Path::new("/Users/yuhan/Projects/koshi")
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

/// A title report (OSC 0) cut in half is carried whole, so the title changes
/// once, to the whole payload.
#[test]
fn a_split_title_report_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;ti");

    assert_eq!(engine.get_terminal_state().get_title(), None);
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b]0;ti");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"tle\x07");

    assert_eq!(
        restored_engine.get_terminal_state().get_title(),
        Some("title")
    );
    assert_eq!(get_terminal_cell_character(&restored_engine, 0, 0), ' ');
}

/// A CSI cut in half is carried whole, so its final byte completes the sequence
/// in the next parser instead of printing as text.
#[test]
fn a_split_csi_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // SGR 31 (red foreground) cut off before its final `m`.
    let _ = engine.process_pty_output(b"\x1b[31");

    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[31");

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

/// A UTF-8 code point cut in half is carried whole, so the next parser prints
/// the glyph rather than two replacement characters.
#[test]
fn a_split_code_point_is_carried_whole() {
    let mut engine = build_test_terminal_engine();

    // 'é' is 0xC3 0xA9; only its first byte arrives.
    let _ = engine.process_pty_output(b"\xc3");

    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(engine.undecoded_terminal_bytes(), b"\xc3");

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

    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b]0;koshi");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\x07");

    assert_eq!(
        restored_engine.get_terminal_state().get_title(),
        Some("koshi")
    );
}

/// Text after a finished sequence leaves the parser holding nothing, so the
/// carry never replays glyphs that already reached the screen.
#[test]
fn text_after_a_finished_sequence_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]0;koshi\x07ab");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");

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

    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[1\n3");
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
/// bytes, so the rest of the body is swallowed after the swap instead of
/// printing as text.
#[test]
fn a_split_device_control_string_carries_its_opening_bytes() {
    let mut engine = build_test_terminal_engine();

    // A sixel image: `ESC P q` opens the string and the payload follows.
    let _ = engine.process_pty_output(b"\x1bPq#0;2;0;0;0");

    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1bPq");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );

    // More payload, with no escape byte in the chunk: what is carried stays the
    // opening bytes and does not grow with the image.
    let _ = engine.process_pty_output(b"#0~~@@");

    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1bPq");

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
/// boundary, so nothing is carried and the text after it prints as text.
#[test]
fn a_device_control_string_closed_by_the_eight_bit_terminator_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    // `ESC P q` opens the string, `0x9c` closes it, and `Z` follows it.
    let _ = engine.process_pty_output(b"\x1bPq#0~~\x9cZ");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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

/// `CAN` (`0x18`) abandons the sequence it lands in without dispatching it, so
/// the parser is back on a sequence boundary and nothing is carried.
#[test]
fn a_cancelled_sequence_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    // SGR 31 abandoned mid-parameter by `CAN`.
    let _ = engine.process_pty_output(b"\x1b[31\x18");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");

    // The next chunk starts a fresh text run, and none of it is carried.
    let _ = engine.process_pty_output(b"Z");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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
        assert_eq!(engine.undecoded_terminal_bytes(), b"");

        let _ = engine.process_pty_output(&[b'\n'; READ_CHUNK_BYTE_COUNT]);
        assert_eq!(engine.undecoded_terminal_bytes(), b"");
    }
}

/// A clipboard write (OSC 52) can span many reads. The carry holds the payload
/// up to `MAX_UNDECODED_BYTE_COUNT` and drops it past that, so the pane's memory does not
/// grow with the payload.
#[test]
fn a_large_clipboard_write_keeps_bounded_carry_and_terminal_state() {
    const CLIPBOARD_PAYLOAD_BYTE_COUNT: usize = 8 * 1024 * 1024;
    let clipboard_sequence_opening_bytes = b"\x1b]52;c;";
    let clipboard_payload_bytes = vec![b'A'; CLIPBOARD_PAYLOAD_BYTE_COUNT];

    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(clipboard_sequence_opening_bytes);
    assert_eq!(
        engine.undecoded_terminal_bytes(),
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
            assert_eq!(engine.undecoded_terminal_bytes().len(), held_byte_count);
        } else {
            assert_eq!(engine.undecoded_terminal_bytes(), b"");
        }
    }

    // The payload passed `MAX_UNDECODED_BYTE_COUNT`, so the carry is empty. The real
    // parser still swallows the body: no part of it printed.
    assert_eq!(engine.undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );

    // koshi handles no clipboard write, so the terminator only closes the
    // sequence and the `Z` after it prints.
    let _ = engine.process_pty_output(b"\x07Z");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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
/// parser on a sequence boundary, so nothing is carried.
#[test]
fn a_finished_device_control_string_is_not_carried() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1bPq#0~~\x1b\\");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");

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
/// application program command, so each one carries its two opening bytes and
/// no more, however long the body runs.
#[test]
fn a_string_whose_body_the_parser_drops_carries_only_its_opening_bytes() {
    for string_opening_bytes in [b"\x1bX", b"\x1b^", b"\x1b_"] {
        let mut engine = build_test_terminal_engine();

        let _ = engine.process_pty_output(string_opening_bytes);
        assert_eq!(engine.undecoded_terminal_bytes(), string_opening_bytes);

        // A megabyte of body arriving one read at a time adds nothing.
        let string_body_bytes = vec![b'y'; 1024 * 1024];
        for body_chunk in string_body_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
            let _ = engine.process_pty_output(body_chunk);
            assert_eq!(engine.undecoded_terminal_bytes(), string_opening_bytes);
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

        assert_eq!(restored_engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b");

    // The `_` and the start of a kitty graphics payload arrive next.
    let _ = engine.process_pty_output(b"_Ga=T,f=100;iVBORw0KGgo");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b_");

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

/// An operating system command longer than `MAX_UNDECODED_BYTE_COUNT` stops being held, so
/// one pane cannot grow the engine's memory without a bound. The real parser
/// still swallows the body, and the sequence's end returns the carry to empty.
#[test]
fn an_operating_system_command_past_the_limit_is_not_held() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b]52;c;");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b]52;c;");

    // One chunk short of the limit, the whole sequence is still held.
    let under_limit_input_bytes = vec![b'A'; MAX_UNDECODED_BYTE_COUNT - READ_CHUNK_BYTE_COUNT];
    for terminal_input_chunk in under_limit_input_bytes.chunks(READ_CHUNK_BYTE_COUNT) {
        let _ = engine.process_pty_output(terminal_input_chunk);
    }
    assert_eq!(
        engine.undecoded_terminal_bytes().len(),
        7 + MAX_UNDECODED_BYTE_COUNT - READ_CHUNK_BYTE_COUNT
    );

    // The next chunk passes the limit, and the carry drops to empty. The
    // buffer is released, not cleared, so the pane keeps no room for it.
    let _ = engine.process_pty_output(&[b'A'; READ_CHUNK_BYTE_COUNT]);
    assert_eq!(engine.undecoded_terminal_bytes(), b"");
    assert_eq!(engine.undecoded_terminal_bytes.capacity(), 0);

    // More body changes nothing, and none of it prints.
    let _ = engine.process_pty_output(&[b'A'; READ_CHUNK_BYTE_COUNT]);
    assert_eq!(engine.undecoded_terminal_bytes(), b"");
    assert_eq!(engine.undecoded_terminal_bytes.capacity(), 0);
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );

    // The terminator closes the sequence and the `Z` after it prints.
    let _ = engine.process_pty_output(b"\x07Z");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[");

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
            engine.undecoded_terminal_bytes().len(),
            expected_held_byte_count,
            "after chunk {chunk_round}"
        );
    }

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');

    // `m` finishes it as an SGR koshi ignores; the `Z` after it prints.
    let _ = engine.process_pty_output(b"mZ");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"BC\x07Z");

    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"");

    let _ = engine.process_pty_output(b"a\xc3");
    assert_eq!(engine.process_pty_output(b""), b"");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\xc3");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), 'a');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );

    let _ = engine.process_pty_output(b"\xa9\x1b[3");
    assert_eq!(engine.process_pty_output(b""), b"");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[3");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\xf0\x9f");

    let _ = engine.process_pty_output(b"\x98");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\xf0\x9f\x98");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), ' ');

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\x80");

    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\xc3");

    let _ = engine.process_pty_output(b"\x1b[31");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[31");
    assert_eq!(get_terminal_cell_character(&engine, 0, 0), '\u{fffd}');
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 1)
    );

    let _ = engine.process_pty_output(b"mZ");
    assert_eq!(engine.undecoded_terminal_bytes(), b"");

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
    assert_eq!(engine.undecoded_terminal_bytes(), b"");

    let _ = engine.process_pty_output(b"Z");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b");

    let _ = engine.process_pty_output(b"[31mZ");
    assert_eq!(engine.undecoded_terminal_bytes(), b"");

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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b]0;hi");

    let _ = engine.process_pty_output(b"\x1b\\Z");

    assert_eq!(engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"\\Z");

    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"");
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
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1bP");

    let _ = engine.process_pty_output(b"q#0~~");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1bPq");

    let mut restored_engine = rebuild_terminal_engine(engine);
    let _ = restored_engine.process_pty_output(b"@@\x1b\\Z");

    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"");
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

    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[3");
}

/// A control sequence the parser ignores dispatches nothing: the scan holds
/// it, and every control byte after it, until the next escape byte or printed
/// character. Replaying it leaves the parser on a sequence boundary.
#[test]
fn an_ignored_control_sequence_is_held_until_the_next_escape_or_print() {
    let mut engine = build_test_terminal_engine();

    let _ = engine.process_pty_output(b"\x1b[3?m");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[3?m");

    let _ = engine.process_pty_output(b"\n");
    assert_eq!(engine.undecoded_terminal_bytes(), b"\x1b[3?m\n");
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (1, 0)
    );

    let mut restored_engine = rebuild_terminal_engine(engine);
    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"\x1b[3?m\n");
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (1, 0)
    );

    let _ = restored_engine.process_pty_output(b"Z");
    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"");
    assert_eq!(get_terminal_cell_character(&restored_engine, 1, 0), 'Z');
    assert_eq!(
        restored_engine
            .get_terminal_state()
            .get_active_cursor_position(),
        (1, 1)
    );

    let _ = restored_engine.process_pty_output(b"\x1b[3?m\x1b[3");
    assert_eq!(restored_engine.undecoded_terminal_bytes(), b"\x1b[3");
}

/// The engine holds its OSC buffer inline, so its own size bounds how much one
/// unterminated sequence can accumulate.
#[test]
fn the_engine_carries_a_bounded_osc_buffer() {
    // One engine exists per pane, so its size is a per-pane cost. The bound is
    // an absolute figure: expressing it against `OSC_BUFFER_BYTE_CAPACITY` would rise with
    // the capacity it is meant to bound.
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
    // vte holds the open sequence between calls, so the cap must apply to the
    // assembled payload rather than to one chunk.
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
    // A multi-byte character delivered one byte at a time must be filtered as
    // the character it forms, not passed through as bytes.
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
    // The parser stops taking bytes at `OSC_BUFFER_BYTE_CAPACITY`, so a longer sequence
    // reaches `osc_dispatch` short. For a title that changes nothing: the cut
    // to `MAX_REPORTED_TEXT_BYTE_COUNT` happens well below the capacity, so both
    // lengths yield the same bytes.
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
