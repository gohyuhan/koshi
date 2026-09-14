//! Contract tests for bounded Sixel encoding.

use super::*;

use std::io::{self, Write};
use std::sync::Arc;

fn build_test_image(
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

fn collect_sixel_output(mut sixel_encoder: SixelEncoder) -> Vec<u8> {
    let mut output_bytes = Vec::new();
    while let Some(output_chunk) = sixel_encoder
        .take_next_chunk(usize::MAX)
        .expect("chunk size is valid")
    {
        output_bytes.extend_from_slice(output_chunk);
    }
    output_bytes
}

fn encode_sixel_image(
    image_pixel_width: u32,
    image_pixel_height: u32,
    rgba_bytes: &[u8],
    background_color: [u8; 3],
) -> Vec<u8> {
    collect_sixel_output(
        SixelEncoder::from_image(
            build_test_image(image_pixel_width, image_pixel_height, rgba_bytes.to_vec()),
            background_color,
        )
        .expect("image encodes"),
    )
}

#[test]
fn default_options_use_the_largest_supported_palette() {
    assert_eq!(
        SixelEncodeOptions::default().maximum_palette_color_count,
        MAX_PALETTE_COLOR_COUNT
    );
}

#[test]
fn matches_one_pixel_protocol_fixture() {
    let output_bytes = encode_sixel_image(1, 1, &[255, 0, 0, 255], [0, 0, 0]);

    assert_eq!(output_bytes, b"\x1bP7;1q\"1;1;1;1#0;2;100;0;0#0@\x1b\\");
}

#[test]
fn blends_partial_alpha_and_keeps_zero_alpha_transparent() {
    let blended = encode_sixel_image(1, 1, &[255, 0, 0, 128], [0, 0, 255]);
    assert_eq!(blended, b"\x1bP7;1q\"1;1;1;1#0;2;50;0;50#0@\x1b\\");

    let white = encode_sixel_image(1, 1, &[255, 255, 255, 128], [255, 255, 255]);
    assert_eq!(white, b"\x1bP7;1q\"1;1;1;1#0;2;100;100;100#0@\x1b\\");

    let transparent = encode_sixel_image(1, 1, &[255, 0, 0, 0], [1, 2, 3]);
    assert_eq!(transparent, b"\x1bP7;1q\"1;1;1;1?\x1b\\");
}

#[test]
fn emits_exact_color_bands_and_rle_boundaries() {
    let rgba_bytes = [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255];
    let output_bytes = encode_sixel_image(3, 1, &rgba_bytes, [0, 0, 0]);
    assert_eq!(
        output_bytes,
        b"\x1bP7;1q\"1;1;3;1#0;2;100;0;0#1;2;0;0;100#0!2@$#1!2?@\x1b\\"
    );

    let repeated = encode_sixel_image(10, 1, &[255, 0, 0, 255].repeat(10), [0, 0, 0]);
    assert_eq!(repeated, b"\x1bP7;1q\"1;1;10;1#0;2;100;0;0#0!10@\x1b\\");

    let encoded_sixel_bytes = encode_sixel_image(1, 7, &[255, 0, 0, 255].repeat(7), [0, 0, 0]);
    assert_eq!(
        encoded_sixel_bytes,
        b"\x1bP7;1q\"1;1;1;7#0;2;100;0;0#0~-#0@\x1b\\"
    );
}

#[test]
fn quantized_palette_is_bounded_deterministic_and_exact() {
    let rgba_bytes = vec![0, 0, 0, 255, 64, 0, 0, 255, 128, 0, 0, 255, 192, 0, 0, 255];
    let encode_options = SixelEncodeOptions::with_maximum_palette_color_count(2);
    let first_output = collect_sixel_output(
        SixelEncoder::with_options(
            build_test_image(4, 1, rgba_bytes.clone()),
            [0, 0, 0],
            encode_options,
        )
        .expect("quantized colors encode"),
    );
    let second_output = collect_sixel_output(
        SixelEncoder::with_options(
            build_test_image(4, 1, rgba_bytes),
            [0, 0, 0],
            encode_options,
        )
        .expect("quantized colors encode"),
    );

    assert_eq!(first_output, second_output);
    assert_eq!(
        first_output,
        b"\x1bP7;1q\"1;1;4;1#0;2;13;0;0#1;2;63;0;0#0!2@$#1!2?!2@\x1b\\"
    );
}

#[test]
fn rejects_input_before_preparation() {
    let encode_error =
        SixelEncoder::from_image(build_test_image(2, 1, vec![255, 0, 0, 255]), [0, 0, 0])
            .expect_err("short RGBA is rejected");
    match encode_error {
        SixelEncodeError::RgbaLengthMismatch {
            expected_rgba_byte_count,
            actual_rgba_byte_count,
        } => {
            assert_eq!(expected_rgba_byte_count, 8);
            assert_eq!(actual_rgba_byte_count, 4);
        }
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }

    let encode_error = SixelEncoder::from_image(build_test_image(0, 1, Vec::new()), [0, 0, 0])
        .expect_err("zero dimensions are rejected");
    match encode_error {
        SixelEncodeError::InvalidDimensions {
            pixel_width,
            pixel_height,
        } => {
            assert_eq!(pixel_width, 0);
            assert_eq!(pixel_height, 1);
        }
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }

    let encode_error =
        SixelEncoder::from_image(build_test_image(4097, 4097, Vec::new()), [0, 0, 0])
            .expect_err("oversized dimensions are rejected");
    match encode_error {
        SixelEncodeError::InvalidDimensions {
            pixel_width,
            pixel_height,
        } => {
            assert_eq!(pixel_width, 4097);
            assert_eq!(pixel_height, 4097);
        }
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }

    let encode_error = SixelEncoder::with_options(
        build_test_image(1, 1, vec![0, 0, 0, 255]),
        [0, 0, 0],
        SixelEncodeOptions::with_maximum_palette_color_count(1),
    )
    .expect_err("too-small palette is rejected");
    match encode_error {
        SixelEncodeError::InvalidPaletteSize {
            requested_palette_color_count,
        } => assert_eq!(requested_palette_color_count, 1),
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
}

#[test]
fn emits_bounded_chunks_and_preserves_all_bytes() {
    let expected_output =
        encode_sixel_image(2, 1, [255, 0, 0, 255].repeat(2).as_slice(), [0, 0, 0]);
    let mut sixel_encoder = SixelEncoder::from_image(
        build_test_image(2, 1, [255, 0, 0, 255].repeat(2)),
        [0, 0, 0],
    )
    .expect("image encodes");
    let mut actual_output = Vec::new();
    while let Some(output_chunk) = sixel_encoder
        .take_next_chunk(1)
        .expect("chunk size is valid")
    {
        assert_eq!(output_chunk.len(), 1);
        actual_output.extend_from_slice(output_chunk);
    }

    assert_eq!(actual_output, expected_output);
    assert_eq!(
        sixel_encoder
            .take_next_chunk(usize::MAX)
            .expect("finished encoder"),
        None
    );

    let mut sixel_encoder =
        SixelEncoder::from_image(build_test_image(1, 1, vec![0, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let output_chunk = sixel_encoder
        .take_next_chunk(usize::MAX)
        .expect("large request is valid")
        .expect("header is available");
    assert!(output_chunk.len() <= MAX_SIXEL_CHUNK_BYTE_COUNT);
}

#[test]
fn write_to_emits_the_same_bytes_as_chunked_output() {
    let rgba_bytes = [255, 0, 0, 255].repeat(14);
    let expected_output = encode_sixel_image(2, 7, &rgba_bytes, [0, 0, 0]);
    let mut sixel_encoder = SixelEncoder::from_image(build_test_image(2, 7, rgba_bytes), [0, 0, 0])
        .expect("image encodes");
    let mut recording_writer = RecordingWriter {
        written_bytes: Vec::new(),
    };

    sixel_encoder
        .write_to(&mut recording_writer)
        .expect("writer accepts output");

    assert_eq!(recording_writer.written_bytes, expected_output);
    assert_eq!(
        sixel_encoder.take_next_chunk(1).expect("finished encoder"),
        None
    );
}

#[test]
fn generation_failure_is_sticky_after_header_output() {
    let mut sixel_encoder =
        SixelEncoder::from_image(build_test_image(1, 1, vec![255, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let first_output_chunk = sixel_encoder
        .take_next_chunk(1)
        .expect("first chunk is valid")
        .expect("header is available");
    assert_eq!(first_output_chunk, b"\x1b");
    sixel_encoder.inject_generation_failure_for_test();

    loop {
        match sixel_encoder.take_next_chunk(MAX_SIXEL_CHUNK_BYTE_COUNT) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("failed encoder reported completion"),
            Err(encode_error) => {
                assert_palette_mapping_error(encode_error);
                break;
            }
        }
    }
    assert_encoder_failed_error(
        sixel_encoder
            .take_next_chunk(1)
            .expect_err("failed encoder remains failed"),
    );

    let mut recording_writer = RecordingWriter {
        written_bytes: Vec::new(),
    };
    assert_encoder_failed_error(
        sixel_encoder
            .write_next_chunk(&mut recording_writer, 1)
            .expect_err("failed encoder rejects writes"),
    );
    assert!(recording_writer.written_bytes.is_empty());
    assert_encoder_failed_error(
        sixel_encoder
            .write_to(&mut recording_writer)
            .expect_err("failed encoder rejects complete writes"),
    );
}

#[test]
fn rejects_zero_chunk_size() {
    let mut sixel_encoder =
        SixelEncoder::from_image(build_test_image(1, 1, vec![0, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let encode_error = sixel_encoder
        .take_next_chunk(0)
        .expect_err("zero chunk is rejected");
    match encode_error {
        SixelEncodeError::ZeroChunkSize => {}
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
}

#[test]
fn writer_failure_preserves_first_and_partial_chunks() {
    let mut sixel_encoder =
        SixelEncoder::from_image(build_test_image(1, 1, vec![255, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let mut always_fail_writer = AlwaysFailWriter;
    let encode_error = sixel_encoder
        .write_to(&mut always_fail_writer)
        .expect_err("first write fails");
    assert_io_error_kind(encode_error, io::ErrorKind::BrokenPipe);

    let mut sixel_encoder =
        SixelEncoder::from_image(build_test_image(1, 1, vec![255, 0, 0, 255]), [0, 0, 0])
            .expect("image encodes");
    let mut partial_writer = PartialFailWriter {
        written_bytes: Vec::new(),
    };
    let encode_error = sixel_encoder
        .write_next_chunk(&mut partial_writer, 4)
        .expect_err("partial write fails");
    assert_io_error_kind(encode_error, io::ErrorKind::BrokenPipe);
    assert_eq!(partial_writer.written_bytes, b"\x1bP");

    let mut retry_writer = RecordingWriter {
        written_bytes: Vec::new(),
    };
    assert!(sixel_encoder
        .write_next_chunk(&mut retry_writer, 4)
        .expect("retry writes the pending bytes"));
    assert_eq!(retry_writer.written_bytes, b"\x1bP7;");
}

fn assert_io_error_kind(encode_error: SixelEncodeError, expected_io_kind: io::ErrorKind) {
    match encode_error {
        SixelEncodeError::Io(io_error) => assert_eq!(io_error.kind(), expected_io_kind),
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
}

fn assert_palette_mapping_error(encode_error: SixelEncodeError) {
    match encode_error {
        SixelEncodeError::PaletteMapping => {}
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
}

fn assert_encoder_failed_error(encode_error: SixelEncodeError) {
    match encode_error {
        SixelEncodeError::EncoderFailed => {}
        unexpected_error => panic!("unexpected error: {unexpected_error:?}"),
    }
}

struct AlwaysFailWriter;

impl Write for AlwaysFailWriter {
    fn write(&mut self, _requested_bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PartialFailWriter {
    written_bytes: Vec<u8>,
}

impl Write for PartialFailWriter {
    fn write(&mut self, requested_bytes: &[u8]) -> io::Result<usize> {
        let written_byte_count = requested_bytes.len().min(2);
        self.written_bytes
            .extend_from_slice(&requested_bytes[..written_byte_count]);
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "partial writer failure",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecordingWriter {
    written_bytes: Vec<u8>,
}

impl Write for RecordingWriter {
    fn write(&mut self, requested_bytes: &[u8]) -> io::Result<usize> {
        self.written_bytes.extend_from_slice(requested_bytes);
        Ok(requested_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
