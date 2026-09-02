//! Tests for bounded Sixel, kitty, and iTerm2 image decoding.

use super::*;

use crate::engine::{GraphicsTransportState, TerminalEngine};
use crate::state::ImagePlacementError;
use koshi_core::process::PtySize;

fn red_png() -> Vec<u8> {
    use image::ImageEncoder;

    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel image encodes");
    bytes
}

fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = red_png();
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    let mut crc = 0xffff_ffffu32;
    for &byte in &bytes[12..29] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
    bytes
}

fn animated_gif() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
        let frames = [
            image::Frame::new(image::RgbaImage::from_pixel(
                1,
                1,
                image::Rgba([255, 0, 0, 255]),
            )),
            image::Frame::new(image::RgbaImage::from_pixel(
                1,
                1,
                image::Rgba([0, 0, 255, 255]),
            )),
        ];
        encoder
            .encode_frames(frames)
            .expect("the two-frame GIF encodes");
    }
    bytes
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(12 + kind.len() + data.len());
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

fn animated_png() -> Vec<u8> {
    let source = red_png();
    let mut bytes = source[..8].to_vec();
    let mut at = 8;
    let mut inserted_animation = false;
    let mut first_frame_data = Vec::new();
    while at < source.len() {
        let length = u32::from_be_bytes(source[at..at + 4].try_into().expect("PNG length"));
        let end = at + 12 + usize::try_from(length).expect("PNG length fits");
        let kind: &[u8; 4] = source[at + 4..at + 8].try_into().expect("PNG chunk type");
        let data = &source[at + 8..end - 4];
        match kind {
            b"IDAT" if !inserted_animation => {
                bytes.extend_from_slice(&png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
                bytes.extend_from_slice(&png_chunk(b"fcTL", &png_frame_control(0)));
                bytes.extend_from_slice(&png_chunk(kind, data));
                first_frame_data.extend_from_slice(data);
                inserted_animation = true;
            }
            b"IEND" if inserted_animation => {
                bytes.extend_from_slice(&png_chunk(b"fcTL", &png_frame_control(1)));
                let mut frame_data = Vec::with_capacity(4 + first_frame_data.len());
                frame_data.extend_from_slice(&2u32.to_be_bytes());
                frame_data.extend_from_slice(&first_frame_data);
                bytes.extend_from_slice(&png_chunk(b"fdAT", &frame_data));
                bytes.extend_from_slice(&png_chunk(kind, data));
            }
            _ => bytes.extend_from_slice(&source[at..end]),
        }
        at = end;
    }
    bytes
}

fn png_frame_control(sequence: u32) -> [u8; 26] {
    let mut data = [0; 26];
    data[..4].copy_from_slice(&sequence.to_be_bytes());
    data[4..8].copy_from_slice(&1u32.to_be_bytes());
    data[8..12].copy_from_slice(&1u32.to_be_bytes());
    data[20..22].copy_from_slice(&1u16.to_be_bytes());
    data[22..24].copy_from_slice(&10u16.to_be_bytes());
    data
}

fn red_webp() -> Vec<u8> {
    use image::ImageEncoder;

    let mut bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel WebP encodes");
    bytes
}

fn webp_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + data.len() + data.len() % 2);
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    if data.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes
}

fn animated_webp() -> Vec<u8> {
    let source = red_webp();
    let mut at = 12;
    let mut frame = Vec::new();
    while at < source.len() {
        let length = usize::try_from(u32::from_le_bytes(
            source[at + 4..at + 8].try_into().expect("WebP length"),
        ))
        .expect("WebP length fits");
        if &source[at..at + 4] == b"VP8L" {
            frame.extend_from_slice(&source[at + 8..at + 8 + length]);
        }
        at += 8 + length + length % 2;
    }

    let vp8x = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let anim = [0, 0, 0, 0, 0, 0];
    let mut frame_data = [0; 16];
    frame_data[12] = 1;
    let mut anmf = frame_data.to_vec();
    anmf.extend_from_slice(&webp_chunk(b"VP8L", &frame));

    let chunks = [
        webp_chunk(b"VP8X", &vp8x),
        webp_chunk(b"ANIM", &anim),
        webp_chunk(b"ANMF", &anmf),
        webp_chunk(b"ANMF", &anmf),
    ];
    let body_length: usize = 4 + chunks.iter().map(Vec::len).sum::<usize>();
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(body_length as u32).to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    for chunk in chunks {
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

fn kitty_raw_rgba() -> Vec<u8> {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    format!("\x1b_Gf=32,s=1,v=1;{payload}\x1b\\").into_bytes()
}

fn kitty_c1_raw_rgba() -> Vec<u8> {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let mut bytes = vec![0x9f];
    bytes.extend_from_slice(format!("Gf=32,s=1,v=1;{payload}").as_bytes());
    bytes.push(0x9c);
    bytes
}

fn kitty_display_cell_rgba(move_cursor: bool) -> Vec<u8> {
    kitty_display_cell_rgba_size(1, 1, move_cursor)
}

fn kitty_display_cell_rgba_size(columns: u32, rows: u32, move_cursor: bool) -> Vec<u8> {
    let payload = STANDARD.encode([255, 0, 0, 255].repeat((columns * rows) as usize));
    let cursor = if move_cursor { 0 } else { 1 };
    format!("\x1b_Ga=T,f=32,s={columns},v={rows},c={columns},r={rows},C={cursor};{payload}\x1b\\")
        .into_bytes()
}

fn kitty_display_cell_rgba_identity(
    image_id: u32,
    placement_id: u32,
    move_cursor: bool,
) -> Vec<u8> {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let cursor = if move_cursor { 0 } else { 1 };
    format!(
        "\x1b_Ga=T,f=32,s=1,v=1,i={image_id},p={placement_id},c=1,r=1,C={cursor};{payload}\x1b\\"
    )
    .into_bytes()
}

fn one_sixel() -> Vec<u8> {
    b"\x1bPq\"1;1;1;1#1;2;100;0;0#1@\x1b\\".to_vec()
}

fn iterm_file(bytes: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(bytes);
    format!(
        "\x1b]1337;File=inline=1;size={};width=1px;height=1px;preserveAspectRatio=0:{}\x07",
        bytes.len(),
        encoded
    )
    .into_bytes()
}

fn iterm_cell_file(bytes: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(bytes);
    format!(
        "\x1b]1337;File=inline=1;size={};width=1;height=1;preserveAspectRatio=0:{}\x07",
        bytes.len(),
        encoded
    )
    .into_bytes()
}

fn iterm_multipart(bytes: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(bytes);
    let split = encoded.len() / 2;
    format!(
        "\x1b]1337;MultipartFile=inline=1;size={}\x07\
\x1b]1337;FilePart={}\x07\
\x1b]1337;FilePart={}\x07\
\x1b]1337;FileEnd\x07",
        bytes.len(),
        &encoded[..split],
        &encoded[split..],
    )
    .into_bytes()
}

fn tmux_wrap(inner: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x1bPtmux;".to_vec();
    for &byte in inner {
        if byte == 0x1b {
            bytes.push(0x1b);
        }
        bytes.push(byte);
    }
    bytes.extend_from_slice(b"\x1b\\");
    bytes
}

fn screen_wrap(inner: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x1bP".to_vec();
    bytes.extend_from_slice(inner);
    bytes.extend_from_slice(b"\x1b\\");
    bytes
}

fn only_event(parser: &mut GraphicsParser, bytes: &[u8]) -> Result<DecodedGraphics, GraphicsError> {
    let events = parser.advance(bytes);
    assert_eq!(events.len(), 1);
    events.into_iter().next().expect("one event")
}

#[test]
fn sixel_decodes_one_red_pixel_without_terminal_state() {
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &one_sixel()).expect("the Sixel decodes");

    assert_eq!(result.protocol, GraphicsProtocol::Sixel);
    assert_eq!(result.image.width, 1);
    assert_eq!(result.image.height, 1);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn sixel_header_accepts_omitted_optional_parameters() {
    let mut parser = GraphicsParser::default();

    let result =
        only_event(&mut parser, b"\x1bP;2q#1;2;100;0;0@\x1b\\").expect("the Sixel header decodes");

    assert_eq!(result.image.width, 1);
    assert_eq!(result.image.height, 6);
    assert_eq!(
        result.image.rgba,
        [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        result.display.sixel_background,
        Some(SixelBackground::Terminal)
    );
}

#[test]
fn sixel_supports_hls_colors_and_the_background_select_parameter() {
    let mut parser = GraphicsParser::default();
    let hls =
        only_event(&mut parser, b"\x1bPq#1;1;0;50;100@\x1b\\").expect("the HLS color decodes");
    assert_eq!(&hls.image.rgba[..4], [0, 0, 255, 255]);

    let mut parser = GraphicsParser::default();
    let background =
        only_event(&mut parser, b"\x1bP0;0q?\x1b\\").expect("the opaque background decodes");
    assert_eq!(background.image.width, 1);
    assert_eq!(background.image.height, 6);
    assert_eq!(background.image.rgba, [0, 0, 0, 0].repeat(6));
    assert_eq!(
        background.display.sixel_background,
        Some(SixelBackground::Terminal)
    );

    let mut parser = GraphicsParser::default();
    let transparent =
        only_event(&mut parser, b"\x1bP0;1q?\x1b\\").expect("the transparent background decodes");
    assert_eq!(transparent.image.rgba, [0, 0, 0, 0].repeat(6));
    assert_eq!(
        transparent.display.sixel_background,
        Some(SixelBackground::Preserve)
    );
}

#[test]
fn sixel_growth_keeps_a_valid_image_near_the_dimension_limit() {
    let mut bytes = b"\x1bPq!9000@!1000@".to_vec();
    bytes.extend_from_slice(b"\x1b\\");
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &bytes).expect("the growing Sixel decodes");

    assert_eq!(result.image.width, 10_000);
    assert_eq!(result.image.height, 6);
}

#[test]
fn cancelling_a_chunked_transfer_clears_its_multipart_state() {
    let first = "\x1b_Gf=32,s=1,v=1,m=1;/wAA\x1b\\";
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(first.as_bytes()).is_empty());
    assert!(parser.advance(b"\x18").is_empty());
    let result = only_event(&mut parser, kitty_raw_rgba().as_slice()).expect("a new transfer");

    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn graphics_accepts_seven_bit_and_c1_string_openings_and_terminators() {
    let mut parser = GraphicsParser::default();
    let mut sixel = one_sixel();
    sixel[0] = 0x90;
    sixel.remove(1);
    let terminator = sixel.len() - 2;
    sixel.truncate(terminator);
    sixel.push(0x9c);
    assert_eq!(
        only_event(&mut parser, &sixel)
            .expect("C1 Sixel decodes")
            .protocol,
        GraphicsProtocol::Sixel
    );

    let mut parser = GraphicsParser::default();
    let mut kitty = kitty_raw_rgba();
    kitty[0] = 0x9f;
    kitty.remove(1);
    let terminator = kitty.len() - 2;
    kitty.truncate(terminator);
    kitty.push(0x9c);
    assert_eq!(
        only_event(&mut parser, &kitty)
            .expect("C1 kitty decodes")
            .protocol,
        GraphicsProtocol::Kitty
    );

    let mut parser = GraphicsParser::default();
    let mut iterm = iterm_file(&red_png());
    iterm[0] = 0x9d;
    iterm.remove(1);
    let terminator = iterm.len() - 1;
    iterm[terminator] = 0x9c;
    assert_eq!(
        only_event(&mut parser, &iterm)
            .expect("C1 iTerm2 decodes")
            .protocol,
        GraphicsProtocol::Iterm2
    );

    let mut parser = GraphicsParser::default();
    let mut tmux = tmux_wrap(&kitty_raw_rgba());
    tmux[0] = 0x90;
    tmux.remove(1);
    assert_eq!(
        only_event(&mut parser, &tmux)
            .expect("C1 tmux decodes")
            .protocol,
        GraphicsProtocol::Kitty
    );

    let mut parser = GraphicsParser::default();
    let mut screen = screen_wrap(&kitty_raw_rgba());
    screen[0] = 0x90;
    screen.remove(1);
    assert_eq!(
        only_event(&mut parser, &screen)
            .expect("C1 Screen decodes")
            .protocol,
        GraphicsProtocol::Kitty
    );
}

#[test]
fn kitty_decodes_one_red_rgba_pixel() {
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &kitty_raw_rgba()).expect("kitty decodes");

    assert_eq!(result.protocol, GraphicsProtocol::Kitty);
    assert_eq!(result.action, ImageAction::Transmit);
    assert_eq!(result.image.width, 1);
    assert_eq!(result.image.height, 1);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn kitty_decodes_one_red_rgb_pixel() {
    let payload = STANDARD.encode([255, 0, 0]);
    let bytes = format!("\x1b_Gf=24,s=1,v=1;{payload}\x1b\\");
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, bytes.as_bytes()).expect("kitty RGB decodes");

    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn kitty_transmit_and_display_action_is_kept_with_the_image_record() {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let bytes = format!("\x1b_Ga=T,f=32,s=1,v=1;{payload}\x1b\\").into_bytes();
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &bytes).expect("kitty transmit-and-display decodes");

    assert_eq!(result.action, ImageAction::TransmitAndDisplay);
}

#[test]
fn kitty_rejects_an_image_that_names_both_id_forms() {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let bytes = format!("\x1b_Gi=1,I=2,f=32,s=1,v=1;{payload}\x1b\\");
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, bytes.as_bytes()),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_continuation_rejects_non_chunk_control_fields() {
    let payload = STANDARD.encode([255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255]);
    let split = payload.len() / 2;
    let first = format!("\x1b_Ga=T,f=32,s=3,v=1,m=1;{}\x1b\\", &payload[..split]);
    let second = format!("\x1b_Gf=32,m=0;{}\x1b\\", &payload[split..]);
    let mut parser = GraphicsParser::default();

    let first_events = parser.advance(first.as_bytes());
    assert!(
        first_events.is_empty(),
        "unexpected first events: {first_events:?}"
    );
    assert_eq!(
        only_event(&mut parser, second.as_bytes()),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_continuation_requires_the_more_flag() {
    let payload = STANDARD.encode([255, 0, 0, 255, 255, 0, 0, 255]);
    let first = format!("\x1b_Gf=32,s=2,v=1,m=1;{}\x1b\\", &payload[..4]);
    let second = format!("\x1b_G;{}\x1b\\", &payload[4..]);
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(first.as_bytes()).is_empty());
    assert_eq!(
        only_event(&mut parser, second.as_bytes()),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_nonfinal_chunk_requires_complete_base64_quartets() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, b"\x1b_Gf=100,m=1;AAA\x1b\\"),
        Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_rejects_a_non_zlib_compression_value() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, b"\x1b_Gf=24,s=1,v=1,o=0;AAAA\x1b\\"),
        Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            format: "0".to_string(),
        })
    );
}

#[test]
fn kitty_zlib_compresses_rgb_data_before_the_raw_decode() {
    use std::io::Write;

    let mut compressed = Vec::new();
    let mut encoder =
        flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
    encoder
        .write_all(&[255, 0, 0])
        .expect("the RGB pixel compresses");
    encoder.finish().expect("the zlib stream finishes");
    let payload = STANDARD.encode(compressed);
    let bytes = format!("\x1b_Gf=24,s=1,v=1,o=z;{payload}\x1b\\").into_bytes();
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &bytes).expect("compressed kitty decodes");

    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn kitty_compressed_png_requires_its_uncompressed_size() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, b"\x1b_Gf=100,o=z;AAAA\x1b\\"),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_zlib_rejects_bytes_after_the_compressed_stream() {
    use std::io::Write;

    let mut compressed = Vec::new();
    let mut encoder =
        flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
    encoder
        .write_all(&[255, 0, 0])
        .expect("the RGB pixel compresses");
    encoder.finish().expect("the zlib stream finishes");
    compressed.extend_from_slice(b"trailing");
    let payload = STANDARD.encode(compressed);
    let bytes = format!("\x1b_Gf=24,s=1,v=1,o=z;{payload}\x1b\\").into_bytes();
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, &bytes),
        Err(GraphicsError::DecodeFailure {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn zlib_output_may_reach_the_rgba_image_byte_limit() {
    use std::io::Write;

    let source = vec![0; MAX_GRAPHICS_TRANSFER_BYTES + 1];
    let mut compressed = Vec::new();
    let mut encoder = flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::fast());
    encoder
        .write_all(&source)
        .expect("the bounded source compresses");
    encoder.finish().expect("the zlib stream finishes");

    let decoded = decompress_bounded(GraphicsProtocol::Kitty, &compressed)
        .expect("the compressed source stays within the RGBA byte limit");

    assert_eq!(decoded.len(), source.len());
}

#[test]
fn iterm_file_decodes_png_and_keeps_display_hints() {
    let mut parser = GraphicsParser::default();
    let bytes = red_png();

    let result = only_event(&mut parser, &iterm_file(&bytes)).expect("the iTerm2 file decodes");

    assert_eq!(result.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(result.image.width, 1);
    assert_eq!(result.image.height, 1);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
    assert_eq!(result.display.width, Some(ImageDimension::Pixels(1)));
    assert_eq!(result.display.height, Some(ImageDimension::Pixels(1)));
    assert!(!result.display.preserve_aspect_ratio);
}

#[test]
fn animated_iterm_gif_is_rejected_instead_of_being_silently_frozen() {
    let mut parser = GraphicsParser::default();
    let bytes = animated_gif();

    let encoded = STANDARD.encode(bytes);
    let command = format!("\x1b]1337;File=inline=1:{encoded}\x07");

    assert_eq!(
        only_event(&mut parser, command.as_bytes()),
        Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Iterm2,
            format: "animated GIF".to_string(),
        })
    );
}

#[test]
fn animated_png_and_webp_are_rejected_instead_of_being_silently_frozen() {
    for (format, bytes) in [
        ("animated PNG", animated_png()),
        ("animated WebP", animated_webp()),
    ] {
        assert_eq!(
            decode_raster(GraphicsProtocol::Iterm2, &bytes),
            Err(GraphicsError::UnsupportedMedia {
                protocol: GraphicsProtocol::Iterm2,
                format: format.to_string(),
            })
        );
    }
}

#[test]
fn iterm_multipart_parts_join_in_order() {
    let mut parser = GraphicsParser::default();
    let bytes = red_png();

    let result =
        only_event(&mut parser, &iterm_multipart(&bytes)).expect("the multipart image ends");

    assert_eq!(result.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn iterm_dimensions_keep_cell_pixel_percent_and_auto_units() {
    let bytes = red_png();
    let encoded = STANDARD.encode(bytes);
    let command = format!(
        "\x1b]1337;File=inline=1;width=2;height=3px;preserveAspectRatio=1;name=red;foo=bar:{}\x07",
        encoded
    );
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, command.as_bytes()).expect("the iTerm2 image decodes");

    assert_eq!(result.display.width, Some(ImageDimension::Cells(2)));
    assert_eq!(result.display.height, Some(ImageDimension::Pixels(3)));
    assert!(result.display.preserve_aspect_ratio);

    let encoded = STANDARD.encode(red_png());
    let command = format!(
        "\x1b]1337;File=inline=1;width=10%;height=auto:{}\x07",
        encoded
    );
    let mut parser = GraphicsParser::default();
    let result =
        only_event(&mut parser, command.as_bytes()).expect("the second iTerm2 image decodes");

    assert_eq!(result.display.width, Some(ImageDimension::Percent(10)));
    assert_eq!(result.display.height, Some(ImageDimension::Auto));
}

#[test]
fn iterm_rejects_non_inline_and_mismatched_sizes() {
    let encoded = STANDARD.encode(red_png());
    let mut parser = GraphicsParser::default();
    let not_inline = format!("\x1b]1337;File=inline=0:{encoded}\x07");
    assert_eq!(
        only_event(&mut parser, not_inline.as_bytes()),
        Err(GraphicsError::UnsupportedAction {
            protocol: GraphicsProtocol::Iterm2,
            action: "inline=0".to_string(),
        })
    );

    let mut parser = GraphicsParser::default();
    let mismatch = format!("\x1b]1337;File=inline=1;size=1:{encoded}\x07");
    assert_eq!(
        only_event(&mut parser, mismatch.as_bytes()),
        Err(GraphicsError::DeclaredSizeMismatch {
            protocol: GraphicsProtocol::Iterm2,
            expected: 1,
            actual: red_png().len(),
        })
    );
}

#[test]
fn tmux_and_screen_wrappers_expose_the_enclosed_kitty_image() {
    for wrapped in [tmux_wrap(&kitty_raw_rgba()), screen_wrap(&kitty_raw_rgba())] {
        let mut parser = GraphicsParser::default();

        let result = only_event(&mut parser, &wrapped).expect("the wrapper exposes kitty");

        assert_eq!(result.protocol, GraphicsProtocol::Kitty);
        assert_eq!(result.image.rgba, [255, 0, 0, 255]);
    }
}

#[test]
fn screen_and_tmux_wrappers_keep_two_inner_kitty_images() {
    let mut inner = kitty_raw_rgba();
    inner.extend_from_slice(&kitty_raw_rgba());

    for wrapped in [tmux_wrap(&inner), screen_wrap(&inner)] {
        let mut parser = GraphicsParser::default();
        let events = parser.advance(&wrapped);

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]
                .as_ref()
                .expect("the first wrapped image decodes")
                .image
                .rgba,
            [255, 0, 0, 255]
        );
        assert_eq!(
            events[1]
                .as_ref()
                .expect("the second wrapped image decodes")
                .image
                .rgba,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn sos_and_pm_hold_c1_apc_bytes_until_the_string_terminator() {
    for prefix in [[0x1b, b'X'], [0x1b, b'^'], [0x98, 0], [0x9e, 0]] {
        let mut bytes = prefix[..if prefix[1] == 0 { 1 } else { 2 }].to_vec();
        bytes.extend_from_slice(&kitty_c1_raw_rgba());
        bytes.push(0x9c);
        let mut parser = GraphicsParser::default();

        assert!(parser.advance(&bytes).is_empty());
        assert_eq!(
            only_event(&mut parser, &kitty_c1_raw_rgba())
                .expect("the next APC decodes after the silent string")
                .image
                .rgba,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn screen_wrapper_closes_after_a_bel_terminated_iterm_image() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, &screen_wrap(&iterm_file(&red_png())))
            .expect("the Screen-wrapped iTerm2 image decodes")
            .image
            .rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn screen_wrapper_preserves_inner_string_terminators() {
    let mut sixel = one_sixel();
    sixel.truncate(sixel.len() - 2);
    sixel.push(0x9c);
    let mut kitty = kitty_raw_rgba();
    kitty.truncate(kitty.len() - 2);
    kitty.push(0x9c);
    let mut iterm = iterm_file(&red_png());
    iterm.pop();
    iterm.push(0x9c);

    for (name, inner) in [("Sixel", sixel), ("kitty", kitty), ("iTerm2", iterm)] {
        let mut parser = GraphicsParser::default();
        let events = parser.advance(&screen_wrap(&inner));
        assert_eq!(events.len(), 1, "the {name} image has one event");
        let result = events
            .into_iter()
            .next()
            .expect("the Screen-wrapped image event")
            .expect("the Screen-wrapped image decodes");
        assert_eq!(result.image.width, 1);
        assert_eq!(result.image.height, 1);
    }
}

#[test]
fn screen_wrappers_keep_an_inner_st_after_a_split() {
    let inner = kitty_raw_rgba();
    let split = inner.len() / 2;
    let mut bytes = screen_wrap(&inner[..split]);
    bytes.extend_from_slice(&screen_wrap(&inner[split..]));
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &bytes).expect("the split Screen image decodes");

    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn passthrough_wrappers_preserve_inner_c1_terminators() {
    let mut inner = kitty_raw_rgba();
    inner.truncate(inner.len() - 2);
    inner.push(0x9c);
    let mut screen = b"\x1bP".to_vec();
    screen.extend_from_slice(&inner);
    screen.extend_from_slice(b"\x1b\\");

    for (name, wrapped) in [("Screen", screen), ("tmux", tmux_wrap(&inner))] {
        let mut parser = GraphicsParser::default();
        let result = only_event(&mut parser, &wrapped)
            .unwrap_or_else(|error| panic!("the {name}-wrapped image failed: {error:?}"));
        assert_eq!(result.image.rgba, [255, 0, 0, 255]);
    }
}

#[test]
fn screen_wrappers_join_an_iterm_transfer_split_between_dcs_strings() {
    let inner = iterm_file(&red_png());
    let split = inner.len() / 2;
    let mut bytes = screen_wrap(&inner[..split]);
    bytes.extend_from_slice(&screen_wrap(&inner[split..]));
    let mut parser = GraphicsParser::default();

    let events = parser.advance(&bytes);
    assert_eq!(events.len(), 1);
    let result = events
        .into_iter()
        .next()
        .expect("one Screen event")
        .expect("the split Screen transfer decodes");

    assert_eq!(result.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn tmux_wrappers_join_an_iterm_transfer_split_between_dcs_strings() {
    let inner = iterm_file(&red_png());
    let split = inner.len() / 2;
    let mut bytes = tmux_wrap(&inner[..split]);
    bytes.extend_from_slice(&tmux_wrap(&inner[split..]));
    let mut parser = GraphicsParser::default();

    let events = parser.advance(&bytes);
    assert_eq!(events.len(), 1);
    let result = events
        .into_iter()
        .next()
        .expect("one tmux event")
        .expect("the split tmux transfer decodes");

    assert_eq!(result.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn cancellation_aborts_each_graphics_escape_state() {
    let prefixes: &[&[u8]] = &[
        b"\x1b\x18",
        b"\x1bP\x18",
        b"\x1bPq?\x1b\x18",
        b"\x1b_Gf=32,s=1,v=1;AAAA\x1b\x18",
        b"\x1b]1337;File=inline=1:AAAA\x1b\x18",
        b"\x1bPtmux;A\x1b\x18",
        b"\x1bP\x1bA\x1b\x18",
    ];

    for prefix in prefixes {
        let mut parser = GraphicsParser::default();
        assert!(parser.advance(prefix).is_empty());
        let result = only_event(&mut parser, &kitty_raw_rgba()).expect("the new image decodes");
        assert_eq!(result.image.rgba, [255, 0, 0, 255]);
    }
}

#[test]
fn cancelling_a_split_screen_transfer_clears_nested_state() {
    let inner = iterm_file(&red_png());
    let split = inner.len() / 2;
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(&screen_wrap(&inner[..split])).is_empty());
    assert!(parser.screen_continuation());
    assert!(parser.advance(b"\x18").is_empty());
    assert!(!parser.screen_continuation());
    assert!(parser.screen_inner.is_none());

    let result = only_event(&mut parser, &iterm_file(&red_png())).expect("a new Screen transfer");
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn cancelling_a_split_tmux_transfer_clears_nested_state() {
    let inner = iterm_file(&red_png());
    let split = inner.len() / 2;
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(&tmux_wrap(&inner[..split])).is_empty());
    assert!(parser.tmux_continuation());
    assert!(parser.advance(b"\x1a").is_empty());
    assert!(!parser.tmux_continuation());
    assert!(parser.tmux_inner.is_none());

    let result = only_event(&mut parser, &iterm_file(&red_png())).expect("a new tmux transfer");
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_screen_body_at_the_limit_can_close_and_next_image_still_decodes() {
    let body = vec![b'A'; MAX_SCREEN_PASSTHROUGH_BYTES];
    let mut bytes = screen_wrap(&body);
    bytes.extend_from_slice(&kitty_raw_rgba());
    let mut parser = GraphicsParser::default();

    let events = parser.advance(&bytes);
    assert_eq!(events.len(), 1);
    let result = events
        .into_iter()
        .next()
        .expect("one image event")
        .expect("the image decodes");
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn screen_passthrough_has_a_bounded_body() {
    let mut parser = GraphicsParser::default();
    let body = vec![b'A'; MAX_SCREEN_PASSTHROUGH_BYTES];
    let mut inner = vec![0x1b, b'_'];
    inner.extend_from_slice(&body);
    let bytes = screen_wrap(&inner);

    assert_eq!(
        only_event(&mut parser, &bytes),
        Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn every_byte_boundary_preserves_each_protocol_result() {
    for bytes in [
        one_sixel(),
        kitty_raw_rgba(),
        iterm_file(&red_png()),
        tmux_wrap(&kitty_raw_rgba()),
        screen_wrap(&kitty_raw_rgba()),
    ] {
        let mut whole = GraphicsParser::default();
        let expected = only_event(&mut whole, &bytes).expect("the whole transfer decodes");

        let mut split = GraphicsParser::default();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(split.advance(&[byte]));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(
            events
                .pop()
                .expect("one split event")
                .expect("split decodes"),
            expected
        );
    }
}

#[test]
fn malformed_base64_returns_a_typed_error_and_consumes_the_string() {
    let mut parser = GraphicsParser::default();
    let bytes = b"\x1b_Gf=32,s=1,v=1;not-base64\x1b\\Z";

    let result = only_event(&mut parser, bytes);

    assert_eq!(
        result,
        Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Kitty,
        })
    );
    assert!(parser.advance(b"Z").is_empty());
}

#[test]
fn ordinary_apc_and_iterm_commands_do_not_create_graphics_events() {
    let mut parser = GraphicsParser::default();

    assert!(parser
        .advance(b"\x1b_ordinary application command\x1b\\\x1b]1337;SetMark=mark\x07")
        .is_empty());

    let result = only_event(&mut parser, &kitty_raw_rgba()).expect("the next kitty image decodes");
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn unterminated_non_graphics_strings_are_silent_at_finish() {
    let mut parser = GraphicsParser::default();

    assert!(parser
        .advance(b"\x1b_ordinary application command")
        .is_empty());
    assert!(parser.finish().is_empty());

    assert!(parser.advance(b"\x1b]0").is_empty());
    assert!(parser.finish().is_empty());

    assert!(parser.advance(b"\x1b]1337;SetMark=mark").is_empty());
    assert!(parser.finish().is_empty());

    assert!(parser.advance(b"\x1bPtx").is_empty());
    assert!(parser.finish().is_empty());
}

#[test]
fn finishing_a_silent_string_leaves_the_next_image_readable() {
    for bytes in [b"\x1b_ordinary".as_slice(), b"\x1bPtx", b"\x1b]0"] {
        let mut parser = GraphicsParser::default();
        assert!(parser.advance(bytes).is_empty());
        assert!(parser.finish().is_empty());
        assert_eq!(
            only_event(&mut parser, &kitty_raw_rgba())
                .expect("the image after the silent string decodes")
                .image
                .rgba,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn empty_passthrough_wrappers_are_silent_at_finish() {
    for bytes in [b"\x1bPtmux;".as_slice(), b"\x1bP\x1b"] {
        let mut parser = GraphicsParser::default();
        assert!(parser.advance(bytes).is_empty());
        assert!(parser.finish().is_empty());
    }
}

#[test]
fn ordinary_dcs_strings_do_not_create_graphics_events_or_capture_inner_bytes() {
    let mut bytes = b"\x1bPtx".to_vec();
    bytes.extend_from_slice(&kitty_raw_rgba());
    bytes.extend_from_slice(b"\x1b\\");
    bytes.extend_from_slice(&kitty_raw_rgba());
    let mut parser = GraphicsParser::default();

    let events = parser.advance(&bytes);

    assert_eq!(events.len(), 1);
    assert_eq!(
        events
            .into_iter()
            .next()
            .expect("the event after the ordinary DCS")
            .expect("the kitty image decodes")
            .image
            .rgba,
        [255, 0, 0, 255]
    );

    let mut sixel_like = b"\x1bP1;2;3;4X".to_vec();
    sixel_like.extend_from_slice(&kitty_raw_rgba());
    assert!(parser.advance(&sixel_like).is_empty());
}

#[test]
fn oversized_ignored_strings_remain_silent_after_an_engine_swap() {
    let strings = [
        (GraphicsProtocol::Sixel, b"\x1bPtx".as_slice()),
        (GraphicsProtocol::Kitty, b"\x1b_ordinary"),
        (GraphicsProtocol::Iterm2, b"\x1b]0"),
    ];

    for (protocol, prefix) in strings {
        let mut bytes = prefix.to_vec();
        bytes.extend(std::iter::repeat_n(b'A', MAX_GRAPHICS_CARRY_BYTES + 1));
        let mut parser = GraphicsParser::default();
        assert!(parser.advance(&bytes).is_empty());
        let transport = parser
            .transport_state()
            .expect("the ignored string has transport state");
        assert_eq!(
            transport.abandonment,
            Some(GraphicsAbandonment::SilentSequence(protocol))
        );

        let mut resumed = GraphicsParser::default();
        resumed.restore_carry(&[], transport);
        assert!(resumed.advance(b"\x1b\\").is_empty());
        assert_eq!(
            only_event(&mut resumed, &kitty_raw_rgba())
                .expect("the image after the ignored string decodes")
                .image
                .rgba,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn c1_dcs_terminators_end_empty_and_wrapped_strings() {
    let mut parser = GraphicsParser::default();
    let mut empty = vec![0x90, 0x9c];
    empty.extend_from_slice(&kitty_raw_rgba());
    assert_eq!(
        only_event(&mut parser, &empty)
            .expect("the kitty image decodes")
            .image
            .rgba,
        [255, 0, 0, 255]
    );

    let mut screen = vec![0x90];
    screen.extend_from_slice(&kitty_raw_rgba());
    screen.push(0x9c);
    let mut parser = GraphicsParser::default();
    assert_eq!(
        only_event(&mut parser, &screen)
            .expect("the Screen-wrapped kitty image decodes")
            .image
            .rgba,
        [255, 0, 0, 255]
    );

    let mut tmux = tmux_wrap(&kitty_raw_rgba());
    tmux[0] = 0x90;
    tmux.remove(1);
    tmux.truncate(tmux.len() - 2);
    tmux.push(0x9c);
    let mut parser = GraphicsParser::default();
    assert_eq!(
        only_event(&mut parser, &tmux)
            .expect("the tmux-wrapped kitty image decodes")
            .image
            .rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn an_unterminated_transfer_is_reported_as_truncated() {
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(b"\x1b_Gf=32,s=1,v=1;").is_empty());
    assert_eq!(
        parser.finish(),
        [Err(GraphicsError::Truncated {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
}

#[test]
fn a_zero_raw_dimension_is_rejected_before_payload_decode() {
    let mut parser = GraphicsParser::default();
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let bytes = format!("\x1b_Gf=32,s=0,v=1;{payload}\x1b\\").into_bytes();

    assert_eq!(
        only_event(&mut parser, &bytes),
        Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn an_invalid_sixel_command_returns_a_typed_error() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, b"\x1bPq!x@\x1b\\"),
        Err(GraphicsError::InvalidCommand {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn an_oversized_sixel_header_returns_a_typed_error() {
    let mut bytes = b"\x1bP".to_vec();
    bytes.extend(std::iter::repeat_n(b'1', MAX_GRAPHICS_CONTROL_BYTES + 1));
    bytes.extend_from_slice(b"q\x1b\\");
    let mut parser = GraphicsParser::default();

    assert_eq!(
        only_event(&mut parser, &bytes),
        Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn discarded_sequence_bytes_do_not_wrap() {
    let mut parser = GraphicsParser {
        state: GraphicsState::Discard(DiscardParser {
            kind: StringKind::Apc,
            error: GraphicsError::TransferTooLarge {
                protocol: GraphicsProtocol::Kitty,
            },
            escaped: false,
            report: false,
        }),
        sequence_bytes: usize::MAX,
        ..GraphicsParser::default()
    };
    let mut events = Vec::new();

    parser.feed_byte(b'A', &mut events);

    assert_eq!(parser.sequence_bytes, usize::MAX);
    assert_eq!(events, Vec::new());
}

#[test]
fn unsupported_kitty_media_returns_a_typed_error() {
    let mut parser = GraphicsParser::default();
    let bytes = b"\x1b_Gf=101,s=1,v=1;AAAA\x1b\\";

    assert_eq!(
        only_event(&mut parser, bytes),
        Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            format: "101".to_string(),
        })
    );
}

#[test]
fn unsupported_kitty_controls_return_typed_action_errors() {
    for key in [b'd', b'H', b'O', b'P', b'Q', b'V'] {
        let bytes = format!("\x1b_G{}=1;AAAA\x1b\\", key as char).into_bytes();
        let mut parser = GraphicsParser::default();

        assert_eq!(
            only_event(&mut parser, &bytes),
            Err(GraphicsError::UnsupportedAction {
                protocol: GraphicsProtocol::Kitty,
                action: format!("control {}", key as char),
            })
        );
    }
}

#[test]
fn a_sixel_pixel_limit_is_checked_before_allocation() {
    let mut parser = GraphicsParser::default();
    let bytes = b"\x1bPq\"1;1;4097;4097#1@\x1b\\";

    assert_eq!(
        only_event(&mut parser, bytes),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn a_kitty_pixel_limit_is_checked_before_payload_decode() {
    let mut parser = GraphicsParser::default();
    let bytes = b"\x1b_Gf=32,s=4097,v=4097;AAAA\x1b\\";

    assert_eq!(
        only_event(&mut parser, bytes),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn a_hostile_iterm_size_does_not_reserve_the_declared_payload() {
    let mut parser = GraphicsParser::default();
    let bytes = b"\x1b]1337;File=inline=1;size=4294967295:AAAA\x07";

    assert_eq!(
        only_event(&mut parser, bytes),
        Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Iterm2,
        })
    );
}

#[test]
fn a_raster_dimension_limit_returns_image_too_large_before_decode() {
    let bytes = png_with_dimensions(4097, 4097);

    assert_eq!(
        decode_raster(GraphicsProtocol::Iterm2, &bytes),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Iterm2,
        })
    );
}

#[test]
fn rejected_graphics_do_not_change_the_terminal_state() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });
    let before = engine.state().clone();

    let _ = engine.advance(b"\x1b_Ga=p,f=32,s=1,v=1;AAAA\x1b\\");

    assert_eq!(engine.state(), &before);
    assert_eq!(
        engine.take_graphics(),
        [Err(GraphicsError::UnsupportedAction {
            protocol: GraphicsProtocol::Kitty,
            action: "p".to_string(),
        })]
    );
}

#[test]
fn an_image_without_cell_dimensions_is_rejected_without_state_mutation() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });
    let before = engine.state().clone();

    let _ = engine.advance(&one_sixel());

    assert_eq!(engine.state(), &before);
    assert_eq!(
        engine.take_graphics(),
        [Err(GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            reason: ImagePlacementError::MissingCellDimensions {
                width: None,
                height: None,
            },
        })]
    );
}

#[test]
fn engine_places_a_cell_sized_image_at_the_cursor_anchor() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });
    let _ = engine.advance(b"\x1b[2;3H");

    let _ = engine.advance(&kitty_display_cell_rgba(false));

    let placements = engine.state().image_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].anchor(), (1, 2));
    assert_eq!(placements[0].dimensions(), (1, 1));
    assert_eq!(placements[0].covered_cells().collect::<Vec<_>>(), [(1, 2)]);
    let events = engine.take_graphics();
    assert_eq!(events.len(), 1);
    let record = events
        .into_iter()
        .next()
        .expect("the image event")
        .expect("the image decodes");
    assert_eq!(record.anchor, (1, 2));
    assert_eq!(record.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn engine_moves_cursor_after_an_accepted_image_placement() {
    let mut engine = TerminalEngine::new(PtySize { cols: 10, rows: 8 });
    let _ = engine.advance(b"\x1b[3;4H");

    let _ = engine.advance(&kitty_display_cell_rgba_size(3, 2, true));

    assert_eq!(engine.state().active_cursor_position(), (4, 6));
    let placement = &engine.state().image_placements()[0];
    assert_eq!(placement.anchor(), (2, 3));
    assert_eq!(placement.dimensions(), (2, 3));
}

#[test]
fn engine_retransmitting_a_kitty_image_removes_old_placements() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 4 });
    let _ = engine.advance(&kitty_display_cell_rgba_identity(7, 3, false));
    let _ = engine.advance(b"\x1b[2;2H");
    let _ = engine.advance(&kitty_display_cell_rgba_identity(7, 4, false));
    let _ = engine.advance(b"\x1b[3;3H");
    let _ = engine.advance(&kitty_display_cell_rgba_identity(7, 3, false));

    let placements = engine.state().image_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].record().display.image_id, Some(7));
    assert_eq!(placements[0].record().display.placement_id, Some(3));
    assert_eq!(placements[0].anchor(), (2, 2));
}

#[test]
fn engine_records_the_anchor_before_a_subsequent_cursor_move_in_one_chunk() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });
    let mut bytes = kitty_display_cell_rgba(false);
    bytes.extend_from_slice(b"\x1b[2;3H");

    let _ = engine.advance(&bytes);

    let record = engine
        .take_graphics()
        .into_iter()
        .next()
        .expect("the image event")
        .expect("the image decodes");
    assert_eq!(record.anchor, (0, 0));
    assert_eq!(engine.state().active_cursor_position(), (1, 2));
}

#[test]
fn graphics_queue_reports_dropped_events_at_its_bound() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });
    let mut bytes = Vec::new();
    for _ in 0..65 {
        bytes.extend_from_slice(&kitty_display_cell_rgba(false));
    }

    let _ = engine.advance(&bytes);

    assert_eq!(engine.state().image_placements().len(), 65);

    let expected_record = Ok(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
        },
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            cell_columns: Some(1),
            cell_rows: Some(1),
            move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    let mut expected = vec![expected_record; 64];
    expected.push(Err(GraphicsError::QueueFull { dropped: 1 }));

    assert_eq!(engine.take_graphics(), expected);
}

#[test]
fn restarting_preserves_the_graphics_queue_overflow_report() {
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });
    let mut bytes = Vec::new();
    for _ in 0..66 {
        bytes.extend_from_slice(&one_sixel());
    }
    let _ = engine.advance(&bytes);
    let events = engine.take_graphics();
    let state = engine.state().clone();

    let mut resumed = TerminalEngine::from_state_with_graphics_and_events(state, b"", b"", &events);

    assert_eq!(
        events.last(),
        Some(&Err(GraphicsError::QueueFull { dropped: 2 }))
    );
    assert_eq!(resumed.take_graphics(), events);
}

#[test]
fn graphics_queue_error_names_both_limits() {
    assert_eq!(
        GraphicsError::QueueFull { dropped: 2 }.to_string(),
        "2 graphics events were dropped because the graphics event count or image-byte limit was reached"
    );
}

#[test]
fn a_kitty_chunked_transfer_decodes_only_after_the_final_chunk() {
    let mut parser = GraphicsParser::default();
    let encoded = STANDARD.encode([255, 0, 0, 255]);
    let split = encoded.len() / 2;
    let first = format!("\x1b_Gf=32,s=1,v=1,m=1;{}\x1b\\", &encoded[..split]);
    let second = format!("\x1b_Gm=0;{}\x1b\\", &encoded[split..]);

    assert!(parser.advance(first.as_bytes()).is_empty());
    let result = only_event(&mut parser, second.as_bytes()).expect("the final chunk decodes");
    assert_eq!(result.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_three_chunk_kitty_transfer_keeps_each_chunk_once() {
    let raw = vec![255; 1750 * 4];
    let encoded = STANDARD.encode(raw);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(MAX_KITTY_CHUNK_BYTES).collect();
    assert_eq!(chunks.len(), 3);
    assert!(chunks[..2]
        .iter()
        .all(|chunk| chunk.len().is_multiple_of(4)));
    let mut bytes = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let header = if index == 0 {
            "\x1b_Ga=T,f=32,s=1750,v=1,m=1;".to_string()
        } else if index + 1 == chunks.len() {
            "\x1b_Gm=0;".to_string()
        } else {
            "\x1b_Gm=1;".to_string()
        };
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
    }
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, &bytes).expect("the three chunks decode");

    assert_eq!(result.action, ImageAction::TransmitAndDisplay);
    assert_eq!(result.image.width, 1750);
    assert_eq!(result.image.height, 1);
    assert_eq!(result.image.rgba.len(), 1750 * 4);
    assert!(result.image.rgba.iter().all(|byte| *byte == 255));
}

#[test]
fn a_kitty_transfer_that_exceeds_the_carry_budget_is_abandoned_after_an_engine_swap() {
    let encoded = STANDARD.encode(vec![255; 16_384 * 4]);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(MAX_KITTY_CHUNK_BYTES).collect();
    let mut parser = GraphicsParser::default();
    let mut cut = None;
    for (index, chunk) in chunks.iter().enumerate() {
        let header = if index == 0 {
            "\x1b_Gf=32,s=16384,v=1,m=1;".to_string()
        } else {
            "\x1b_Gm=1;".to_string()
        };
        let mut bytes = header.into_bytes();
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
        assert!(parser.advance(&bytes).is_empty());
        if !parser
            .transport_state()
            .expect("the multipart transfer has state")
            .carryable
        {
            cut = Some(index + 1);
            break;
        }
    }
    let cut = cut.expect("the transfer passes the carry budget");
    let transport = parser
        .transport_state()
        .expect("the oversized transfer has transport state");
    assert_eq!(
        transport.abandonment,
        Some(GraphicsAbandonment::Transfer(GraphicsProtocol::Kitty)),
        "{transport:?}"
    );

    let mut resumed = GraphicsParser::default();
    resumed.restore_carry(&[], transport);
    for (index, chunk) in chunks.iter().enumerate().skip(cut) {
        let header = if index + 1 == chunks.len() {
            "\x1b_Gm=0;"
        } else {
            "\x1b_Gm=1;"
        };
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
        let events = resumed.advance(&bytes);
        if index + 1 == chunks.len() {
            assert_eq!(
                events,
                [Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Kitty,
                })]
            );
        } else {
            assert!(events.is_empty());
        }
    }
}

#[test]
fn an_active_kitty_chunk_that_exceeds_the_carry_budget_is_drained_after_an_engine_swap() {
    let encoded = STANDARD.encode(vec![255; 16_384 * 4]);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(MAX_KITTY_CHUNK_BYTES).collect();
    let mut parser = GraphicsParser::default();
    let mut first = b"\x1b_Gf=32,s=16384,v=1,m=1;".to_vec();
    first.extend_from_slice(chunks[0]);
    first.extend_from_slice(b"\x1b\\");
    assert!(parser.advance(&first).is_empty());

    let mut cut = None;
    for (index, chunk) in chunks.iter().enumerate().skip(1) {
        let header = b"\x1b_Gm=1;";
        let transport = parser
            .transport_state()
            .expect("the multipart transfer has state");
        if transport.carry.len() + header.len() + chunk.len() > MAX_GRAPHICS_CARRY_BYTES {
            let prefix_len = MAX_GRAPHICS_CARRY_BYTES - transport.carry.len() - header.len() + 1;
            let mut prefix = header.to_vec();
            prefix.extend_from_slice(&chunk[..prefix_len]);
            assert!(parser.advance(&prefix).is_empty());
            let transport = parser
                .transport_state()
                .expect("the active chunk has transport state");
            assert_eq!(
                transport.abandonment,
                Some(GraphicsAbandonment::Transfer(GraphicsProtocol::Kitty))
            );
            cut = Some((index, prefix_len));
            break;
        }
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
        assert!(parser.advance(&bytes).is_empty());
    }
    let (cut, prefix_len) = cut.expect("the active chunk passes the carry budget");

    let transport = parser
        .transport_state()
        .expect("the oversized active chunk has transport state");
    let mut resumed = GraphicsParser::default();
    resumed.restore_carry(&[], transport);
    let mut bytes = chunks[cut][prefix_len..].to_vec();
    bytes.extend_from_slice(b"\x1b\\");
    assert!(resumed.advance(&bytes).is_empty());

    for (index, chunk) in chunks.iter().enumerate().skip(cut + 1) {
        let header = if index + 1 == chunks.len() {
            b"\x1b_Gm=0;"
        } else {
            b"\x1b_Gm=1;"
        };
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
        let events = resumed.advance(&bytes);
        if index + 1 == chunks.len() {
            assert_eq!(
                events,
                [Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Kitty,
                })]
            );
        } else {
            assert!(events.is_empty());
        }
    }
}

#[test]
fn an_open_graphics_sequence_that_exceeds_the_carry_budget_is_drained_after_an_engine_swap() {
    let mut bytes = b"\x1b_Gf=32,s=1,v=1;".to_vec();
    bytes.extend(std::iter::repeat_n(b'A', MAX_GRAPHICS_CARRY_BYTES + 1));
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(&bytes).is_empty());
    let transport = parser
        .transport_state()
        .expect("the open sequence has transport state");
    assert_eq!(
        transport.abandonment,
        Some(GraphicsAbandonment::Sequence(GraphicsProtocol::Kitty)),
        "{transport:?}"
    );

    let mut resumed = GraphicsParser::default();
    resumed.restore_carry(&[], transport);

    assert_eq!(
        resumed.advance(b"\x1b\\"),
        [Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
}

#[test]
fn an_iterm_transfer_that_exceeds_the_carry_budget_is_abandoned_after_an_engine_swap() {
    let encoded = STANDARD.encode(vec![0; 16_384 * 4]);
    let first = format!(
        "\x1b]1337;MultipartFile=inline=1;size={}:{}\x07",
        16_384 * 4,
        encoded
    )
    .into_bytes();
    let mut parser = GraphicsParser::default();

    assert!(parser.advance(&first).is_empty());
    let transport = parser
        .transport_state()
        .expect("the multipart transfer has transport state");
    assert_eq!(
        transport.abandonment,
        Some(GraphicsAbandonment::Transfer(GraphicsProtocol::Iterm2)),
        "{transport:?}"
    );

    let mut resumed = GraphicsParser::default();
    resumed.restore_carry(&[], transport);
    assert!(resumed.advance(b"\x1b]1337;FilePart=AAAA\x07").is_empty());
    assert_eq!(
        resumed.advance(b"\x1b]1337;FileEnd\x07"),
        [Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Iterm2,
        })]
    );
}

#[test]
fn kitty_display_metadata_is_preserved_exactly() {
    let payload = STANDARD.encode([255, 0, 0, 255]);
    let bytes = format!(
        "\x1b_Ga=T,f=32,s=1,v=1,I=8,p=9,N=1,U=1,w=1,h=1,c=2,r=3,x=4,y=5,X=6,Y=7,C=1,z=-2;{payload}\x1b\\"
    );
    let mut parser = GraphicsParser::default();

    let result = only_event(&mut parser, bytes.as_bytes()).expect("the kitty image decodes");

    assert_eq!(result.display.image_id, None);
    assert_eq!(result.display.image_number, Some(8));
    assert_eq!(result.display.placement_id, Some(9));
    assert_eq!(result.display.usage_hints, 1);
    assert!(result.display.unicode_placeholder);
    assert_eq!(result.display.width, Some(ImageDimension::Pixels(1)));
    assert_eq!(result.display.height, Some(ImageDimension::Pixels(1)));
    assert_eq!(result.display.cell_columns, Some(2));
    assert_eq!(result.display.cell_rows, Some(3));
    assert_eq!(result.display.source_offset_x, Some(4));
    assert_eq!(result.display.source_offset_y, Some(5));
    assert_eq!(result.display.cell_offset_x, Some(6));
    assert_eq!(result.display.cell_offset_y, Some(7));
    assert!(!result.display.move_cursor);
    assert_eq!(result.display.z_index, -2);
}

#[test]
fn a_chunked_kitty_transfer_survives_an_engine_swap() {
    let encoded = STANDARD.encode([255, 0, 0, 255]);
    let split = encoded.len() / 2;
    let first = format!("\x1b_Gf=32,s=1,v=1,m=1;{}\x1b\\", &encoded[..split]);
    let second = format!("\x1b_Gm=0;{}\x1b\\", &encoded[split..]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(first.as_bytes());
    let carried = engine.undecoded().to_vec();
    let graphics_carried = engine.graphics_undecoded().to_vec();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics(state, &carried, &graphics_carried);
    let _ = next.advance(second.as_bytes());

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed image event")
        .expect("the resumed image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_chunked_kitty_transfer_survives_two_engine_swaps() {
    let encoded = STANDARD.encode([255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255]);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 chunks are ASCII"))
        .collect();
    let first = format!("\x1b_Gf=32,s=3,v=1,m=1;{}\x1b\\", chunks[0]);
    let second = format!("\x1b_Gm=1;{}\x1b\\", chunks[1]);
    let third = format!("\x1b_Gm=1;{}\x1b\\", chunks[2]);
    let fourth = format!("\x1b_Gm=0;{}\x1b\\", chunks[3]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(first.as_bytes());
    let carried = engine.undecoded().to_vec();
    let graphics_carried = engine.graphics_undecoded().to_vec();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics(state, &carried, &graphics_carried);
    let _ = next.advance(second.as_bytes());
    let carried = next.undecoded().to_vec();
    let graphics_carried = next.graphics_undecoded().to_vec();
    let state = next.into_state();
    let mut final_engine =
        TerminalEngine::from_state_with_graphics(state, &carried, &graphics_carried);
    let _ = final_engine.advance(third.as_bytes());
    let _ = final_engine.advance(fourth.as_bytes());

    let event = final_engine
        .take_graphics()
        .into_iter()
        .next()
        .expect("the twice-resumed image event")
        .expect("the twice-resumed image decodes");
    assert_eq!(event.image.width, 3);
    assert_eq!(event.image.height, 1);
    assert_eq!(event.image.rgba, [255, 0, 0, 255].repeat(3));
}

#[test]
fn chunked_kitty_transfer_carry_survives_an_unrelated_escape() {
    let encoded = STANDARD.encode([255, 0, 0, 255]);
    let split = encoded.len() / 2;
    let first = format!("\x1b_Gf=32,s=1,v=1,m=1;{}\x1b\\", &encoded[..split]);
    let second = format!("\x1b_Gm=0;{}\x1b\\", &encoded[split..]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(first.as_bytes());
    let _ = engine.advance(b"\x1b[2J");
    let carried = engine.undecoded().to_vec();
    let graphics_carried = engine.graphics_undecoded().to_vec();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics(state, &carried, &graphics_carried);
    let _ = next.advance(second.as_bytes());

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed image event")
        .expect("the resumed image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn an_iterm_multipart_transfer_survives_an_engine_swap() {
    let bytes = red_png();
    let encoded = STANDARD.encode(&bytes);
    let split = encoded.len() / 2;
    let first = format!(
        "\x1b]1337;MultipartFile=inline=1;width=1;height=1;size={}\x07\
\x1b]1337;FilePart={}\x07",
        bytes.len(),
        &encoded[..split],
    );
    let second = format!(
        "\x1b]1337;FilePart={}\x07\x1b]1337;FileEnd\x07",
        &encoded[split..],
    );
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(first.as_bytes());
    let carried = engine.undecoded().to_vec();
    let graphics_carried = engine.graphics_undecoded().to_vec();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics(state, &carried, &graphics_carried);
    let _ = next.advance(second.as_bytes());

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed multipart event")
        .expect("the resumed multipart image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_screen_transfer_survives_an_engine_swap() {
    let inner = iterm_cell_file(&red_png());
    let split = inner.len() / 2;
    let first = screen_wrap(&inner[..split]);
    let second = screen_wrap(&inner[split..]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(&first);
    let carried = engine.undecoded().to_vec();
    let graphics_carried = engine.graphics_undecoded().to_vec();
    let screen_continuation = engine.graphics_screen_continuation();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics_and_events_and_screen(
        state,
        &carried,
        &graphics_carried,
        &[],
        screen_continuation,
        false,
    );
    let _ = next.advance(&second);

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed Screen image event")
        .expect("the resumed Screen image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_c1_screen_wrapper_with_an_inner_transfer_survives_an_engine_swap() {
    let inner = iterm_cell_file(&red_png());
    let split = inner.len() / 2;
    let first = screen_wrap(&inner[..split]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(&first);
    let _ = engine.advance(&[0x90]);
    assert!(engine.graphics_screen_continuation());
    assert!(engine.graphics_screen_wrapper_active());
    let undecoded = engine.undecoded().to_vec();
    let graphics_undecoded = engine.graphics_undecoded().to_vec();
    let transport = engine
        .graphics_transport_state()
        .expect("the split wrapper has transport state");
    assert!(transport.screen_inner.is_some());
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics_and_events_and_wrappers(
        state,
        &undecoded,
        &graphics_undecoded,
        &[],
        transport,
    );
    let mut second = inner[split..].to_vec();
    second.extend_from_slice(b"\x1b\\");
    let _ = next.advance(&second);

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed C1 Screen image event")
        .expect("the resumed C1 Screen image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_tmux_transfer_survives_an_engine_swap() {
    let inner = iterm_cell_file(&red_png());
    let split = inner.len() / 2;
    let first = tmux_wrap(&inner[..split]);
    let second = tmux_wrap(&inner[split..]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(&first);
    let carried = engine.undecoded().to_vec();
    let graphics_carried = engine.graphics_undecoded().to_vec();
    let tmux_continuation = engine.graphics_tmux_continuation();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics_and_events_and_wrappers(
        state,
        &carried,
        &graphics_carried,
        &[],
        GraphicsTransportState {
            tmux_continuation,
            ..GraphicsTransportState::default()
        },
    );
    let _ = next.advance(&second);

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed tmux image event")
        .expect("the resumed tmux image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn a_c1_tmux_wrapper_with_an_inner_transfer_survives_an_engine_swap() {
    let inner = iterm_cell_file(&red_png());
    let split = inner.len() / 2;
    let first = tmux_wrap(&inner[..split]);
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(&first);
    let _ = engine.advance(&[0x90]);
    assert!(engine.graphics_tmux_continuation());
    assert!(engine.graphics_tmux_wrapper_active());
    let undecoded = engine.undecoded().to_vec();
    let graphics_undecoded = engine.graphics_undecoded().to_vec();
    let transport = engine
        .graphics_transport_state()
        .expect("the split wrapper has transport state");
    assert!(transport.tmux_inner.is_some());
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics_and_events_and_wrappers(
        state,
        &undecoded,
        &graphics_undecoded,
        &[],
        transport,
    );
    let mut second = b"tmux;".to_vec();
    second.extend_from_slice(&inner[split..]);
    second.extend_from_slice(b"\x1b\\");
    let _ = next.advance(&second);

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed C1 tmux image event")
        .expect("the resumed C1 tmux image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn nested_passthrough_wrappers_survive_an_engine_swap() {
    let inner = iterm_cell_file(&red_png());
    let split = inner.len() / 2;
    let first = tmux_wrap(&screen_wrap(&inner[..split]));
    let second = tmux_wrap(&screen_wrap(&inner[split..]));
    let mut engine = TerminalEngine::new(PtySize { cols: 8, rows: 2 });

    let _ = engine.advance(&first);
    let transport = engine
        .graphics_transport_state()
        .expect("the nested wrappers have transport state");
    assert!(transport.tmux_inner.is_some());
    assert!(transport
        .tmux_inner
        .as_ref()
        .expect("the tmux parser")
        .screen_inner
        .is_some());
    let undecoded = engine.undecoded().to_vec();
    let graphics_undecoded = engine.graphics_undecoded().to_vec();
    let state = engine.into_state();
    let mut next = TerminalEngine::from_state_with_graphics_and_events_and_wrappers(
        state,
        &undecoded,
        &graphics_undecoded,
        &[],
        transport,
    );
    let _ = next.advance(&second);

    let event = next
        .take_graphics()
        .into_iter()
        .next()
        .expect("the resumed nested image event")
        .expect("the resumed nested image decodes");
    assert_eq!(event.image.rgba, [255, 0, 0, 255]);
}

#[test]
fn passthrough_wrapper_nesting_stays_bounded() {
    let mut bytes = kitty_raw_rgba();
    for _ in 0..=MAX_GRAPHICS_WRAPPER_DEPTH {
        bytes = tmux_wrap(&bytes);
    }
    let mut parser = GraphicsParser::default();

    assert_eq!(
        parser.advance(&bytes),
        [Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })]
    );
}
