//! Protocol-driven checks for Kitty upload and placement lifetimes.

use super::*;

use crate::engine::TerminalEngine;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::ImageEncoder;
use koshi_core::process::PtySize;
use std::time::Duration;

const UPLOAD: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=1,c=2,r=2,C=1,i=7,q=2;/wAA/w==\x1b\\";

fn engine() -> TerminalEngine {
    TerminalEngine::new(PtySize { cols: 20, rows: 10 })
}

fn rgba_png(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(rgba, width, height, image::ColorType::Rgba8.into())
        .expect("the PNG encodes");
    bytes
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12 + data.len());
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for &byte in kind.iter().chain(data) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    bytes.extend_from_slice(&(!crc).to_be_bytes());
    bytes
}

fn png_image_data(png: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut at = 8;
    while at < png.len() {
        let length = usize::try_from(u32::from_be_bytes(
            png[at..at + 4].try_into().expect("PNG chunk length"),
        ))
        .expect("PNG chunk length fits");
        let end = at + 12 + length;
        if &png[at + 4..at + 8] == b"IDAT" {
            data.extend_from_slice(&png[at + 8..end - 4]);
        }
        at = end;
    }
    data
}

fn apng_with_separate_default(default: [u8; 4], frame: [u8; 4]) -> Vec<u8> {
    let default_png = rgba_png(&default, 1, 1);
    let frame_data = png_image_data(&rgba_png(&frame, 1, 1));
    let mut frame_control = [0; 26];
    frame_control[4..8].copy_from_slice(&1u32.to_be_bytes());
    frame_control[8..12].copy_from_slice(&1u32.to_be_bytes());
    frame_control[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame_control[22..24].copy_from_slice(&10u16.to_be_bytes());
    let mut animation_data = 1u32.to_be_bytes().to_vec();
    animation_data.extend_from_slice(&frame_data);

    let mut bytes = default_png[..33].to_vec();
    bytes.extend_from_slice(&png_chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
    bytes.extend_from_slice(&png_chunk(b"IDAT", &png_image_data(&default_png)));
    bytes.extend_from_slice(&png_chunk(b"fcTL", &frame_control));
    bytes.extend_from_slice(&png_chunk(b"fdAT", &animation_data));
    bytes.extend_from_slice(&png_chunk(b"IEND", &[]));
    bytes
}

fn display_animation_base(engine: &mut TerminalEngine, width: u32, height: u32, rgba: &[u8]) {
    let payload = STANDARD.encode(rgba);
    let command = format!(
        "\x1b_Ga=T,i=7,f=32,s={width},v={height},c={width},r={height},C=1,q=2;{payload}\x1b\\"
    );
    assert_eq!(engine.advance(command.as_bytes()), b"");
}

fn select_second_frame(engine: &mut TerminalEngine) {
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=7,c=2,s=1,q=2\x1b\\"), b"");
}

fn add_blue_and_green_frames(engine: &mut TerminalEngine) {
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=2;AP8A/w==\x1b\\"),
        b""
    );
}

#[test]
fn a_chat_scroll_with_an_auto_scrollbar_preserves_kitty_pixels() {
    assert_chat_scroll_preserves_pixels(true);
}

#[test]
fn kitty_placements_survive_text_writes_on_both_screens_and_in_history() {
    for screen in ["", "\x1b[?1049h", "history"] {
        for z in [-2, -1, 0, 1] {
            for text in [" ", "a", "界", "a\u{0301}", "界\u{0301}"] {
                let mut engine = engine();
                if screen != "history" {
                    assert_eq!(engine.advance(screen.as_bytes()), b"");
                }
                assert_eq!(
                    engine.advance(
                        format!("\x1b_Ga=T,f=32,s=1,v=1,c=4,r=4,C=1,i=7,z={z},q=2;/wAA/w==\x1b\\")
                            .as_bytes()
                    ),
                    b""
                );
                if screen == "history" {
                    assert_eq!(engine.advance(b"\x1b[2S"), b"");
                }
                let before = engine.state().image_placements_for_view(0);
                assert_eq!(
                    before
                        .iter()
                        .map(|p| p.record.image.rgba.clone())
                        .collect::<Vec<_>>(),
                    [vec![255, 0, 0, 255]]
                );
                let history = engine.state().image_placements_for_view(2);
                assert_eq!(engine.advance(b"\x1b[1;1H"), b"");
                assert_eq!(engine.advance(text.as_bytes()), b"");
                assert_eq!(
                    engine.state().image_placements_for_view(0),
                    before,
                    "screen={screen:?}, z={z}, text={text:?}"
                );
                assert_eq!(
                    engine.state().image_placements_for_view(2),
                    history,
                    "screen={screen:?}, z={z}, text={text:?}"
                );
            }
        }
    }
}

#[test]
fn overwriting_a_unicode_placeholder_removes_only_its_visible_cells() {
    for screen in [b"".as_slice(), b"\x1b[?1049h"] {
        let mut engine = engine();
        assert_eq!(engine.advance(screen), b"");
        assert_eq!(engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\\x1b_Ga=p,i=7,p=3,c=1,r=1,U=1,C=1,q=2\x1b\\"), b"");
        let marker = "\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}";
        assert_eq!(engine.advance(marker.as_bytes()), b"");
        let before = engine.state().image_placements_for_view(0);
        assert_eq!(
            before
                .iter()
                .map(|p| p.record.image.rgba.clone())
                .collect::<Vec<_>>(),
            [vec![255, 0, 0, 255]]
        );
        assert_eq!(engine.advance(b"\x1b[1;1H "), b"");
        assert_eq!(engine.state().image_placements_for_view(0), []);
        assert_eq!(engine.advance(b"\x1b[1;1H"), b"");
        assert_eq!(engine.advance(marker.as_bytes()), b"");
        assert_eq!(engine.state().image_placements_for_view(0), before);
    }
}

#[test]
fn a_chat_scroll_with_a_hidden_scrollbar_preserves_kitty_pixels() {
    assert_chat_scroll_preserves_pixels(false);
}

fn assert_chat_scroll_preserves_pixels(scrollbar: bool) {
    let mut engine = TerminalEngine::new(PtySize { cols: 20, rows: 14 });
    assert_eq!(engine.advance(b"\x1b[?1049h\x1b[?2026h\x1b[1;1H\x1b[2K\x1b_Ga=T,f=32,s=1,v=1,i=42,c=4,C=1,y=0,h=1,r=1,q=2;/wAA/w==\x1b\\\x1b[?2026l"), b"");
    for (image_row, rows, source) in [(1, 3, ",y=0,h=1"), (2, 4, "")] {
        assert_eq!(engine.advance(b"\x1b[?2026h\x1b_Ga=d,d=a,q=2\x1b\\"), b"");
        for row in 1..=10 {
            let mut line = format!("\x1b[{row};1H\x1b[2K");
            if row == image_row {
                line.push_str(&format!(
                    "\x1b_Ga=p,q=2,i=42,c=4,r={rows},C=1{source}\x1b\\"
                ));
            } else {
                let text = if row < image_row {
                    "before".to_owned()
                } else if row < image_row + rows {
                    String::new()
                } else {
                    format!("after{}", row - image_row - rows)
                };
                if scrollbar {
                    line.push_str(&format!("{text:19}\x1b[90m│\x1b[39m"));
                } else {
                    line.push_str(&text);
                }
                line.push_str("\x1b[0m\x1b]8;;\x1b\\");
            }
            assert_eq!(engine.advance(line.as_bytes()), b"");
        }
        assert_eq!(engine.advance(b"\x1b[?2026l"), b"");
        let placements = engine.state().image_placements();
        assert_eq!(
            placements
                .iter()
                .map(|p| (p.anchor(), p.dimensions(), p.record.image.rgba.clone()))
                .collect::<Vec<_>>(),
            [((image_row - 1, 0), (rows, 4), vec![255, 0, 0, 255])]
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

fn assert_alternate_screen_redraw(place: &[u8]) {
    let mut engine = engine();
    assert_eq!(engine.advance(b"\x1b[?1049h"), b"");
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=a,q=2\x1b\\"), b"");
    assert_eq!(anchors(&engine), []);
    assert_eq!(engine.advance(b"\x1b[5;3H"), b"");
    assert_eq!(engine.advance(place), b"\x1b_Gi=7;OK\x1b\\");
    assert_eq!(anchors(&engine), [(4, 2)]);
    let placements = engine.state().image_placements();
    assert_eq!(placements[0].dimensions(), (2, 2));
    assert_eq!(
        placements[0].geometry().full_size,
        koshi_core::geometry::Size { cols: 2, rows: 2 }
    );
    assert_eq!(placements[0].record.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_failed_numbered_upload_does_not_reply_with_an_older_image_id() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,I=9,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=1,I=9;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,I=9,f=32,s=1,v=1,c=1,r=1,U=1,P=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=0,I=9;EINVAL:invalid image placement\x1b\\"
    );
    assert_eq!(
        engine
            .state()
            .kitty_images
            .iter()
            .map(|image| image.display.image_id)
            .collect::<Vec<_>>(),
        [Some(1)]
    );
}

#[test]
fn a_continuation_error_uses_the_first_chunks_reply_identity() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=9,f=32,s=1,v=1,m=1;/wAA\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Gm=0;!\x1b\\"),
        b"\x1b_Gi=9;EINVAL:invalid image data\x1b\\"
    );
    assert_eq!(engine.state().kitty_images, []);
}

#[test]
fn a_delete_range_frees_uploads_without_placements() {
    let mut engine = engine();
    for id in 1..=3 {
        assert_eq!(
            engine.advance(format!("\x1b_Ga=t,i={id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()),
            b""
        );
    }
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=R,x=1,y=2\x1b\\"), b"");
    assert_eq!(
        engine
            .state()
            .kitty_images
            .iter()
            .map(|image| image.display.image_id)
            .collect::<Vec<_>>(),
        [Some(3)]
    );
}

#[test]
fn restoring_old_placements_rebuilds_the_numbered_upload_store() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,I=9,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let mut value = serde_json::to_value(engine.state()).expect("serialize");
    value
        .as_object_mut()
        .expect("object")
        .remove("kitty_images");
    let state: TerminalState = serde_json::from_value(value).expect("restore old state");
    let mut engine = TerminalEngine::from_state(state, &[]);
    assert_eq!(
        engine.advance(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,I=9,c=2,r=3,C=1\x1b\\"),
        b"\x1b_Gi=1,I=9;OK\x1b\\"
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(
        (placement.anchor(), placement.columns, placement.rows),
        ((0, 0), 2, 3)
    );
}

fn anchors(engine: &TerminalEngine) -> Vec<(u16, u16)> {
    engine
        .state()
        .image_placements()
        .iter()
        .map(|p| p.anchor())
        .collect()
}

#[test]
fn a_relative_placement_uses_its_parent_and_keeps_the_cursor_in_place() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=1,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,H=2,V=1,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(0, 0), (1, 2)]);
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
}

#[test]
fn a_relative_placement_can_share_its_image_id_with_its_parent() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=2,P=7,H=2,V=1,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );

    assert_eq!(anchors(&engine), [(0, 0), (1, 2)]);
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
}

#[test]
fn an_existing_relative_placement_uses_only_earlier_parent_placements() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    for command in [
        b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\".as_slice(),
        b"\x1b_Ga=p,i=7,p=2,P=7,V=1,c=1,r=1,C=0,q=2\x1b\\".as_slice(),
        b"\x1b_Ga=p,i=7,p=3,P=7,V=20,c=1,r=1,C=0,q=2\x1b\\".as_slice(),
    ] {
        assert_eq!(engine.advance(command), b"");
    }

    let child = engine
        .state()
        .primary_image_placements
        .iter()
        .find(|placement| placement.record.display.placement_id == Some(2))
        .expect("the middle relative placement is retained");
    assert_eq!(
        engine
            .state()
            .resolve_relative_anchor(&child.record, Some(child.id)),
        Ok(Some((1, 0)))
    );
}

#[test]
fn erasing_the_screen_removes_a_relative_placement_with_a_placeholder_parent() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=3,c=1,r=1,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=4,P=7,Q=3,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );
    assert_eq!(engine.state().image_placements().len(), 1);

    assert_eq!(engine.advance(b"\x1b[2J"), b"");
    assert!(engine.state().image_placements().is_empty());
    assert!(engine.state().image_placements_for_view(0).is_empty());
}

#[test]
fn a_relative_placement_without_a_parent_is_rejected_without_state_change() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,c=1,r=1,q=0\x1b\\"),
        b"\x1b_Gi=8,p=2;ENOPARENT:relative image parent not found\x1b\\"
    );
    assert_eq!(anchors(&engine), []);
}

#[test]
fn a_relative_chain_beyond_the_supported_depth_reports_etoodeep() {
    let mut engine = engine();
    for id in 1..=10 {
        assert_eq!(
            engine.advance(format!("\x1b_Ga=t,i={id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()),
            b""
        );
    }
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=1,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    for id in 2..=9 {
        let parent = id - 1;
        assert_eq!(
            engine.advance(
                format!("\x1b_Ga=p,i={id},p={id},P={parent},Q={parent},c=1,r=1,C=1,q=2\x1b\\")
                    .as_bytes()
            ),
            b""
        );
    }
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=10,p=10,P=9,Q=9,c=1,r=1,C=1,q=0\x1b\\"),
        b"\x1b_Gi=10,p=10;ETOODEEP:relative image chain too deep\x1b\\"
    );
}

#[test]
fn a_relative_cycle_reports_ecycle() {
    let mut engine = engine();
    for id in 1..=2 {
        assert_eq!(
            engine.advance(format!("\x1b_Ga=t,i={id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()),
            b""
        );
    }
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=1,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=2,p=2,P=1,Q=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );

    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=1,p=1,P=2,Q=2,c=1,r=1,C=1,q=0\x1b\\"),
        b"\x1b_Gi=1,p=1;ECYCLE:relative image cycle\x1b\\"
    );
}

#[test]
fn deleting_a_relative_parent_also_deletes_its_children() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(0, 0), (0, 0)]);
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"), b"");
    assert!(anchors(&engine).is_empty());
}

#[test]
fn visible_delete_keeps_a_relative_image_outside_the_horizontal_view() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,H=-20,V=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b[S"), b"");
    let image_ids = |engine: &TerminalEngine| {
        let mut ids = engine
            .state()
            .primary_image_history
            .iter()
            .map(|placement| placement.record.display.image_id)
            .collect::<Vec<_>>();
        ids.extend(
            engine
                .state()
                .primary_image_placements
                .iter()
                .map(|placement| placement.record.display.image_id),
        );
        ids
    };
    assert_eq!(image_ids(&engine), [Some(7), Some(8)]);
    assert_eq!(
        engine.state().primary_image_history[0]
            .record
            .display
            .image_id,
        Some(7)
    );
    let child = &engine.state().primary_image_placements[0];
    assert_eq!(
        (
            child.record.display.image_id,
            child.columns,
            child.rows,
            engine
                .state()
                .resolve_relative_anchor(&child.record, Some(child.id)),
        ),
        (Some(8), 1, 1, Ok(Some((2, -20))))
    );

    assert_eq!(engine.advance(b"\x1b_Ga=d,d=a,q=2\x1b\\"), b"");

    assert_eq!(image_ids(&engine), [Some(7), Some(8)]);
}

#[test]
fn a_unicode_placeholder_is_rendered_from_the_matching_cell() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert!(engine.state().image_placements().is_empty());
    assert_eq!(
        engine.advance("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    let placements = engine.state().image_placements_for_view(0);
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].anchor(), (0, 0));
    assert_eq!(placements[0].dimensions(), (1, 1));
    assert_eq!(
        placements[0].geometry().offset,
        koshi_core::geometry::Point { x: 0, y: 0 }
    );
}

#[test]
fn a_unicode_placeholder_follows_the_image_into_scrollback() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    assert_eq!(engine.state().image_placements_for_view(0).len(), 1);

    assert_eq!(engine.advance(b"\x1b[10;1H\n"), b"");

    let scrolled = engine.state().image_placements_for_view(1);
    assert_eq!(scrolled.len(), 1);
    assert_eq!(scrolled[0].dimensions(), (1, 1));
    assert_eq!(
        scrolled[0].geometry().full_size,
        koshi_core::geometry::Size { cols: 2, rows: 2 }
    );
}

#[test]
fn a_relative_placement_uses_the_top_left_of_a_partial_unicode_placeholder() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance("\x1b[38;5;7m\u{10eeee}\u{030d}\u{0305}".as_bytes()),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=4,P=7,Q=3,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );

    let child = engine
        .state()
        .primary_image_placements
        .iter()
        .find(|placement| placement.record.display.placement_id == Some(4))
        .expect("the relative child is retained");
    assert_eq!(
        engine
            .state()
            .resolve_relative_anchor(&child.record, Some(child.id)),
        Ok(Some((-1, 0)))
    );
    let visible = engine.state().image_placements_for_view(0);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].geometry().offset.y, 1);
}

#[test]
fn kitty_animation_frame_control_changes_the_displayed_frame() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=7,c=2,s=3,q=2\x1b\\"), b"");
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [0, 0, 255, 255]
    );
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=f,i=7,r=2,q=2\x1b\\"), b"");
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn kitty_animation_loop_value_counts_completed_playbacks_after_restore() {
    for (loop_value, expected_playbacks) in [(2, 1), (3, 2)] {
        let mut source = engine();
        display_animation_base(&mut source, 1, 1, &[255, 0, 0, 255]);
        assert_eq!(
            source.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
            b""
        );
        assert_eq!(
            source.advance(format!("\x1b_Ga=a,i=7,s=3,v={loop_value},q=2\x1b\\").as_bytes()),
            b""
        );
        let state: TerminalState = serde_json::from_slice(
            &serde_json::to_vec(source.state()).expect("the state serializes"),
        )
        .expect("the state restores");
        let mut restored = TerminalEngine::from_state(state, &[]);
        let placements = restored.state().image_placements();
        let animation = placements[0]
            .record()
            .animation
            .as_ref()
            .expect("the animation is retained");
        assert_eq!(
            animation.loop_policy(),
            LoopPolicy::finite(expected_playbacks).expect("the playback count is valid")
        );
        assert_eq!(animation.frames()[0].image().rgba, [255, 0, 0, 255]);
        assert_eq!(animation.frames()[1].image().rgba, [0, 0, 255, 255]);
        assert_eq!(animation.frames()[0].delay().numerator_ms(), 0);
        assert_eq!(animation.frames()[1].delay().numerator_ms(), 10);
        assert_eq!(
            restored.state().next_animation_delay(),
            Some(Duration::from_millis(8))
        );

        if expected_playbacks == 2 {
            let _ = restored.advance_animations(Duration::from_millis(18));
            assert_eq!(
                restored.state().image_placements()[0].record().image.rgba,
                [255, 0, 0, 255]
            );
            assert_eq!(
                restored.state().next_animation_delay(),
                Some(Duration::from_millis(8))
            );
        }
        let _ = restored.advance_animations(Duration::from_millis(18));
        assert_eq!(
            restored.state().image_placements()[0].record().image.rgba,
            [0, 0, 255, 255]
        );
        assert_eq!(restored.state().next_animation_delay(), None);
    }
}

#[test]
fn kitty_png_animation_frame_uses_png_dimensions() {
    let mut engine = engine();
    display_animation_base(&mut engine, 2, 1, &[255, 0, 0, 255, 255, 0, 0, 255]);
    let payload = STANDARD.encode(rgba_png(&[0, 0, 255, 255], 1, 1));

    assert_eq!(
        engine.advance(
            format!("\x1b_Ga=f,i=7,f=100,s=2,v=2,x=1,c=1,X=1,q=2;{payload}\x1b\\").as_bytes()
        ),
        b""
    );
    select_second_frame(&mut engine);

    let placements = engine.state().image_placements();
    let image = &placements[0].record().image;
    assert_eq!((image.width, image.height), (2, 1));
    assert_eq!(
        image.rgba,
        [255, 0, 0, 255, 0, 0, 255, 255],
        "the 1x1 PNG updates one pixel even when s and v declare 2x2"
    );
}

#[test]
fn kitty_apng_animation_frame_uses_only_the_default_png_image() {
    let mut engine = engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);
    let payload = STANDARD.encode(apng_with_separate_default(
        [0, 0, 255, 255],
        [0, 255, 0, 255],
    ));

    assert_eq!(
        engine.advance(format!("\x1b_Ga=f,i=7,f=100,q=2;{payload}\x1b\\").as_bytes()),
        b""
    );
    select_second_frame(&mut engine);

    let placements = engine.state().image_placements();
    let animation = placements[0]
        .record()
        .animation
        .as_ref()
        .expect("the Kitty animation is retained");
    assert_eq!(animation.frame_count(), 2);
    assert_eq!(animation.frames()[0].image().rgba, [255, 0, 0, 255]);
    assert_eq!(animation.frames()[1].image().rgba, [0, 0, 255, 255]);
}

#[test]
fn kitty_png_animation_frame_rejects_invalid_or_mislabeled_data_atomically() {
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .write_image(&[0, 255, 0], 1, 1, image::ColorType::Rgb8.into())
        .expect("the JPEG encodes");

    for invalid in [b"\x89PNG\r\n\x1a\n".to_vec(), jpeg] {
        let mut engine = engine();
        display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);
        let payload = STANDARD.encode(invalid);

        assert_eq!(
            engine.advance(format!("\x1b_Ga=f,i=7,f=100;{payload}\x1b\\").as_bytes()),
            b"\x1b_Gi=7;EINVAL:invalid image placement\x1b\\"
        );

        let placement = &engine.state().image_placements()[0];
        assert_eq!(placement.record().image.rgba, [255, 0, 0, 255]);
        assert_eq!(placement.record().animation, None);
    }
}

#[test]
fn kitty_rgb_animation_frame_remains_opaque_rgba() {
    let mut engine = engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=24,s=1,v=1,q=2;AP8A\x1b\\"),
        b""
    );
    select_second_frame(&mut engine);

    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [0, 255, 0, 255]
    );
}

#[test]
fn animation_frame_number_beyond_the_next_frame_appends() {
    let mut engine = engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,r=999,q=2;AAD//w==\x1b\\"),
        b""
    );

    let placements = engine.state().image_placements();
    let animation = placements[0]
        .record()
        .animation
        .as_ref()
        .expect("the appended frame is retained");
    assert_eq!(animation.frame_count(), 2);
    assert_eq!(animation.frames()[0].image().rgba, [255, 0, 0, 255]);
    assert_eq!(animation.frames()[1].image().rgba, [0, 0, 255, 255]);
}

#[test]
fn animation_delete_defaults_to_the_root_and_clamps_to_the_last_frame() {
    let mut root = engine();
    display_animation_base(&mut root, 1, 1, &[255, 0, 0, 255]);
    add_blue_and_green_frames(&mut root);
    assert_eq!(root.advance(b"\x1b_Ga=d,d=f,i=7,q=0\x1b\\"), b"");
    let placements = root.state().image_placements();
    let frames = placements[0]
        .record()
        .animation
        .as_ref()
        .expect("two frames remain");
    assert_eq!(frames.frames()[0].image().rgba, [0, 0, 255, 255]);
    assert_eq!(frames.frames()[1].image().rgba, [0, 255, 0, 255]);

    let mut last = engine();
    display_animation_base(&mut last, 1, 1, &[255, 0, 0, 255]);
    add_blue_and_green_frames(&mut last);
    assert_eq!(last.advance(b"\x1b_Ga=d,d=f,i=7,r=999,q=0\x1b\\"), b"");
    let placements = last.state().image_placements();
    let frames = placements[0]
        .record()
        .animation
        .as_ref()
        .expect("two frames remain");
    assert_eq!(frames.frames()[0].image().rgba, [255, 0, 0, 255]);
    assert_eq!(frames.frames()[1].image().rgba, [0, 0, 255, 255]);
}

#[test]
fn deleting_the_selected_root_middle_or_last_frame_selects_the_exact_survivor() {
    for (selected, deleted, expected) in [
        (1, 1, [0, 0, 255, 255]),
        (2, 2, [0, 255, 0, 255]),
        (3, 3, [0, 0, 255, 255]),
    ] {
        let mut engine = engine();
        display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);
        add_blue_and_green_frames(&mut engine);
        assert_eq!(
            engine.advance(format!("\x1b_Ga=a,i=7,c={selected},s=3,q=0\x1b\\").as_bytes()),
            b""
        );
        assert_eq!(
            engine.advance(format!("\x1b_Ga=d,d=f,i=7,r={deleted},q=0\x1b\\").as_bytes()),
            b""
        );
        assert_eq!(
            engine.state().image_placements()[0].record().image.rgba,
            expected,
            "selected frame {selected}"
        );
    }
}

#[test]
fn frame_delete_without_extra_frames_obeys_lowercase_and_uppercase_forms() {
    let mut engine = engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(engine.advance(b"\x1b_Ga=d,d=f,i=7,q=0\x1b\\"), b"");
    assert_eq!(anchors(&engine), [(0, 0)]);
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=F,i=7,q=0\x1b\\"), b"");
    assert_eq!(anchors(&engine), []);
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,c=1,r=1,q=0\x1b\\"),
        b"\x1b_Gi=7;ENOENT:image not found\x1b\\"
    );
}

#[test]
fn animation_reply_policy_matches_command_kind() {
    let mut engine = engine();
    display_animation_base(&mut engine, 1, 1, &[255, 0, 0, 255]);

    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,q=0;AAD//w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=c,i=7,r=1,c=2,w=1,h=1,q=0\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=7,c=2,s=3,q=0\x1b\\"), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=999,c=1,s=3,q=0\x1b\\"), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=f,i=7,r=2,q=0\x1b\\"), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=f,i=999,r=1,q=0\x1b\\"), b"");
}

#[test]
fn animation_frame_and_relative_errors_use_specific_kitty_codes() {
    let mut animation_engine = engine();
    display_animation_base(&mut animation_engine, 1, 1, &[255, 0, 0, 255]);
    assert_eq!(
        animation_engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,c=99,q=0;AAD//w==\x1b\\"),
        b"\x1b_Gi=7;ENOENT:animation frame not found\x1b\\"
    );

    let mut state = engine().into_state();
    let display = ImageDisplay {
        image_id: Some(7),
        placement_id: Some(3),
        ..ImageDisplay::default()
    };
    for (error, expected) in [
        (
            ImagePlacementError::NoParent,
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
        state.reply_kitty(&display, Some(error), true);
        assert_eq!(state.take_replies(), expected);
    }
}

#[test]
fn loading_animation_state_survives_a_state_restore() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=7,s=2,q=2\x1b\\"), b"");

    let value = serde_json::to_value(engine.state()).expect("serialize loading animation");
    let restored: TerminalState = serde_json::from_value(value).expect("restore loading animation");

    assert_eq!(
        restored.image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn redraw_removes_the_old_position_and_displays_the_stored_image() {
    let mut engine = engine();
    assert_eq!(engine.advance(b"\x1b[3;1H"), b"");
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(anchors(&engine), [(2, 0)]);
    assert_eq!(
        engine.advance(b"\x1b_Ga=d,d=a,q=2\x1b\\\x1b[6;1H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(5, 0)]);
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
    assert_eq!(engine.state().active_cursor_position(), (5, 0));
}

#[test]
fn placing_one_upload_twice_shares_pixels_and_keeps_both_positions() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(
        engine.advance(b"\x1b[5;4H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(0, 0), (4, 3)]);
    let placements = engine.state().image_placements();
    assert!(Arc::ptr_eq(
        &placements[0].record().image,
        &placements[1].record().image
    ));
    assert_eq!(
        placements[0].content_id(),
        placements[1].content_id(),
        "one Kitty upload has one canonical content identity"
    );
}

#[test]
fn retransmitting_a_kitty_id_gets_a_new_content_identity() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    let first = engine.state().image_placements()[0].content_id();

    assert_eq!(engine.advance(UPLOAD), b"");
    let placements = engine.state().image_placements();
    assert_eq!(placements.len(), 1);
    assert_ne!(first, placements[0].content_id());
}

#[test]
fn a_named_placement_moves_without_adding_another_placement() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(
        engine.advance(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    let id = engine.state().image_placements()[0].id();
    assert_eq!(
        engine.advance(b"\x1b[7;2H\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(6, 1)]);
    assert_eq!(engine.state().image_placements()[0].id(), id);
}

#[test]
fn freeing_an_upload_makes_a_new_placement_report_the_missing_image() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=I,i=7\x1b\\"), b"");
    assert_eq!(anchors(&engine), []);
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,c=1,r=1,C=1\x1b\\"),
        b"\x1b_Gi=7;ENOENT:image not found\x1b\\"
    );
    assert_eq!(anchors(&engine), []);
}

#[test]
fn stored_pixels_survive_a_state_restore_with_no_visible_placement() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d\x1b\\"), b"");
    let state: TerminalState =
        serde_json::from_slice(&serde_json::to_vec(engine.state()).expect("serialize"))
            .expect("restore");
    let mut restored = TerminalEngine::from_state(state, &[]);
    assert_eq!(
        restored.advance(b"\x1b[4;3H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&restored), [(3, 2)]);
    assert_eq!(
        restored.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn query_replies_before_device_attributes_and_does_not_store_pixels() {
    let mut engine = engine();
    let reply = engine.advance(b"\x1b_Ga=q,i=31,f=32,s=1,v=1;/wAA/w==\x1b\\\x1b[c");
    assert_eq!(reply, b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c");
    assert_eq!(engine.state().kitty_images, []);
    assert_eq!(anchors(&engine), []);
    assert_eq!(engine.take_graphics(), []);
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=31,c=1,r=1\x1b\\"),
        b"\x1b_Gi=31;ENOENT:image not found\x1b\\"
    );
}

#[test]
fn every_byte_split_keeps_the_same_redraw_result() {
    let redraw = b"\x1b_Ga=d\x1b\\\x1b[4;3H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\";
    for split in 0..=redraw.len() {
        let mut engine = engine();
        assert_eq!(engine.advance(UPLOAD), b"");
        assert_eq!(engine.advance(&redraw[..split]), b"");
        assert_eq!(engine.advance(&redraw[split..]), b"");
        assert_eq!(anchors(&engine), [(3, 2)], "split {split}");
    }
}

#[test]
fn deleting_aborts_an_incomplete_upload_and_allows_a_new_one() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1,i=7,q=2,m=1;/wAA\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b_Ga=d\x1b\\"), b"");
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(anchors(&engine), [(0, 0)]);
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn deleting_aborts_an_incomplete_animation_upload_and_executes_the_delete() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,m=1,q=2;AAD/\x1b\\"),
        b""
    );

    assert_eq!(engine.advance(b"\x1b_Ga=d,d=a,q=2\x1b\\"), b"");
    assert_eq!(anchors(&engine), []);
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(anchors(&engine), [(0, 0)]);
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn deleting_one_named_placement_keeps_pixels_used_by_another() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\\x1b[5;1H\x1b_Ga=p,i=7,p=2,c=1,r=1,C=1,q=2\x1b\\"), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=I,i=7,p=1\x1b\\"), b"");
    assert_eq!(anchors(&engine), [(4, 0)]);
    assert_eq!(engine.state().kitty_images[0].display.image_id, Some(7));
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=I,i=7,p=2\x1b\\"), b"");
    assert_eq!(engine.state().kitty_images, []);
}

#[test]
fn uppercase_visible_delete_frees_only_unreferenced_upload_data() {
    let upload = b"\x1b_Ga=t,f=32,s=1,v=1,i=7,q=2;/wAA/w==\x1b\\";
    let place = b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\";

    let mut sole = engine();
    assert_eq!(sole.advance(upload), b"");
    assert_eq!(sole.advance(place), b"");
    assert_eq!(sole.advance(b"\x1b_Ga=d,d=A,q=2\x1b\\"), b"");
    assert_eq!(sole.state().kitty_images, []);
    assert_eq!(sole.state().image_placements(), []);

    let mut shared = engine();
    assert_eq!(shared.advance(upload), b"");
    assert_eq!(shared.advance(place), b"");
    assert_eq!(
        shared.advance(b"\x1b_Ga=p,i=7,p=2,c=1,r=1,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(shared.advance(b"\x1b_Ga=d,d=A,q=2\x1b\\"), b"");
    assert_eq!(shared.state().image_placements(), []);
    assert_eq!(shared.state().kitty_images.len(), 2);
    assert_eq!(shared.advance(b"\x1b_Ga=d,d=I,i=7,p=2,q=2\x1b\\"), b"");
    assert_eq!(shared.state().kitty_images, []);
}

#[test]
fn a_hard_reset_removes_uploads_as_well_as_placements() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1bc"), b"");
    assert_eq!(engine.state().kitty_images, []);
    assert_eq!(anchors(&engine), []);
}
