//! Tests for validated animated raster values and media decoding.

use super::*;

use image::ImageEncoder;
use std::sync::Arc;

fn one_pixel_image(red: u8) -> DecodedImage {
    DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![red, 0, 0, 255],
    }
}

fn gif_frame(pixel: u8) -> Vec<u8> {
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
        if pixel == 0 { 0x44 } else { 0x4c },
        1,
        0,
    ]
}

fn gif_with_frames() -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]);
    bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    bytes.extend_from_slice(&gif_frame(0));
    bytes.extend_from_slice(&gif_frame(1));
    bytes.push(0x3b);
    bytes
}

fn gif_with_one_frame() -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]);
    bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    bytes.extend_from_slice(&gif_frame(0));
    bytes.push(0x3b);
    bytes
}

fn gif_with_disposal() -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&[2, 0, 1, 0, 0x80, 0, 0]);
    bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    bytes.extend_from_slice(&[0x2c, 0, 0, 0, 0, 2, 0, 1, 0, 0, 2, 2, 0x04, 0x05, 0]);
    bytes.extend_from_slice(&[0x21, 0xf9, 4, 0x08, 0, 0, 0, 0]);
    bytes.extend_from_slice(&[0x2c, 1, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4c, 0x01, 0]);
    bytes.extend_from_slice(&[0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4c, 0x01, 0]);
    bytes.push(0x3b);
    bytes
}

fn loop_extension(identifier: &[u8; 11], repeat_count: u16) -> Vec<u8> {
    let mut bytes = vec![0x21, 0xff, 11];
    bytes.extend_from_slice(identifier);
    bytes.extend_from_slice(&[3, 1]);
    bytes.extend_from_slice(&repeat_count.to_le_bytes());
    bytes.push(0);
    bytes
}

fn insert_before(bytes: &[u8], marker: u8, insertion: &[u8]) -> Vec<u8> {
    let index = bytes
        .iter()
        .rposition(|byte| *byte == marker)
        .expect("fixture marker exists");
    let mut result = Vec::with_capacity(bytes.len() + insertion.len());
    result.extend_from_slice(&bytes[..index]);
    result.extend_from_slice(insertion);
    result.extend_from_slice(&bytes[index..]);
    result
}

fn insert_before_second_frame(bytes: &[u8], insertion: &[u8]) -> Vec<u8> {
    let first = bytes
        .iter()
        .position(|byte| *byte == 0x2c)
        .expect("first frame marker exists");
    let second = bytes[first + 1..]
        .iter()
        .position(|byte| *byte == 0x2c)
        .map(|offset| first + 1 + offset)
        .expect("second frame marker exists");
    let mut result = Vec::with_capacity(bytes.len() + insertion.len());
    result.extend_from_slice(&bytes[..second]);
    result.extend_from_slice(insertion);
    result.extend_from_slice(&bytes[second..]);
    result
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

fn png_frame_control(sequence: u32) -> [u8; 26] {
    let mut data = [0; 26];
    data[..4].copy_from_slice(&sequence.to_be_bytes());
    data[4..8].copy_from_slice(&1u32.to_be_bytes());
    data[8..12].copy_from_slice(&1u32.to_be_bytes());
    data[20..22].copy_from_slice(&1u16.to_be_bytes());
    data[22..24].copy_from_slice(&10u16.to_be_bytes());
    data
}

fn animated_png(loop_count: u32) -> Vec<u8> {
    let mut source = Vec::new();
    image::codecs::png::PngEncoder::new(&mut source)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");

    let mut bytes = source[..8].to_vec();
    let mut at = 8;
    let mut inserted_animation = false;
    let mut first_frame_data = Vec::new();
    while at < source.len() {
        let length = u32::from_be_bytes(
            source[at..at + 4]
                .try_into()
                .expect("PNG length has four bytes"),
        );
        let end = at + 12 + usize::try_from(length).expect("PNG length fits");
        let kind: &[u8; 4] = source[at + 4..at + 8]
            .try_into()
            .expect("PNG chunk type has four bytes");
        let data = &source[at + 8..end - 4];
        match kind {
            b"IDAT" if !inserted_animation => {
                let mut actl = [0; 8];
                actl[..4].copy_from_slice(&2u32.to_be_bytes());
                actl[4..].copy_from_slice(&loop_count.to_be_bytes());
                bytes.extend_from_slice(&png_chunk(b"acTL", &actl));
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

fn png_idat_data(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut source = Vec::new();
    image::codecs::png::PngEncoder::new(&mut source)
        .write_image(rgba, width, height, image::ColorType::Rgba8.into())
        .expect("PNG frame encodes");

    let mut data = Vec::new();
    let mut at = 8;
    while at < source.len() {
        let length = u32::from_be_bytes(
            source[at..at + 4]
                .try_into()
                .expect("PNG length has four bytes"),
        ) as usize;
        let end = at + 12 + length;
        if &source[at + 4..at + 8] == b"IDAT" {
            data.extend_from_slice(&source[at + 8..end - 4]);
        }
        at = end;
    }
    data
}

#[allow(clippy::too_many_arguments)]
fn apng_frame_control(
    sequence: u32,
    width: u32,
    height: u32,
    left: u32,
    top: u32,
    delay_numerator: u16,
    delay_denominator: u16,
    dispose: u8,
    blend: u8,
) -> [u8; 26] {
    let mut data = [0; 26];
    data[..4].copy_from_slice(&sequence.to_be_bytes());
    data[4..8].copy_from_slice(&width.to_be_bytes());
    data[8..12].copy_from_slice(&height.to_be_bytes());
    data[12..16].copy_from_slice(&left.to_be_bytes());
    data[16..20].copy_from_slice(&top.to_be_bytes());
    data[20..22].copy_from_slice(&delay_numerator.to_be_bytes());
    data[22..24].copy_from_slice(&delay_denominator.to_be_bytes());
    data[24] = dispose;
    data[25] = blend;
    data
}

fn asymmetric_apng() -> Vec<u8> {
    let first_data = png_idat_data(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1);
    let second_data = png_idat_data(&[0, 0, 255, 128], 1, 1);
    let mut ihdr = [0; 13];
    ihdr[..4].copy_from_slice(&2u32.to_be_bytes());
    ihdr[4..8].copy_from_slice(&1u32.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;

    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    bytes.extend_from_slice(&png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
    bytes.extend_from_slice(&png_chunk(
        b"fcTL",
        &apng_frame_control(0, 2, 1, 0, 0, 1, 10, 1, 0),
    ));
    bytes.extend_from_slice(&png_chunk(b"IDAT", &first_data));
    bytes.extend_from_slice(&png_chunk(
        b"fcTL",
        &apng_frame_control(1, 1, 1, 1, 0, 2, 10, 0, 0),
    ));
    let mut frame_data = Vec::with_capacity(4 + second_data.len());
    frame_data.extend_from_slice(&2u32.to_be_bytes());
    frame_data.extend_from_slice(&second_data);
    bytes.extend_from_slice(&png_chunk(b"fdAT", &frame_data));
    bytes.extend_from_slice(&png_chunk(b"IEND", &[]));
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

fn animated_webp(loop_count: u16) -> Vec<u8> {
    let mut source = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut source)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel WebP encodes");
    let mut at = 12;
    let mut frame = Vec::new();
    while at < source.len() {
        let length = usize::try_from(u32::from_le_bytes(
            source[at + 4..at + 8]
                .try_into()
                .expect("WebP chunk length has four bytes"),
        ))
        .expect("WebP length fits");
        if &source[at..at + 4] == b"VP8L" {
            frame.extend_from_slice(&source[at + 8..at + 8 + length]);
        }
        at += 8 + length + length % 2;
    }

    let vp8x = [0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut anim = [0; 6];
    anim[4..].copy_from_slice(&loop_count.to_le_bytes());
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

fn webp_vp8l_data(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut source = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut source)
        .write_image(rgba, width, height, image::ColorType::Rgba8.into())
        .expect("WebP frame encodes");
    let mut at = 12;
    while at < source.len() {
        let length = usize::try_from(u32::from_le_bytes(
            source[at + 4..at + 8]
                .try_into()
                .expect("WebP chunk length has four bytes"),
        ))
        .expect("WebP length fits");
        if &source[at..at + 4] == b"VP8L" {
            return source[at + 8..at + 8 + length].to_vec();
        }
        at += 8 + length + length % 2;
    }
    panic!("lossless WebP has a VP8L chunk");
}

fn webp_frame_header(
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    duration_ms: u32,
    dispose: bool,
    blend: bool,
) -> Vec<u8> {
    let mut data = [0; 16];
    data[..3].copy_from_slice(&(left / 2).to_le_bytes()[..3]);
    data[3..6].copy_from_slice(&(top / 2).to_le_bytes()[..3]);
    data[6..9].copy_from_slice(&(width - 1).to_le_bytes()[..3]);
    data[9..12].copy_from_slice(&(height - 1).to_le_bytes()[..3]);
    data[12..15].copy_from_slice(&duration_ms.to_le_bytes()[..3]);
    data[15] = u8::from(dispose) | (u8::from(!blend) << 1);
    data.to_vec()
}

fn asymmetric_webp() -> Vec<u8> {
    let first = webp_vp8l_data(
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255, 255, 0, 255, 255,
        ],
        4,
        1,
    );
    let second = webp_vp8l_data(&[0, 0, 255, 128, 255, 255, 255, 255], 2, 1);
    let vp8x = [0x12, 0, 0, 0, 3, 0, 0, 0, 0, 0];
    let anim = [0; 6];
    let mut first_frame = webp_frame_header(0, 0, 4, 1, 10, true, false);
    first_frame.extend_from_slice(&webp_chunk(b"VP8L", &first));
    let mut second_frame = webp_frame_header(2, 0, 2, 1, 20, false, false);
    second_frame.extend_from_slice(&webp_chunk(b"VP8L", &second));
    let chunks = [
        webp_chunk(b"VP8X", &vp8x),
        webp_chunk(b"ANIM", &anim),
        webp_chunk(b"ANMF", &first_frame),
        webp_chunk(b"ANMF", &second_frame),
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

#[test]
fn frame_delay_and_loop_policy_reject_zero_values() {
    assert_eq!(
        FrameDelay::new(1, 0).expect_err("zero delay denominator is invalid"),
        AnimationError::InvalidDelayDenominator
    );
    assert_eq!(
        LoopPolicy::finite(0).expect_err("zero finite playback count is invalid"),
        AnimationError::InvalidPlaybackCount
    );
    let frame = AnimationFrame::new(
        one_pixel_image(255),
        FrameDelay::new(1, 1).expect("delay is valid"),
    )
    .expect("frame is valid");
    assert_eq!(
        DecodedAnimation::new(vec![frame], LoopPolicy::Finite(0))
            .expect_err("zero finite playback count is invalid"),
        AnimationError::InvalidPlaybackCount
    );
    assert_eq!(
        FrameDelay::new(0, 1)
            .expect("zero delay is data")
            .numerator_ms(),
        0
    );
}

#[test]
fn animation_constructor_validates_canvas_and_exposes_private_state() {
    let delay = FrameDelay::new(10, 1).expect("delay is valid");
    let first_image = Arc::new(one_pixel_image(255));
    let first = AnimationFrame::new(Arc::clone(&first_image), delay).expect("first frame is valid");
    let second = AnimationFrame::new(one_pixel_image(0), delay).expect("second frame is valid");
    let animation = DecodedAnimation::new(
        vec![first.clone(), second.clone()],
        LoopPolicy::finite(3).expect("finite loop count is valid"),
    )
    .expect("animation is valid");

    assert_eq!(animation.frame_count(), 2);
    assert_eq!(animation.dimensions(), (1, 1));
    assert_eq!(animation.frames()[0], first);
    assert_eq!(animation.frames()[1].image().rgba, vec![0, 0, 0, 255]);
    assert_eq!(animation.loop_policy(), LoopPolicy::Finite(3));
    let first_shared = first.image_shared();
    let clone_shared = first.clone().image_shared();
    assert!(Arc::ptr_eq(&first_image, &first_shared));
    assert!(Arc::ptr_eq(&first_shared, &clone_shared));
}

#[test]
fn gapless_frames_are_distinct_from_zero_delay_media_frames() {
    let image = one_pixel_image(255);
    let gapless = AnimationFrame::new_gapless(image).expect("the gapless frame is valid");
    assert!(gapless.gapless());
    assert_eq!(
        gapless.delay(),
        FrameDelay::new(0, 1).expect("zero delay is valid")
    );

    let media = AnimationFrame::new(
        one_pixel_image(0),
        FrameDelay::new(0, 1).expect("zero delay is valid"),
    )
    .expect("the media frame is valid");
    assert!(!media.gapless());
}

#[test]
fn animation_serde_round_trip_and_validation_are_bounded() {
    let delay = FrameDelay::new(7, 10).expect("delay is valid");
    let frame = AnimationFrame::new(one_pixel_image(255), delay).expect("frame is valid");
    let source = DecodedMedia::Animation(
        DecodedAnimation::new(vec![frame], LoopPolicy::Infinite).expect("animation is valid"),
    );
    let value = serde_json::to_value(&source).expect("animation serializes");
    assert_eq!(
        serde_json::from_value::<DecodedMedia>(value.clone()).expect("animation deserializes"),
        source
    );
    assert!(serde_json::from_value::<DecodedMedia>(serde_json::json!({
        "Animation": {
            "frames": [],
            "loop_policy": "Infinite"
        }
    }))
    .is_err());
    assert!(serde_json::from_value::<DecodedMedia>(serde_json::json!({
        "Animation": {
            "frames": [{
                "image": {"width": 1, "height": 1, "rgba": [255, 0, 0, 255]},
                "delay": {"numerator_ms": 1, "denominator_ms": 0}
            }],
            "loop_policy": "Infinite"
        }
    }))
    .is_err());
}

#[test]
fn gif_without_loop_extension_is_one_playback_and_pixels_are_coalesced() {
    let media =
        decode_media(GraphicsProtocol::Kitty, &gif_with_frames()).expect("two-frame GIF decodes");
    let DecodedMedia::Animation(animation) = media else {
        panic!("two GIF frames must produce an animation");
    };

    assert_eq!(animation.loop_policy(), LoopPolicy::Finite(1));
    assert_eq!(animation.frame_count(), 2);
    assert_eq!(
        animation.frames()[0].delay(),
        FrameDelay::new(0, 1).expect("delay is valid")
    );
    assert_eq!(animation.frames()[0].image().rgba, vec![255, 0, 0, 255]);
    assert_eq!(animation.frames()[1].image().rgba, vec![0, 0, 255, 255]);
}

#[test]
fn gif_disposal_produces_complete_canvas_frames() {
    let media = decode_media(GraphicsProtocol::Kitty, &gif_with_disposal()).expect("GIF decodes");
    let DecodedMedia::Animation(animation) = media else {
        panic!("three GIF frames must produce an animation");
    };

    assert_eq!(animation.dimensions(), (2, 1));
    assert_eq!(
        animation.frames()[1].image().rgba,
        vec![255, 0, 0, 255, 0, 0, 255, 255]
    );
    assert_eq!(
        animation.frames()[2].image().rgba,
        vec![0, 0, 255, 255, 0, 0, 0, 0]
    );
}

#[test]
fn gif_loop_extensions_use_total_playbacks_and_can_follow_a_frame() {
    let after_frame =
        insert_before_second_frame(&gif_with_frames(), &loop_extension(b"NETSCAPE2.0", 2));
    let infinite = insert_before(
        &gif_with_one_frame(),
        0x3b,
        &loop_extension(b"ANIMEXTS1.0", 0),
    );

    let DecodedMedia::Animation(animation) =
        decode_media(GraphicsProtocol::Kitty, &after_frame).expect("GIF loop metadata decodes")
    else {
        panic!("loop metadata must produce an animation");
    };
    assert_eq!(animation.loop_policy(), LoopPolicy::Finite(3));

    let DecodedMedia::Animation(animation) =
        decode_media(GraphicsProtocol::Kitty, &infinite).expect("infinite GIF decodes")
    else {
        panic!("infinite loop metadata must produce an animation");
    };
    assert_eq!(animation.loop_policy(), LoopPolicy::Infinite);
}

#[test]
fn malformed_gif_loop_metadata_is_typed_and_trailing_metadata_is_seen() {
    let mut malformed = gif_with_one_frame();
    let malformed_extension = [
        0x21, 0xff, 11, b'N', b'E', b'T', b'S', b'C', b'A', b'P', b'E', b'2', b'.', b'0', 2, 1, 0,
        0,
    ];
    malformed.splice(19..19, malformed_extension);
    assert_eq!(
        decode_media(GraphicsProtocol::Kitty, &malformed)
            .expect_err("short loop metadata must be rejected"),
        GraphicsError::DecodeFailure {
            protocol: GraphicsProtocol::Kitty
        }
    );

    let trailing = insert_before(
        &gif_with_one_frame(),
        0x3b,
        &loop_extension(b"NETSCAPE2.0", 0),
    );
    let DecodedMedia::Animation(animation) =
        decode_media(GraphicsProtocol::Kitty, &trailing).expect("trailing loop metadata decodes")
    else {
        panic!("trailing loop metadata must be retained");
    };
    assert_eq!(animation.loop_policy(), LoopPolicy::Infinite);
}

#[test]
fn apng_and_webp_preserve_format_loop_semantics() {
    let DecodedMedia::Animation(apng) =
        decode_media(GraphicsProtocol::Iterm2, &animated_png(2)).expect("APNG decodes")
    else {
        panic!("APNG must produce an animation");
    };
    assert_eq!(apng.loop_policy(), LoopPolicy::Finite(2));
    assert_eq!(apng.frame_count(), 2);
    assert_eq!(
        apng.frames()[0].delay(),
        FrameDelay::new(100, 1).expect("delay is valid")
    );

    let DecodedMedia::Animation(webp) =
        decode_media(GraphicsProtocol::Iterm2, &animated_webp(0)).expect("WebP decodes")
    else {
        panic!("animated WebP must produce an animation");
    };
    assert_eq!(webp.loop_policy(), LoopPolicy::Infinite);
    assert_eq!(webp.frame_count(), 2);
}

#[test]
fn apng_and_webp_coalesce_offsets_alpha_and_disposal() {
    let DecodedMedia::Animation(apng) =
        decode_media(GraphicsProtocol::Iterm2, &asymmetric_apng()).expect("APNG decodes")
    else {
        panic!("asymmetric APNG must produce an animation");
    };
    assert_eq!(apng.dimensions(), (2, 1));
    assert_eq!(apng.frame_count(), 2);
    assert_eq!(
        apng.frames()[0].image().rgba,
        vec![255, 0, 0, 255, 0, 255, 0, 255]
    );
    assert_eq!(
        apng.frames()[1].image().rgba,
        vec![0, 0, 0, 0, 0, 0, 255, 128]
    );

    let DecodedMedia::Animation(webp) =
        decode_media(GraphicsProtocol::Iterm2, &asymmetric_webp()).expect("WebP decodes")
    else {
        panic!("asymmetric WebP must produce an animation");
    };
    assert_eq!(webp.dimensions(), (4, 1));
    assert_eq!(webp.frame_count(), 2);
    assert_eq!(
        webp.frames()[0].image().rgba,
        vec![255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255, 255, 0, 255, 255,]
    );
    assert_eq!(
        webp.frames()[1].image().rgba,
        vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 128, 255, 255, 255, 255,]
    );
}

#[test]
fn static_raster_uses_the_static_media_variant() {
    let mut data = Vec::new();
    image::codecs::png::PngEncoder::new(&mut data)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");

    let media = decode_media(GraphicsProtocol::Iterm2, &data).expect("static PNG decodes");
    assert_eq!(
        media,
        DecodedMedia::Static(DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
        })
    );
}

#[test]
fn animation_frame_and_aggregate_limits_reject_invalid_values() {
    let delay = FrameDelay::new(1, 1).expect("delay is valid");
    let invalid = DecodedImage {
        width: 2,
        height: 1,
        rgba: vec![0, 0, 0, 255],
    };
    assert_eq!(
        AnimationFrame::new(invalid, delay).expect_err("short RGBA data is invalid"),
        AnimationError::InvalidFrameImage
    );

    let frame = AnimationFrame::new(one_pixel_image(255), delay).expect("frame is valid");
    let too_many = vec![frame; MAX_ANIMATION_FRAMES + 1];
    assert_eq!(
        DecodedAnimation::new(too_many, LoopPolicy::Finite(1))
            .expect_err("frame count limit is enforced"),
        AnimationError::TooManyFrames
    );

    let mut frames = String::from("[");
    for index in 0..=MAX_ANIMATION_FRAMES {
        if index > 0 {
            frames.push(',');
        }
        frames.push_str(
            r#"{"image":{"width":1,"height":1,"rgba":[255,0,0,255]},"delay":{"numerator_ms":1,"denominator_ms":1}}"#,
        );
    }
    frames.push(']');
    let value = serde_json::json!({
        "Animation": {
            "frames": serde_json::from_str::<serde_json::Value>(&frames).expect("frames JSON"),
            "loop_policy": {"Finite": 1}
        }
    });
    assert!(serde_json::from_value::<DecodedMedia>(value).is_err());
}
