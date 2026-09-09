//! Image codec tests: bounded transfers, raw pixels, and raster decoding.

use super::*;

use crate::MAX_GRAPHICS_CONTROL_BYTES;

#[test]
fn base64_accepts_padded_and_unpadded_payloads() {
    assert_eq!(
        decode_base64(GraphicsProtocol::Kitty, b"AAECAw==").expect("padded base64"),
        vec![0, 1, 2, 3]
    );
    assert_eq!(
        decode_base64(GraphicsProtocol::Kitty, b"AAECAw").expect("unpadded base64"),
        vec![0, 1, 2, 3]
    );
}

#[test]
fn base64_rejects_invalid_data_with_the_protocol_error() {
    assert_eq!(
        decode_base64(GraphicsProtocol::Iterm2, b"not base64!")
            .expect_err("invalid base64 must be rejected"),
        GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Iterm2
        }
    );
}

#[test]
fn zlib_data_round_trips_and_trailing_bytes_are_rejected() {
    use std::io::Write;

    let mut compressed = Vec::new();
    let mut encoder =
        flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
    encoder.write_all(b"image").expect("zlib input writes");
    encoder.finish().expect("zlib stream finishes");

    assert_eq!(
        decompress_bounded(GraphicsProtocol::Kitty, &compressed).expect("zlib decodes"),
        b"image"
    );
    compressed.push(0);
    assert_eq!(
        decompress_bounded(GraphicsProtocol::Kitty, &compressed)
            .expect_err("trailing bytes must be rejected"),
        GraphicsError::DecodeFailure {
            protocol: GraphicsProtocol::Kitty
        }
    );
}

#[test]
fn zlib_data_requires_the_complete_stream_trailer() {
    use std::io::Write;

    let mut compressed = Vec::new();
    let mut encoder =
        flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
    encoder.write_all(b"image").expect("zlib input writes");
    encoder.finish().expect("zlib stream finishes");

    for end in 0..compressed.len() {
        assert_eq!(
            decompress_bounded(GraphicsProtocol::Kitty, &compressed[..end])
                .expect_err("every truncated prefix must be rejected"),
            GraphicsError::DecodeFailure {
                protocol: GraphicsProtocol::Kitty
            },
            "truncated prefix length {end}"
        );
    }
}

#[test]
fn zlib_prefix_returns_the_first_stream_and_consumed_length() {
    use std::io::Write;

    let compress = |input: &[u8]| {
        let mut compressed = Vec::new();
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default());
        encoder.write_all(input).expect("zlib input writes");
        encoder.finish().expect("zlib stream finishes");
        compressed
    };
    let first = compress(b"image");
    let second = compress(b"next image");
    let mut streams = first.clone();
    streams.extend_from_slice(&second);

    let (decoded, consumed) =
        decompress_bounded_prefix(GraphicsProtocol::Kitty, &streams).expect("first stream decodes");

    assert_eq!(decoded, b"image");
    assert_eq!(consumed, first.len());
    assert_eq!(&streams[consumed..], second.as_slice());
}

#[test]
fn decode_png_rejects_unknown_data_with_a_typed_media_error() {
    assert_eq!(
        decode_png(GraphicsProtocol::Kitty, &[]).expect_err("empty data is not a PNG"),
        GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            format: "unknown".to_owned(),
        }
    );
}

#[test]
fn validate_dimensions_accepts_the_pixel_limit_and_rejects_one_more() {
    let height = MAX_IMAGE_PIXELS / MAX_IMAGE_SIDE;
    assert_eq!(
        validate_dimensions(GraphicsProtocol::Sixel, MAX_IMAGE_SIDE, height),
        Ok(())
    );
    assert_eq!(
        validate_dimensions(GraphicsProtocol::Sixel, MAX_IMAGE_SIDE, height + 1),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Sixel
        })
    );
}

#[test]
fn graphics_error_deserialization_rejects_oversized_text() {
    let value = serde_json::to_value(GraphicsError::UnsupportedAction {
        protocol: GraphicsProtocol::Kitty,
        action: "x".repeat(MAX_GRAPHICS_CONTROL_BYTES + 1),
    })
    .expect("graphics error serializes");

    let error = serde_json::from_value::<GraphicsError>(value)
        .expect_err("oversized graphics error text must be rejected");
    assert_eq!(
        error.to_string(),
        format!("graphics error text exceeds {MAX_GRAPHICS_CONTROL_BYTES} bytes")
    );
}

#[test]
fn raw_rgb_expands_pixels_to_opaque_rgba() {
    let image =
        raw_rgb(GraphicsProtocol::Kitty, 2, 1, &[1, 2, 3, 4, 5, 6]).expect("two RGB pixels decode");

    assert_eq!(image.width, 2);
    assert_eq!(image.height, 1);
    assert_eq!(image.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
}

#[test]
fn raw_rgba_rejects_invalid_dimensions_and_lengths() {
    assert_eq!(
        raw_rgba(GraphicsProtocol::Kitty, 0, 1, &[]).expect_err("zero width must be rejected"),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Kitty
        }
    );
    assert_eq!(
        raw_rgba(GraphicsProtocol::Kitty, 2, 1, &[1, 2, 3, 4])
            .expect_err("short RGBA data must be rejected"),
        GraphicsError::DeclaredSizeMismatch {
            protocol: GraphicsProtocol::Kitty,
            expected: 8,
            actual: 4,
        }
    );
    assert_eq!(
        raw_rgb(GraphicsProtocol::Kitty, MAX_IMAGE_SIDE as u32 + 1, 1, &[])
            .expect_err("an oversized side must be rejected before reading raw data"),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Kitty
        }
    );
}

#[test]
fn checked_lengths_reject_overflow_before_allocation() {
    assert_eq!(
        checked_rgba_len(GraphicsProtocol::Sixel, usize::MAX, 2)
            .expect_err("dimension multiplication must be checked"),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Sixel
        }
    );
}

#[test]
fn raster_decoder_returns_rgba_pixels() {
    use image::ImageEncoder;

    let mut data = Vec::new();
    image::codecs::png::PngEncoder::new(&mut data)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");

    let image = decode_raster(GraphicsProtocol::Iterm2, &data).expect("PNG decodes");
    assert_eq!(image.width, 1);
    assert_eq!(image.height, 1);
    assert_eq!(image.rgba, vec![255, 0, 0, 255]);
}
