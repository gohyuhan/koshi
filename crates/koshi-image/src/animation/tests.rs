//! Tests for validated animated raster values and media decoding.

use super::*;

use image::ImageEncoder;
use std::sync::Arc;

/// The pixel width and height of the first frame's canvas.
fn get_animation_canvas_pixel_dimensions(decoded_animation: &DecodedAnimation) -> (u32, u32) {
    let first_frame_image = decoded_animation.list_frames()[0].get_decoded_image();
    (
        first_frame_image.pixel_width,
        first_frame_image.pixel_height,
    )
}

fn build_one_pixel_image(red_channel: u8) -> DecodedImage {
    DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![red_channel, 0, 0, 255],
    }
}

fn build_gif_frame(red_pixel_channel: u8) -> Vec<u8> {
    vec![
        0x2c,
        0,
        0,
        0,
        0,
        1,
        0,
        1,
        0,
        0,
        2,
        2,
        if red_pixel_channel == 0 { 0x44 } else { 0x4c },
        1,
        0,
    ]
}

fn build_gif_with_frames() -> Vec<u8> {
    let mut gif_bytes = b"GIF89a".to_vec();
    gif_bytes.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]);
    gif_bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    gif_bytes.extend_from_slice(&build_gif_frame(0));
    gif_bytes.extend_from_slice(&build_gif_frame(1));
    gif_bytes.push(0x3b);
    gif_bytes
}

fn build_gif_with_one_frame() -> Vec<u8> {
    let mut gif_bytes = b"GIF89a".to_vec();
    gif_bytes.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]);
    gif_bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    gif_bytes.extend_from_slice(&build_gif_frame(0));
    gif_bytes.push(0x3b);
    gif_bytes
}

fn build_gif_with_disposal() -> Vec<u8> {
    let mut gif_bytes = b"GIF89a".to_vec();
    gif_bytes.extend_from_slice(&[2, 0, 1, 0, 0x80, 0, 0]);
    gif_bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    gif_bytes.extend_from_slice(&[0x2c, 0, 0, 0, 0, 2, 0, 1, 0, 0, 2, 2, 0x04, 0x05, 0]);
    gif_bytes.extend_from_slice(&[0x21, 0xf9, 4, 0x08, 0, 0, 0, 0]);
    gif_bytes.extend_from_slice(&[0x2c, 1, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4c, 0x01, 0]);
    gif_bytes.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4c, 0x01, 0]);
    gif_bytes.push(0x3b);
    gif_bytes
}

fn build_gif_loop_extension(application_identifier: &[u8; 11], repeat_count: u16) -> Vec<u8> {
    let mut extension_bytes = vec![0x21, 0xff, 11];
    extension_bytes.extend_from_slice(application_identifier);
    extension_bytes.extend_from_slice(&[3, 1]);
    extension_bytes.extend_from_slice(&repeat_count.to_le_bytes());
    extension_bytes.push(0);
    extension_bytes
}

fn insert_gif_extension_before_trailer(source_gif_bytes: &[u8], extension_bytes: &[u8]) -> Vec<u8> {
    let trailer_byte_offset = source_gif_bytes
        .iter()
        .rposition(|gif_byte| *gif_byte == 0x3b)
        .expect("GIF trailer exists");
    let mut extended_gif_bytes = Vec::with_capacity(source_gif_bytes.len() + extension_bytes.len());
    extended_gif_bytes.extend_from_slice(&source_gif_bytes[..trailer_byte_offset]);
    extended_gif_bytes.extend_from_slice(extension_bytes);
    extended_gif_bytes.extend_from_slice(&source_gif_bytes[trailer_byte_offset..]);
    extended_gif_bytes
}

fn insert_gif_extension_before_second_image_frame(
    source_gif_bytes: &[u8],
    extension_bytes: &[u8],
) -> Vec<u8> {
    let first_frame_byte_offset = source_gif_bytes
        .iter()
        .position(|source_byte| *source_byte == 0x2c)
        .expect("first frame marker exists");
    let second_frame_byte_offset = source_gif_bytes[first_frame_byte_offset + 1..]
        .iter()
        .position(|source_byte| *source_byte == 0x2c)
        .map(|relative_byte_offset| first_frame_byte_offset + 1 + relative_byte_offset)
        .expect("second frame marker exists");
    let mut extended_gif_bytes = Vec::with_capacity(source_gif_bytes.len() + extension_bytes.len());
    extended_gif_bytes.extend_from_slice(&source_gif_bytes[..second_frame_byte_offset]);
    extended_gif_bytes.extend_from_slice(extension_bytes);
    extended_gif_bytes.extend_from_slice(&source_gif_bytes[second_frame_byte_offset..]);
    extended_gif_bytes
}

fn build_png_chunk(chunk_type: &[u8; 4], chunk_payload_bytes: &[u8]) -> Vec<u8> {
    let mut png_chunk_bytes = Vec::with_capacity(12 + chunk_payload_bytes.len());
    png_chunk_bytes.extend_from_slice(&(chunk_payload_bytes.len() as u32).to_be_bytes());
    png_chunk_bytes.extend_from_slice(chunk_type);
    png_chunk_bytes.extend_from_slice(chunk_payload_bytes);
    let mut crc = 0xffff_ffffu32;
    for &chunk_byte in chunk_type.iter().chain(chunk_payload_bytes) {
        crc ^= u32::from(chunk_byte);
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

fn build_one_pixel_apng_frame_control(frame_sequence_number: u32) -> [u8; 26] {
    let mut frame_control_bytes = [0; 26];
    frame_control_bytes[..4].copy_from_slice(&frame_sequence_number.to_be_bytes());
    frame_control_bytes[4..8].copy_from_slice(&1u32.to_be_bytes());
    frame_control_bytes[8..12].copy_from_slice(&1u32.to_be_bytes());
    frame_control_bytes[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame_control_bytes[22..24].copy_from_slice(&10u16.to_be_bytes());
    frame_control_bytes
}

fn build_animated_png(animation_loop_count: u32) -> Vec<u8> {
    let mut source_png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut source_png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");

    let mut animated_png_bytes = source_png_bytes[..8].to_vec();
    let mut png_byte_offset = 8;
    let mut has_inserted_animation = false;
    let mut first_frame_payload_bytes = Vec::new();
    while png_byte_offset < source_png_bytes.len() {
        let chunk_payload_byte_count = u32::from_be_bytes(
            source_png_bytes[png_byte_offset..png_byte_offset + 4]
                .try_into()
                .expect("PNG length has four bytes"),
        );
        let chunk_end_byte_offset = png_byte_offset
            + 12
            + usize::try_from(chunk_payload_byte_count).expect("PNG length fits");
        let chunk_type: &[u8; 4] = source_png_bytes[png_byte_offset + 4..png_byte_offset + 8]
            .try_into()
            .expect("PNG chunk type has four bytes");
        let chunk_payload_bytes = &source_png_bytes[png_byte_offset + 8..chunk_end_byte_offset - 4];
        match chunk_type {
            b"IDAT" if !has_inserted_animation => {
                let mut animation_control_bytes = [0; 8];
                animation_control_bytes[..4].copy_from_slice(&2u32.to_be_bytes());
                animation_control_bytes[4..].copy_from_slice(&animation_loop_count.to_be_bytes());
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(b"acTL", &animation_control_bytes));
                animated_png_bytes.extend_from_slice(&build_png_chunk(
                    b"fcTL",
                    &build_one_pixel_apng_frame_control(0),
                ));
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(chunk_type, chunk_payload_bytes));
                first_frame_payload_bytes.extend_from_slice(chunk_payload_bytes);
                has_inserted_animation = true;
            }
            b"IEND" if has_inserted_animation => {
                animated_png_bytes.extend_from_slice(&build_png_chunk(
                    b"fcTL",
                    &build_one_pixel_apng_frame_control(1),
                ));
                let mut second_frame_payload_bytes =
                    Vec::with_capacity(4 + first_frame_payload_bytes.len());
                second_frame_payload_bytes.extend_from_slice(&2u32.to_be_bytes());
                second_frame_payload_bytes.extend_from_slice(&first_frame_payload_bytes);
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(b"fdAT", &second_frame_payload_bytes));
                animated_png_bytes
                    .extend_from_slice(&build_png_chunk(chunk_type, chunk_payload_bytes));
            }
            _ => animated_png_bytes
                .extend_from_slice(&source_png_bytes[png_byte_offset..chunk_end_byte_offset]),
        }
        png_byte_offset = chunk_end_byte_offset;
    }
    animated_png_bytes
}

fn build_png_idat_bytes(rgba_bytes: &[u8], width_pixels: u32, height_pixels: u32) -> Vec<u8> {
    let mut source_png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut source_png_bytes)
        .write_image(
            rgba_bytes,
            width_pixels,
            height_pixels,
            image::ColorType::Rgba8.into(),
        )
        .expect("PNG frame encodes");

    let mut image_data_bytes = Vec::new();
    let mut png_byte_offset = 8;
    while png_byte_offset < source_png_bytes.len() {
        let chunk_payload_byte_count = u32::from_be_bytes(
            source_png_bytes[png_byte_offset..png_byte_offset + 4]
                .try_into()
                .expect("PNG length has four bytes"),
        ) as usize;
        let chunk_end_byte_offset = png_byte_offset + 12 + chunk_payload_byte_count;
        if &source_png_bytes[png_byte_offset + 4..png_byte_offset + 8] == b"IDAT" {
            image_data_bytes.extend_from_slice(
                &source_png_bytes[png_byte_offset + 8..chunk_end_byte_offset - 4],
            );
        }
        png_byte_offset = chunk_end_byte_offset;
    }
    image_data_bytes
}

#[allow(clippy::too_many_arguments)]
fn build_apng_frame_control(
    frame_sequence_number: u32,
    frame_width_pixels: u32,
    frame_height_pixels: u32,
    frame_left_pixels: u32,
    frame_top_pixels: u32,
    delay_numerator: u16,
    delay_denominator: u16,
    disposal_method: u8,
    blend_method: u8,
) -> [u8; 26] {
    let mut frame_control_bytes = [0; 26];
    frame_control_bytes[..4].copy_from_slice(&frame_sequence_number.to_be_bytes());
    frame_control_bytes[4..8].copy_from_slice(&frame_width_pixels.to_be_bytes());
    frame_control_bytes[8..12].copy_from_slice(&frame_height_pixels.to_be_bytes());
    frame_control_bytes[12..16].copy_from_slice(&frame_left_pixels.to_be_bytes());
    frame_control_bytes[16..20].copy_from_slice(&frame_top_pixels.to_be_bytes());
    frame_control_bytes[20..22].copy_from_slice(&delay_numerator.to_be_bytes());
    frame_control_bytes[22..24].copy_from_slice(&delay_denominator.to_be_bytes());
    frame_control_bytes[24] = disposal_method;
    frame_control_bytes[25] = blend_method;
    frame_control_bytes
}

fn build_asymmetric_apng() -> Vec<u8> {
    let first_frame_image_data_bytes =
        build_png_idat_bytes(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1);
    let second_frame_image_data_bytes = build_png_idat_bytes(&[0, 0, 255, 128], 1, 1);
    let mut image_header_bytes = [0; 13];
    image_header_bytes[..4].copy_from_slice(&2u32.to_be_bytes());
    image_header_bytes[4..8].copy_from_slice(&1u32.to_be_bytes());
    image_header_bytes[8] = 8;
    image_header_bytes[9] = 6;

    let mut animated_png_bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    animated_png_bytes.extend_from_slice(&build_png_chunk(b"IHDR", &image_header_bytes));
    animated_png_bytes.extend_from_slice(&build_png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
    animated_png_bytes.extend_from_slice(&build_png_chunk(
        b"fcTL",
        &build_apng_frame_control(0, 2, 1, 0, 0, 1, 10, 1, 0),
    ));
    animated_png_bytes.extend_from_slice(&build_png_chunk(b"IDAT", &first_frame_image_data_bytes));
    animated_png_bytes.extend_from_slice(&build_png_chunk(
        b"fcTL",
        &build_apng_frame_control(1, 1, 1, 1, 0, 2, 10, 0, 0),
    ));
    let mut second_frame_payload_bytes =
        Vec::with_capacity(4 + second_frame_image_data_bytes.len());
    second_frame_payload_bytes.extend_from_slice(&2u32.to_be_bytes());
    second_frame_payload_bytes.extend_from_slice(&second_frame_image_data_bytes);
    animated_png_bytes.extend_from_slice(&build_png_chunk(b"fdAT", &second_frame_payload_bytes));
    animated_png_bytes.extend_from_slice(&build_png_chunk(b"IEND", &[]));
    animated_png_bytes
}

fn build_webp_chunk(chunk_type: &[u8; 4], chunk_payload_bytes: &[u8]) -> Vec<u8> {
    let mut webp_chunk_bytes =
        Vec::with_capacity(8 + chunk_payload_bytes.len() + chunk_payload_bytes.len() % 2);
    webp_chunk_bytes.extend_from_slice(chunk_type);
    webp_chunk_bytes.extend_from_slice(&(chunk_payload_bytes.len() as u32).to_le_bytes());
    webp_chunk_bytes.extend_from_slice(chunk_payload_bytes);
    if chunk_payload_bytes.len() % 2 == 1 {
        webp_chunk_bytes.push(0);
    }
    webp_chunk_bytes
}

fn build_animated_webp(animation_loop_count: u16) -> Vec<u8> {
    let mut source_webp_bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut source_webp_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel WebP encodes");
    let mut webp_byte_offset = 12;
    let mut frame_image_data_bytes = Vec::new();
    while webp_byte_offset < source_webp_bytes.len() {
        let chunk_payload_byte_count = usize::try_from(u32::from_le_bytes(
            source_webp_bytes[webp_byte_offset + 4..webp_byte_offset + 8]
                .try_into()
                .expect("WebP chunk length has four bytes"),
        ))
        .expect("WebP length fits");
        if &source_webp_bytes[webp_byte_offset..webp_byte_offset + 4] == b"VP8L" {
            frame_image_data_bytes.extend_from_slice(
                &source_webp_bytes
                    [webp_byte_offset + 8..webp_byte_offset + 8 + chunk_payload_byte_count],
            );
        }
        webp_byte_offset += 8 + chunk_payload_byte_count + chunk_payload_byte_count % 2;
    }

    let webp_extended_header_bytes = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut animation_control_bytes = [0; 6];
    animation_control_bytes[4..].copy_from_slice(&animation_loop_count.to_le_bytes());
    let mut animation_frame_header_bytes = [0; 16];
    animation_frame_header_bytes[12] = 1;
    let mut animation_frame_payload_bytes = animation_frame_header_bytes.to_vec();
    animation_frame_payload_bytes
        .extend_from_slice(&build_webp_chunk(b"VP8L", &frame_image_data_bytes));

    let webp_chunks = [
        build_webp_chunk(b"VP8X", &webp_extended_header_bytes),
        build_webp_chunk(b"ANIM", &animation_control_bytes),
        build_webp_chunk(b"ANMF", &animation_frame_payload_bytes),
        build_webp_chunk(b"ANMF", &animation_frame_payload_bytes),
    ];
    let riff_body_byte_count: usize = 4 + webp_chunks.iter().map(Vec::len).sum::<usize>();
    let mut animated_webp_bytes = b"RIFF".to_vec();
    animated_webp_bytes.extend_from_slice(&(riff_body_byte_count as u32).to_le_bytes());
    animated_webp_bytes.extend_from_slice(b"WEBP");
    for webp_chunk_bytes in webp_chunks {
        animated_webp_bytes.extend_from_slice(&webp_chunk_bytes);
    }
    animated_webp_bytes
}

fn build_webp_vp8l_bytes(rgba_bytes: &[u8], width_pixels: u32, height_pixels: u32) -> Vec<u8> {
    let mut source_webp_bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut source_webp_bytes)
        .write_image(
            rgba_bytes,
            width_pixels,
            height_pixels,
            image::ColorType::Rgba8.into(),
        )
        .expect("WebP frame encodes");
    let mut webp_byte_offset = 12;
    while webp_byte_offset < source_webp_bytes.len() {
        let chunk_payload_byte_count = usize::try_from(u32::from_le_bytes(
            source_webp_bytes[webp_byte_offset + 4..webp_byte_offset + 8]
                .try_into()
                .expect("WebP chunk length has four bytes"),
        ))
        .expect("WebP length fits");
        if &source_webp_bytes[webp_byte_offset..webp_byte_offset + 4] == b"VP8L" {
            return source_webp_bytes
                [webp_byte_offset + 8..webp_byte_offset + 8 + chunk_payload_byte_count]
                .to_vec();
        }
        webp_byte_offset += 8 + chunk_payload_byte_count + chunk_payload_byte_count % 2;
    }
    panic!("lossless WebP has a VP8L chunk");
}

fn build_webp_frame_header(
    frame_left_pixels: u32,
    frame_top_pixels: u32,
    frame_width_pixels: u32,
    frame_height_pixels: u32,
    frame_duration_ms: u32,
    has_disposal: bool,
    has_blend: bool,
) -> Vec<u8> {
    let mut frame_header_bytes = [0; 16];
    frame_header_bytes[..3].copy_from_slice(&(frame_left_pixels / 2).to_le_bytes()[..3]);
    frame_header_bytes[3..6].copy_from_slice(&(frame_top_pixels / 2).to_le_bytes()[..3]);
    frame_header_bytes[6..9].copy_from_slice(&(frame_width_pixels - 1).to_le_bytes()[..3]);
    frame_header_bytes[9..12].copy_from_slice(&(frame_height_pixels - 1).to_le_bytes()[..3]);
    frame_header_bytes[12..15].copy_from_slice(&frame_duration_ms.to_le_bytes()[..3]);
    frame_header_bytes[15] = u8::from(has_disposal) | (u8::from(!has_blend) << 1);
    frame_header_bytes.to_vec()
}

fn build_asymmetric_webp() -> Vec<u8> {
    let first_frame_image_data_bytes = build_webp_vp8l_bytes(
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255, 255, 0, 255, 255,
        ],
        4,
        1,
    );
    let second_frame_image_data_bytes =
        build_webp_vp8l_bytes(&[0, 0, 255, 128, 255, 255, 255, 255], 2, 1);
    let webp_extended_header_bytes = [0x12, 0, 0, 0, 3, 0, 0, 0, 0, 0];
    let animation_control_bytes = [0; 6];
    let mut first_frame_payload_bytes = build_webp_frame_header(0, 0, 4, 1, 10, true, false);
    first_frame_payload_bytes
        .extend_from_slice(&build_webp_chunk(b"VP8L", &first_frame_image_data_bytes));
    let mut second_frame_payload_bytes = build_webp_frame_header(2, 0, 2, 1, 20, false, false);
    second_frame_payload_bytes
        .extend_from_slice(&build_webp_chunk(b"VP8L", &second_frame_image_data_bytes));
    let webp_chunks = [
        build_webp_chunk(b"VP8X", &webp_extended_header_bytes),
        build_webp_chunk(b"ANIM", &animation_control_bytes),
        build_webp_chunk(b"ANMF", &first_frame_payload_bytes),
        build_webp_chunk(b"ANMF", &second_frame_payload_bytes),
    ];
    let riff_body_byte_count: usize = 4 + webp_chunks.iter().map(Vec::len).sum::<usize>();
    let mut animated_webp_bytes = b"RIFF".to_vec();
    animated_webp_bytes.extend_from_slice(&(riff_body_byte_count as u32).to_le_bytes());
    animated_webp_bytes.extend_from_slice(b"WEBP");
    for webp_chunk_bytes in webp_chunks {
        animated_webp_bytes.extend_from_slice(&webp_chunk_bytes);
    }
    animated_webp_bytes
}

#[test]
fn frame_delay_and_loop_policy_reject_zero_values() {
    assert_eq!(
        FrameDelay::from_millisecond_ratio(1, 0).expect_err("zero delay denominator is invalid"),
        AnimationError::InvalidDelayDenominator
    );
    assert_eq!(
        LoopPolicy::from_finite_playback_count(0)
            .expect_err("zero finite playback count is invalid"),
        AnimationError::InvalidPlaybackCount
    );
    let frame = AnimationFrame::from_image_and_delay(
        build_one_pixel_image(255),
        FrameDelay::from_millisecond_ratio(1, 1).expect("delay is valid"),
    )
    .expect("frame is valid");
    assert_eq!(
        DecodedAnimation::from_frames_and_loop_policy(vec![frame], LoopPolicy::Finite(0))
            .expect_err("zero finite playback count is invalid"),
        AnimationError::InvalidPlaybackCount
    );
    assert_eq!(
        FrameDelay::from_millisecond_ratio(0, 1)
            .expect("zero delay is data")
            .get_numerator_ms(),
        0
    );
}

#[test]
fn animation_constructor_keeps_frames_and_loop_policy() {
    let frame_delay = FrameDelay::from_millisecond_ratio(10, 1).expect("delay is valid");
    let first_decoded_image = Arc::new(build_one_pixel_image(255));
    let first_frame =
        AnimationFrame::from_image_and_delay(Arc::clone(&first_decoded_image), frame_delay)
            .expect("first frame is valid");
    let second_frame = AnimationFrame::from_image_and_delay(build_one_pixel_image(0), frame_delay)
        .expect("second frame is valid");
    let animation = DecodedAnimation::from_frames_and_loop_policy(
        vec![first_frame.clone(), second_frame.clone()],
        LoopPolicy::from_finite_playback_count(3).expect("finite loop count is valid"),
    )
    .expect("animation is valid");

    assert_eq!(animation.get_frame_count(), 2);
    assert_eq!(get_animation_canvas_pixel_dimensions(&animation), (1, 1));
    assert_eq!(animation.list_frames()[0], first_frame);
    assert_eq!(
        animation.list_frames()[1].get_decoded_image().rgba_bytes,
        vec![0, 0, 0, 255]
    );
    assert_eq!(animation.get_loop_policy(), LoopPolicy::Finite(3));
    let first_shared_image = first_frame.clone_decoded_image();
    let cloned_shared_image = first_frame.clone().clone_decoded_image();
    assert!(Arc::ptr_eq(&first_decoded_image, &first_shared_image));
    assert!(Arc::ptr_eq(&first_shared_image, &cloned_shared_image));
}

#[test]
fn gapless_frames_are_distinct_from_zero_delay_media_frames() {
    let decoded_image = build_one_pixel_image(255);
    let gapless_frame =
        AnimationFrame::from_gapless_image(decoded_image).expect("the gapless frame is valid");
    assert!(gapless_frame.is_gapless());
    assert_eq!(
        gapless_frame.get_frame_delay(),
        FrameDelay::from_millisecond_ratio(0, 1).expect("zero delay is valid")
    );

    let zero_delay_frame = AnimationFrame::from_image_and_delay(
        build_one_pixel_image(0),
        FrameDelay::from_millisecond_ratio(0, 1).expect("zero delay is valid"),
    )
    .expect("the media frame is valid");
    assert!(!zero_delay_frame.is_gapless());
}

#[test]
fn animation_serde_round_trip_and_validation_are_bounded() {
    let frame_delay = FrameDelay::from_millisecond_ratio(7, 10).expect("delay is valid");
    let frame = AnimationFrame::from_image_and_delay(build_one_pixel_image(255), frame_delay)
        .expect("frame is valid");
    let decoded_media = DecodedMedia::Animation(
        DecodedAnimation::from_frames_and_loop_policy(vec![frame], LoopPolicy::Infinite)
            .expect("animation is valid"),
    );
    let serialized_media_json = serde_json::to_value(&decoded_media).expect("animation serializes");
    assert_eq!(
        serde_json::from_value::<DecodedMedia>(serialized_media_json.clone())
            .expect("animation deserializes"),
        decoded_media
    );
    let empty_animation_error = serde_json::from_value::<DecodedMedia>(serde_json::json!({
        "Animation": {
            "frames": [],
            "loop_policy": "Infinite"
        }
    }))
    .expect_err("an animation with no frames is refused");
    assert_eq!(
        empty_animation_error.to_string(),
        "animation must contain at least one frame"
    );
    let zero_delay_denominator_error = serde_json::from_value::<DecodedMedia>(serde_json::json!({
        "Animation": {
            "frames": [{
                "decoded_image": {"pixel_width": 1, "pixel_height": 1, "rgba_bytes": [255, 0, 0, 255]},
                "frame_delay": {"numerator_ms": 1, "denominator_ms": 0}
            }],
            "loop_policy": "Infinite"
        }
    }))
    .expect_err("a zero delay denominator is refused");
    assert_eq!(
        zero_delay_denominator_error.to_string(),
        "animation delay denominator must be nonzero"
    );
}

#[test]
fn gif_without_loop_extension_is_one_playback_and_pixels_are_coalesced() {
    let decoded_media = decode_media(GraphicsProtocol::Kitty, &build_gif_with_frames())
        .expect("two-frame GIF decodes");
    let DecodedMedia::Animation(animation) = decoded_media else {
        panic!("two GIF frames must produce an animation");
    };

    assert_eq!(animation.get_loop_policy(), LoopPolicy::Finite(1));
    assert_eq!(animation.get_frame_count(), 2);
    assert_eq!(
        animation.list_frames()[0].get_frame_delay(),
        FrameDelay::from_millisecond_ratio(0, 1).expect("delay is valid")
    );
    assert_eq!(
        animation.list_frames()[0].get_decoded_image().rgba_bytes,
        vec![255, 0, 0, 255]
    );
    assert_eq!(
        animation.list_frames()[1].get_decoded_image().rgba_bytes,
        vec![0, 0, 255, 255]
    );
}

#[test]
fn gif_disposal_produces_complete_canvas_frames() {
    let decoded_media =
        decode_media(GraphicsProtocol::Kitty, &build_gif_with_disposal()).expect("GIF decodes");
    let DecodedMedia::Animation(animation) = decoded_media else {
        panic!("three GIF frames must produce an animation");
    };

    assert_eq!(get_animation_canvas_pixel_dimensions(&animation), (2, 1));
    assert_eq!(
        animation.list_frames()[1].get_decoded_image().rgba_bytes,
        vec![255, 0, 0, 255, 0, 0, 255, 255]
    );
    assert_eq!(
        animation.list_frames()[2].get_decoded_image().rgba_bytes,
        vec![0, 0, 255, 255, 0, 0, 0, 0]
    );
}

#[test]
fn gif_loop_extensions_use_total_playbacks_and_can_follow_a_frame() {
    let gif_bytes_with_loop_extension_after_first_frame =
        insert_gif_extension_before_second_image_frame(
            &build_gif_with_frames(),
            &build_gif_loop_extension(b"NETSCAPE2.0", 2),
        );
    let gif_bytes_with_infinite_loop_extension = insert_gif_extension_before_trailer(
        &build_gif_with_one_frame(),
        &build_gif_loop_extension(b"ANIMEXTS1.0", 0),
    );

    let DecodedMedia::Animation(animation) = decode_media(
        GraphicsProtocol::Kitty,
        &gif_bytes_with_loop_extension_after_first_frame,
    )
    .expect("GIF loop metadata decodes") else {
        panic!("loop metadata must produce an animation");
    };
    assert_eq!(animation.get_loop_policy(), LoopPolicy::Finite(3));

    let DecodedMedia::Animation(animation) = decode_media(
        GraphicsProtocol::Kitty,
        &gif_bytes_with_infinite_loop_extension,
    )
    .expect("infinite GIF decodes") else {
        panic!("infinite loop metadata must produce an animation");
    };
    assert_eq!(animation.get_loop_policy(), LoopPolicy::Infinite);
}

#[test]
fn decode_gif_rejects_malformed_loop_metadata_and_reads_trailing_metadata() {
    let mut malformed_gif_bytes = build_gif_with_one_frame();
    let malformed_extension = [
        0x21, 0xff, 11, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 2, 1, 0,
        0,
    ];
    malformed_gif_bytes.splice(19..19, malformed_extension);
    assert_eq!(
        decode_media(GraphicsProtocol::Kitty, &malformed_gif_bytes)
            .expect_err("short loop metadata must be rejected"),
        GraphicsError::DecodeFailure {
            protocol: GraphicsProtocol::Kitty
        }
    );

    let gif_bytes_with_trailing_loop_extension = insert_gif_extension_before_trailer(
        &build_gif_with_one_frame(),
        &build_gif_loop_extension(b"NETSCAPE2.0", 0),
    );
    let DecodedMedia::Animation(animation) = decode_media(
        GraphicsProtocol::Kitty,
        &gif_bytes_with_trailing_loop_extension,
    )
    .expect("trailing loop metadata decodes") else {
        panic!("trailing loop metadata must be retained");
    };
    assert_eq!(animation.get_loop_policy(), LoopPolicy::Infinite);
}

#[test]
fn apng_and_webp_preserve_format_loop_semantics() {
    let DecodedMedia::Animation(apng_animation) =
        decode_media(GraphicsProtocol::Iterm2, &build_animated_png(2)).expect("APNG decodes")
    else {
        panic!("APNG must produce an animation");
    };
    assert_eq!(apng_animation.get_loop_policy(), LoopPolicy::Finite(2));
    assert_eq!(apng_animation.get_frame_count(), 2);
    assert_eq!(
        apng_animation.list_frames()[0].get_frame_delay(),
        FrameDelay::from_millisecond_ratio(100, 1).expect("delay is valid")
    );

    let DecodedMedia::Animation(webp_animation) =
        decode_media(GraphicsProtocol::Iterm2, &build_animated_webp(0)).expect("WebP decodes")
    else {
        panic!("animated WebP must produce an animation");
    };
    assert_eq!(webp_animation.get_loop_policy(), LoopPolicy::Infinite);
    assert_eq!(webp_animation.get_frame_count(), 2);
}

#[test]
fn apng_and_webp_coalesce_offsets_alpha_and_disposal() {
    let DecodedMedia::Animation(apng_animation) =
        decode_media(GraphicsProtocol::Iterm2, &build_asymmetric_apng()).expect("APNG decodes")
    else {
        panic!("asymmetric APNG must produce an animation");
    };
    assert_eq!(
        get_animation_canvas_pixel_dimensions(&apng_animation),
        (2, 1)
    );
    assert_eq!(apng_animation.get_frame_count(), 2);
    assert_eq!(
        apng_animation.list_frames()[0]
            .get_decoded_image()
            .rgba_bytes,
        vec![255, 0, 0, 255, 0, 255, 0, 255]
    );
    assert_eq!(
        apng_animation.list_frames()[1]
            .get_decoded_image()
            .rgba_bytes,
        vec![0, 0, 0, 0, 0, 0, 255, 128]
    );

    let DecodedMedia::Animation(webp_animation) =
        decode_media(GraphicsProtocol::Iterm2, &build_asymmetric_webp()).expect("WebP decodes")
    else {
        panic!("asymmetric WebP must produce an animation");
    };
    assert_eq!(
        get_animation_canvas_pixel_dimensions(&webp_animation),
        (4, 1)
    );
    assert_eq!(webp_animation.get_frame_count(), 2);
    assert_eq!(
        webp_animation.list_frames()[0]
            .get_decoded_image()
            .rgba_bytes,
        vec![255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255, 255, 0, 255, 255,]
    );
    assert_eq!(
        webp_animation.list_frames()[1]
            .get_decoded_image()
            .rgba_bytes,
        vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 128, 255, 255, 255, 255,]
    );
}

#[test]
fn static_raster_uses_the_static_media_variant() {
    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");

    let decoded_media =
        decode_media(GraphicsProtocol::Iterm2, &png_bytes).expect("static PNG decodes");
    assert_eq!(
        decoded_media,
        DecodedMedia::Static(DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255],
        })
    );
}

#[test]
fn animation_frame_and_aggregate_limits_reject_invalid_values() {
    let frame_delay = FrameDelay::from_millisecond_ratio(1, 1).expect("delay is valid");
    let invalid_decoded_image = DecodedImage {
        pixel_width: 2,
        pixel_height: 1,
        rgba_bytes: vec![0, 0, 0, 255],
    };
    assert_eq!(
        AnimationFrame::from_image_and_delay(invalid_decoded_image, frame_delay)
            .expect_err("short RGBA data is invalid"),
        AnimationError::InvalidFrameImage
    );

    let frame = AnimationFrame::from_image_and_delay(build_one_pixel_image(255), frame_delay)
        .expect("frame is valid");
    let too_many_animation_frames = vec![frame; MAX_ANIMATION_FRAME_COUNT + 1];
    assert_eq!(
        DecodedAnimation::from_frames_and_loop_policy(
            too_many_animation_frames,
            LoopPolicy::Finite(1),
        )
        .expect_err("frame count limit is enforced"),
        AnimationError::TooManyFrames
    );

    let mut serialized_frames_json = String::from("[");
    for frame_index in 0..=MAX_ANIMATION_FRAME_COUNT {
        if frame_index > 0 {
            serialized_frames_json.push(',');
        }
        serialized_frames_json.push_str(
            r#"{"decoded_image":{"pixel_width":1,"pixel_height":1,"rgba_bytes":[255,0,0,255]},"frame_delay":{"numerator_ms":1,"denominator_ms":1}}"#,
        );
    }
    serialized_frames_json.push(']');
    let too_many_frames_media_json = serde_json::json!({
        "Animation": {
            "frames": serde_json::from_str::<serde_json::Value>(&serialized_frames_json)
                .expect("frames JSON"),
            "loop_policy": {"Finite": 1}
        }
    });
    let too_many_frames_error = serde_json::from_value::<DecodedMedia>(too_many_frames_media_json)
        .expect_err("one frame past the limit is refused");
    assert_eq!(
        too_many_frames_error.to_string(),
        "animation frame count exceeds the graphics limit"
    );
}
