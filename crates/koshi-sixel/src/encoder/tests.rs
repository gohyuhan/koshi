//! Contract tests for bounded Sixel encoding.

use super::*;

use std::io::{self, Write};
use std::sync::Arc;

fn image(width: u32, height: u32, rgba: Vec<u8>) -> Arc<koshi_image::DecodedImage> {
    Arc::new(koshi_image::DecodedImage {
        width,
        height,
        rgba,
    })
}

fn collect(mut encoder: SixelEncoder) -> Vec<u8> {
    let mut output = Vec::new();
    while let Some(chunk) = encoder.next_chunk(usize::MAX).expect("chunk size is valid") {
        output.extend_from_slice(chunk);
    }
    output
}

fn encode(width: u32, height: u32, rgba: &[u8], background: [u8; 3]) -> Vec<u8> {
    collect(
        SixelEncoder::new(image(width, height, rgba.to_vec()), background).expect("image encodes"),
    )
}

#[test]
fn matches_one_pixel_protocol_fixture() {
    let bytes = encode(1, 1, &[255, 0, 0, 255], [0, 0, 0]);

    assert_eq!(bytes, b"\x1bP7;1q\"1;1;1;1#0;2;100;0;0#0@\x1b\\");
}

#[test]
fn blends_partial_alpha_and_keeps_zero_alpha_transparent() {
    let blended = encode(1, 1, &[255, 0, 0, 128], [0, 0, 255]);
    assert_eq!(blended, b"\x1bP7;1q\"1;1;1;1#0;2;50;0;50#0@\x1b\\");

    let white = encode(1, 1, &[255, 255, 255, 128], [255, 255, 255]);
    assert_eq!(white, b"\x1bP7;1q\"1;1;1;1#0;2;100;100;100#0@\x1b\\");

    let transparent = encode(1, 1, &[255, 0, 0, 0], [1, 2, 3]);
    assert_eq!(transparent, b"\x1bP7;1q\"1;1;1;1?\x1b\\");
}

#[test]
fn emits_exact_color_bands_and_rle_boundaries() {
    let colors = [255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255];
    let bytes = encode(3, 1, &colors, [0, 0, 0]);
    assert_eq!(
        bytes,
        b"\x1bP7;1q\"1;1;3;1#0;2;100;0;0#1;2;0;0;100#0!2@$#1!2?@\x1b\\"
    );

    let repeated = encode(10, 1, &[255, 0, 0, 255].repeat(10), [0, 0, 0]);
    assert_eq!(repeated, b"\x1bP7;1q\"1;1;10;1#0;2;100;0;0#0!10@\x1b\\");

    let rows = encode(1, 7, &[255, 0, 0, 255].repeat(7), [0, 0, 0]);
    assert_eq!(rows, b"\x1bP7;1q\"1;1;1;7#0;2;100;0;0#0~-#0@\x1b\\");
}

#[test]
fn quantized_palette_is_bounded_deterministic_and_exact() {
    let rgba = vec![0, 0, 0, 255, 64, 0, 0, 255, 128, 0, 0, 255, 192, 0, 0, 255];
    let options = SixelEncodeOptions::new(2);
    let first = collect(
        SixelEncoder::with_options(image(4, 1, rgba.clone()), [0, 0, 0], options)
            .expect("quantized colors encode"),
    );
    let second = collect(
        SixelEncoder::with_options(image(4, 1, rgba), [0, 0, 0], options)
            .expect("quantized colors encode"),
    );

    assert_eq!(first, second);
    assert_eq!(
        first,
        b"\x1bP7;1q\"1;1;4;1#0;2;13;0;0#1;2;63;0;0#0!2@$#1!2?!2@\x1b\\"
    );
}

#[test]
fn rejects_input_before_preparation() {
    let error = SixelEncoder::new(image(2, 1, vec![255, 0, 0, 255]), [0, 0, 0])
        .expect_err("short RGBA is rejected");
    match error {
        SixelEncodeError::RgbaLengthMismatch { expected, actual } => {
            assert_eq!(expected, 8);
            assert_eq!(actual, 4);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let error = SixelEncoder::new(image(0, 1, Vec::new()), [0, 0, 0])
        .expect_err("zero dimensions are rejected");
    match error {
        SixelEncodeError::InvalidDimensions { width, height } => {
            assert_eq!(width, 0);
            assert_eq!(height, 1);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let error = SixelEncoder::new(image(4097, 4097, Vec::new()), [0, 0, 0])
        .expect_err("oversized dimensions are rejected");
    match error {
        SixelEncodeError::InvalidDimensions { width, height } => {
            assert_eq!(width, 4097);
            assert_eq!(height, 4097);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let error = SixelEncoder::with_options(
        image(1, 1, vec![0, 0, 0, 255]),
        [0, 0, 0],
        SixelEncodeOptions::new(1),
    )
    .expect_err("too-small palette is rejected");
    match error {
        SixelEncodeError::InvalidPaletteSize { requested } => assert_eq!(requested, 1),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn emits_bounded_chunks_and_preserves_all_bytes() {
    let expected = encode(2, 1, [255, 0, 0, 255].repeat(2).as_slice(), [0, 0, 0]);
    let mut encoder = SixelEncoder::new(image(2, 1, [255, 0, 0, 255].repeat(2)), [0, 0, 0])
        .expect("image encodes");
    let mut actual = Vec::new();
    while let Some(chunk) = encoder.next_chunk(1).expect("chunk size is valid") {
        assert_eq!(chunk.len(), 1);
        actual.extend_from_slice(chunk);
    }

    assert_eq!(actual, expected);
    assert_eq!(
        encoder.next_chunk(usize::MAX).expect("finished encoder"),
        None
    );

    let mut encoder =
        SixelEncoder::new(image(1, 1, vec![0, 0, 0, 255]), [0, 0, 0]).expect("image encodes");
    let chunk = encoder
        .next_chunk(usize::MAX)
        .expect("large request is valid")
        .expect("header is available");
    assert!(chunk.len() <= MAX_SIXEL_CHUNK_BYTES);
}

#[test]
fn generation_failure_is_sticky_after_header_output() {
    let mut encoder =
        SixelEncoder::new(image(1, 1, vec![255, 0, 0, 255]), [0, 0, 0]).expect("image encodes");
    let first = encoder
        .next_chunk(1)
        .expect("first chunk is valid")
        .expect("header is available");
    assert_eq!(first, b"\x1b");
    encoder.inject_generation_failure_for_test();

    loop {
        match encoder.next_chunk(MAX_SIXEL_CHUNK_BYTES) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("failed encoder reported completion"),
            Err(error) => {
                assert_palette_mapping(error);
                break;
            }
        }
    }
    assert_encoder_failed(
        encoder
            .next_chunk(1)
            .expect_err("failed encoder remains failed"),
    );

    let mut writer = RecordingWriter { bytes: Vec::new() };
    assert_encoder_failed(
        encoder
            .write_next_chunk(&mut writer, 1)
            .expect_err("failed encoder rejects writes"),
    );
    assert!(writer.bytes.is_empty());
    assert_encoder_failed(
        encoder
            .write_to(&mut writer)
            .expect_err("failed encoder rejects complete writes"),
    );
}

#[test]
fn rejects_zero_chunk_size() {
    let mut encoder =
        SixelEncoder::new(image(1, 1, vec![0, 0, 0, 255]), [0, 0, 0]).expect("image encodes");
    let error = encoder.next_chunk(0).expect_err("zero chunk is rejected");
    match error {
        SixelEncodeError::ZeroChunkSize => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn writer_failure_preserves_first_and_partial_chunks() {
    let mut encoder =
        SixelEncoder::new(image(1, 1, vec![255, 0, 0, 255]), [0, 0, 0]).expect("image encodes");
    let mut writer = AlwaysFailWriter;
    let error = encoder
        .write_to(&mut writer)
        .expect_err("first write fails");
    assert_io_kind(error, io::ErrorKind::BrokenPipe);

    let mut encoder =
        SixelEncoder::new(image(1, 1, vec![255, 0, 0, 255]), [0, 0, 0]).expect("image encodes");
    let mut partial = PartialFailWriter { bytes: Vec::new() };
    let error = encoder
        .write_next_chunk(&mut partial, 4)
        .expect_err("partial write fails");
    assert_io_kind(error, io::ErrorKind::BrokenPipe);
    assert_eq!(partial.bytes, b"\x1bP");

    let mut retry = RecordingWriter { bytes: Vec::new() };
    assert!(encoder
        .write_next_chunk(&mut retry, 4)
        .expect("retry writes the pending bytes"));
    assert_eq!(retry.bytes, b"\x1bP7;");
}

fn assert_io_kind(error: SixelEncodeError, expected: io::ErrorKind) {
    match error {
        SixelEncodeError::Io(error) => assert_eq!(error.kind(), expected),
        other => panic!("unexpected error: {other:?}"),
    }
}

fn assert_palette_mapping(error: SixelEncodeError) {
    match error {
        SixelEncodeError::PaletteMapping => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

fn assert_encoder_failed(error: SixelEncodeError) {
    match error {
        SixelEncodeError::EncoderFailed => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

struct AlwaysFailWriter;

impl Write for AlwaysFailWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PartialFailWriter {
    bytes: Vec<u8>,
}

impl Write for PartialFailWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = bytes.len().min(2);
        self.bytes.extend_from_slice(&bytes[..written]);
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
    bytes: Vec<u8>,
}

impl Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
