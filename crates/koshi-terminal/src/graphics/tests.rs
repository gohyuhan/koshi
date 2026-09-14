//! Tests for bounded Sixel, kitty, and iTerm2 image decoding.

use super::*;

impl GraphicsParser {
    fn decode_completed_graphics_events(
        &mut self,
        graphics_input_bytes: &[u8],
    ) -> Vec<Result<DecodedGraphics, GraphicsError>> {
        self.process_graphics_operations(graphics_input_bytes)
            .into_iter()
            .map(|graphics_event_result| {
                graphics_event_result.and_then(|graphics_operation| match graphics_operation {
                    GraphicsOperation::Image(decoded_graphics) => Ok(decoded_graphics),
                    GraphicsOperation::Failure { graphics_error, .. } => Err(graphics_error),
                    GraphicsOperation::Command(graphics_command) => {
                        panic!("image decoder test received {graphics_command:?}")
                    }
                    GraphicsOperation::Sixel(sixel_graphic) => {
                        let mut palette = koshi_sixel::SixelPalette::default();
                        palette.apply_palette_changes(sixel_graphic.get_palette_changes());
                        let indexed_image = sixel_graphic
                            .get_indexed_image()
                            .expect("the Sixel has drawable pixels");
                        Ok(DecodedGraphics {
                            is_query: false,
                            protocol: GraphicsProtocol::Sixel,
                            image: indexed_image
                                .resolve_indexed_image(&palette, palette.get_register_color(0))?,
                            animation: None,
                            action: ImageAction::Display,
                            display: ImageDisplay {
                                sixel_background: Some(sixel_graphic.get_sixel_background()),
                                ..ImageDisplay::default()
                            },
                        })
                    }
                })
            })
            .collect()
    }
}

use crate::engine::{GraphicsTransportState, TerminalEngine};
use crate::state::ImagePlacementError;
use koshi_core::process::PtySize;
use koshi_image::{decode_raster, decompress_bounded};
use std::panic::{catch_unwind, AssertUnwindSafe};

fn build_red_png_bytes() -> Vec<u8> {
    use image::ImageEncoder;

    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel image encodes");
    png_bytes
}

fn png_with_dimensions(image_pixel_width: u32, image_pixel_height: u32) -> Vec<u8> {
    let mut png_bytes = build_red_png_bytes();
    png_bytes[16..20].copy_from_slice(&image_pixel_width.to_be_bytes());
    png_bytes[20..24].copy_from_slice(&image_pixel_height.to_be_bytes());
    let mut png_crc = 0xffff_ffffu32;
    for &crc_byte in &png_bytes[12..29] {
        png_crc ^= u32::from(crc_byte);
        for _ in 0..8 {
            png_crc = if png_crc & 1 == 1 {
                (png_crc >> 1) ^ 0xedb8_8320
            } else {
                png_crc >> 1
            };
        }
    }
    png_bytes[29..33].copy_from_slice(&(!png_crc).to_be_bytes());
    png_bytes
}

fn build_animated_gif() -> Vec<u8> {
    let mut gif_bytes = Vec::new();
    {
        let mut gif_encoder = image::codecs::gif::GifEncoder::new(&mut gif_bytes);
        let gif_frames = [
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
        gif_encoder
            .encode_frames(gif_frames)
            .expect("the two-frame GIF encodes");
    }
    gif_bytes
}

fn build_png_chunk(chunk_type_bytes: &[u8; 4], chunk_payload_bytes: &[u8]) -> Vec<u8> {
    let mut png_chunk_bytes =
        Vec::with_capacity(12 + chunk_type_bytes.len() + chunk_payload_bytes.len());
    png_chunk_bytes.extend_from_slice(&(chunk_payload_bytes.len() as u32).to_be_bytes());
    png_chunk_bytes.extend_from_slice(chunk_type_bytes);
    png_chunk_bytes.extend_from_slice(chunk_payload_bytes);
    let mut crc = 0xffff_ffffu32;
    for &crc_byte in chunk_type_bytes.iter().chain(chunk_payload_bytes) {
        crc ^= u32::from(crc_byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    png_chunk_bytes.extend_from_slice(&(!crc).to_be_bytes());
    png_chunk_bytes
}

fn build_animated_png() -> Vec<u8> {
    let png_bytes = build_red_png_bytes();
    let mut animated_png_bytes = png_bytes[..8].to_vec();
    let mut chunk_start_byte_index = 8;
    let mut has_inserted_animation_chunks = false;
    let mut first_frame_bytes = Vec::new();
    while chunk_start_byte_index < png_bytes.len() {
        let chunk_byte_count = u32::from_be_bytes(
            png_bytes[chunk_start_byte_index..chunk_start_byte_index + 4]
                .try_into()
                .expect("PNG length"),
        );
        let chunk_end_byte_index = chunk_start_byte_index
            + 12
            + usize::try_from(chunk_byte_count).expect("PNG length fits");
        let chunk_type_bytes: &[u8; 4] = png_bytes
            [chunk_start_byte_index + 4..chunk_start_byte_index + 8]
            .try_into()
            .expect("PNG chunk type");
        let chunk_payload_bytes = &png_bytes[chunk_start_byte_index + 8..chunk_end_byte_index - 4];
        match chunk_type_bytes {
            b"IDAT" if !has_inserted_animation_chunks => {
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(b"fcTL", &build_png_frame_control(0)));
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(chunk_type_bytes, chunk_payload_bytes));
                first_frame_bytes.extend_from_slice(chunk_payload_bytes);
                has_inserted_animation_chunks = true;
            }
            b"IEND" if has_inserted_animation_chunks => {
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(b"fcTL", &build_png_frame_control(1)));
                let mut animation_frame_payload_bytes =
                    Vec::with_capacity(4 + first_frame_bytes.len());
                animation_frame_payload_bytes.extend_from_slice(&2u32.to_be_bytes());
                animation_frame_payload_bytes.extend_from_slice(&first_frame_bytes);
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(b"fdAT", &animation_frame_payload_bytes));
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(chunk_type_bytes, chunk_payload_bytes));
            }
            _ => animated_png_bytes
                .extend_from_slice(&png_bytes[chunk_start_byte_index..chunk_end_byte_index]),
        }
        chunk_start_byte_index = chunk_end_byte_index;
    }
    animated_png_bytes
}

fn build_png_frame_control(frame_sequence_number: u32) -> [u8; 26] {
    let mut frame_control_bytes = [0; 26];
    frame_control_bytes[..4].copy_from_slice(&frame_sequence_number.to_be_bytes());
    frame_control_bytes[4..8].copy_from_slice(&1u32.to_be_bytes());
    frame_control_bytes[8..12].copy_from_slice(&1u32.to_be_bytes());
    frame_control_bytes[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame_control_bytes[22..24].copy_from_slice(&10u16.to_be_bytes());
    frame_control_bytes
}

fn build_red_webp_bytes() -> Vec<u8> {
    use image::ImageEncoder;

    let mut webp_bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel WebP encodes");
    webp_bytes
}

fn build_webp_chunk(chunk_type_bytes: &[u8; 4], chunk_payload_bytes: &[u8]) -> Vec<u8> {
    let mut webp_chunk_bytes =
        Vec::with_capacity(8 + chunk_payload_bytes.len() + chunk_payload_bytes.len() % 2);
    webp_chunk_bytes.extend_from_slice(chunk_type_bytes);
    webp_chunk_bytes.extend_from_slice(&(chunk_payload_bytes.len() as u32).to_le_bytes());
    webp_chunk_bytes.extend_from_slice(chunk_payload_bytes);
    if chunk_payload_bytes.len() % 2 == 1 {
        webp_chunk_bytes.push(0);
    }
    webp_chunk_bytes
}

fn build_animated_webp() -> Vec<u8> {
    let webp_bytes = build_red_webp_bytes();
    let mut chunk_start_byte_index = 12;
    let mut frame_payload_bytes = Vec::new();
    while chunk_start_byte_index < webp_bytes.len() {
        let chunk_byte_count = usize::try_from(u32::from_le_bytes(
            webp_bytes[chunk_start_byte_index + 4..chunk_start_byte_index + 8]
                .try_into()
                .expect("WebP length"),
        ))
        .expect("WebP length fits");
        if &webp_bytes[chunk_start_byte_index..chunk_start_byte_index + 4] == b"VP8L" {
            frame_payload_bytes.extend_from_slice(
                &webp_bytes
                    [chunk_start_byte_index + 8..chunk_start_byte_index + 8 + chunk_byte_count],
            );
        }
        chunk_start_byte_index += 8 + chunk_byte_count + chunk_byte_count % 2;
    }

    let vp8x = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let animation_control_bytes = [0, 0, 0, 0, 0, 0];
    let mut animation_frame_metadata = [0; 16];
    animation_frame_metadata[12] = 1;
    let mut animation_frame_chunk_bytes = animation_frame_metadata.to_vec();
    animation_frame_chunk_bytes.extend_from_slice(&build_webp_chunk(b"VP8L", &frame_payload_bytes));

    let animation_chunks = [
        build_webp_chunk(b"VP8X", &vp8x),
        build_webp_chunk(b"ANIM", &animation_control_bytes),
        build_webp_chunk(b"ANMF", &animation_frame_chunk_bytes),
        build_webp_chunk(b"ANMF", &animation_frame_chunk_bytes),
    ];
    let riff_body_byte_count: usize = 4 + animation_chunks.iter().map(Vec::len).sum::<usize>();
    let mut animated_webp_bytes = b"RIFF".to_vec();
    animated_webp_bytes.extend_from_slice(&(riff_body_byte_count as u32).to_le_bytes());
    animated_webp_bytes.extend_from_slice(b"WEBP");
    for animation_chunk_bytes in animation_chunks {
        animated_webp_bytes.extend_from_slice(&animation_chunk_bytes);
    }
    animated_webp_bytes
}

fn build_kitty_raw_rgba() -> Vec<u8> {
    let base64_image_bytes = STANDARD.encode([255, 0, 0, 255]);
    format!("\x1b_Gf=32,s=1,v=1;{base64_image_bytes}\x1b\\").into_bytes()
}

fn build_kitty_c1_raw_rgba() -> Vec<u8> {
    let base64_image_bytes = STANDARD.encode([255, 0, 0, 255]);
    let mut graphics_input_bytes = vec![0x9f];
    graphics_input_bytes
        .extend_from_slice(format!("Gf=32,s=1,v=1;{base64_image_bytes}").as_bytes());
    graphics_input_bytes.push(0x9c);
    graphics_input_bytes
}

fn build_kitty_display_cell_rgba(should_move_cursor: bool) -> Vec<u8> {
    build_kitty_display_cell_rgba_size(1, 1, should_move_cursor)
}

fn build_kitty_display_cell_rgba_size(
    column_count: u32,
    row_count: u32,
    should_move_cursor: bool,
) -> Vec<u8> {
    let base64_image_bytes =
        STANDARD.encode([255, 0, 0, 255].repeat((column_count * row_count) as usize));
    let cursor_movement_flag = if should_move_cursor { 0 } else { 1 };
    format!(
        "\x1b_Ga=T,f=32,s={column_count},v={row_count},c={column_count},r={row_count},C={cursor_movement_flag};{base64_image_bytes}\x1b\\"
    )
        .into_bytes()
}

fn build_kitty_display_cell_rgba_identity(
    image_id: u32,
    placement_id: u32,
    should_move_cursor: bool,
) -> Vec<u8> {
    let base64_image_bytes = STANDARD.encode([255, 0, 0, 255]);
    let cursor_movement_flag = if should_move_cursor { 0 } else { 1 };
    format!(
        "\x1b_Ga=T,f=32,s=1,v=1,i={image_id},p={placement_id},c=1,r=1,C={cursor_movement_flag};{base64_image_bytes}\x1b\\"
    )
    .into_bytes()
}

fn build_one_sixel() -> Vec<u8> {
    b"\x1bPq\"1;1;1;1#1;2;100;0;0#1@\x1b\\".to_vec()
}

fn build_iterm_file(image_bytes: &[u8]) -> Vec<u8> {
    let base64_image_bytes = STANDARD.encode(image_bytes);
    format!(
        "\x1b]1337;File=inline=1;size={};width=1px;height=1px;preserveAspectRatio=0:{}\x07",
        image_bytes.len(),
        base64_image_bytes
    )
    .into_bytes()
}

fn build_iterm_cell_file(image_bytes: &[u8]) -> Vec<u8> {
    let base64_image_bytes = STANDARD.encode(image_bytes);
    format!(
        "\x1b]1337;File=inline=1;size={};width=1;height=1;preserveAspectRatio=0:{}\x07",
        image_bytes.len(),
        base64_image_bytes
    )
    .into_bytes()
}

fn build_iterm_multipart(image_bytes: &[u8]) -> Vec<u8> {
    let base64_image_bytes = STANDARD.encode(image_bytes);
    let split_byte_index = base64_image_bytes.len() / 2;
    format!(
        "\x1b]1337;MultipartFile=inline=1;size={}\x07\
\x1b]1337;FilePart={}\x07\
\x1b]1337;FilePart={}\x07\
\x1b]1337;FileEnd\x07",
        image_bytes.len(),
        &base64_image_bytes[..split_byte_index],
        &base64_image_bytes[split_byte_index..],
    )
    .into_bytes()
}

fn wrap_tmux(inner_graphics_bytes: &[u8]) -> Vec<u8> {
    let mut tmux_wrapped_bytes = b"\x1bPtmux;".to_vec();
    for &graphics_byte in inner_graphics_bytes {
        if graphics_byte == 0x1b {
            tmux_wrapped_bytes.push(0x1b);
        }
        tmux_wrapped_bytes.push(graphics_byte);
    }
    tmux_wrapped_bytes.extend_from_slice(b"\x1b\\");
    tmux_wrapped_bytes
}

fn wrap_screen(inner_graphics_bytes: &[u8]) -> Vec<u8> {
    let mut screen_wrapped_bytes = b"\x1bP".to_vec();
    screen_wrapped_bytes.extend_from_slice(inner_graphics_bytes);
    screen_wrapped_bytes.extend_from_slice(b"\x1b\\");
    screen_wrapped_bytes
}

fn get_only_graphics_event(
    parser: &mut GraphicsParser,
    graphics_input_bytes: &[u8],
) -> Result<DecodedGraphics, GraphicsError> {
    let completed_graphics_events = parser.decode_completed_graphics_events(graphics_input_bytes);
    assert_eq!(completed_graphics_events.len(), 1);
    completed_graphics_events
        .into_iter()
        .next()
        .expect("one event")
}

fn get_only_sixel_graphic(
    parser: &mut GraphicsParser,
    graphics_input_bytes: &[u8],
) -> Result<koshi_sixel::SixelGraphic, GraphicsError> {
    let completed_graphics_events = parser.process_graphics_operations(graphics_input_bytes);
    assert_eq!(completed_graphics_events.len(), 1);
    match completed_graphics_events
        .into_iter()
        .next()
        .expect("one event")?
    {
        GraphicsOperation::Sixel(sixel_graphic) => Ok(sixel_graphic),
        graphics_operation => {
            panic!("expected a Sixel graphics_operation, got {graphics_operation:?}")
        }
    }
}

#[test]
fn sixel_decodes_one_red_pixel_without_terminal_state() {
    let mut parser = GraphicsParser::default();

    let graphics_event =
        get_only_graphics_event(&mut parser, &build_one_sixel()).expect("the Sixel decodes");

    assert_eq!(graphics_event.protocol, GraphicsProtocol::Sixel);
    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 6);
    assert_eq!(&graphics_event.image.rgba_bytes[..4], [255, 0, 0, 255]);
    assert_eq!(
        &graphics_event.image.rgba_bytes[4..],
        &[0, 0, 0, 255].repeat(5)
    );
}

#[test]
fn sixel_header_accepts_omitted_optional_parameters() {
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, b"\x1bP;2q#1;2;100;0;0@\x1b\\")
        .expect("the Sixel header decodes");

    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 12);
    assert_eq!(&graphics_event.image.rgba_bytes[..4], [255, 0, 0, 255]);
    assert_eq!(
        &graphics_event.image.rgba_bytes[4..],
        [255, 0, 0, 255]
            .into_iter()
            .chain([0, 0, 0, 255].repeat(10))
            .collect::<Vec<_>>()
            .as_slice()
    );
    assert_eq!(
        graphics_event.display.sixel_background,
        Some(SixelBackground::Terminal)
    );
}

#[test]
fn sixel_supports_hls_colors_and_the_background_select_parameter() {
    let mut parser = GraphicsParser::default();
    let hls_graphics_event = get_only_graphics_event(&mut parser, b"\x1bPq#1;1;0;50;100@\x1b\\")
        .expect("the HLS color decodes");
    assert_eq!(&hls_graphics_event.image.rgba_bytes[..4], [0, 0, 255, 255]);

    let mut parser = GraphicsParser::default();
    let background_sixel_graphic = get_only_sixel_graphic(&mut parser, b"\x1bP0;0q?\x1b\\")
        .expect("the opaque background metadata decodes");
    let mut palette = koshi_sixel::SixelPalette::default();
    palette.apply_palette_changes(background_sixel_graphic.get_palette_changes());
    let background_image = background_sixel_graphic
        .get_indexed_image()
        .expect("the opaque background has pixels");
    let background_image = background_image
        .resolve_indexed_image(&palette, palette.get_register_color(0))
        .expect("the opaque background resolves");
    assert_eq!(background_image.rgba_bytes, [0, 0, 0, 255].repeat(12));
    assert_eq!(
        background_sixel_graphic.get_sixel_background(),
        SixelBackground::Terminal
    );

    let mut parser = GraphicsParser::default();
    let transparent_sixel_graphic = get_only_sixel_graphic(&mut parser, b"\x1bP0;1q?\x1b\\")
        .expect("the transparent background metadata decodes");
    assert!(transparent_sixel_graphic.get_indexed_image().is_none());
    assert_eq!(
        transparent_sixel_graphic.get_sixel_background(),
        SixelBackground::Preserve
    );
}

#[test]
fn sixel_growth_keeps_a_valid_image_near_the_dimension_limit() {
    let mut sixel_input_bytes = b"\x1bPq!9000@!1000@".to_vec();
    sixel_input_bytes.extend_from_slice(b"\x1b\\");
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, &sixel_input_bytes)
        .expect("the growing Sixel decodes");

    assert_eq!(graphics_event.image.pixel_width, 10_000);
    assert_eq!(graphics_event.image.pixel_height, 12);
}

#[test]
fn sixel_growth_preserves_the_existing_canvas_when_fallback_would_shrink_it() {
    let sixel_input_bytes = b"\x1bPq!3000~-~-~$!13000~\x1b\\";

    let growth_result = catch_unwind(AssertUnwindSafe(|| {
        let mut parser = GraphicsParser::default();
        get_only_graphics_event(&mut parser, sixel_input_bytes)
    }))
    .expect("Sixel growth must not panic");

    let graphics_event = growth_result.expect("the Sixel image remains within the final limits");
    assert_eq!(graphics_event.image.pixel_width, 13_000);
    assert_eq!(graphics_event.image.pixel_height, 36);
    assert_eq!(graphics_event.image.rgba_bytes.len(), 13_000 * 36 * 4);
    assert_eq!(&graphics_event.image.rgba_bytes[0..4], &[0, 0, 0, 255]);
}

#[test]
fn sixel_growth_preserves_existing_width_when_fallback_would_shrink_it() {
    let mut sixel_input_bytes = b"\x1bPq!1000~".to_vec();
    for _ in 0..16 {
        sixel_input_bytes.extend_from_slice(b"-~");
    }
    sixel_input_bytes.extend_from_slice(b"\x1b\\");

    let growth_result = catch_unwind(AssertUnwindSafe(|| {
        let mut parser = GraphicsParser::default();
        get_only_graphics_event(&mut parser, &sixel_input_bytes)
    }))
    .expect("Sixel growth must not panic");

    let graphics_event = growth_result.expect("the Sixel image remains within the final limits");
    assert_eq!(graphics_event.image.pixel_width, 1_000);
    assert_eq!(graphics_event.image.pixel_height, 204);
    assert_eq!(graphics_event.image.rgba_bytes.len(), 1_000 * 204 * 4);
    assert_eq!(&graphics_event.image.rgba_bytes[0..4], &[0, 0, 0, 255]);
}

#[test]
fn cancelling_a_chunked_transfer_clears_its_multipart_state() {
    let first_kitty_chunk = "\x1b_Gf=32,s=1,v=1,m=1;/wAA\x1b\\";
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(first_kitty_chunk.as_bytes())
        .is_empty());
    assert!(parser.decode_completed_graphics_events(b"\x18").is_empty());
    let graphics_event = get_only_graphics_event(&mut parser, build_kitty_raw_rgba().as_slice())
        .expect("a new transfer");

    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn kitty_multipart_animation_frame_emits_one_command_after_the_final_chunk() {
    let base64_image_bytes = STANDARD.encode([255, 0, 0, 255]);
    let first_animation_chunk = format!(
        "\x1b_Ga=f,i=7,f=32,s=1,v=1,r=1,m=1;{}\x1b\\",
        &base64_image_bytes[..4]
    );
    let final_animation_chunk = format!("\x1b_Ga=f,m=0;{}\x1b\\", &base64_image_bytes[4..]);
    let mut parser = GraphicsParser::default();

    let first_animation_events =
        parser.process_graphics_operations(first_animation_chunk.as_bytes());
    assert!(
        first_animation_events.is_empty(),
        "unexpected first animation events: {first_animation_events:?}"
    );
    let final_animation_events =
        parser.process_graphics_operations(final_animation_chunk.as_bytes());
    assert_eq!(final_animation_events.len(), 1);
    let kitty_command = match final_animation_events
        .into_iter()
        .next()
        .expect("one event")
    {
        Ok(GraphicsOperation::Command(kitty_command)) => kitty_command,
        unexpected_graphics_operation => {
            panic!("unexpected animation event: {unexpected_graphics_operation:?}")
        }
    };
    assert_eq!(
        kitty_command.get_command_kind(),
        KittyCommandKind::AnimationFrame
    );
    assert_eq!(
        kitty_command
            .get_animation_command()
            .expect("animation command")
            .encoded_payload_bytes,
        base64_image_bytes.as_bytes()
    );
}

#[test]
fn graphics_accepts_seven_bit_and_c1_string_openings_and_terminators() {
    let mut parser = GraphicsParser::default();
    let mut sixel_input_bytes = build_one_sixel();
    sixel_input_bytes[0] = 0x90;
    sixel_input_bytes.remove(1);
    let terminator_byte_index = sixel_input_bytes.len() - 2;
    sixel_input_bytes.truncate(terminator_byte_index);
    sixel_input_bytes.push(0x9c);
    assert_eq!(
        get_only_graphics_event(&mut parser, &sixel_input_bytes)
            .expect("C1 Sixel decodes")
            .protocol,
        GraphicsProtocol::Sixel
    );

    let mut parser = GraphicsParser::default();
    let mut kitty_input_bytes = build_kitty_raw_rgba();
    kitty_input_bytes[0] = 0x9f;
    kitty_input_bytes.remove(1);
    let terminator_byte_index = kitty_input_bytes.len() - 2;
    kitty_input_bytes.truncate(terminator_byte_index);
    kitty_input_bytes.push(0x9c);
    assert_eq!(
        get_only_graphics_event(&mut parser, &kitty_input_bytes)
            .expect("C1 kitty decodes")
            .protocol,
        GraphicsProtocol::Kitty
    );

    let mut parser = GraphicsParser::default();
    let mut iterm_input_bytes = build_iterm_file(&build_red_png_bytes());
    iterm_input_bytes[0] = 0x9d;
    iterm_input_bytes.remove(1);
    let terminator_byte_index = iterm_input_bytes.len() - 1;
    iterm_input_bytes[terminator_byte_index] = 0x9c;
    assert_eq!(
        get_only_graphics_event(&mut parser, &iterm_input_bytes)
            .expect("C1 iTerm2 decodes")
            .protocol,
        GraphicsProtocol::Iterm2
    );

    let mut parser = GraphicsParser::default();
    let mut tmux_wrapped_input = wrap_tmux(&build_kitty_raw_rgba());
    tmux_wrapped_input[0] = 0x90;
    tmux_wrapped_input.remove(1);
    assert_eq!(
        get_only_graphics_event(&mut parser, &tmux_wrapped_input)
            .expect("C1 tmux decodes")
            .protocol,
        GraphicsProtocol::Kitty
    );

    let mut parser = GraphicsParser::default();
    let mut screen_wrapped_input = wrap_screen(&build_kitty_raw_rgba());
    screen_wrapped_input[0] = 0x90;
    screen_wrapped_input.remove(1);
    assert_eq!(
        get_only_graphics_event(&mut parser, &screen_wrapped_input)
            .expect("C1 Screen decodes")
            .protocol,
        GraphicsProtocol::Kitty
    );
}

#[test]
fn kitty_decodes_one_red_rgba_pixel() {
    let mut parser = GraphicsParser::default();

    let graphics_event =
        get_only_graphics_event(&mut parser, &build_kitty_raw_rgba()).expect("kitty decodes");

    assert_eq!(graphics_event.protocol, GraphicsProtocol::Kitty);
    assert_eq!(graphics_event.action, ImageAction::Transmit);
    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 1);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn kitty_decodes_one_red_rgb_pixel() {
    let base64_rgb_bytes = STANDARD.encode([255, 0, 0]);
    let kitty_rgb_input = format!("\x1b_Gf=24,s=1,v=1;{base64_rgb_bytes}\x1b\\");
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, kitty_rgb_input.as_bytes())
        .expect("kitty RGB decodes");

    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn kitty_transmit_and_display_action_is_kept_with_the_image_record() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let kitty_transmit_display_input =
        format!("\x1b_Ga=T,f=32,s=1,v=1;{base64_rgba_bytes}\x1b\\").into_bytes();
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, &kitty_transmit_display_input)
        .expect("kitty transmit-and-display decodes");

    assert_eq!(graphics_event.action, ImageAction::TransmitAndDisplay);
}

#[test]
fn kitty_rejects_an_image_that_names_both_id_forms() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let conflicting_id_input = format!("\x1b_Gi=1,I=2,f=32,s=1,v=1;{base64_rgba_bytes}\x1b\\");
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(&mut parser, conflicting_id_input.as_bytes()),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_continuation_rejects_non_chunk_control_fields() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255]);
    let split_byte_index = base64_rgba_bytes.len() / 2;
    let first_kitty_chunk = format!(
        "\x1b_Ga=T,f=32,s=3,v=1,m=1;{}\x1b\\",
        &base64_rgba_bytes[..split_byte_index]
    );
    let final_kitty_chunk = format!(
        "\x1b_Gf=32,m=0;{}\x1b\\",
        &base64_rgba_bytes[split_byte_index..]
    );
    let mut parser = GraphicsParser::default();

    let first_kitty_events = parser.decode_completed_graphics_events(first_kitty_chunk.as_bytes());
    assert!(
        first_kitty_events.is_empty(),
        "unexpected first kitty events: {first_kitty_events:?}"
    );
    assert_eq!(
        get_only_graphics_event(&mut parser, final_kitty_chunk.as_bytes()),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_continuation_requires_the_more_flag() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255, 255, 0, 0, 255]);
    let first_kitty_chunk = format!("\x1b_Gf=32,s=2,v=1,m=1;{}\x1b\\", &base64_rgba_bytes[..4]);
    let final_kitty_chunk = format!("\x1b_G;{}\x1b\\", &base64_rgba_bytes[4..]);
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(first_kitty_chunk.as_bytes())
        .is_empty());
    assert_eq!(
        get_only_graphics_event(&mut parser, final_kitty_chunk.as_bytes()),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_nonfinal_chunk_requires_complete_base64_quartets() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(&mut parser, b"\x1b_Gf=100,m=1;AAA\x1b\\"),
        Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_rejects_a_non_zlib_compression_value() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(&mut parser, b"\x1b_Gf=24,s=1,v=1,o=0;AAAA\x1b\\"),
        Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            media_format: "0".to_string(),
        })
    );
}

#[test]
fn kitty_zlib_compresses_rgb_data_before_the_raw_decode() {
    use std::io::Write;

    let mut compressed_bytes = Vec::new();
    let mut zlib_encoder =
        flate2::write::ZlibEncoder::new(&mut compressed_bytes, flate2::Compression::default());
    zlib_encoder
        .write_all(&[255, 0, 0])
        .expect("the RGB pixel compresses");
    zlib_encoder.finish().expect("the zlib stream finishes");
    let base64_compressed_rgb_bytes = STANDARD.encode(compressed_bytes);
    let compressed_kitty_input =
        format!("\x1b_Gf=24,s=1,v=1,o=z;{base64_compressed_rgb_bytes}\x1b\\").into_bytes();
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, &compressed_kitty_input)
        .expect("compressed kitty decodes");

    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn kitty_compressed_png_requires_its_uncompressed_size() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(&mut parser, b"\x1b_Gf=100,o=z;AAAA\x1b\\"),
        Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn kitty_zlib_rejects_bytes_after_the_compressed_stream() {
    use std::io::Write;

    let mut compressed_bytes = Vec::new();
    let mut zlib_encoder =
        flate2::write::ZlibEncoder::new(&mut compressed_bytes, flate2::Compression::default());
    zlib_encoder
        .write_all(&[255, 0, 0])
        .expect("the RGB pixel compresses");
    zlib_encoder.finish().expect("the zlib stream finishes");
    compressed_bytes.extend_from_slice(b"trailing");
    let base64_compressed_rgb_bytes = STANDARD.encode(compressed_bytes);
    let compressed_kitty_input =
        format!("\x1b_Gf=24,s=1,v=1,o=z;{base64_compressed_rgb_bytes}\x1b\\").into_bytes();
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(&mut parser, &compressed_kitty_input),
        Err(GraphicsError::DecodeFailure {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn zlib_output_may_reach_the_rgba_image_byte_limit() {
    use std::io::Write;

    let oversized_rgba_bytes = vec![0; MAX_GRAPHICS_TRANSFER_BYTE_COUNT + 1];
    let mut compressed = Vec::new();
    let mut encoder = flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::fast());
    encoder
        .write_all(&oversized_rgba_bytes)
        .expect("the bounded source compresses");
    encoder.finish().expect("the zlib stream finishes");

    let decoded_rgba_bytes = decompress_bounded(GraphicsProtocol::Kitty, &compressed)
        .expect("the compressed source stays within the RGBA byte limit");

    assert_eq!(decoded_rgba_bytes.len(), oversized_rgba_bytes.len());
}

#[test]
fn iterm_file_decodes_png_and_keeps_display_hints() {
    let mut parser = GraphicsParser::default();
    let png_image_bytes = build_red_png_bytes();

    let graphics_event = get_only_graphics_event(&mut parser, &build_iterm_file(&png_image_bytes))
        .expect("the iTerm2 file decodes");

    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 1);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
    assert_eq!(
        graphics_event.display.requested_width,
        Some(ImageDimension::Pixels(1))
    );
    assert_eq!(
        graphics_event.display.requested_height,
        Some(ImageDimension::Pixels(1))
    );
    assert!(!graphics_event.display.is_aspect_ratio_preserved);
}

#[test]
fn animated_iterm_gif_keeps_all_frames_and_starts_on_the_first() {
    let mut parser = GraphicsParser::default();
    let animated_gif_bytes = build_animated_gif();

    let base64_gif_bytes = STANDARD.encode(animated_gif_bytes);
    let iterm_gif_command = format!("\x1b]1337;File=inline=1:{base64_gif_bytes}\x07");

    let graphics_event = get_only_graphics_event(&mut parser, iterm_gif_command.as_bytes())
        .expect("the animated GIF decodes");
    let animation = graphics_event
        .animation
        .as_ref()
        .expect("animation is retained");
    assert_eq!(animation.get_frame_count(), 2);
    assert_eq!(
        graphics_event.image,
        *animation.list_frames()[0].get_decoded_image()
    );
}

#[test]
fn animated_png_and_webp_keep_all_frames_and_start_on_the_first() {
    for animated_image_bytes in [build_animated_png(), build_animated_webp()] {
        let mut parser = GraphicsParser::default();
        let graphics_event =
            get_only_graphics_event(&mut parser, &build_iterm_file(&animated_image_bytes))
                .expect("animated media decodes");
        let animation = graphics_event
            .animation
            .as_ref()
            .expect("animation is retained");
        assert_eq!(animation.get_frame_count(), 2);
        assert_eq!(
            graphics_event.image,
            *animation.list_frames()[0].get_decoded_image()
        );
    }
}

#[test]
fn iterm_multipart_parts_join_in_order() {
    let mut parser = GraphicsParser::default();
    let png_image_bytes = build_red_png_bytes();

    let graphics_event =
        get_only_graphics_event(&mut parser, &build_iterm_multipart(&png_image_bytes))
            .expect("the multipart image ends");

    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn iterm_dimensions_keep_cell_pixel_percent_and_auto_units() {
    let png_image_bytes = build_red_png_bytes();
    let base64_image_bytes = STANDARD.encode(png_image_bytes);
    let first_iterm_command = format!(
        "\x1b]1337;File=inline=1;width=2;height=3px;preserveAspectRatio=1;name=red;foo=bar:{}\x07",
        base64_image_bytes
    );
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, first_iterm_command.as_bytes())
        .expect("the iTerm2 image decodes");

    assert_eq!(
        graphics_event.display.requested_width,
        Some(ImageDimension::Cells(2))
    );
    assert_eq!(
        graphics_event.display.requested_height,
        Some(ImageDimension::Pixels(3))
    );
    assert!(graphics_event.display.is_aspect_ratio_preserved);

    let second_base64_image_bytes = STANDARD.encode(build_red_png_bytes());
    let second_iterm_command = format!(
        "\x1b]1337;File=inline=1;width=10%;height=auto:{}\x07",
        second_base64_image_bytes
    );
    let mut parser = GraphicsParser::default();
    let graphics_event = get_only_graphics_event(&mut parser, second_iterm_command.as_bytes())
        .expect("the second iTerm2 image decodes");

    assert_eq!(
        graphics_event.display.requested_width,
        Some(ImageDimension::Percent(10))
    );
    assert_eq!(
        graphics_event.display.requested_height,
        Some(ImageDimension::Auto)
    );
}

#[test]
fn iterm_rejects_non_inline_and_accepts_mismatched_size_hints() {
    let base64_image_bytes = STANDARD.encode(build_red_png_bytes());
    let mut parser = GraphicsParser::default();
    let non_inline_command = format!("\x1b]1337;File=inline=0:{base64_image_bytes}\x07");
    assert_eq!(
        get_only_graphics_event(&mut parser, non_inline_command.as_bytes()),
        Err(GraphicsError::UnsupportedAction {
            protocol: GraphicsProtocol::Iterm2,
            action: "inline=0".to_string(),
        })
    );

    let mut parser = GraphicsParser::default();
    let mismatched_size_hint_input = format!(
        "\x1b]1337;File=inline=1;size=1;width=1px;height=1px;preserveAspectRatio=0:{base64_image_bytes}\x07"
    );
    let graphics_event =
        get_only_graphics_event(&mut parser, mismatched_size_hint_input.as_bytes())
            .expect("size is a progress hint");
    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 1);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
    assert_eq!(
        graphics_event.display.requested_width,
        Some(ImageDimension::Pixels(1))
    );
    assert_eq!(
        graphics_event.display.requested_height,
        Some(ImageDimension::Pixels(1))
    );
    assert!(!graphics_event.display.is_aspect_ratio_preserved);
}

#[test]
fn iterm_multipart_accepts_mismatched_size_hints() {
    let png_image_bytes = build_red_png_bytes();
    let base64_image_bytes = STANDARD.encode(&png_image_bytes);
    let multipart_command = format!(
        "\x1b]1337;MultipartFile=inline=1;size=1;width=1px;height=1px;preserveAspectRatio=0\x07\
         \x1b]1337;FilePart={base64_image_bytes}\x07\
         \x1b]1337;FileEnd\x07"
    );
    let mut parser = GraphicsParser::default();
    let graphics_event = get_only_graphics_event(&mut parser, multipart_command.as_bytes())
        .expect("size is a progress hint");
    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 1);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
    assert_eq!(
        graphics_event.display.requested_width,
        Some(ImageDimension::Pixels(1))
    );
    assert_eq!(
        graphics_event.display.requested_height,
        Some(ImageDimension::Pixels(1))
    );
    assert!(!graphics_event.display.is_aspect_ratio_preserved);
}

#[test]
fn tmux_and_screen_wrappers_expose_the_enclosed_kitty_image() {
    for wrapped_graphics_bytes in [
        wrap_tmux(&build_kitty_raw_rgba()),
        wrap_screen(&build_kitty_raw_rgba()),
    ] {
        let mut parser = GraphicsParser::default();

        let graphics_event = get_only_graphics_event(&mut parser, &wrapped_graphics_bytes)
            .expect("the wrapper exposes kitty");

        assert_eq!(graphics_event.protocol, GraphicsProtocol::Kitty);
        assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
    }
}

#[test]
fn screen_and_tmux_wrappers_keep_two_inner_kitty_images() {
    let mut inner_kitty_bytes = build_kitty_raw_rgba();
    inner_kitty_bytes.extend_from_slice(&build_kitty_raw_rgba());

    for wrapped_graphics_bytes in [
        wrap_tmux(&inner_kitty_bytes),
        wrap_screen(&inner_kitty_bytes),
    ] {
        let mut parser = GraphicsParser::default();
        let completed_graphics_events =
            parser.decode_completed_graphics_events(&wrapped_graphics_bytes);

        assert_eq!(completed_graphics_events.len(), 2);
        assert_eq!(
            completed_graphics_events[0]
                .as_ref()
                .expect("the first wrapped image decodes")
                .image
                .rgba_bytes,
            [255, 0, 0, 255]
        );
        assert_eq!(
            completed_graphics_events[1]
                .as_ref()
                .expect("the second wrapped image decodes")
                .image
                .rgba_bytes,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn sos_and_pm_hold_c1_apc_bytes_until_the_string_terminator() {
    for control_string_prefix in [[0x1b, b'X'], [0x1b, b'^'], [0x98, 0], [0x9e, 0]] {
        let mut control_string_input_bytes =
            control_string_prefix[..if control_string_prefix[1] == 0 { 1 } else { 2 }].to_vec();
        control_string_input_bytes.extend_from_slice(&build_kitty_c1_raw_rgba());
        control_string_input_bytes.push(0x9c);
        let mut parser = GraphicsParser::default();

        assert!(parser
            .decode_completed_graphics_events(&control_string_input_bytes)
            .is_empty());
        assert_eq!(
            get_only_graphics_event(&mut parser, &build_kitty_c1_raw_rgba())
                .expect("the next APC decodes after the silent string")
                .image
                .rgba_bytes,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn screen_wrapper_closes_after_a_bel_terminated_iterm_image() {
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(
            &mut parser,
            &wrap_screen(&build_iterm_file(&build_red_png_bytes()))
        )
        .expect("the Screen-wrapped iTerm2 image decodes")
        .image
        .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn screen_wrapper_preserves_inner_string_terminators() {
    let mut sixel_input_bytes = build_one_sixel();
    sixel_input_bytes.truncate(sixel_input_bytes.len() - 2);
    sixel_input_bytes.push(0x9c);
    let mut kitty_input_bytes = build_kitty_raw_rgba();
    kitty_input_bytes.truncate(kitty_input_bytes.len() - 2);
    kitty_input_bytes.push(0x9c);
    let mut iterm_input_bytes = build_iterm_file(&build_red_png_bytes());
    iterm_input_bytes.pop();
    iterm_input_bytes.push(0x9c);

    for (graphics_protocol_name, graphics_payload) in [
        ("Sixel", sixel_input_bytes),
        ("kitty", kitty_input_bytes),
        ("iTerm2", iterm_input_bytes),
    ] {
        let mut parser = GraphicsParser::default();
        let completed_graphics_events =
            parser.decode_completed_graphics_events(&wrap_screen(&graphics_payload));
        assert_eq!(
            completed_graphics_events.len(),
            1,
            "the {graphics_protocol_name} image has one event"
        );
        let graphics_event = completed_graphics_events
            .into_iter()
            .next()
            .expect("the Screen-wrapped image event")
            .expect("the Screen-wrapped image decodes");
        assert_eq!(graphics_event.image.pixel_width, 1);
        assert_eq!(
            graphics_event.image.pixel_height,
            if graphics_protocol_name == "Sixel" {
                6
            } else {
                1
            }
        );
    }
}

#[test]
fn screen_wrappers_keep_an_inner_st_after_a_split() {
    let inner_kitty_bytes = build_kitty_raw_rgba();
    let split_byte_index = inner_kitty_bytes.len() / 2;
    let mut split_screen_bytes = wrap_screen(&inner_kitty_bytes[..split_byte_index]);
    split_screen_bytes.extend_from_slice(&wrap_screen(&inner_kitty_bytes[split_byte_index..]));
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, &split_screen_bytes)
        .expect("the split Screen image decodes");

    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn passthrough_wrappers_preserve_inner_c1_terminators() {
    let mut inner_kitty_bytes = build_kitty_raw_rgba();
    inner_kitty_bytes.truncate(inner_kitty_bytes.len() - 2);
    inner_kitty_bytes.push(0x9c);
    let mut screen_wrapped_bytes = b"\x1bP".to_vec();
    screen_wrapped_bytes.extend_from_slice(&inner_kitty_bytes);
    screen_wrapped_bytes.extend_from_slice(b"\x1b\\");

    for (wrapper_name, wrapped_graphics_payload) in [
        ("Screen", screen_wrapped_bytes),
        ("tmux", wrap_tmux(&inner_kitty_bytes)),
    ] {
        let mut parser = GraphicsParser::default();
        let graphics_event = get_only_graphics_event(&mut parser, &wrapped_graphics_payload)
            .unwrap_or_else(|graphics_error| {
                panic!("the {wrapper_name}-wrapped image failed: {graphics_error:?}")
            });
        assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
    }
}

#[test]
fn screen_wrappers_join_an_iterm_transfer_split_between_dcs_strings() {
    let inner_iterm_bytes = build_iterm_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let mut split_screen_bytes = wrap_screen(&inner_iterm_bytes[..split_byte_index]);
    split_screen_bytes.extend_from_slice(&wrap_screen(&inner_iterm_bytes[split_byte_index..]));
    let mut parser = GraphicsParser::default();

    let completed_graphics_events = parser.decode_completed_graphics_events(&split_screen_bytes);
    assert_eq!(completed_graphics_events.len(), 1);
    let graphics_event = completed_graphics_events
        .into_iter()
        .next()
        .expect("one Screen event")
        .expect("the split Screen transfer decodes");

    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn tmux_wrappers_join_an_iterm_transfer_split_between_dcs_strings() {
    let inner_iterm_bytes = build_iterm_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let mut split_tmux_bytes = wrap_tmux(&inner_iterm_bytes[..split_byte_index]);
    split_tmux_bytes.extend_from_slice(&wrap_tmux(&inner_iterm_bytes[split_byte_index..]));
    let mut parser = GraphicsParser::default();

    let completed_graphics_events = parser.decode_completed_graphics_events(&split_tmux_bytes);
    assert_eq!(completed_graphics_events.len(), 1);
    let graphics_event = completed_graphics_events
        .into_iter()
        .next()
        .expect("one tmux event")
        .expect("the split tmux transfer decodes");

    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn cancellation_aborts_each_graphics_escape_state() {
    let graphics_escape_prefixes: &[&[u8]] = &[
        b"\x1b\x18",
        b"\x1bP\x18",
        b"\x1bPq?\x1b\x18",
        b"\x1b_Gf=32,s=1,v=1;AAAA\x1b\x18",
        b"\x1b]1337;File=inline=1:AAAA\x1b\x18",
        b"\x1bPtmux;A\x1b\x18",
        b"\x1bP\x1bA\x1b\x18",
    ];

    for graphics_escape_prefix in graphics_escape_prefixes {
        let mut parser = GraphicsParser::default();
        assert!(parser
            .decode_completed_graphics_events(graphics_escape_prefix)
            .is_empty());
        let graphics_event = get_only_graphics_event(&mut parser, &build_kitty_raw_rgba())
            .expect("the new image decodes");
        assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
    }
}

#[test]
fn cancelling_a_split_screen_transfer_clears_nested_state() {
    let inner_iterm_bytes = build_iterm_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(&wrap_screen(&inner_iterm_bytes[..split_byte_index]))
        .is_empty());
    assert!(parser.is_screen_continuation());
    assert!(parser.decode_completed_graphics_events(b"\x18").is_empty());
    assert!(!parser.is_screen_continuation());
    assert!(parser.screen_inner_parser.is_none());

    let graphics_event =
        get_only_graphics_event(&mut parser, &build_iterm_file(&build_red_png_bytes()))
            .expect("a new Screen transfer");
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn cancelling_a_split_tmux_transfer_clears_nested_state() {
    let inner_iterm_bytes = build_iterm_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(&wrap_tmux(&inner_iterm_bytes[..split_byte_index]))
        .is_empty());
    assert!(parser.is_tmux_continuation());
    assert!(parser.decode_completed_graphics_events(b"\x1a").is_empty());
    assert!(!parser.is_tmux_continuation());
    assert!(parser.tmux_inner_parser.is_none());

    let graphics_event =
        get_only_graphics_event(&mut parser, &build_iterm_file(&build_red_png_bytes()))
            .expect("a new tmux transfer");
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn a_screen_body_at_the_limit_can_close_and_next_image_still_decodes() {
    let screen_body_bytes = vec![b'A'; MAX_SCREEN_PASSTHROUGH_BYTE_COUNT];
    let mut screen_input_bytes = wrap_screen(&screen_body_bytes);
    screen_input_bytes.extend_from_slice(&build_kitty_raw_rgba());
    let mut parser = GraphicsParser::default();

    let completed_graphics_events = parser.decode_completed_graphics_events(&screen_input_bytes);
    assert_eq!(completed_graphics_events.len(), 1);
    let graphics_event = completed_graphics_events
        .into_iter()
        .next()
        .expect("one image event")
        .expect("the image decodes");
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn screen_passthrough_has_a_bounded_body() {
    let mut parser = GraphicsParser::default();
    let screen_body_bytes = vec![b'A'; MAX_SCREEN_PASSTHROUGH_BYTE_COUNT];
    let mut inner_screen_bytes = vec![0x1b, b'_'];
    inner_screen_bytes.extend_from_slice(&screen_body_bytes);
    let screen_input_bytes = wrap_screen(&inner_screen_bytes);

    assert_eq!(
        get_only_graphics_event(&mut parser, &screen_input_bytes),
        Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn every_byte_boundary_preserves_each_protocol_result() {
    for graphics_input_bytes in [
        build_one_sixel(),
        build_kitty_raw_rgba(),
        build_iterm_file(&build_red_png_bytes()),
        wrap_tmux(&build_kitty_raw_rgba()),
        wrap_screen(&build_kitty_raw_rgba()),
    ] {
        let mut whole_graphics_parser = GraphicsParser::default();
        let expected_graphics_event =
            get_only_graphics_event(&mut whole_graphics_parser, &graphics_input_bytes)
                .expect("the whole transfer decodes");

        let mut split_graphics_parser = GraphicsParser::default();
        let mut split_completed_graphics_events = Vec::new();
        for graphics_input_byte in graphics_input_bytes {
            split_completed_graphics_events.extend(
                split_graphics_parser.decode_completed_graphics_events(&[graphics_input_byte]),
            );
        }

        assert_eq!(split_completed_graphics_events.len(), 1);
        assert_eq!(
            split_completed_graphics_events
                .pop()
                .expect("one split event")
                .expect("split decodes"),
            expected_graphics_event
        );
    }
}

#[test]
fn terminal_payload_compaction_preserves_every_engine_byte_boundary() {
    let kitty_input_bytes = build_kitty_display_cell_rgba(false);
    for graphics_input_bytes in [
        kitty_input_bytes.clone(),
        build_iterm_cell_file(&build_red_png_bytes()),
        build_one_sixel(),
        wrap_tmux(&kitty_input_bytes),
        wrap_screen(&kitty_input_bytes),
    ] {
        let mut terminal_input_bytes = b"A".to_vec();
        terminal_input_bytes.extend_from_slice(&graphics_input_bytes);
        terminal_input_bytes.push(b'B');

        let mut whole_terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 2,
        });
        let whole_engine_replies = whole_terminal_engine.process_pty_output(&terminal_input_bytes);
        let whole_terminal_state = whole_terminal_engine.get_terminal_state().clone();
        let whole_completed_graphics_events = whole_terminal_engine.take_graphics_events();

        let mut split_terminal_engine = TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 2,
        });
        let mut split_engine_replies = Vec::new();
        for terminal_input_byte in terminal_input_bytes {
            split_engine_replies
                .extend(split_terminal_engine.process_pty_output(&[terminal_input_byte]));
        }

        assert_eq!(split_engine_replies, whole_engine_replies);
        assert_eq!(
            split_terminal_engine.get_terminal_state(),
            &whole_terminal_state
        );
        assert_eq!(
            split_terminal_engine.take_graphics_events(),
            whole_completed_graphics_events
        );
    }
}

#[test]
fn malformed_base64_returns_a_typed_error_and_consumes_the_string() {
    let mut parser = GraphicsParser::default();
    let malformed_kitty_input = b"\x1b_Gf=32,s=1,v=1;not-base64\x1b\\Z";

    let graphics_event_result = get_only_graphics_event(&mut parser, malformed_kitty_input);

    assert_eq!(
        graphics_event_result,
        Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Kitty,
        })
    );
    assert!(parser.decode_completed_graphics_events(b"Z").is_empty());
}

#[test]
fn ordinary_apc_and_iterm_commands_do_not_create_completed_graphics_events() {
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(
            b"\x1b_ordinary application command\x1b\\\x1b]1337;SetMark=mark\x07"
        )
        .is_empty());

    let graphics_event = get_only_graphics_event(&mut parser, &build_kitty_raw_rgba())
        .expect("the next kitty image decodes");
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn chunked_clipboard_data_is_consumed_as_complete_runs() {
    const CLIPBOARD_PAYLOAD_BYTE_COUNT: usize = 8 * 1024 * 1024;
    const CLIPBOARD_CHUNK_BYTE_COUNT: usize = 8192;

    let mut parser = GraphicsParser::default();
    let clipboard_opening_scan = parser.process_graphics_operations_with_offsets(b"\x1b]52;c;");
    assert_eq!(clipboard_opening_scan.completed_graphics_events, []);
    assert_eq!(clipboard_opening_scan.terminal_inert_ranges, []);

    let clipboard_payload_bytes = vec![b'A'; CLIPBOARD_PAYLOAD_BYTE_COUNT];
    for payload_chunk_bytes in clipboard_payload_bytes.chunks(CLIPBOARD_CHUNK_BYTE_COUNT) {
        assert_eq!(
            parser.feed_discard_bytes(payload_chunk_bytes),
            Some(payload_chunk_bytes.len())
        );
    }
    assert_eq!(
        parser.graphics_sequence_byte_count,
        b"\x1b]52;c;".len() + CLIPBOARD_PAYLOAD_BYTE_COUNT
    );
    assert_eq!(parser.get_graphics_carry_bytes(), Some([].as_slice()));
    assert_eq!(parser.feed_discard_bytes(b"\x07"), None);

    let terminator_scan = parser.process_graphics_operations_with_offsets(b"\x07");
    assert_eq!(terminator_scan.completed_graphics_events, []);
    assert_eq!(terminator_scan.terminal_inert_ranges, []);
    assert_eq!(parser.get_graphics_carry_bytes(), None);
}

#[test]
fn ordinary_control_string_data_is_consumed_as_complete_runs() {
    const CONTROL_STRING_PAYLOAD_BYTE_COUNT: usize = 8192;

    for (control_string_opening, control_string_terminator) in [
        (b"\x1b_not-image".as_slice(), b"\x1b\\".as_slice()),
        (b"\x1bPX".as_slice(), b"\x1b\\".as_slice()),
        (b"\x1b]1337;SetMark=".as_slice(), b"\x07".as_slice()),
    ] {
        let mut parser = GraphicsParser::default();
        let opening_scan = parser.process_graphics_operations_with_offsets(control_string_opening);
        assert_eq!(opening_scan.completed_graphics_events, []);
        assert_eq!(opening_scan.terminal_inert_ranges, []);

        if control_string_terminator != b"\x07" {
            assert_eq!(parser.feed_discard_bytes(b"A\x07B"), Some(3));
        }
        assert_eq!(
            parser.feed_discard_bytes(&[b'A'; CONTROL_STRING_PAYLOAD_BYTE_COUNT]),
            Some(CONTROL_STRING_PAYLOAD_BYTE_COUNT)
        );
        assert_eq!(parser.feed_discard_bytes(control_string_terminator), None);

        let terminator_scan =
            parser.process_graphics_operations_with_offsets(control_string_terminator);
        assert_eq!(terminator_scan.completed_graphics_events, []);
        assert_eq!(terminator_scan.terminal_inert_ranges, []);
        assert_eq!(parser.get_graphics_carry_bytes(), None);
    }
}

#[test]
fn discarded_string_bulk_scan_stays_silent_past_the_graphics_limit() {
    let mut parser = GraphicsParser::default();
    assert!(parser
        .decode_completed_graphics_events(b"\x1b]52;c;")
        .is_empty());
    parser.graphics_sequence_byte_count = MAX_GRAPHICS_TRANSFER_BYTE_COUNT - 1;
    parser.pending_graphics_bytes.clear();
    parser.is_carryable = false;

    assert_eq!(parser.feed_discard_bytes(b"AB"), Some(2));
    let graphics_scan = parser.process_graphics_operations_with_offsets(b"\x07");

    assert_eq!(graphics_scan.terminal_inert_ranges, []);
    assert_eq!(graphics_scan.completed_graphics_events, []);
    assert_eq!(parser.get_graphics_carry_bytes(), None);
}

#[test]
fn unterminated_non_graphics_strings_are_silent_at_finish() {
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(b"\x1b_ordinary application command")
        .is_empty());
    assert!(parser.finish_graphics_stream().is_empty());

    assert!(parser
        .decode_completed_graphics_events(b"\x1b]0")
        .is_empty());
    assert!(parser.finish_graphics_stream().is_empty());

    assert!(parser
        .decode_completed_graphics_events(b"\x1b]1337;SetMark=mark")
        .is_empty());
    assert!(parser.finish_graphics_stream().is_empty());

    assert!(parser
        .decode_completed_graphics_events(b"\x1bPtx")
        .is_empty());
    assert!(parser.finish_graphics_stream().is_empty());
}

#[test]
fn finishing_a_silent_string_leaves_the_next_image_readable() {
    for silent_string_input in [b"\x1b_ordinary".as_slice(), b"\x1bPtx", b"\x1b]0"] {
        let mut parser = GraphicsParser::default();
        assert!(parser
            .decode_completed_graphics_events(silent_string_input)
            .is_empty());
        assert!(parser.finish_graphics_stream().is_empty());
        assert_eq!(
            get_only_graphics_event(&mut parser, &build_kitty_raw_rgba())
                .expect("the image after the silent string decodes")
                .image
                .rgba_bytes,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn finishing_an_incomplete_utf8_character_discards_its_carry() {
    let mut parser = GraphicsParser::default();

    assert!(parser.decode_completed_graphics_events(b"\xe2").is_empty());
    assert_eq!(parser.get_graphics_carry_bytes(), Some(&b"\xe2"[..]));
    assert!(parser.finish_graphics_stream().is_empty());
    assert_eq!(parser.get_graphics_carry_bytes(), None);
    assert!(parser.get_graphics_transport_state().is_none());

    assert_eq!(
        get_only_graphics_event(&mut parser, &build_kitty_raw_rgba())
            .expect("the image after the incomplete character decodes")
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn empty_passthrough_wrappers_are_silent_at_finish() {
    for passthrough_prefix in [b"\x1bPtmux;".as_slice(), b"\x1bP\x1b"] {
        let mut parser = GraphicsParser::default();
        assert!(parser
            .decode_completed_graphics_events(passthrough_prefix)
            .is_empty());
        assert!(parser.finish_graphics_stream().is_empty());
    }
}

#[test]
fn ordinary_dcs_strings_do_not_create_completed_graphics_events_or_capture_inner_bytes() {
    let mut dcs_input_bytes = b"\x1bPtx".to_vec();
    dcs_input_bytes.extend_from_slice(&build_kitty_raw_rgba());
    dcs_input_bytes.extend_from_slice(b"\x1b\\");
    dcs_input_bytes.extend_from_slice(&build_kitty_raw_rgba());
    let mut parser = GraphicsParser::default();

    let completed_graphics_events = parser.decode_completed_graphics_events(&dcs_input_bytes);

    assert_eq!(completed_graphics_events.len(), 1);
    assert_eq!(
        completed_graphics_events
            .into_iter()
            .next()
            .expect("the event after the ordinary DCS")
            .expect("the kitty image decodes")
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );

    let mut sixel_like_input_bytes = b"\x1bP1;2;3;4X".to_vec();
    sixel_like_input_bytes.extend_from_slice(&build_kitty_raw_rgba());
    assert!(parser
        .decode_completed_graphics_events(&sixel_like_input_bytes)
        .is_empty());
}

#[test]
fn oversized_ignored_strings_remain_silent_after_an_engine_swap() {
    let ignored_string_prefixes = [
        (GraphicsProtocol::Sixel, b"\x1bPtx".as_slice()),
        (GraphicsProtocol::Kitty, b"\x1b_ordinary"),
        (GraphicsProtocol::Iterm2, b"\x1b]0"),
    ];

    for (graphics_protocol, ignored_string_prefix) in ignored_string_prefixes {
        let mut ignored_string_input_bytes = ignored_string_prefix.to_vec();
        ignored_string_input_bytes
            .extend(std::iter::repeat_n(b'A', MAX_GRAPHICS_CARRY_BYTE_COUNT + 1));
        let mut parser = GraphicsParser::default();
        assert!(parser
            .decode_completed_graphics_events(&ignored_string_input_bytes)
            .is_empty());
        let graphics_transport_state = parser
            .get_graphics_transport_state()
            .expect("the ignored string has transport state");
        assert_eq!(
            graphics_transport_state.graphics_abandonment,
            Some(GraphicsAbandonment::SilentSequence(graphics_protocol))
        );

        let mut resumed_graphics_parser = GraphicsParser::default();
        resumed_graphics_parser.restore_graphics_carry_state(&[], graphics_transport_state);
        assert!(resumed_graphics_parser
            .decode_completed_graphics_events(b"\x1b\\")
            .is_empty());
        assert_eq!(
            get_only_graphics_event(&mut resumed_graphics_parser, &build_kitty_raw_rgba())
                .expect("the image after the ignored string decodes")
                .image
                .rgba_bytes,
            [255, 0, 0, 255]
        );
    }
}

#[test]
fn c1_dcs_terminators_end_empty_and_wrapped_strings() {
    let mut parser = GraphicsParser::default();
    let mut empty_dcs_input = vec![0x90, 0x9c];
    empty_dcs_input.extend_from_slice(&build_kitty_raw_rgba());
    assert_eq!(
        get_only_graphics_event(&mut parser, &empty_dcs_input)
            .expect("the kitty image decodes")
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );

    let mut screen_wrapped_input = vec![0x90];
    screen_wrapped_input.extend_from_slice(&build_kitty_raw_rgba());
    screen_wrapped_input.push(0x9c);
    let mut parser = GraphicsParser::default();
    assert_eq!(
        get_only_graphics_event(&mut parser, &screen_wrapped_input)
            .expect("the Screen-wrapped kitty image decodes")
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );

    let mut tmux_wrapped_input = wrap_tmux(&build_kitty_raw_rgba());
    tmux_wrapped_input[0] = 0x90;
    tmux_wrapped_input.remove(1);
    tmux_wrapped_input.truncate(tmux_wrapped_input.len() - 2);
    tmux_wrapped_input.push(0x9c);
    let mut parser = GraphicsParser::default();
    assert_eq!(
        get_only_graphics_event(&mut parser, &tmux_wrapped_input)
            .expect("the tmux-wrapped kitty image decodes")
            .image
            .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn an_unterminated_transfer_is_reported_as_truncated() {
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(b"\x1b_Gf=32,s=1,v=1;")
        .is_empty());
    assert_eq!(
        parser.finish_graphics_stream(),
        [Err(GraphicsError::Truncated {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
}

#[test]
fn a_zero_raw_dimension_is_rejected_before_payload_decode() {
    let mut parser = GraphicsParser::default();
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let zero_width_kitty_input =
        format!("\x1b_Gf=32,s=0,v=1;{base64_rgba_bytes}\x1b\\").into_bytes();

    assert_eq!(
        get_only_graphics_event(&mut parser, &zero_width_kitty_input),
        Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn a_sixel_repeat_without_a_count_draws_one_sixel() {
    let mut parser = GraphicsParser::default();

    let graphics_event =
        get_only_graphics_event(&mut parser, b"\x1bPq!x@\x1b\\").expect("the Sixel decodes");
    assert_eq!(graphics_event.image.pixel_width, 2);
    assert_eq!(graphics_event.image.pixel_height, 12);
}

#[test]
fn an_oversized_sixel_header_returns_a_typed_error() {
    let mut oversized_sixel_input = b"\x1bP".to_vec();
    oversized_sixel_input.extend(std::iter::repeat_n(
        b'1',
        MAX_GRAPHICS_CONTROL_BYTE_COUNT + 1,
    ));
    oversized_sixel_input.extend_from_slice(b"q\x1b\\");
    let mut parser = GraphicsParser::default();

    assert_eq!(
        get_only_graphics_event(&mut parser, &oversized_sixel_input),
        Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn discarded_sequence_bytes_do_not_wrap() {
    let mut parser = GraphicsParser {
        graphics_state: GraphicsState::Discard(DiscardParser {
            discarded_string_kind: StringKind::Apc,
            graphics_error: GraphicsError::TransferTooLarge {
                protocol: GraphicsProtocol::Kitty,
            },
            is_escaped: false,
            should_report: false,
        }),
        graphics_sequence_byte_count: usize::MAX,
        ..GraphicsParser::default()
    };
    let mut completed_graphics_events = Vec::new();

    parser.feed_graphics_byte(b'A', &mut completed_graphics_events);

    assert_eq!(parser.graphics_sequence_byte_count, usize::MAX);
    assert_eq!(completed_graphics_events, Vec::new());
}

#[test]
fn unsupported_kitty_media_returns_a_typed_error() {
    let mut parser = GraphicsParser::default();
    let unsupported_kitty_input = b"\x1b_Gf=101,s=1,v=1;AAAA\x1b\\";

    assert_eq!(
        get_only_graphics_event(&mut parser, unsupported_kitty_input),
        Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            media_format: "101".to_string(),
        })
    );
}

#[test]
fn unsupported_kitty_controls_return_typed_action_errors() {
    for (kitty_control_field, unsupported_action_description) in
        [("d=1", "control d"), ("t=x", "transfer medium x")]
    {
        let kitty_control_input = format!("\x1b_G{kitty_control_field};AAAA\x1b\\").into_bytes();
        let mut parser = GraphicsParser::default();

        assert_eq!(
            get_only_graphics_event(&mut parser, &kitty_control_input),
            Err(GraphicsError::UnsupportedAction {
                protocol: GraphicsProtocol::Kitty,
                action: unsupported_action_description.to_string(),
            })
        );
    }
}

#[test]
fn kitty_relative_controls_require_image_dimensions() {
    for kitty_control_key in [b'H', b'P', b'Q', b'V'] {
        let kitty_control_input =
            format!("\x1b_G{}=1;AAAA\x1b\\", kitty_control_key as char).into_bytes();
        let mut parser = GraphicsParser::default();

        assert_eq!(
            get_only_graphics_event(&mut parser, &kitty_control_input),
            Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            })
        );
    }
}

#[test]
fn a_sixel_pixel_limit_is_checked_before_allocation() {
    let mut parser = GraphicsParser::default();
    let oversized_sixel_input = b"\x1bPq\"1;1;4097;4097#1@\x1b\\";

    assert_eq!(
        get_only_graphics_event(&mut parser, oversized_sixel_input),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })
    );
}

#[test]
fn a_kitty_pixel_limit_is_checked_before_payload_decode() {
    let mut parser = GraphicsParser::default();
    let oversized_kitty_input = b"\x1b_Gf=32,s=4097,v=4097;AAAA\x1b\\";

    assert_eq!(
        get_only_graphics_event(&mut parser, oversized_kitty_input),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Kitty,
        })
    );
}

#[test]
fn a_hostile_iterm_size_is_only_a_hint() {
    let mut parser = GraphicsParser::default();
    let hostile_iterm_input = b"\x1b]1337;File=inline=1;size=4294967295:AAAA\x07";

    assert_eq!(
        get_only_graphics_event(&mut parser, hostile_iterm_input),
        Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Iterm2,
            media_format: "unknown".to_string(),
        })
    );
}

#[test]
fn a_raster_dimension_limit_returns_image_too_large_before_decode() {
    let oversized_png_bytes = png_with_dimensions(4097, 4097);

    assert_eq!(
        decode_raster(GraphicsProtocol::Iterm2, &oversized_png_bytes),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Iterm2,
        })
    );
}

#[test]
fn rejected_graphics_do_not_change_the_terminal_state() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    let terminal_state_before_rejected_graphics = engine.get_terminal_state().clone();

    let _ = engine.process_pty_output(b"\x1b_Ga=p,f=32,s=1,v=1;AAAA\x1b\\");

    assert_eq!(
        engine.get_terminal_state(),
        &terminal_state_before_rejected_graphics
    );
    assert_eq!(
        engine.take_graphics_events(),
        [Err(GraphicsError::InvalidCommand {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
}

#[test]
fn an_image_without_cell_dimensions_is_rejected_without_state_mutation() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    let terminal_state_before_rejected_graphics = engine.get_terminal_state().clone();

    let _ = engine.process_pty_output(&build_one_sixel());

    assert_eq!(
        engine.get_terminal_state(),
        &terminal_state_before_rejected_graphics
    );
    assert_eq!(
        engine.take_graphics_events(),
        [Err(GraphicsError::PlacementRejected {
            protocol: GraphicsProtocol::Sixel,
            placement_error: ImagePlacementError::MissingCellDimensions {
                requested_width: None,
                requested_height: None,
            },
        })]
    );
}

#[test]
fn engine_places_a_cell_sized_image_at_the_cursor_anchor() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    let _ = engine.process_pty_output(b"\x1b[2;3H");

    let _ = engine.process_pty_output(&build_kitty_display_cell_rgba(false));

    let image_placements = engine.get_terminal_state().list_image_placements();
    assert_eq!(image_placements.len(), 1);
    assert_eq!(image_placements[0].get_image_anchor(), (1, 2));
    assert_eq!(image_placements[0].get_image_cell_dimensions(), (1, 1));
    assert_eq!(
        image_placements[0].list_covered_cells().collect::<Vec<_>>(),
        [(1, 2)]
    );
    let completed_graphics_events = engine.take_graphics_events();
    assert_eq!(completed_graphics_events.len(), 1);
    let graphics_event_record = completed_graphics_events
        .into_iter()
        .next()
        .expect("the image event")
        .expect("the image decodes");
    assert_eq!(graphics_event_record.anchor, (1, 2));
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn engine_moves_cursor_after_an_accepted_image_placement() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 10,
        row_count: 8,
    });
    let _ = engine.process_pty_output(b"\x1b[3;4H");

    let _ = engine.process_pty_output(&build_kitty_display_cell_rgba_size(3, 2, true));

    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (4, 6)
    );
    let image_placement = &engine.get_terminal_state().list_image_placements()[0];
    assert_eq!(image_placement.get_image_anchor(), (2, 3));
    assert_eq!(image_placement.get_image_cell_dimensions(), (2, 3));
}

#[test]
fn engine_retransmitting_a_kitty_image_removes_old_placements() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 4,
    });
    let _ = engine.process_pty_output(&build_kitty_display_cell_rgba_identity(7, 3, false));
    let _ = engine.process_pty_output(b"\x1b[2;2H");
    let _ = engine.process_pty_output(&build_kitty_display_cell_rgba_identity(7, 4, false));
    let _ = engine.process_pty_output(b"\x1b[3;3H");
    let _ = engine.process_pty_output(&build_kitty_display_cell_rgba_identity(7, 3, false));

    let image_placements = engine.get_terminal_state().list_image_placements();
    assert_eq!(image_placements.len(), 1);
    assert_eq!(
        image_placements[0].get_image_record().display.image_id,
        Some(7)
    );
    assert_eq!(
        image_placements[0].get_image_record().display.placement_id,
        Some(3)
    );
    assert_eq!(image_placements[0].get_image_anchor(), (2, 2));
}

#[test]
fn engine_records_the_anchor_before_a_subsequent_cursor_move_in_one_chunk() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    let mut terminal_input_bytes = build_kitty_display_cell_rgba(false);
    terminal_input_bytes.extend_from_slice(b"\x1b[2;3H");

    let _ = engine.process_pty_output(&terminal_input_bytes);

    let graphics_event_record = engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the image event")
        .expect("the image decodes");
    assert_eq!(graphics_event_record.anchor, (0, 0));
    assert_eq!(
        engine.get_terminal_state().get_active_cursor_position(),
        (1, 2)
    );
}

#[test]
fn graphics_queue_reports_dropped_events_at_its_bound() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    let mut terminal_input_bytes = Vec::new();
    for _ in 0..65 {
        terminal_input_bytes.extend_from_slice(&build_kitty_display_cell_rgba(false));
    }

    let _ = engine.process_pty_output(&terminal_input_bytes);

    assert_eq!(
        engine.get_terminal_state().list_image_placements().len(),
        65
    );

    let expected_graphics_event = Ok(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255],
        })
        .into(),
        animation: None,
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            requested_column_count: Some(1),
            requested_row_count: Some(1),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    let mut expected_completed_graphics_events = vec![expected_graphics_event; 64];
    expected_completed_graphics_events.push(Err(GraphicsError::QueueFull {
        dropped_event_count: 1,
    }));

    assert_eq!(
        engine.take_graphics_events(),
        expected_completed_graphics_events
    );
}

#[test]
fn restarting_preserves_the_graphics_queue_overflow_report() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    let mut sixel_input_bytes = Vec::new();
    for _ in 0..66 {
        sixel_input_bytes.extend_from_slice(&build_one_sixel());
    }
    let _ = engine.process_pty_output(&sixel_input_bytes);
    let completed_graphics_events = engine.take_graphics_events();
    let terminal_state_before_restart = engine.get_terminal_state().clone();

    let mut resumed_terminal_engine = TerminalEngine::from_terminal_state_with_graphics_and_events(
        terminal_state_before_restart,
        b"",
        b"",
        &completed_graphics_events,
    );

    assert_eq!(
        completed_graphics_events.last(),
        Some(&Err(GraphicsError::QueueFull {
            dropped_event_count: 2,
        }))
    );
    assert_eq!(
        resumed_terminal_engine.take_graphics_events(),
        completed_graphics_events
    );
}

#[test]
fn graphics_queue_error_names_both_limits() {
    assert_eq!(
        GraphicsError::QueueFull {
            dropped_event_count: 2,
        }
        .to_string(),
        "2 graphics events were dropped because the graphics event count or image-byte limit was reached"
    );
}

#[test]
fn a_kitty_chunked_transfer_decodes_only_after_the_final_chunk() {
    let mut parser = GraphicsParser::default();
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let split_byte_index = base64_rgba_bytes.len() / 2;
    let first_kitty_chunk = format!(
        "\x1b_Gf=32,s=1,v=1,m=1;{}\x1b\\",
        &base64_rgba_bytes[..split_byte_index]
    );
    let final_kitty_chunk = format!("\x1b_Gm=0;{}\x1b\\", &base64_rgba_bytes[split_byte_index..]);

    assert!(parser
        .decode_completed_graphics_events(first_kitty_chunk.as_bytes())
        .is_empty());
    let graphics_event = get_only_graphics_event(&mut parser, final_kitty_chunk.as_bytes())
        .expect("the final chunk decodes");
    assert_eq!(graphics_event.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn kitty_scan_marks_only_the_base64_payload_as_terminal_inert() {
    let kitty_input_bytes = build_kitty_raw_rgba();
    let payload_start_byte_index = kitty_input_bytes
        .iter()
        .position(|kitty_input_byte| *kitty_input_byte == b';')
        .expect("the Kitty header has a separator")
        + 1;
    let payload_end_byte_index = kitty_input_bytes.len() - 2;
    let mut parser = GraphicsParser::default();

    let graphics_scan = parser.process_graphics_operations_with_offsets(&kitty_input_bytes);

    let expected_payload_range = payload_start_byte_index..payload_end_byte_index;
    assert_eq!(
        graphics_scan.terminal_inert_ranges.as_slice(),
        std::slice::from_ref(&expected_payload_range)
    );
    assert_eq!(graphics_scan.completed_graphics_events.len(), 1);
    let (event_end_byte_offset, graphics_operation_result) = graphics_scan
        .completed_graphics_events
        .into_iter()
        .next()
        .expect("one image event");
    let GraphicsOperation::Image(decoded_graphics) =
        graphics_operation_result.expect("the image decodes")
    else {
        panic!("expected an image");
    };
    assert_eq!(event_end_byte_offset, kitty_input_bytes.len() - 1);
    assert_eq!(decoded_graphics.protocol, GraphicsProtocol::Kitty);
    assert_eq!(decoded_graphics.image.pixel_width, 1);
    assert_eq!(decoded_graphics.image.pixel_height, 1);
    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn kitty_scan_keeps_non_base64_body_bytes_on_the_terminal_path() {
    let kitty_input_bytes = b"\x1b_Gf=32,s=1,v=1;AAAA\nBBBB\x1b\\";
    let payload_start_byte_index = kitty_input_bytes
        .iter()
        .position(|kitty_input_byte| *kitty_input_byte == b';')
        .expect("the Kitty header has a separator")
        + 1;
    let newline_byte_index = kitty_input_bytes
        .iter()
        .position(|kitty_input_byte| *kitty_input_byte == b'\n')
        .expect("the body has a newline");
    let mut parser = GraphicsParser::default();

    let graphics_scan = parser.process_graphics_operations_with_offsets(kitty_input_bytes);

    assert_eq!(
        graphics_scan.terminal_inert_ranges,
        [
            payload_start_byte_index..newline_byte_index,
            newline_byte_index + 1..kitty_input_bytes.len() - 2
        ]
    );
    assert_eq!(
        graphics_scan.completed_graphics_events,
        [(
            kitty_input_bytes.len() - 1,
            Err(GraphicsError::InvalidBase64 {
                protocol: GraphicsProtocol::Kitty,
            }),
        )]
    );
}

#[test]
fn iterm_scan_marks_only_the_base64_payload_as_terminal_inert() {
    let iterm_input_bytes = build_iterm_file(&build_red_png_bytes());
    let payload_start_byte_index = iterm_input_bytes
        .iter()
        .position(|iterm_input_byte| *iterm_input_byte == b':')
        .expect("the iTerm2 header has a separator")
        + 1;
    let payload_end_byte_index = iterm_input_bytes.len() - 1;
    let mut parser = GraphicsParser::default();

    let graphics_scan = parser.process_graphics_operations_with_offsets(&iterm_input_bytes);

    let expected_payload_range = payload_start_byte_index..payload_end_byte_index;
    assert_eq!(
        graphics_scan.terminal_inert_ranges.as_slice(),
        std::slice::from_ref(&expected_payload_range)
    );
    assert_eq!(graphics_scan.completed_graphics_events.len(), 1);
    let (event_end_byte_offset, graphics_operation_result) = graphics_scan
        .completed_graphics_events
        .into_iter()
        .next()
        .expect("one image event");
    let GraphicsOperation::Image(decoded_graphics) =
        graphics_operation_result.expect("the image decodes")
    else {
        panic!("expected an image");
    };
    assert_eq!(event_end_byte_offset, iterm_input_bytes.len() - 1);
    assert_eq!(decoded_graphics.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(decoded_graphics.image.pixel_width, 1);
    assert_eq!(decoded_graphics.image.pixel_height, 1);
    assert_eq!(decoded_graphics.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn iterm_scan_keeps_non_base64_body_bytes_on_the_terminal_path() {
    let iterm_input_bytes = b"\x1b]1337;File=inline=1:AAAA\nBBBB\x07";
    let payload_start_byte_index = iterm_input_bytes
        .iter()
        .position(|iterm_input_byte| *iterm_input_byte == b':')
        .expect("the iTerm2 header has a separator")
        + 1;
    let newline_byte_index = iterm_input_bytes
        .iter()
        .position(|iterm_input_byte| *iterm_input_byte == b'\n')
        .expect("the body has a newline");
    let mut parser = GraphicsParser::default();

    let graphics_scan = parser.process_graphics_operations_with_offsets(iterm_input_bytes);

    assert_eq!(
        graphics_scan.terminal_inert_ranges,
        [
            payload_start_byte_index..newline_byte_index,
            newline_byte_index + 1..iterm_input_bytes.len() - 1
        ]
    );
    assert_eq!(
        graphics_scan.completed_graphics_events,
        [(
            iterm_input_bytes.len() - 1,
            Err(GraphicsError::InvalidBase64 {
                protocol: GraphicsProtocol::Iterm2,
            }),
        )]
    );
}

#[test]
fn sixel_scan_marks_printable_body_bytes_as_terminal_inert() {
    let sixel_input_bytes = build_one_sixel();
    let payload_start_byte_index = sixel_input_bytes
        .iter()
        .position(|sixel_input_byte| *sixel_input_byte == b'q')
        .expect("the Sixel header has its final byte")
        + 1;
    let payload_end_byte_index = sixel_input_bytes.len() - 2;
    let mut parser = GraphicsParser::default();

    let graphics_scan = parser.process_graphics_operations_with_offsets(&sixel_input_bytes);

    let expected_payload_range = payload_start_byte_index..payload_end_byte_index;
    assert_eq!(
        graphics_scan.terminal_inert_ranges.as_slice(),
        std::slice::from_ref(&expected_payload_range)
    );
    assert_eq!(graphics_scan.completed_graphics_events.len(), 1);
    let (event_end_byte_offset, graphics_operation_result) = graphics_scan
        .completed_graphics_events
        .into_iter()
        .next()
        .expect("one image event");
    let GraphicsOperation::Sixel(sixel_graphic) =
        graphics_operation_result.expect("the image decodes")
    else {
        panic!("expected a Sixel image");
    };
    assert_eq!(event_end_byte_offset, sixel_input_bytes.len() - 1);
    let indexed_graphics_image = sixel_graphic
        .get_indexed_image()
        .expect("the Sixel has drawable pixels");
    assert_eq!(indexed_graphics_image.get_width_pixels(), 1);
    assert_eq!(indexed_graphics_image.get_height_pixels(), 6);
}

#[test]
fn sixel_scan_keeps_an_invalid_printable_body_byte_on_the_terminal_path() {
    let invalid_sixel_input = b"\x1bPq<\x1b\\";
    let mut parser = GraphicsParser::default();

    let graphics_scan = parser.process_graphics_operations_with_offsets(invalid_sixel_input);

    assert_eq!(graphics_scan.terminal_inert_ranges, []);
    assert_eq!(
        graphics_scan.completed_graphics_events,
        [(
            invalid_sixel_input.len() - 1,
            Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            }),
        )]
    );
}

#[test]
fn wrapper_scans_mark_plain_body_runs_as_terminal_inert() {
    let kitty_input_bytes = build_kitty_raw_rgba();
    let tmux_wrapped_bytes = wrap_tmux(&kitty_input_bytes);
    let screen_wrapped_bytes = wrap_screen(&kitty_input_bytes);
    let mut tmux_parser = GraphicsParser::default();
    let mut screen_parser = GraphicsParser::default();

    let tmux_scan = tmux_parser.process_graphics_operations_with_offsets(&tmux_wrapped_bytes);
    let screen_scan = screen_parser.process_graphics_operations_with_offsets(&screen_wrapped_bytes);

    assert_eq!(
        tmux_scan.terminal_inert_ranges,
        [
            9..tmux_wrapped_bytes.len() - 5,
            tmux_wrapped_bytes.len() - 3..tmux_wrapped_bytes.len() - 2
        ]
    );
    let screen_payload_range = 3..screen_wrapped_bytes.len() - 4;
    assert_eq!(
        screen_scan.terminal_inert_ranges.as_slice(),
        std::slice::from_ref(&screen_payload_range)
    );
}

#[test]
fn a_three_chunk_kitty_transfer_keeps_each_chunk_once() {
    let raw_rgba_bytes = vec![255; 1750 * 4];
    let base64_rgba_bytes = STANDARD.encode(raw_rgba_bytes);
    let kitty_base64_chunks: Vec<&[u8]> = base64_rgba_bytes
        .as_bytes()
        .chunks(MAX_KITTY_CHUNK_BYTE_COUNT)
        .collect();
    assert_eq!(kitty_base64_chunks.len(), 3);
    assert!(kitty_base64_chunks[..2]
        .iter()
        .all(|kitty_base64_chunk| kitty_base64_chunk.len().is_multiple_of(4)));
    let mut kitty_transfer_bytes = Vec::new();
    for (chunk_index, kitty_base64_chunk) in kitty_base64_chunks.iter().enumerate() {
        let kitty_chunk_header = if chunk_index == 0 {
            "\x1b_Ga=T,f=32,s=1750,v=1,m=1;".to_string()
        } else if chunk_index + 1 == kitty_base64_chunks.len() {
            "\x1b_Gm=0;".to_string()
        } else {
            "\x1b_Gm=1;".to_string()
        };
        kitty_transfer_bytes.extend_from_slice(kitty_chunk_header.as_bytes());
        kitty_transfer_bytes.extend_from_slice(kitty_base64_chunk);
        kitty_transfer_bytes.extend_from_slice(b"\x1b\\");
    }
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, &kitty_transfer_bytes)
        .expect("the three chunks decode");

    assert_eq!(graphics_event.action, ImageAction::TransmitAndDisplay);
    assert_eq!(graphics_event.image.pixel_width, 1750);
    assert_eq!(graphics_event.image.pixel_height, 1);
    assert_eq!(graphics_event.image.rgba_bytes.len(), 1750 * 4);
    assert!(graphics_event
        .image
        .rgba_bytes
        .iter()
        .all(|rgba_byte| *rgba_byte == 255));
}

#[test]
fn a_kitty_transfer_that_exceeds_the_carry_budget_is_abandoned_after_an_engine_swap() {
    let base64_rgba_bytes = STANDARD.encode(vec![255; 16_384 * 4]);
    let kitty_base64_chunks: Vec<&[u8]> = base64_rgba_bytes
        .as_bytes()
        .chunks(MAX_KITTY_CHUNK_BYTE_COUNT)
        .collect();
    let mut parser = GraphicsParser::default();
    let mut carry_budget_chunk_count = None;
    for (chunk_index, kitty_base64_chunk) in kitty_base64_chunks.iter().enumerate() {
        let kitty_chunk_header = if chunk_index == 0 {
            "\x1b_Gf=32,s=16384,v=1,m=1;".to_string()
        } else {
            "\x1b_Gm=1;".to_string()
        };
        let mut kitty_chunk_bytes = kitty_chunk_header.into_bytes();
        kitty_chunk_bytes.extend_from_slice(kitty_base64_chunk);
        kitty_chunk_bytes.extend_from_slice(b"\x1b\\");
        assert!(parser
            .decode_completed_graphics_events(&kitty_chunk_bytes)
            .is_empty());
        if !parser
            .get_graphics_transport_state()
            .expect("the multipart transfer has state")
            .is_carryable
        {
            carry_budget_chunk_count = Some(chunk_index + 1);
            break;
        }
    }
    let carry_budget_chunk_count =
        carry_budget_chunk_count.expect("the transfer passes the carry budget");
    let graphics_transport_state = parser
        .get_graphics_transport_state()
        .expect("the oversized transfer has transport state");
    assert_eq!(
        graphics_transport_state.graphics_abandonment,
        Some(GraphicsAbandonment::Transfer(GraphicsProtocol::Kitty)),
        "{graphics_transport_state:?}"
    );

    let mut resumed_graphics_parser = GraphicsParser::default();
    resumed_graphics_parser.restore_graphics_carry_state(&[], graphics_transport_state);
    for (chunk_index, kitty_base64_chunk) in kitty_base64_chunks
        .iter()
        .enumerate()
        .skip(carry_budget_chunk_count)
    {
        let kitty_chunk_header = if chunk_index + 1 == kitty_base64_chunks.len() {
            "\x1b_Gm=0;"
        } else {
            "\x1b_Gm=1;"
        };
        let mut kitty_chunk_bytes = kitty_chunk_header.as_bytes().to_vec();
        kitty_chunk_bytes.extend_from_slice(kitty_base64_chunk);
        kitty_chunk_bytes.extend_from_slice(b"\x1b\\");
        let completed_graphics_events =
            resumed_graphics_parser.decode_completed_graphics_events(&kitty_chunk_bytes);
        if chunk_index + 1 == kitty_base64_chunks.len() {
            assert_eq!(
                completed_graphics_events,
                [Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Kitty,
                })]
            );
        } else {
            assert!(completed_graphics_events.is_empty());
        }
    }
}

#[test]
fn an_active_kitty_chunk_that_exceeds_the_carry_budget_is_drained_after_an_engine_swap() {
    let base64_rgba_bytes = STANDARD.encode(vec![255; 16_384 * 4]);
    let kitty_base64_chunks: Vec<&[u8]> = base64_rgba_bytes
        .as_bytes()
        .chunks(MAX_KITTY_CHUNK_BYTE_COUNT)
        .collect();
    let mut parser = GraphicsParser::default();
    let mut first_kitty_chunk = b"\x1b_Gf=32,s=16384,v=1,m=1;".to_vec();
    first_kitty_chunk.extend_from_slice(kitty_base64_chunks[0]);
    first_kitty_chunk.extend_from_slice(b"\x1b\\");
    assert!(parser
        .decode_completed_graphics_events(&first_kitty_chunk)
        .is_empty());

    let mut carry_budget_cut = None;
    for (chunk_index, kitty_base64_chunk) in kitty_base64_chunks.iter().enumerate().skip(1) {
        let kitty_chunk_header = b"\x1b_Gm=1;";
        let graphics_transport_state = parser
            .get_graphics_transport_state()
            .expect("the multipart transfer has state");
        if graphics_transport_state.carry_bytes.len()
            + kitty_chunk_header.len()
            + kitty_base64_chunk.len()
            > MAX_GRAPHICS_CARRY_BYTE_COUNT
        {
            let prefix_byte_count = MAX_GRAPHICS_CARRY_BYTE_COUNT
                - graphics_transport_state.carry_bytes.len()
                - kitty_chunk_header.len()
                + 1;
            let mut carry_budget_prefix = kitty_chunk_header.to_vec();
            carry_budget_prefix.extend_from_slice(&kitty_base64_chunk[..prefix_byte_count]);
            assert!(parser
                .decode_completed_graphics_events(&carry_budget_prefix)
                .is_empty());
            let graphics_transport_state = parser
                .get_graphics_transport_state()
                .expect("the active chunk has transport state");
            assert_eq!(
                graphics_transport_state.graphics_abandonment,
                Some(GraphicsAbandonment::Transfer(GraphicsProtocol::Kitty))
            );
            carry_budget_cut = Some((chunk_index, prefix_byte_count));
            break;
        }
        let mut kitty_chunk_bytes = kitty_chunk_header.to_vec();
        kitty_chunk_bytes.extend_from_slice(kitty_base64_chunk);
        kitty_chunk_bytes.extend_from_slice(b"\x1b\\");
        assert!(parser
            .decode_completed_graphics_events(&kitty_chunk_bytes)
            .is_empty());
    }
    let (carry_budget_chunk_index, prefix_byte_count) =
        carry_budget_cut.expect("the active chunk passes the carry budget");

    let graphics_transport_state = parser
        .get_graphics_transport_state()
        .expect("the oversized active chunk has transport state");
    let mut resumed_graphics_parser = GraphicsParser::default();
    resumed_graphics_parser.restore_graphics_carry_state(&[], graphics_transport_state);
    let mut remaining_active_chunk_bytes =
        kitty_base64_chunks[carry_budget_chunk_index][prefix_byte_count..].to_vec();
    remaining_active_chunk_bytes.extend_from_slice(b"\x1b\\");
    assert!(resumed_graphics_parser
        .decode_completed_graphics_events(&remaining_active_chunk_bytes)
        .is_empty());

    for (chunk_index, kitty_base64_chunk) in kitty_base64_chunks
        .iter()
        .enumerate()
        .skip(carry_budget_chunk_index + 1)
    {
        let kitty_chunk_header = if chunk_index + 1 == kitty_base64_chunks.len() {
            b"\x1b_Gm=0;"
        } else {
            b"\x1b_Gm=1;"
        };
        let mut kitty_chunk_bytes = kitty_chunk_header.to_vec();
        kitty_chunk_bytes.extend_from_slice(kitty_base64_chunk);
        kitty_chunk_bytes.extend_from_slice(b"\x1b\\");
        let completed_graphics_events =
            resumed_graphics_parser.decode_completed_graphics_events(&kitty_chunk_bytes);
        if chunk_index + 1 == kitty_base64_chunks.len() {
            assert_eq!(
                completed_graphics_events,
                [Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Kitty,
                })]
            );
        } else {
            assert!(completed_graphics_events.is_empty());
        }
    }
}

#[test]
fn an_open_graphics_sequence_that_exceeds_the_carry_budget_is_drained_after_an_engine_swap() {
    let mut oversized_kitty_input = b"\x1b_Gf=32,s=1,v=1;".to_vec();
    oversized_kitty_input.extend(std::iter::repeat_n(b'A', MAX_GRAPHICS_CARRY_BYTE_COUNT + 1));
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(&oversized_kitty_input)
        .is_empty());
    let graphics_transport_state = parser
        .get_graphics_transport_state()
        .expect("the open sequence has transport state");
    assert_eq!(
        graphics_transport_state.graphics_abandonment,
        Some(GraphicsAbandonment::Sequence(GraphicsProtocol::Kitty)),
        "{graphics_transport_state:?}"
    );

    let mut resumed_graphics_parser = GraphicsParser::default();
    resumed_graphics_parser.restore_graphics_carry_state(&[], graphics_transport_state);

    assert_eq!(
        resumed_graphics_parser.decode_completed_graphics_events(b"\x1b\\"),
        [Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Kitty,
        })]
    );
}

#[test]
fn an_iterm_transfer_that_exceeds_the_carry_budget_is_abandoned_after_an_engine_swap() {
    let base64_image_bytes = STANDARD.encode(vec![0; 16_384 * 4]);
    let oversized_iterm_input = format!(
        "\x1b]1337;MultipartFile=inline=1;size={}:{}\x07",
        16_384 * 4,
        base64_image_bytes
    )
    .into_bytes();
    let mut parser = GraphicsParser::default();

    assert!(parser
        .decode_completed_graphics_events(&oversized_iterm_input)
        .is_empty());
    let graphics_transport_state = parser
        .get_graphics_transport_state()
        .expect("the multipart transfer has transport state");
    assert_eq!(
        graphics_transport_state.graphics_abandonment,
        Some(GraphicsAbandonment::Transfer(GraphicsProtocol::Iterm2)),
        "{graphics_transport_state:?}"
    );

    let mut resumed_graphics_parser = GraphicsParser::default();
    resumed_graphics_parser.restore_graphics_carry_state(&[], graphics_transport_state);
    assert!(resumed_graphics_parser
        .decode_completed_graphics_events(b"\x1b]1337;FilePart=AAAA\x07")
        .is_empty());
    assert_eq!(
        resumed_graphics_parser.decode_completed_graphics_events(b"\x1b]1337;FileEnd\x07"),
        [Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Iterm2,
        })]
    );
}

#[test]
fn kitty_display_metadata_is_preserved_exactly() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let kitty_metadata_input = format!(
        "\x1b_Ga=T,f=32,s=1,v=1,I=8,p=9,N=1,U=1,w=1,h=1,c=2,r=3,x=4,y=5,X=6,Y=7,C=1,z=-2;{base64_rgba_bytes}\x1b\\"
    );
    let mut parser = GraphicsParser::default();

    let graphics_event = get_only_graphics_event(&mut parser, kitty_metadata_input.as_bytes())
        .expect("the kitty image decodes");

    assert_eq!(graphics_event.display.image_id, None);
    assert_eq!(graphics_event.display.image_number, Some(8));
    assert_eq!(graphics_event.display.placement_id, Some(9));
    assert_eq!(graphics_event.display.usage_hints, 1);
    assert!(graphics_event.display.is_unicode_placeholder);
    assert_eq!(
        graphics_event.display.requested_width,
        Some(ImageDimension::Pixels(1))
    );
    assert_eq!(
        graphics_event.display.requested_height,
        Some(ImageDimension::Pixels(1))
    );
    assert_eq!(graphics_event.display.requested_column_count, Some(2));
    assert_eq!(graphics_event.display.requested_row_count, Some(3));
    assert_eq!(graphics_event.display.source_pixel_offset_x, Some(4));
    assert_eq!(graphics_event.display.source_pixel_offset_y, Some(5));
    assert_eq!(graphics_event.display.cell_pixel_offset_x, Some(6));
    assert_eq!(graphics_event.display.cell_pixel_offset_y, Some(7));
    assert!(!graphics_event.display.should_move_cursor);
    assert_eq!(graphics_event.display.z_index, -2);
}

#[test]
fn a_chunked_kitty_transfer_survives_an_engine_swap() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let split_byte_index = base64_rgba_bytes.len() / 2;
    let first_kitty_chunk = format!(
        "\x1b_Gf=32,s=1,v=1,m=1;{}\x1b\\",
        &base64_rgba_bytes[..split_byte_index]
    );
    let final_kitty_chunk = format!("\x1b_Gm=0;{}\x1b\\", &base64_rgba_bytes[split_byte_index..]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(first_kitty_chunk.as_bytes());
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine = TerminalEngine::from_terminal_state_with_graphics(
        terminal_state,
        &terminal_undecoded_bytes,
        &graphics_undecoded_bytes,
    );
    let _ = resumed_terminal_engine.process_pty_output(final_kitty_chunk.as_bytes());

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed image event")
        .expect("the resumed image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn a_chunked_kitty_transfer_survives_two_engine_swaps() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255]);
    let kitty_base64_chunks: Vec<&str> = base64_rgba_bytes
        .as_bytes()
        .chunks(4)
        .map(|kitty_base64_chunk| {
            std::str::from_utf8(kitty_base64_chunk).expect("base64 chunks are ASCII")
        })
        .collect();
    let first_kitty_chunk = format!("\x1b_Gf=32,s=3,v=1,m=1;{}\x1b\\", kitty_base64_chunks[0]);
    let second_kitty_chunk = format!("\x1b_Gm=1;{}\x1b\\", kitty_base64_chunks[1]);
    let third_kitty_chunk = format!("\x1b_Gm=1;{}\x1b\\", kitty_base64_chunks[2]);
    let final_kitty_chunk = format!("\x1b_Gm=0;{}\x1b\\", kitty_base64_chunks[3]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(first_kitty_chunk.as_bytes());
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine = TerminalEngine::from_terminal_state_with_graphics(
        terminal_state,
        &terminal_undecoded_bytes,
        &graphics_undecoded_bytes,
    );
    let _ = resumed_terminal_engine.process_pty_output(second_kitty_chunk.as_bytes());
    let terminal_undecoded_bytes = resumed_terminal_engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = resumed_terminal_engine.undecoded_graphics_bytes().to_vec();
    let terminal_state = resumed_terminal_engine.into_terminal_state();
    let mut final_terminal_engine = TerminalEngine::from_terminal_state_with_graphics(
        terminal_state,
        &terminal_undecoded_bytes,
        &graphics_undecoded_bytes,
    );
    let _ = final_terminal_engine.process_pty_output(third_kitty_chunk.as_bytes());
    let _ = final_terminal_engine.process_pty_output(final_kitty_chunk.as_bytes());

    let graphics_event_record = final_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the twice-resumed image event")
        .expect("the twice-resumed image decodes");
    assert_eq!(graphics_event_record.image.pixel_width, 3);
    assert_eq!(graphics_event_record.image.pixel_height, 1);
    assert_eq!(
        graphics_event_record.image.rgba_bytes,
        [255, 0, 0, 255].repeat(3)
    );
}

#[test]
fn chunked_kitty_transfer_carry_survives_an_unrelated_escape() {
    let base64_rgba_bytes = STANDARD.encode([255, 0, 0, 255]);
    let split_byte_index = base64_rgba_bytes.len() / 2;
    let first_kitty_chunk = format!(
        "\x1b_Gf=32,s=1,v=1,m=1;{}\x1b\\",
        &base64_rgba_bytes[..split_byte_index]
    );
    let final_kitty_chunk = format!("\x1b_Gm=0;{}\x1b\\", &base64_rgba_bytes[split_byte_index..]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(first_kitty_chunk.as_bytes());
    let _ = engine.process_pty_output(b"\x1b[2J");
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine = TerminalEngine::from_terminal_state_with_graphics(
        terminal_state,
        &terminal_undecoded_bytes,
        &graphics_undecoded_bytes,
    );
    let _ = resumed_terminal_engine.process_pty_output(final_kitty_chunk.as_bytes());

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed image event")
        .expect("the resumed image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn an_iterm_multipart_transfer_survives_an_engine_swap() {
    let png_image_bytes = build_red_png_bytes();
    let base64_image_bytes = STANDARD.encode(&png_image_bytes);
    let split_byte_index = base64_image_bytes.len() / 2;
    let first_iterm_chunk = format!(
        "\x1b]1337;MultipartFile=inline=1;width=1;height=1;size={}\x07\
\x1b]1337;FilePart={}\x07",
        png_image_bytes.len(),
        &base64_image_bytes[..split_byte_index],
    );
    let final_iterm_chunk = format!(
        "\x1b]1337;FilePart={}\x07\x1b]1337;FileEnd\x07",
        &base64_image_bytes[split_byte_index..],
    );
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(first_iterm_chunk.as_bytes());
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine = TerminalEngine::from_terminal_state_with_graphics(
        terminal_state,
        &terminal_undecoded_bytes,
        &graphics_undecoded_bytes,
    );
    let _ = resumed_terminal_engine.process_pty_output(final_iterm_chunk.as_bytes());

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed multipart event")
        .expect("the resumed multipart image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn a_screen_transfer_survives_an_engine_swap() {
    let inner_iterm_bytes = build_iterm_cell_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let first_screen_chunk = wrap_screen(&inner_iterm_bytes[..split_byte_index]);
    let final_screen_chunk = wrap_screen(&inner_iterm_bytes[split_byte_index..]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(&first_screen_chunk);
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let is_screen_continuation = engine.is_graphics_screen_continuation();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine =
        TerminalEngine::from_terminal_state_with_graphics_and_events_and_screen(
            terminal_state,
            &terminal_undecoded_bytes,
            &graphics_undecoded_bytes,
            &[],
            is_screen_continuation,
            false,
        );
    let _ = resumed_terminal_engine.process_pty_output(&final_screen_chunk);

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed Screen image event")
        .expect("the resumed Screen image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn a_c1_screen_wrapper_with_an_inner_transfer_survives_an_engine_swap() {
    let inner_iterm_bytes = build_iterm_cell_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let first_screen_chunk = wrap_screen(&inner_iterm_bytes[..split_byte_index]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(&first_screen_chunk);
    let _ = engine.process_pty_output(&[0x90]);
    assert!(engine.is_graphics_screen_continuation());
    assert!(engine.is_graphics_screen_wrapper_active());
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let graphics_transport_state = engine
        .get_graphics_transport_state()
        .expect("the split wrapper has transport state");
    assert!(graphics_transport_state.screen_inner_transport.is_some());
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine =
        TerminalEngine::from_terminal_state_with_graphics_and_events_and_wrappers(
            terminal_state,
            &terminal_undecoded_bytes,
            &graphics_undecoded_bytes,
            &[],
            graphics_transport_state,
        );
    let mut final_screen_chunk = inner_iterm_bytes[split_byte_index..].to_vec();
    final_screen_chunk.extend_from_slice(b"\x1b\\");
    let _ = resumed_terminal_engine.process_pty_output(&final_screen_chunk);

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed C1 Screen image event")
        .expect("the resumed C1 Screen image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn a_tmux_transfer_survives_an_engine_swap() {
    let inner_iterm_bytes = build_iterm_cell_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let first_tmux_chunk = wrap_tmux(&inner_iterm_bytes[..split_byte_index]);
    let final_tmux_chunk = wrap_tmux(&inner_iterm_bytes[split_byte_index..]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(&first_tmux_chunk);
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let is_tmux_continuation = engine.is_graphics_tmux_continuation();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine =
        TerminalEngine::from_terminal_state_with_graphics_and_events_and_wrappers(
            terminal_state,
            &terminal_undecoded_bytes,
            &graphics_undecoded_bytes,
            &[],
            GraphicsTransportState {
                is_tmux_continuation,
                ..GraphicsTransportState::default()
            },
        );
    let _ = resumed_terminal_engine.process_pty_output(&final_tmux_chunk);

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed tmux image event")
        .expect("the resumed tmux image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn a_c1_tmux_wrapper_with_an_inner_transfer_survives_an_engine_swap() {
    let inner_iterm_bytes = build_iterm_cell_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let first_tmux_chunk = wrap_tmux(&inner_iterm_bytes[..split_byte_index]);
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(&first_tmux_chunk);
    let _ = engine.process_pty_output(&[0x90]);
    assert!(engine.is_graphics_tmux_continuation());
    assert!(engine.is_graphics_tmux_wrapper_active());
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let graphics_transport_state = engine
        .get_graphics_transport_state()
        .expect("the split wrapper has transport state");
    assert!(graphics_transport_state.tmux_inner_transport.is_some());
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine =
        TerminalEngine::from_terminal_state_with_graphics_and_events_and_wrappers(
            terminal_state,
            &terminal_undecoded_bytes,
            &graphics_undecoded_bytes,
            &[],
            graphics_transport_state,
        );
    let mut final_tmux_chunk = b"tmux;".to_vec();
    final_tmux_chunk.extend_from_slice(&inner_iterm_bytes[split_byte_index..]);
    final_tmux_chunk.extend_from_slice(b"\x1b\\");
    let _ = resumed_terminal_engine.process_pty_output(&final_tmux_chunk);

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed C1 tmux image event")
        .expect("the resumed C1 tmux image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn nested_passthrough_wrappers_survive_an_engine_swap() {
    let inner_iterm_bytes = build_iterm_cell_file(&build_red_png_bytes());
    let split_byte_index = inner_iterm_bytes.len() / 2;
    let first_nested_chunk = wrap_tmux(&wrap_screen(&inner_iterm_bytes[..split_byte_index]));
    let final_nested_chunk = wrap_tmux(&wrap_screen(&inner_iterm_bytes[split_byte_index..]));
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });

    let _ = engine.process_pty_output(&first_nested_chunk);
    let graphics_transport_state = engine
        .get_graphics_transport_state()
        .expect("the nested wrappers have transport state");
    assert!(graphics_transport_state.tmux_inner_transport.is_some());
    assert!(graphics_transport_state
        .tmux_inner_transport
        .as_ref()
        .expect("the tmux parser")
        .screen_inner_transport
        .is_some());
    let terminal_undecoded_bytes = engine.undecoded_terminal_bytes().to_vec();
    let graphics_undecoded_bytes = engine.undecoded_graphics_bytes().to_vec();
    let terminal_state = engine.into_terminal_state();
    let mut resumed_terminal_engine =
        TerminalEngine::from_terminal_state_with_graphics_and_events_and_wrappers(
            terminal_state,
            &terminal_undecoded_bytes,
            &graphics_undecoded_bytes,
            &[],
            graphics_transport_state,
        );
    let _ = resumed_terminal_engine.process_pty_output(&final_nested_chunk);

    let graphics_event_record = resumed_terminal_engine
        .take_graphics_events()
        .into_iter()
        .next()
        .expect("the resumed nested image event")
        .expect("the resumed nested image decodes");
    assert_eq!(graphics_event_record.image.rgba_bytes, [255, 0, 0, 255]);
}

#[test]
fn passthrough_wrapper_nesting_stays_bounded() {
    let mut nested_graphics_bytes = build_kitty_raw_rgba();
    for _ in 0..=MAX_GRAPHICS_WRAPPER_DEPTH {
        nested_graphics_bytes = wrap_tmux(&nested_graphics_bytes);
    }
    let mut parser = GraphicsParser::default();

    assert_eq!(
        parser.decode_completed_graphics_events(&nested_graphics_bytes),
        [Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Sixel,
        })]
    );
}
