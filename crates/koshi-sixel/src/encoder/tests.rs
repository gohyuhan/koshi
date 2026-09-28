//! Contract tests for bounded Sixel encoding.

use super::*;

impl SixelEncoder {
    /// Mark palette generation as failed with [`SixelEncodeError::PaletteMapping`].
    fn inject_generation_failure_for_test(&mut self) {
        self.is_generation_failed = true;
        self.generation_error = Some(SixelEncodeError::PaletteMapping);
    }
}

use std::sync::Arc;

fn build_sixel_test_image(
    image_pixel_width: u32,
    image_pixel_height: u32,
    rgba_bytes: Vec<u8>,
) -> Arc<koshi_image::DecodedImage> {
    Arc::new(koshi_image::DecodedImage {
        pixel_width: image_pixel_width,
        pixel_height: image_pixel_height,
        rgba_bytes,
    })
}

fn collect_sixel_output_bytes(mut sixel_encoder: SixelEncoder) -> Vec<u8> {
    let mut sixel_output_bytes = Vec::new();
    while let Some(output_chunk_bytes) = sixel_encoder
        .take_next_chunk(usize::MAX)
        .expect("chunk size is valid")
    {
        sixel_output_bytes.extend_from_slice(output_chunk_bytes);
    }
    sixel_output_bytes
}

fn encode_sixel_image(
    image_pixel_width: u32,
    image_pixel_height: u32,
    rgba_bytes: &[u8],
    background_rgb: [u8; 3],
) -> Vec<u8> {
    collect_sixel_output_bytes(
        SixelEncoder::from_image(
            build_sixel_test_image(image_pixel_width, image_pixel_height, rgba_bytes.to_vec()),
            background_rgb,
        )
        .expect("image encodes"),
    )
}

#[test]
fn default_sixel_options_use_the_largest_supported_palette() {
    assert_eq!(
        SixelEncodeOptions::default().maximum_palette_color_count,
        MAX_PALETTE_COLOR_COUNT
    );
}

#[test]
fn sixel_encoder_matches_one_pixel_protocol_fixture() {
    let sixel_output_bytes = encode_sixel_image(1, 1, &[255, 0, 0, 255], [0, 0, 0]);

    assert_eq!(
        sixel_output_bytes,
        b"\x1bP7;1q\"1;1;1;1#0;2;100;0;0#0@\x1b\\"
    );
}

#[test]
fn sixel_encoder_blends_partial_alpha_and_keeps_zero_alpha_transparent() {
    let partial_alpha_sixel_output = encode_sixel_image(1, 1, &[255, 0, 0, 128], [0, 0, 255]);
    assert_eq!(
        partial_alpha_sixel_output,
        b"\x1bP7;1q\"1;1;1;1#0;2;50;0;50#0@\x1b\\"
    );

    let partial_alpha_white_sixel_output =
        encode_sixel_image(1, 1, &[255, 255, 255, 128], [255, 255, 255]);
    assert_eq!(
        partial_alpha_white_sixel_output,
        b"\x1bP7;1q\"1;1;1;1#0;2;100;100;100#0@\x1b\\"
    );

    let zero_alpha_sixel_output = encode_sixel_image(1, 1, &[255, 0, 0, 0], [1, 2, 3]);
    assert_eq!(zero_alpha_sixel_output, b"\x1bP7;1q\"1;1;1;1?\x1b\\");
}

#[test]
fn sixel_encoder_emits_exact_color_bands_and_rle_boundaries() {
    let rgba_bytes = [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255];
    let sixel_output_bytes = encode_sixel_image(3, 1, &rgba_bytes, [0, 0, 0]);
    assert_eq!(
        sixel_output_bytes,
        b"\x1bP7;1q\"1;1;3;1#0;2;100;0;0#1;2;0;0;100#0!2@$#1!2?@\x1b\\"
    );

    let repeated_red_sixel_output =
        encode_sixel_image(10, 1, &[255, 0, 0, 255].repeat(10), [0, 0, 0]);
    assert_eq!(
        repeated_red_sixel_output,
        b"\x1bP7;1q\"1;1;10;1#0;2;100;0;0#0!10@\x1b\\"
    );

    let encoded_sixel_bytes = encode_sixel_image(1, 7, &[255, 0, 0, 255].repeat(7), [0, 0, 0]);
    assert_eq!(
        encoded_sixel_bytes,
        b"\x1bP7;1q\"1;1;1;7#0;2;100;0;0#0~-#0@\x1b\\"
    );
}

#[test]
fn sixel_encoder_quantizes_to_a_bounded_deterministic_palette() {
    let rgba_bytes = vec![0, 0, 0, 255, 64, 0, 0, 255, 128, 0, 0, 255, 192, 0, 0, 255];
    let encode_options = SixelEncodeOptions::with_maximum_palette_color_count(2);
    let first_sixel_output_bytes = collect_sixel_output_bytes(
        SixelEncoder::with_options(
            build_sixel_test_image(4, 1, rgba_bytes.clone()),
            [0, 0, 0],
            encode_options,
        )
        .expect("quantized colors encode"),
    );
    let second_sixel_output_bytes = collect_sixel_output_bytes(
        SixelEncoder::with_options(
            build_sixel_test_image(4, 1, rgba_bytes),
            [0, 0, 0],
            encode_options,
        )
        .expect("quantized colors encode"),
    );

    assert_eq!(first_sixel_output_bytes, second_sixel_output_bytes);
    assert_eq!(
        first_sixel_output_bytes,
        b"\x1bP7;1q\"1;1;4;1#0;2;13;0;0#1;2;63;0;0#0!2@$#1!2?!2@\x1b\\"
    );
}

#[test]
fn sixel_encoder_rejects_invalid_image_and_palette_options() {
    let sixel_encode_error = SixelEncoder::from_image(
        build_sixel_test_image(2, 1, vec![255, 0, 0, 255]),
        [0, 0, 0],
    )
    .expect_err("short RGBA is rejected");
    match sixel_encode_error {
        SixelEncodeError::RgbaLengthMismatch {
            expected_rgba_byte_count,
            actual_rgba_byte_count,
        } => {
            assert_eq!(expected_rgba_byte_count, 8);
            assert_eq!(actual_rgba_byte_count, 4);
        }
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }

    let sixel_encode_error =
        SixelEncoder::from_image(build_sixel_test_image(0, 1, Vec::new()), [0, 0, 0])
            .expect_err("zero dimensions are rejected");
    match sixel_encode_error {
        SixelEncodeError::InvalidDimensions {
            pixel_width,
            pixel_height,
        } => {
            assert_eq!(pixel_width, 0);
            assert_eq!(pixel_height, 1);
        }
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }

    let sixel_encode_error =
        SixelEncoder::from_image(build_sixel_test_image(4097, 4097, Vec::new()), [0, 0, 0])
            .expect_err("oversized dimensions are rejected");
    match sixel_encode_error {
        SixelEncodeError::InvalidDimensions {
            pixel_width,
            pixel_height,
        } => {
            assert_eq!(pixel_width, 4097);
            assert_eq!(pixel_height, 4097);
        }
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }

    let sixel_encode_error = SixelEncoder::with_options(
        build_sixel_test_image(1, 1, vec![0, 0, 0, 255]),
        [0, 0, 0],
        SixelEncodeOptions::with_maximum_palette_color_count(1),
    )
    .expect_err("too-small palette is rejected");
    match sixel_encode_error {
        SixelEncodeError::InvalidPaletteSize {
            requested_palette_color_count,
        } => assert_eq!(requested_palette_color_count, 1),
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }
}

#[test]
fn sixel_encoder_emits_bounded_chunks_and_preserves_all_bytes() {
    let expected_sixel_output_bytes =
        encode_sixel_image(2, 1, [255, 0, 0, 255].repeat(2).as_slice(), [0, 0, 0]);
    let mut sixel_encoder = SixelEncoder::from_image(
        build_sixel_test_image(2, 1, [255, 0, 0, 255].repeat(2)),
        [0, 0, 0],
    )
    .expect("image encodes");
    let mut actual_sixel_output_bytes = Vec::new();
    while let Some(output_chunk_bytes) = sixel_encoder
        .take_next_chunk(1)
        .expect("chunk size is valid")
    {
        assert_eq!(output_chunk_bytes.len(), 1);
        actual_sixel_output_bytes.extend_from_slice(output_chunk_bytes);
    }

    assert_eq!(actual_sixel_output_bytes, expected_sixel_output_bytes);
    assert_eq!(
        sixel_encoder
            .take_next_chunk(usize::MAX)
            .expect("finished encoder"),
        None
    );

    let mut sixel_encoder =
        SixelEncoder::from_image(build_sixel_test_image(1, 1, vec![0, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let output_chunk_bytes = sixel_encoder
        .take_next_chunk(usize::MAX)
        .expect("large request is valid")
        .expect("header is available");
    assert!(output_chunk_bytes.len() <= MAX_SIXEL_CHUNK_BYTE_COUNT);
}

#[test]
fn sixel_encoder_keeps_generation_failure_after_header_output() {
    let mut sixel_encoder = SixelEncoder::from_image(
        build_sixel_test_image(1, 1, vec![255, 0, 0, 255]),
        [0, 0, 0],
    )
    .expect("image encodes");
    let first_output_chunk_bytes = sixel_encoder
        .take_next_chunk(1)
        .expect("first chunk is valid")
        .expect("header is available");
    assert_eq!(first_output_chunk_bytes, b"\x1b");
    sixel_encoder.inject_generation_failure_for_test();

    loop {
        match sixel_encoder.take_next_chunk(MAX_SIXEL_CHUNK_BYTE_COUNT) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("failed encoder reported completion"),
            Err(sixel_encode_error) => {
                assert_sixel_palette_mapping_error(sixel_encode_error);
                break;
            }
        }
    }
    assert_sixel_encoder_failed_error(
        sixel_encoder
            .take_next_chunk(1)
            .expect_err("failed encoder remains failed"),
    );
}

#[test]
fn sixel_encoder_rejects_zero_chunk_size() {
    let mut sixel_encoder =
        SixelEncoder::from_image(build_sixel_test_image(1, 1, vec![0, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let sixel_encode_error = sixel_encoder
        .take_next_chunk(0)
        .expect_err("zero chunk is rejected");
    match sixel_encode_error {
        SixelEncodeError::ZeroChunkSize => {}
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }
}

fn assert_sixel_palette_mapping_error(sixel_encode_error: SixelEncodeError) {
    match sixel_encode_error {
        SixelEncodeError::PaletteMapping => {}
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }
}

fn assert_sixel_encoder_failed_error(sixel_encode_error: SixelEncodeError) {
    match sixel_encode_error {
        SixelEncodeError::EncoderFailed => {}
        unexpected_sixel_encode_error => {
            panic!("unexpected error: {unexpected_sixel_encode_error:?}")
        }
    }
}
