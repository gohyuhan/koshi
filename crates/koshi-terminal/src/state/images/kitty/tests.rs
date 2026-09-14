//! Protocol-driven checks for Kitty upload and placement lifetimes.

use super::*;

use crate::engine::TerminalEngine;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;
use koshi_core::process::PtySize;
use std::time::Duration;

const KITTY_UPLOAD_SEQUENCE_BYTES: &[u8] =
    b"\x1b_Ga=T,f=32,s=1,v=1,c=2,r=2,C=1,i=7,q=2;/wAA/w==\x1b\\";

fn build_terminal_engine() -> TerminalEngine {
    TerminalEngine::from_pty_size(PtySize {
        column_count: 20,
        row_count: 10,
    })
}

fn build_rgba_png(rgba_bytes: &[u8], image_pixel_width: u32, image_pixel_height: u32) -> Vec<u8> {
    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(
            rgba_bytes,
            image_pixel_width,
            image_pixel_height,
            image::ColorType::Rgba8.into(),
        )
        .expect("the PNG encodes");
    png_bytes
}

fn build_png_chunk(chunk_kind: &[u8; 4], chunk_bytes: &[u8]) -> Vec<u8> {
    let mut png_chunk_bytes = Vec::with_capacity(12 + chunk_bytes.len());
    png_chunk_bytes.extend_from_slice(&(chunk_bytes.len() as u32).to_be_bytes());
    png_chunk_bytes.extend_from_slice(chunk_kind);
    png_chunk_bytes.extend_from_slice(chunk_bytes);
    let mut png_crc = 0xffff_ffffu32;
    for &byte in chunk_kind.iter().chain(chunk_bytes) {
        png_crc ^= u32::from(byte);
        for _ in 0..8 {
            png_crc = if png_crc & 1 == 1 {
                (png_crc >> 1) ^ 0xedb8_8320
            } else {
                png_crc >> 1
            };
        }
    }
    png_chunk_bytes.extend_from_slice(&(!png_crc).to_be_bytes());
    png_chunk_bytes
}

fn extract_png_image_data(encoded_png_bytes: &[u8]) -> Vec<u8> {
    let mut idat_bytes = Vec::new();
    let mut png_byte_offset = 8;
    while png_byte_offset < encoded_png_bytes.len() {
        let chunk_byte_length = usize::try_from(u32::from_be_bytes(
            encoded_png_bytes[png_byte_offset..png_byte_offset + 4]
                .try_into()
                .expect("PNG chunk length"),
        ))
        .expect("PNG chunk length fits");
        let chunk_end_offset = png_byte_offset + 12 + chunk_byte_length;
        if &encoded_png_bytes[png_byte_offset + 4..png_byte_offset + 8] == b"IDAT" {
            idat_bytes
                .extend_from_slice(&encoded_png_bytes[png_byte_offset + 8..chunk_end_offset - 4]);
        }
        png_byte_offset = chunk_end_offset;
    }
    idat_bytes
}

fn build_apng_with_separate_default(
    default_pixel: [u8; 4],
    animation_frame_pixel: [u8; 4],
) -> Vec<u8> {
    let default_png_bytes = build_rgba_png(&default_pixel, 1, 1);
    let animation_frame_data =
        extract_png_image_data(&build_rgba_png(&animation_frame_pixel, 1, 1));
    let mut animation_frame_control = [0; 26];
    animation_frame_control[4..8].copy_from_slice(&1u32.to_be_bytes());
    animation_frame_control[8..12].copy_from_slice(&1u32.to_be_bytes());
    animation_frame_control[20..22].copy_from_slice(&1u16.to_be_bytes());
    animation_frame_control[22..24].copy_from_slice(&10u16.to_be_bytes());
    let mut animation_frame_chunk_data = 1u32.to_be_bytes().to_vec();
    animation_frame_chunk_data.extend_from_slice(&animation_frame_data);

    let mut apng_bytes = default_png_bytes[..33].to_vec();
    apng_bytes.extend_from_slice(&build_png_chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
    apng_bytes.extend_from_slice(&build_png_chunk(
        b"IDAT",
        &extract_png_image_data(&default_png_bytes),
    ));
    apng_bytes.extend_from_slice(&build_png_chunk(b"fcTL", &animation_frame_control));
    apng_bytes.extend_from_slice(&build_png_chunk(b"fdAT", &animation_frame_chunk_data));
    apng_bytes.extend_from_slice(&build_png_chunk(b"IEND", &[]));
    apng_bytes
}

fn display_animation_base(
    terminal_engine: &mut TerminalEngine,
    image_pixel_width: u32,
    image_pixel_height: u32,
    rgba_bytes: &[u8],
) {
    let encoded_rgba_payload = STANDARD.encode(rgba_bytes);
    let kitty_command = format!(
        "\x1b_Ga=T,i=7,f=32,s={image_pixel_width},v={image_pixel_height},c={image_pixel_width},r={image_pixel_height},C=1,q=2;{encoded_rgba_payload}\x1b\\"
    );
    assert_eq!(
        terminal_engine.process_pty_output(kitty_command.as_bytes()),
        b""
    );
}

fn select_second_frame(terminal_engine: &mut TerminalEngine) {
    assert_eq!(
        terminal_engine.process_pty_output(b"\x1b_Ga=a,i=7,c=2,s=1,q=2\x1b\\"),
        b""
    );
}

fn add_blue_and_green_frames(terminal_engine: &mut TerminalEngine) {
    assert_eq!(
        terminal_engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(
        terminal_engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=2;AP8A/w==\x1b\\"),
        b""
    );
}

#[test]
fn a_chat_scroll_with_an_auto_scrollbar_preserves_kitty_pixels() {
    assert_chat_scroll_preserves_pixels(true);
}

#[test]
fn kitty_placements_survive_text_writes_on_both_screens_and_in_history() {
    for screen_command in ["", "\x1b[?1049h", "history"] {
        for z_index in [-2, -1, 0, 1] {
            for terminal_text in [" ", "a", "界", "a\u{0301}", "界\u{0301}"] {
                let mut engine = build_terminal_engine();
                if screen_command != "history" {
                    assert_eq!(engine.process_pty_output(screen_command.as_bytes()), b"");
                }
                assert_eq!(
                    engine.process_pty_output(
                        format!(
                            "\x1b_Ga=T,f=32,s=1,v=1,c=4,r=4,C=1,i=7,z={z_index},q=2;/wAA/w==\x1b\\"
                        )
                        .as_bytes()
                    ),
                    b""
                );
                if screen_command == "history" {
                    assert_eq!(engine.process_pty_output(b"\x1b[2S"), b"");
                }
                let before_image_placements = engine
                    .get_terminal_state()
                    .list_image_placements_for_view(0);
                assert_eq!(
                    before_image_placements
                        .iter()
                        .map(|image_placement| image_placement
                            .image_record
                            .image
                            .rgba_bytes
                            .clone())
                        .collect::<Vec<_>>(),
                    [vec![255, 0, 0, 255]]
                );
                let history_image_placements = engine
                    .get_terminal_state()
                    .list_image_placements_for_view(2);
                assert_eq!(engine.process_pty_output(b"\x1b[1;1H"), b"");
                assert_eq!(engine.process_pty_output(terminal_text.as_bytes()), b"");
                assert_eq!(
                    engine
                        .get_terminal_state()
                        .list_image_placements_for_view(0),
                    before_image_placements,
                    "screen={screen_command:?}, z={z_index}, text={terminal_text:?}"
                );
                assert_eq!(
                    engine
                        .get_terminal_state()
                        .list_image_placements_for_view(2),
                    history_image_placements,
                    "screen={screen_command:?}, z={z_index}, text={terminal_text:?}"
                );
            }
        }
    }
}

#[test]
fn overwriting_a_unicode_placeholder_removes_only_its_visible_cells() {
    for screen_command_bytes in [b"".as_slice(), b"\x1b[?1049h"] {
        let mut engine = build_terminal_engine();
        assert_eq!(engine.process_pty_output(screen_command_bytes), b"");
        assert_eq!(engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\\x1b_Ga=p,i=7,p=3,c=1,r=1,U=1,C=1,q=2\x1b\\"), b"");
        let unicode_placeholder_text = "\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}";
        assert_eq!(
            engine.process_pty_output(unicode_placeholder_text.as_bytes()),
            b""
        );
        let before_image_placements = engine
            .get_terminal_state()
            .list_image_placements_for_view(0);
        assert_eq!(
            before_image_placements
                .iter()
                .map(|image_placement| image_placement.image_record.image.rgba_bytes.clone())
                .collect::<Vec<_>>(),
            [vec![255, 0, 0, 255]]
        );
        assert_eq!(engine.process_pty_output(b"\x1b[1;1H "), b"");
        assert_eq!(
            engine
                .get_terminal_state()
                .list_image_placements_for_view(0),
            []
        );
        assert_eq!(engine.process_pty_output(b"\x1b[1;1H"), b"");
        assert_eq!(
            engine.process_pty_output(unicode_placeholder_text.as_bytes()),
            b""
        );
        assert_eq!(
            engine
                .get_terminal_state()
                .list_image_placements_for_view(0),
            before_image_placements
        );
    }
}

#[test]
fn a_chat_scroll_with_a_hidden_scrollbar_preserves_kitty_pixels() {
    assert_chat_scroll_preserves_pixels(false);
}

fn assert_chat_scroll_preserves_pixels(has_scrollbar: bool) {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 20,
        row_count: 14,
    });
    assert_eq!(engine.process_pty_output(b"\x1b[?1049h\x1b[?2026h\x1b[1;1H\x1b[2K\x1b_Ga=T,f=32,s=1,v=1,i=42,c=4,C=1,y=0,h=1,r=1,q=2;/wAA/w==\x1b\\\x1b[?2026l"), b"");
    for (image_row_index, image_row_count, placement_suffix) in [(1, 3, ",y=0,h=1"), (2, 4, "")] {
        assert_eq!(
            engine.process_pty_output(b"\x1b[?2026h\x1b_Ga=d,d=a,q=2\x1b\\"),
            b""
        );
        for row_index in 1..=10 {
            let mut terminal_line = format!("\x1b[{row_index};1H\x1b[2K");
            if row_index == image_row_index {
                terminal_line.push_str(&format!(
                    "\x1b_Ga=p,q=2,i=42,c=4,r={image_row_count},C=1{placement_suffix}\x1b\\"
                ));
            } else {
                let terminal_line_text = if row_index < image_row_index {
                    "before".to_owned()
                } else if row_index < image_row_index + image_row_count {
                    String::new()
                } else {
                    format!("after{}", row_index - image_row_index - image_row_count)
                };
                if has_scrollbar {
                    terminal_line.push_str(&format!("{terminal_line_text:19}\x1b[90m│\x1b[39m"));
                } else {
                    terminal_line.push_str(&terminal_line_text);
                }
                terminal_line.push_str("\x1b[0m\x1b]8;;\x1b\\");
            }
            assert_eq!(engine.process_pty_output(terminal_line.as_bytes()), b"");
        }
        assert_eq!(engine.process_pty_output(b"\x1b[?2026l"), b"");
        let image_placements = engine.get_terminal_state().list_image_placements();
        assert_eq!(
            image_placements
                .iter()
                .map(|image_placement| (
                    image_placement.get_image_anchor(),
                    image_placement.get_image_cell_dimensions(),
                    image_placement.image_record.image.rgba_bytes.clone()
                ))
                .collect::<Vec<_>>(),
            [(
                (image_row_index - 1, 0),
                (image_row_count, 4),
                vec![255, 0, 0, 255]
            )]
        );
    }
}

#[test]
fn an_alternate_screen_redraw_places_retained_pixels_without_a_payload_separator() {
    assert_alternate_screen_redraw(b"\x1b_Ga=p,i=7,c=2,r=2\x1b\\");
}

#[test]
fn an_alternate_screen_redraw_places_retained_pixels_with_a_payload_separator() {
    assert_alternate_screen_redraw(b"\x1b_Ga=p,i=7,c=2,r=2;\x1b\\");
}

fn assert_alternate_screen_redraw(placement_command_bytes: &[u8]) {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(b"\x1b[?1049h"), b"");
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d,d=a,q=2\x1b\\"), b"");
    assert_eq!(list_image_anchors(&engine), []);
    assert_eq!(engine.process_pty_output(b"\x1b[5;3H"), b"");
    assert_eq!(
        engine.process_pty_output(placement_command_bytes),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(list_image_anchors(&engine), [(4, 2)]);
    let image_placements = engine.get_terminal_state().list_image_placements();
    assert_eq!(image_placements[0].get_image_cell_dimensions(), (2, 2));
    assert_eq!(
        image_placements[0].get_image_geometry().full_size,
        koshi_core::geometry::Size {
            column_count: 2,
            row_count: 2
        }
    );
    assert_eq!(
        image_placements[0].image_record.image.rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn a_failed_numbered_upload_does_not_reply_with_an_older_image_id() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,I=9,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=1,I=9;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,I=9,f=32,s=1,v=1,c=1,r=1,U=1,P=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=0,I=9;EINVAL:invalid image placement\x1b\\"
    );
    assert_eq!(
        engine
            .get_terminal_state()
            .kitty_images
            .iter()
            .map(|kitty_image| kitty_image.display.image_id)
            .collect::<Vec<_>>(),
        [Some(1)]
    );
}

#[test]
fn a_continuation_error_uses_the_first_chunks_reply_identity() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=9,f=32,s=1,v=1,m=1;/wAA\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Gm=0;!\x1b\\"),
        b"\x1b_Gi=9;EINVAL:invalid image data\x1b\\"
    );
    assert_eq!(engine.get_terminal_state().kitty_images, []);
}

#[test]
fn a_delete_range_frees_uploads_without_placements() {
    let mut engine = build_terminal_engine();
    for image_id in 1..=3 {
        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=t,i={image_id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()
            ),
            b""
        );
    }
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=R,x=1,y=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine
            .get_terminal_state()
            .kitty_images
            .iter()
            .map(|kitty_image| kitty_image.display.image_id)
            .collect::<Vec<_>>(),
        [Some(3)]
    );
}

#[test]
fn restoring_old_placements_rebuilds_the_numbered_upload_store() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,I=9,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let mut serialized_terminal_state =
        serde_json::to_value(engine.get_terminal_state()).expect("serialize");
    serialized_terminal_state
        .as_object_mut()
        .expect("object")
        .remove("kitty_images");
    let restored_terminal_state: TerminalState =
        serde_json::from_value(serialized_terminal_state).expect("restore old state");
    let mut engine = TerminalEngine::from_terminal_state(restored_terminal_state, &[]);
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,I=9,c=2,r=3,C=1\x1b\\"),
        b"\x1b_Gi=1,I=9;OK\x1b\\"
    );
    let image_placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(
        (
            image_placement.get_image_anchor(),
            image_placement.column_count,
            image_placement.row_count
        ),
        ((0, 0), 2, 3)
    );
}

fn list_image_anchors(engine: &TerminalEngine) -> Vec<(u16, u16)> {
    engine
        .get_terminal_state()
        .list_image_placements()
        .iter()
        .map(|image_placement| image_placement.get_image_anchor())
        .collect()
}

#[test]
fn a_relative_placement_uses_its_parent_and_keeps_the_cursor_in_place() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=1,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,H=2,V=1,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(0, 0), (1, 2)]);
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
}

#[test]
fn a_relative_placement_can_share_its_image_id_with_its_parent() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=2,P=7,H=2,V=1,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );

    assert_eq!(list_image_anchors(&engine), [(0, 0), (1, 2)]);
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (0, 0)
    );
}

#[test]
fn an_existing_relative_placement_uses_only_earlier_parent_placements() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    for placement_command_bytes in [
        b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\".as_slice(),
        b"\x1b_Ga=p,i=7,p=2,P=7,V=1,c=1,r=1,C=0,q=2\x1b\\".as_slice(),
        b"\x1b_Ga=p,i=7,p=3,P=7,V=20,c=1,r=1,C=0,q=2\x1b\\".as_slice(),
    ] {
        assert_eq!(engine.process_pty_output(placement_command_bytes), b"");
    }

    let relative_child_placement = engine
        .get_terminal_state()
        .primary_image_placements
        .iter()
        .find(|image_placement| image_placement.image_record.display.placement_id == Some(2))
        .expect("the middle relative placement is retained");
    assert_eq!(
        engine.get_terminal_state().resolve_relative_anchor(
            &relative_child_placement.image_record,
            Some(relative_child_placement.image_placement_id),
        ),
        Ok(Some((1, 0)))
    );
}

#[test]
fn erasing_the_screen_removes_a_relative_placement_with_a_placeholder_parent() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=3,c=1,r=1,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=4,P=7,Q=3,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );
    assert_eq!(engine.get_terminal_state().list_image_placements().len(), 1);

    assert_eq!(engine.process_pty_output(b"\x1b[2J"), b"");
    assert!(engine
        .get_terminal_state()
        .list_image_placements()
        .is_empty());
    assert!(engine
        .get_terminal_state()
        .list_image_placements_for_view(0)
        .is_empty());
}

#[test]
fn a_relative_placement_without_a_parent_is_rejected_without_state_change() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,c=1,r=1,q=0\x1b\\"),
        b"\x1b_Gi=8,p=2;ENOPARENT:relative image parent not found\x1b\\"
    );
    assert_eq!(list_image_anchors(&engine), []);
}

#[test]
fn a_relative_chain_beyond_the_supported_depth_reports_etoodeep() {
    let mut engine = build_terminal_engine();
    for image_id in 1..=10 {
        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=t,i={image_id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()
            ),
            b""
        );
    }
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=1,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    for image_id in 2..=9 {
        let parent_image_id = image_id - 1;
        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=p,i={image_id},p={image_id},P={parent_image_id},Q={parent_image_id},c=1,r=1,C=1,q=2\x1b\\")
                    .as_bytes()
            ),
            b""
        );
    }
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=10,p=10,P=9,Q=9,c=1,r=1,C=1,q=0\x1b\\"),
        b"\x1b_Gi=10,p=10;ETOODEEP:relative image chain too deep\x1b\\"
    );
}

#[test]
fn a_relative_cycle_reports_ecycle() {
    let mut engine = build_terminal_engine();
    for image_id in 1..=2 {
        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=t,i={image_id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()
            ),
            b""
        );
    }
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=1,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=2,p=2,P=1,Q=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );

    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=1,p=1,P=2,Q=2,c=1,r=1,C=1,q=0\x1b\\"),
        b"\x1b_Gi=1,p=1;ECYCLE:relative image cycle\x1b\\"
    );
}

#[test]
fn deleting_a_relative_parent_also_deletes_its_children() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(0, 0), (0, 0)]);
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"),
        b""
    );
    assert!(list_image_anchors(&engine).is_empty());
}

#[test]
fn visible_delete_keeps_a_relative_image_outside_the_horizontal_view() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=8,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,H=-20,V=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(engine.process_pty_output(b"\x1b[S"), b"");
    let list_kitty_image_ids = |engine: &TerminalEngine| {
        let mut kitty_image_ids = engine
            .get_terminal_state()
            .primary_image_history
            .iter()
            .map(|image_placement| image_placement.image_record.display.image_id)
            .collect::<Vec<_>>();
        kitty_image_ids.extend(
            engine
                .get_terminal_state()
                .primary_image_placements
                .iter()
                .map(|image_placement| image_placement.image_record.display.image_id),
        );
        kitty_image_ids
    };
    assert_eq!(list_kitty_image_ids(&engine), [Some(7), Some(8)]);
    assert_eq!(
        engine.get_terminal_state().primary_image_history[0]
            .image_record
            .display
            .image_id,
        Some(7)
    );
    let relative_child_placement = &engine.get_terminal_state().primary_image_placements[0];
    assert_eq!(
        (
            relative_child_placement.image_record.display.image_id,
            relative_child_placement.column_count,
            relative_child_placement.row_count,
            engine.get_terminal_state().resolve_relative_anchor(
                &relative_child_placement.image_record,
                Some(relative_child_placement.image_placement_id),
            ),
        ),
        (Some(8), 1, 1, Ok(Some((2, -20))))
    );

    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d,d=a,q=2\x1b\\"), b"");

    assert_eq!(list_kitty_image_ids(&engine), [Some(7), Some(8)]);
}

#[test]
fn a_unicode_placeholder_is_rendered_from_the_matching_cell() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert!(engine
        .get_terminal_state()
        .list_image_placements()
        .is_empty());
    assert_eq!(
        engine.process_pty_output("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    let image_placements = engine
        .get_terminal_state()
        .list_image_placements_for_view(0);
    assert_eq!(image_placements.len(), 1);
    assert_eq!(image_placements[0].get_image_anchor(), (0, 0));
    assert_eq!(image_placements[0].get_image_cell_dimensions(), (1, 1));
    assert_eq!(
        image_placements[0].get_image_geometry().cell_offset,
        koshi_core::geometry::Point { column: 0, row: 0 }
    );
}

#[test]
fn a_unicode_placeholder_follows_the_image_into_scrollback() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    assert_eq!(
        engine
            .get_terminal_state()
            .list_image_placements_for_view(0)
            .len(),
        1
    );

    assert_eq!(engine.process_pty_output(b"\x1b[10;1H\n"), b"");

    let scrolled_image_placements = engine
        .get_terminal_state()
        .list_image_placements_for_view(1);
    assert_eq!(scrolled_image_placements.len(), 1);
    assert_eq!(
        scrolled_image_placements[0].get_image_cell_dimensions(),
        (1, 1)
    );
    assert_eq!(
        scrolled_image_placements[0].get_image_geometry().full_size,
        koshi_core::geometry::Size {
            column_count: 2,
            row_count: 2
        }
    );
}

#[test]
fn a_relative_placement_uses_the_top_left_of_a_partial_unicode_placeholder() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output("\x1b[38;5;7m\u{10eeee}\u{030d}\u{0305}".as_bytes()),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,p=4,P=7,Q=3,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );

    let relative_child_placement = engine
        .get_terminal_state()
        .primary_image_placements
        .iter()
        .find(|image_placement| image_placement.image_record.display.placement_id == Some(4))
        .expect("the relative child is retained");
    assert_eq!(
        engine.get_terminal_state().resolve_relative_anchor(
            &relative_child_placement.image_record,
            Some(relative_child_placement.image_placement_id),
        ),
        Ok(Some((-1, 0)))
    );
    let visible_image_placements = engine
        .get_terminal_state()
        .list_image_placements_for_view(0);
    assert_eq!(visible_image_placements.len(), 1);
    assert_eq!(
        visible_image_placements[0]
            .get_image_geometry()
            .cell_offset
            .row,
        1
    );
}

#[test]
fn kitty_animation_frame_control_changes_the_displayed_frame() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=a,i=7,c=2,s=3,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [0, 0, 255, 255]
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=f,i=7,r=2,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn kitty_animation_loop_value_counts_completed_playbacks_after_restore() {
    for (animation_loop_value, expected_playback_count) in [(2, 1), (3, 2)] {
        let mut source_terminal_engine = build_terminal_engine();
        display_animation_base(&mut source_terminal_engine, 1, 1, &[255, 0, 0, 255]);
        assert_eq!(
            source_terminal_engine
                .process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
            b""
        );
        assert_eq!(
            source_terminal_engine.process_pty_output(
                format!("\x1b_Ga=a,i=7,s=3,v={animation_loop_value},q=2\x1b\\").as_bytes(),
            ),
            b""
        );
        let terminal_state: TerminalState = serde_json::from_slice(
            &serde_json::to_vec(source_terminal_engine.get_terminal_state())
                .expect("the state serializes"),
        )
        .expect("the state restores");
        let mut restored_terminal_engine = TerminalEngine::from_terminal_state(terminal_state, &[]);
        let image_placements = restored_terminal_engine
            .get_terminal_state()
            .list_image_placements();
        let animation = image_placements[0]
            .get_image_record()
            .animation
            .as_ref()
            .expect("the animation is retained");
        assert_eq!(
            animation.get_loop_policy(),
            LoopPolicy::from_finite_playback_count(expected_playback_count)
                .expect("the playback count is valid")
        );
        assert_eq!(
            animation.list_frames()[0].get_decoded_image().rgba_bytes,
            [255, 0, 0, 255]
        );
        assert_eq!(
            animation.list_frames()[1].get_decoded_image().rgba_bytes,
            [0, 0, 255, 255]
        );
        assert_eq!(
            animation.list_frames()[0]
                .get_frame_delay()
                .get_numerator_ms(),
            0
        );
        assert_eq!(
            animation.list_frames()[1]
                .get_frame_delay()
                .get_numerator_ms(),
            10
        );
        assert_eq!(
            restored_terminal_engine
                .get_terminal_state()
                .get_next_image_animation_delay(),
            Some(Duration::from_millis(8))
        );

        if expected_playback_count == 2 {
            let _ = restored_terminal_engine.advance_image_animations(Duration::from_millis(18));
            assert_eq!(
                restored_terminal_engine
                    .get_terminal_state()
                    .list_image_placements()[0]
                    .get_image_record()
                    .image
                    .rgba_bytes,
                [255, 0, 0, 255]
            );
            assert_eq!(
                restored_terminal_engine
                    .get_terminal_state()
                    .get_next_image_animation_delay(),
                Some(Duration::from_millis(8))
            );
        }
        let _ = restored_terminal_engine.advance_image_animations(Duration::from_millis(18));
        assert_eq!(
            restored_terminal_engine
                .get_terminal_state()
                .list_image_placements()[0]
                .get_image_record()
                .image
                .rgba_bytes,
            [0, 0, 255, 255]
        );
        assert_eq!(
            restored_terminal_engine
                .get_terminal_state()
                .get_next_image_animation_delay(),
            None
        );
    }
}

#[test]
fn kitty_png_animation_frame_uses_png_dimensions() {
    let mut engine = build_terminal_engine();
    display_animation_base(&mut engine, 2, 1, &[255, 0, 0, 255, 255, 0, 0, 255]);
    let encoded_animation_frame_payload = STANDARD.encode(build_rgba_png(&[0, 0, 255, 255], 1, 1));

    assert_eq!(
        engine.process_pty_output(
            format!(
                "\x1b_Ga=f,i=7,f=100,s=2,v=2,x=1,c=1,X=1,q=2;{encoded_animation_frame_payload}\x1b\\"
            )
            .as_bytes()
        ),
        b""
    );
    select_second_frame(&mut engine);

    let image_placements = engine.get_terminal_state().list_image_placements();
    let displayed_image = &image_placements[0].get_image_record().image;
    assert_eq!(
        (displayed_image.pixel_width, displayed_image.pixel_height),
        (2, 1)
    );
    assert_eq!(
        displayed_image.rgba_bytes,
        [255, 0, 0, 255, 0, 0, 255, 255],
        "the 1x1 PNG updates one pixel even when s and v declare 2x2"
    );
}

#[test]
fn kitty_apng_animation_frame_uses_only_the_default_png_image() {
    let mut engine = build_terminal_engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);
    let encoded_apng_payload = STANDARD.encode(build_apng_with_separate_default(
        [0, 0, 255, 255],
        [0, 255, 0, 255],
    ));

    assert_eq!(
        engine.process_pty_output(
            format!("\x1b_Ga=f,i=7,f=100,q=2;{encoded_apng_payload}\x1b\\").as_bytes(),
        ),
        b""
    );
    select_second_frame(&mut engine);

    let image_placements = engine.get_terminal_state().list_image_placements();
    let animation = image_placements[0]
        .get_image_record()
        .animation
        .as_ref()
        .expect("the Kitty animation is retained");
    assert_eq!(animation.get_frame_count(), 2);
    assert_eq!(
        animation.list_frames()[0].get_decoded_image().rgba_bytes,
        [255, 0, 0, 255]
    );
    assert_eq!(
        animation.list_frames()[1].get_decoded_image().rgba_bytes,
        [0, 0, 255, 255]
    );
}

#[test]
fn kitty_png_animation_frame_rejects_invalid_or_mislabeled_data_atomically() {
    let mut jpeg_bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg_bytes)
        .write_image(&[0, 255, 0], 1, 1, image::ColorType::Rgb8.into())
        .expect("the JPEG encodes");

    for invalid_image_bytes in [b"\x89PNG\r\n\x1a\n".to_vec(), jpeg_bytes] {
        let mut engine = build_terminal_engine();
        display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);
        let encoded_invalid_image_payload = STANDARD.encode(invalid_image_bytes);

        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=f,i=7,f=100;{encoded_invalid_image_payload}\x1b\\").as_bytes()
            ),
            b"\x1b_Gi=7;EINVAL:invalid image placement\x1b\\"
        );

        let image_placement = &engine.get_terminal_state().list_image_placements()[0];
        assert_eq!(
            image_placement.get_image_record().image.rgba_bytes,
            [255, 0, 0, 255]
        );
        assert_eq!(image_placement.get_image_record().animation, None);
    }
}

#[test]
fn kitty_rgb_animation_frame_remains_opaque_rgba() {
    let mut engine = build_terminal_engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=f,i=7,f=24,s=1,v=1,q=2;AP8A\x1b\\"),
        b""
    );
    select_second_frame(&mut engine);

    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [0, 255, 0, 255]
    );
}

#[test]
fn animation_frame_number_beyond_the_next_frame_appends() {
    let mut engine = build_terminal_engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,r=999,q=2;AAD//w==\x1b\\"),
        b""
    );

    let image_placements = engine.get_terminal_state().list_image_placements();
    let animation = image_placements[0]
        .get_image_record()
        .animation
        .as_ref()
        .expect("the appended frame is retained");
    assert_eq!(animation.get_frame_count(), 2);
    assert_eq!(
        animation.list_frames()[0].get_decoded_image().rgba_bytes,
        [255, 0, 0, 255]
    );
    assert_eq!(
        animation.list_frames()[1].get_decoded_image().rgba_bytes,
        [0, 0, 255, 255]
    );
}

#[test]
fn animation_delete_defaults_to_the_root_and_clamps_to_the_last_frame() {
    let mut root_animation_engine = build_terminal_engine();
    display_animation_base(&mut root_animation_engine, 1, 1, &[255, 0, 0, 255]);
    add_blue_and_green_frames(&mut root_animation_engine);
    assert_eq!(
        root_animation_engine.process_pty_output(b"\x1b_Ga=d,d=f,i=7,q=0\x1b\\"),
        b""
    );
    let root_image_placements = root_animation_engine
        .get_terminal_state()
        .list_image_placements();
    let root_animation = root_image_placements[0]
        .get_image_record()
        .animation
        .as_ref()
        .expect("two frames remain");
    assert_eq!(
        root_animation.list_frames()[0]
            .get_decoded_image()
            .rgba_bytes,
        [0, 0, 255, 255]
    );
    assert_eq!(
        root_animation.list_frames()[1]
            .get_decoded_image()
            .rgba_bytes,
        [0, 255, 0, 255]
    );

    let mut last_animation_engine = build_terminal_engine();
    display_animation_base(&mut last_animation_engine, 1, 1, &[255, 0, 0, 255]);
    add_blue_and_green_frames(&mut last_animation_engine);
    assert_eq!(
        last_animation_engine.process_pty_output(b"\x1b_Ga=d,d=f,i=7,r=999,q=0\x1b\\"),
        b""
    );
    let last_image_placements = last_animation_engine
        .get_terminal_state()
        .list_image_placements();
    let last_animation = last_image_placements[0]
        .get_image_record()
        .animation
        .as_ref()
        .expect("two frames remain");
    assert_eq!(
        last_animation.list_frames()[0]
            .get_decoded_image()
            .rgba_bytes,
        [255, 0, 0, 255]
    );
    assert_eq!(
        last_animation.list_frames()[1]
            .get_decoded_image()
            .rgba_bytes,
        [0, 0, 255, 255]
    );
}

#[test]
fn deleting_the_selected_root_middle_or_last_frame_selects_the_exact_survivor() {
    for (selected_frame_number, deleted_frame_number, expected_frame_rgba_bytes) in [
        (1, 1, [0, 0, 255, 255]),
        (2, 2, [0, 255, 0, 255]),
        (3, 3, [0, 0, 255, 255]),
    ] {
        let mut engine = build_terminal_engine();
        display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);
        add_blue_and_green_frames(&mut engine);
        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=a,i=7,c={selected_frame_number},s=3,q=0\x1b\\").as_bytes(),
            ),
            b""
        );
        assert_eq!(
            engine.process_pty_output(
                format!("\x1b_Ga=d,d=f,i=7,r={deleted_frame_number},q=0\x1b\\").as_bytes(),
            ),
            b""
        );
        assert_eq!(
            engine.get_terminal_state().list_image_placements()[0]
                .get_image_record()
                .image
                .rgba_bytes,
            expected_frame_rgba_bytes,
            "selected frame {selected_frame_number}"
        );
    }
}

#[test]
fn frame_delete_without_extra_frames_obeys_lowercase_and_uppercase_forms() {
    let mut engine = build_terminal_engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=f,i=7,q=0\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(0, 0)]);
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=F,i=7,q=0\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), []);
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,c=1,r=1,q=0\x1b\\"),
        b"\x1b_Gi=7;ENOENT:image not found\x1b\\"
    );
}

#[test]
fn animation_reply_policy_matches_command_kind() {
    let mut engine = build_terminal_engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=0;AAD//w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=c,i=7,r=1,c=2,w=1,h=1,q=0\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=a,i=7,c=2,s=3,q=0\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=a,i=999,c=1,s=3,q=0\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=f,i=7,r=2,q=0\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=f,i=999,r=1,q=0\x1b\\"),
        b""
    );
}

#[test]
fn animation_frame_and_relative_errors_use_specific_kitty_codes() {
    let mut animation_engine = build_terminal_engine();
    display_animation_base(&mut animation_engine, 1, 1, &[255, 0, 0, 255]);
    assert_eq!(
        animation_engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,c=99,q=0;AAD//w==\x1b\\"),
        b"\x1b_Gi=7;ENOENT:animation frame not found\x1b\\"
    );

    let mut terminal_state = build_terminal_engine().into_terminal_state();
    let image_display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        ..ImageDisplay::default()
    };
    for (image_placement_error, expected_reply_bytes) in [
        (
            ImagePlacementError::ParentNotFound,
            b"\x1b_Gi=7,p=3;ENOPARENT:relative image parent not found\x1b\\".as_slice(),
        ),
        (
            ImagePlacementError::RelativeCycle,
            b"\x1b_Gi=7,p=3;ECYCLE:relative image cycle\x1b\\".as_slice(),
        ),
        (
            ImagePlacementError::RelativeDepth,
            b"\x1b_Gi=7,p=3;ETOODEEP:relative image chain too deep\x1b\\".as_slice(),
        ),
    ] {
        terminal_state.reply_to_kitty(&image_display, Some(image_placement_error), true);
        assert_eq!(
            terminal_state.take_device_query_replies(),
            expected_reply_bytes
        );
    }
}

#[test]
fn loading_animation_state_survives_a_state_restore() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=a,i=7,s=2,q=2\x1b\\"),
        b""
    );

    let serialized_terminal_state =
        serde_json::to_value(engine.get_terminal_state()).expect("serialize loading animation");
    let restored_terminal_state: TerminalState =
        serde_json::from_value(serialized_terminal_state).expect("restore loading animation");

    assert_eq!(
        restored_terminal_state.list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn redraw_removes_the_old_position_and_displays_the_stored_image() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(b"\x1b[3;1H"), b"");
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(list_image_anchors(&engine), [(2, 0)]);
    assert_eq!(
        engine.process_pty_output(
            b"\x1b_Ga=d,d=a,q=2\x1b\\\x1b[6;1H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\"
        ),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(5, 0)]);
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (5, 0)
    );
}

#[test]
fn placing_one_upload_twice_shares_pixels_and_keeps_both_positions() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(
        engine.process_pty_output(b"\x1b[5;4H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(0, 0), (4, 3)]);
    let image_placements = engine.get_terminal_state().list_image_placements();
    assert!(Arc::ptr_eq(
        &image_placements[0].get_image_record().image,
        &image_placements[1].get_image_record().image
    ));
    assert_eq!(
        image_placements[0].get_image_content_id(),
        image_placements[1].get_image_content_id(),
        "one Kitty upload has one canonical content identity"
    );
}

#[test]
fn retransmitting_a_kitty_id_gets_a_new_content_identity() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    let first_image_content_id =
        engine.get_terminal_state().list_image_placements()[0].get_image_content_id();

    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    let image_placements = engine.get_terminal_state().list_image_placements();
    assert_eq!(image_placements.len(), 1);
    assert_ne!(
        first_image_content_id,
        image_placements[0].get_image_content_id()
    );
}

#[test]
fn a_named_placement_moves_without_adding_another_placement() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    let image_placement_id =
        engine.get_terminal_state().list_image_placements()[0].get_image_placement_id();
    assert_eq!(
        engine.process_pty_output(b"\x1b[7;2H\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(6, 1)]);
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0].get_image_placement_id(),
        image_placement_id
    );
}

#[test]
fn freeing_an_upload_makes_a_new_placement_report_the_missing_image() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d,d=I,i=7\x1b\\"), b"");
    assert_eq!(list_image_anchors(&engine), []);
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=7,c=1,r=1,C=1\x1b\\"),
        b"\x1b_Gi=7;ENOENT:image not found\x1b\\"
    );
    assert_eq!(list_image_anchors(&engine), []);
}

#[test]
fn stored_pixels_survive_a_state_restore_with_no_visible_placement() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d\x1b\\"), b"");
    let restored_terminal_state: TerminalState = serde_json::from_slice(
        &serde_json::to_vec(engine.get_terminal_state()).expect("serialize"),
    )
    .expect("restore");
    let mut restored_terminal_engine =
        TerminalEngine::from_terminal_state(restored_terminal_state, &[]);
    assert_eq!(
        restored_terminal_engine
            .process_pty_output(b"\x1b[4;3H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&restored_terminal_engine), [(3, 2)]);
    assert_eq!(
        restored_terminal_engine
            .get_terminal_state()
            .list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn query_replies_before_device_attributes_and_does_not_store_pixels() {
    let mut engine = build_terminal_engine();
    let graphics_device_response =
        engine.process_pty_output(b"\x1b_Ga=q,i=31,f=32,s=1,v=1;/wAA/w==\x1b\\\x1b[c");
    assert_eq!(graphics_device_response, b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c");
    assert_eq!(engine.get_terminal_state().kitty_images, []);
    assert_eq!(list_image_anchors(&engine), []);
    assert_eq!(engine.take_graphics_events(), []);
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=p,i=31,c=1,r=1\x1b\\"),
        b"\x1b_Gi=31;ENOENT:image not found\x1b\\"
    );
}

#[test]
fn every_byte_split_keeps_the_same_redraw_result() {
    let redraw_bytes = b"\x1b_Ga=d\x1b\\\x1b[4;3H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\";
    for split_byte_index in 0..=redraw_bytes.len() {
        let mut engine = build_terminal_engine();
        assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
        assert_eq!(
            engine.process_pty_output(&redraw_bytes[..split_byte_index]),
            b""
        );
        assert_eq!(
            engine.process_pty_output(&redraw_bytes[split_byte_index..]),
            b""
        );
        assert_eq!(
            list_image_anchors(&engine),
            [(3, 2)],
            "split {split_byte_index}"
        );
    }
}

#[test]
fn deleting_aborts_an_incomplete_upload_and_allows_a_new_one() {
    let mut engine = build_terminal_engine();
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1,i=7,q=2,m=1;/wAA\x1b\\"),
        b""
    );
    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d\x1b\\"), b"");
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(list_image_anchors(&engine), [(0, 0)]);
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn deleting_aborts_an_incomplete_animation_upload_and_executes_the_delete() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,m=1,q=2;AAD/\x1b\\"),
        b""
    );

    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d,d=a,q=2\x1b\\"), b"");
    assert_eq!(list_image_anchors(&engine), []);
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(list_image_anchors(&engine), [(0, 0)]);
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0]
            .get_image_record()
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn deleting_one_named_placement_keeps_pixels_used_by_another() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(engine.process_pty_output(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\\x1b[5;1H\x1b_Ga=p,i=7,p=2,c=1,r=1,C=1,q=2\x1b\\"), b"");
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=I,i=7,p=1\x1b\\"),
        b""
    );
    assert_eq!(list_image_anchors(&engine), [(4, 0)]);
    assert_eq!(
        engine.get_terminal_state().kitty_images[0].display.image_id,
        Some(7)
    );
    assert_eq!(
        engine.process_pty_output(b"\x1b_Ga=d,d=I,i=7,p=2\x1b\\"),
        b""
    );
    assert_eq!(engine.get_terminal_state().kitty_images, []);
}

#[test]
fn uppercase_visible_delete_frees_only_unreferenced_upload_data() {
    let upload_command_bytes = b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2;/wAA/w==\x1b\\";
    let placement_command_bytes = b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\";

    let mut sole_upload_engine = build_terminal_engine();
    assert_eq!(
        sole_upload_engine.process_pty_output(upload_command_bytes),
        b""
    );
    assert_eq!(
        sole_upload_engine.process_pty_output(placement_command_bytes),
        b""
    );
    assert_eq!(
        sole_upload_engine.process_pty_output(b"\x1b_Ga=d,d=A,q=2\x1b\\"),
        b""
    );
    assert_eq!(sole_upload_engine.get_terminal_state().kitty_images, []);
    assert_eq!(
        sole_upload_engine
            .get_terminal_state()
            .list_image_placements(),
        []
    );

    let mut shared_upload_engine = build_terminal_engine();
    assert_eq!(
        shared_upload_engine.process_pty_output(upload_command_bytes),
        b""
    );
    assert_eq!(
        shared_upload_engine.process_pty_output(placement_command_bytes),
        b""
    );
    assert_eq!(
        shared_upload_engine.process_pty_output(b"\x1b_Ga=p,i=7,p=2,c=1,r=1,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        shared_upload_engine.process_pty_output(b"\x1b_Ga=d,d=A,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        shared_upload_engine
            .get_terminal_state()
            .list_image_placements(),
        []
    );
    assert_eq!(
        shared_upload_engine.get_terminal_state().kitty_images.len(),
        2
    );
    assert_eq!(
        shared_upload_engine.process_pty_output(b"\x1b_Ga=d,d=I,i=7,p=2,q=2\x1b\\"),
        b""
    );
    assert_eq!(shared_upload_engine.get_terminal_state().kitty_images, []);
}

#[test]
fn a_hard_reset_removes_uploads_as_well_as_placements() {
    let mut engine = build_terminal_engine();
    assert_eq!(engine.process_pty_output(KITTY_UPLOAD_SEQUENCE_BYTES), b"");
    assert_eq!(engine.process_pty_output(b"\x1bc"), b"");
    assert_eq!(engine.get_terminal_state().kitty_images, []);
    assert_eq!(list_image_anchors(&engine), []);
}
