//! Image codec tests: bounded transfers, raw pixels, and raster decoding.

use super::*;

use crate::MAX_GRAPHICS_CONTROL_BYTE_COUNT;

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

    for truncated_byte_count in 0..compressed.len() {
        assert_eq!(
            decompress_bounded(GraphicsProtocol::Kitty, &compressed[..truncated_byte_count],)
                .expect_err("every truncated prefix must be rejected"),
            GraphicsError::DecodeFailure {
                protocol: GraphicsProtocol::Kitty
            },
            "truncated prefix length {truncated_byte_count}"
        );
    }
}

#[test]
fn zlib_prefix_returns_the_first_stream_and_consumed_length() {
    use std::io::Write;

    let compress_zlib_bytes = |source_bytes: &[u8]| {
        let mut compressed_bytes = Vec::new();
        let mut zlib_encoder =
            flate2::write::ZlibEncoder::new(&mut compressed_bytes, flate2::Compression::default());
        zlib_encoder
            .write_all(source_bytes)
            .expect("zlib input writes");
        zlib_encoder.finish().expect("zlib stream finishes");
        compressed_bytes
    };
    let first_compressed_stream = compress_zlib_bytes(b"image");
    let second_compressed_stream = compress_zlib_bytes(b"next image");
    let mut concatenated_compressed_streams = first_compressed_stream.clone();
    concatenated_compressed_streams.extend_from_slice(&second_compressed_stream);

    let (decoded_image_bytes, consumed_byte_count) =
        decompress_bounded_prefix(GraphicsProtocol::Kitty, &concatenated_compressed_streams)
            .expect("first stream decodes");

    assert_eq!(decoded_image_bytes, b"image");
    assert_eq!(consumed_byte_count, first_compressed_stream.len());
    assert_eq!(
        &concatenated_compressed_streams[consumed_byte_count..],
        second_compressed_stream.as_slice()
    );
}

#[test]
fn decode_png_rejects_unknown_data_with_a_typed_media_error() {
    assert_eq!(
        decode_png(GraphicsProtocol::Kitty, &[]).expect_err("empty data is not a PNG"),
        GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            media_format: "unknown".to_owned(),
        }
    );
}

#[test]
fn validate_image_dimensions_accepts_the_pixel_limit_and_rejects_one_more() {
    let image_pixel_height = MAX_IMAGE_PIXEL_COUNT / MAX_IMAGE_SIDE_PIXEL_COUNT;
    assert_eq!(
        validate_image_dimensions(
            GraphicsProtocol::Sixel,
            MAX_IMAGE_SIDE_PIXEL_COUNT,
            image_pixel_height,
        ),
        Ok(())
    );
    assert_eq!(
        validate_image_dimensions(
            GraphicsProtocol::Sixel,
            MAX_IMAGE_SIDE_PIXEL_COUNT,
            image_pixel_height + 1,
        ),
        Err(GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Sixel
        })
    );
}

#[test]
fn graphics_error_deserialization_rejects_oversized_text() {
    let serialized_graphics_error = serde_json::to_value(GraphicsError::UnsupportedAction {
        protocol: GraphicsProtocol::Kitty,
        action: "x".repeat(MAX_GRAPHICS_CONTROL_BYTE_COUNT + 1),
    })
    .expect("graphics error serializes");

    let deserialization_error = serde_json::from_value::<GraphicsError>(serialized_graphics_error)
        .expect_err("oversized graphics error text must be rejected");
    assert_eq!(
        deserialization_error.to_string(),
        format!("graphics error text exceeds {MAX_GRAPHICS_CONTROL_BYTE_COUNT} bytes")
    );
}

#[test]
fn decode_raw_rgb_expands_pixels_to_opaque_rgba() {
    let decoded_image = decode_raw_rgb(GraphicsProtocol::Kitty, 2, 1, &[1, 2, 3, 4, 5, 6])
        .expect("two RGB pixels decode");

    assert_eq!(decoded_image.pixel_width, 2);
    assert_eq!(decoded_image.pixel_height, 1);
    assert_eq!(decoded_image.rgba_bytes, vec![1, 2, 3, 255, 4, 5, 6, 255]);
}

#[test]
fn decode_raw_rgba_rejects_invalid_dimensions_and_lengths() {
    assert_eq!(
        decode_raw_rgba(GraphicsProtocol::Kitty, 0, 1, &[])
            .expect_err("zero width must be rejected"),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Kitty
        }
    );
    assert_eq!(
        decode_raw_rgba(GraphicsProtocol::Kitty, 2, 1, &[1, 2, 3, 4])
            .expect_err("short RGBA data must be rejected"),
        GraphicsError::DeclaredSizeMismatch {
            protocol: GraphicsProtocol::Kitty,
            expected_byte_count: 8,
            actual_byte_count: 4,
        }
    );
    assert_eq!(
        decode_raw_rgb(
            GraphicsProtocol::Kitty,
            MAX_IMAGE_SIDE_PIXEL_COUNT as u32 + 1,
            1,
            &[]
        )
        .expect_err("an oversized side must be rejected before reading raw data"),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Kitty
        }
    );
}

#[test]
fn checked_lengths_reject_overflow_before_allocation() {
    assert_eq!(
        compute_rgba_byte_count(GraphicsProtocol::Sixel, usize::MAX, 2)
            .expect_err("dimension multiplication must be checked"),
        GraphicsError::ImageTooLarge {
            protocol: GraphicsProtocol::Sixel
        }
    );
}

#[test]
fn raster_decoder_returns_rgba_pixels() {
    use image::ImageEncoder;

    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("one-pixel PNG encodes");

    let decoded_image = decode_raster(GraphicsProtocol::Iterm2, &png_bytes).expect("PNG decodes");
    assert_eq!(decoded_image.pixel_width, 1);
    assert_eq!(decoded_image.pixel_height, 1);
    assert_eq!(decoded_image.rgba_bytes, vec![255, 0, 0, 255]);
}
