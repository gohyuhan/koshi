//! Bounded base64, compression, raster, and raw-pixel codecs.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use flate2::{Decompress, FlushDecompress, Status};

use crate::{DecodedImage, GraphicsError, GraphicsProtocol};
use crate::{
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT, MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
};

/// Decode standard or unpadded base64 bytes within `MAX_GRAPHICS_TRANSFER_BYTE_COUNT`.
///
/// Returns `TransferTooLarge` when the input or decoded bytes exceed the limit and `InvalidBase64`
/// when the data has invalid alphabet or padding.
pub fn decode_base64(
    protocol: GraphicsProtocol,
    encoded_bytes: &[u8],
) -> Result<Vec<u8>, GraphicsError> {
    if encoded_bytes.len() > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    let decoded_bytes = STANDARD
        .decode(encoded_bytes)
        .or_else(|_| STANDARD_NO_PAD.decode(encoded_bytes))
        .map_err(|_| GraphicsError::InvalidBase64 { protocol })?;
    if decoded_bytes.len() > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    Ok(decoded_bytes)
}

/// Decode one complete zlib stream without exceeding the transfer or image-byte limits.
///
/// Returns `DecodeFailure` for malformed, incomplete, or trailing input, `TransferTooLarge` for
/// oversized input, and `ImageTooLarge` for output above `MAX_IMAGE_BYTE_COUNT`.
pub fn decompress_bounded(
    protocol: GraphicsProtocol,
    compressed_bytes: &[u8],
) -> Result<Vec<u8>, GraphicsError> {
    let (decompressed_bytes, consumed_byte_count) =
        decompress_bounded_prefix(protocol, compressed_bytes)?;
    if consumed_byte_count != compressed_bytes.len() {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    Ok(decompressed_bytes)
}

/// Decode one zlib stream and return its exact consumed input length.
///
/// Returns `DecodeFailure` for malformed or incomplete input, `TransferTooLarge` for oversized
/// input, and `ImageTooLarge` when output exceeds `MAX_IMAGE_BYTE_COUNT`.
pub fn decompress_bounded_prefix(
    protocol: GraphicsProtocol,
    compressed_bytes: &[u8],
) -> Result<(Vec<u8>, usize), GraphicsError> {
    if compressed_bytes.len() > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }

    let mut decoder = Decompress::new(true);
    let mut decompressed_bytes = Vec::new();
    let mut input_byte_offset = 0usize;
    let mut decompression_chunk = [0u8; 8192];

    loop {
        if input_byte_offset > compressed_bytes.len() {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let remaining_compressed_bytes = &compressed_bytes[input_byte_offset..];
        let flush_mode = if remaining_compressed_bytes.is_empty() {
            FlushDecompress::Finish
        } else {
            FlushDecompress::None
        };
        let compressed_input_byte_count_before = decoder.total_in();
        let decompressed_output_byte_count_before = decoder.total_out();
        let decompression_status = decoder
            .decompress(
                remaining_compressed_bytes,
                &mut decompression_chunk,
                flush_mode,
            )
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        let consumed_byte_count = usize::try_from(
            decoder
                .total_in()
                .checked_sub(compressed_input_byte_count_before)
                .ok_or(GraphicsError::DecodeFailure { protocol })?,
        )
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        let produced_byte_count = usize::try_from(
            decoder
                .total_out()
                .checked_sub(decompressed_output_byte_count_before)
                .ok_or(GraphicsError::DecodeFailure { protocol })?,
        )
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        if consumed_byte_count > remaining_compressed_bytes.len()
            || produced_byte_count > decompression_chunk.len()
        {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        input_byte_offset = input_byte_offset
            .checked_add(consumed_byte_count)
            .ok_or(GraphicsError::DecodeFailure { protocol })?;
        if produced_byte_count > 0 {
            let decompressed_byte_count = decompressed_bytes
                .len()
                .checked_add(produced_byte_count)
                .ok_or(GraphicsError::ImageTooLarge { protocol })?;
            if decompressed_byte_count > MAX_IMAGE_BYTE_COUNT {
                return Err(GraphicsError::ImageTooLarge { protocol });
            }
            if decompressed_byte_count > decompressed_bytes.capacity() {
                let target_capacity = decompressed_bytes
                    .capacity()
                    .saturating_mul(2)
                    .max(decompressed_byte_count)
                    .min(MAX_IMAGE_BYTE_COUNT);
                let additional_byte_count = target_capacity
                    .checked_sub(decompressed_bytes.len())
                    .ok_or(GraphicsError::DecodeFailure { protocol })?;
                decompressed_bytes
                    .try_reserve_exact(additional_byte_count)
                    .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
            }
            decompressed_bytes.extend_from_slice(&decompression_chunk[..produced_byte_count]);
        }
        if decompression_status == Status::StreamEnd {
            return Ok((decompressed_bytes, input_byte_offset));
        }
        if consumed_byte_count == 0 && produced_byte_count == 0 {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
    }
}

/// Decode one supported non-animated raster image into validated RGBA pixels.
///
/// Returns `UnsupportedMedia` for unsupported or animated formats and `ImageTooLarge` when the
/// encoded input or decoded dimensions exceed the image limits.
pub fn decode_raster(
    protocol: GraphicsProtocol,
    encoded_image_bytes: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let image_format = guess_image_format(protocol, encoded_image_bytes)?;
    if let Some(animated_format_name) =
        find_animated_raster_format(protocol, image_format, encoded_image_bytes)?
    {
        return Err(GraphicsError::UnsupportedMedia {
            protocol,
            media_format: animated_format_name.to_string(),
        });
    }
    decode_static_raster(protocol, encoded_image_bytes)
}

/// Decode PNG data into validated RGBA pixels.
///
/// Returns `UnsupportedMedia` for unknown data, `DecodeFailure` for non-PNG or invalid PNG data,
/// and `ImageTooLarge` when the encoded input or decoded dimensions exceed the image limits.
pub fn decode_png(
    protocol: GraphicsProtocol,
    encoded_png_bytes: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    if guess_image_format(protocol, encoded_png_bytes)? != image::ImageFormat::Png {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    decode_static_raster(protocol, encoded_png_bytes)
}

pub(crate) fn guess_image_format(
    protocol: GraphicsProtocol,
    encoded_image_bytes: &[u8],
) -> Result<image::ImageFormat, GraphicsError> {
    if encoded_image_bytes.len() > MAX_IMAGE_BYTE_COUNT {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    let image_format =
        image::guess_format(encoded_image_bytes).map_err(|_| GraphicsError::UnsupportedMedia {
            protocol,
            media_format: "unknown".to_string(),
        })?;
    if !matches!(
        image_format,
        image::ImageFormat::Bmp
            | image::ImageFormat::Gif
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Png
            | image::ImageFormat::Tiff
            | image::ImageFormat::WebP
    ) {
        return Err(GraphicsError::UnsupportedMedia {
            protocol,
            media_format: format!("{image_format:?}"),
        });
    }
    Ok(image_format)
}

pub(crate) fn decode_static_raster(
    protocol: GraphicsProtocol,
    encoded_image_bytes: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let decoded_image = catch_unwind(AssertUnwindSafe(|| {
        let mut reader = image::ImageReader::new(Cursor::new(encoded_image_bytes))
            .with_guessed_format()
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        reader.limits(build_raster_limits());
        reader
            .decode()
            .map(|decoded_image| decoded_image.into_rgba8())
            .map_err(|error| map_image_error(protocol, error))
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })??;
    let (pixel_width, pixel_height) = decoded_image.dimensions();
    let pixel_width =
        usize::try_from(pixel_width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let pixel_height =
        usize::try_from(pixel_height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_image_dimensions(protocol, pixel_width, pixel_height)?;
    let rgba_bytes = decoded_image.into_raw();
    let expected_byte_count = compute_rgba_byte_count(protocol, pixel_width, pixel_height)?;
    if rgba_bytes.len() != expected_byte_count {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    Ok(DecodedImage {
        pixel_width: u32::try_from(pixel_width)
            .map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        pixel_height: u32::try_from(pixel_height)
            .map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba_bytes,
    })
}

fn find_animated_raster_format(
    protocol: GraphicsProtocol,
    image_format: image::ImageFormat,
    encoded_image_bytes: &[u8],
) -> Result<Option<&'static str>, GraphicsError> {
    match image_format {
        image::ImageFormat::Gif => gif_has_multiple_frames(protocol, encoded_image_bytes)
            .map(|is_animated| is_animated.then_some("animated GIF")),
        image::ImageFormat::Png => png_is_animated(protocol, encoded_image_bytes)
            .map(|is_animated| is_animated.then_some("animated PNG")),
        image::ImageFormat::WebP => webp_is_animated(protocol, encoded_image_bytes)
            .map(|is_animated| is_animated.then_some("animated WebP")),
        _ => Ok(None),
    }
}

pub(crate) fn png_is_animated(
    protocol: GraphicsProtocol,
    encoded_png_bytes: &[u8],
) -> Result<bool, GraphicsError> {
    catch_unwind(AssertUnwindSafe(|| {
        let decoder = image::codecs::png::PngDecoder::with_limits(
            Cursor::new(encoded_png_bytes),
            build_raster_limits(),
        )
        .map_err(|image_error| map_image_error(protocol, image_error))?;
        decoder
            .is_apng()
            .map_err(|image_error| map_image_error(protocol, image_error))
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

pub(crate) fn webp_is_animated(
    protocol: GraphicsProtocol,
    encoded_webp_bytes: &[u8],
) -> Result<bool, GraphicsError> {
    use image::ImageDecoder;

    catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(encoded_webp_bytes))
            .map_err(|image_error| map_image_error(protocol, image_error))?;
        decoder
            .set_limits(build_raster_limits())
            .map_err(|image_error| map_image_error(protocol, image_error))?;
        Ok(decoder.has_animation())
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn gif_has_multiple_frames(
    protocol: GraphicsProtocol,
    encoded_gif_bytes: &[u8],
) -> Result<bool, GraphicsError> {
    use image::{AnimationDecoder, ImageDecoder};

    catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(encoded_gif_bytes))
            .map_err(|image_error| map_image_error(protocol, image_error))?;
        decoder
            .set_limits(build_raster_limits())
            .map_err(|image_error| map_image_error(protocol, image_error))?;
        let mut gif_frames = decoder.into_frames();
        gif_frames
            .next()
            .transpose()
            .map_err(|image_error| map_image_error(protocol, image_error))?
            .ok_or(GraphicsError::DecodeFailure { protocol })?;
        match gif_frames.next() {
            None => Ok(false),
            Some(Ok(_)) => Ok(true),
            Some(Err(image_error)) => Err(map_image_error(protocol, image_error)),
        }
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

pub(crate) fn map_image_error(
    protocol: GraphicsProtocol,
    image_error: image::ImageError,
) -> GraphicsError {
    match image_error {
        image::ImageError::Limits(limit_error)
            if matches!(
                limit_error.kind(),
                image::error::LimitErrorKind::DimensionError
                    | image::error::LimitErrorKind::InsufficientMemory
            ) =>
        {
            GraphicsError::ImageTooLarge { protocol }
        }
        _ => GraphicsError::DecodeFailure { protocol },
    }
}

pub(crate) fn build_raster_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE_PIXEL_COUNT as u32);
    limits.max_image_height = Some(MAX_IMAGE_SIDE_PIXEL_COUNT as u32);
    limits.max_alloc = Some(MAX_IMAGE_BYTE_COUNT as u64);
    limits
}

/// Convert exactly `width * height` packed RGB bytes into opaque RGBA pixels.
///
/// Returns `DeclaredSizeMismatch` when `rgb_bytes.len()` differs from the required byte count.
pub fn decode_raw_rgb(
    protocol: GraphicsProtocol,
    pixel_width: u32,
    pixel_height: u32,
    rgb_bytes: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let pixel_width =
        usize::try_from(pixel_width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let pixel_height =
        usize::try_from(pixel_height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_image_dimensions(protocol, pixel_width, pixel_height)?;
    let expected_rgb_byte_count = pixel_width
        .checked_mul(pixel_height)
        .and_then(|pixel_count| pixel_count.checked_mul(3))
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if rgb_bytes.len() != expected_rgb_byte_count {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol,
            expected_byte_count: expected_rgb_byte_count,
            actual_byte_count: rgb_bytes.len(),
        });
    }
    let mut rgba_bytes = Vec::with_capacity(compute_rgba_byte_count(
        protocol,
        pixel_width,
        pixel_height,
    )?);
    for rgb_pixel in rgb_bytes.chunks_exact(3) {
        rgba_bytes.extend_from_slice(rgb_pixel);
        rgba_bytes.push(255);
    }
    Ok(DecodedImage {
        pixel_width: u32::try_from(pixel_width)
            .map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        pixel_height: u32::try_from(pixel_height)
            .map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba_bytes,
    })
}

/// Validate exactly `width * height` packed RGBA bytes and copy them into an image.
///
/// Returns `DeclaredSizeMismatch` when `rgba_bytes.len()` differs from the required byte count.
pub fn decode_raw_rgba(
    protocol: GraphicsProtocol,
    pixel_width: u32,
    pixel_height: u32,
    rgba_bytes: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let pixel_width =
        usize::try_from(pixel_width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let pixel_height =
        usize::try_from(pixel_height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_image_dimensions(protocol, pixel_width, pixel_height)?;
    let expected_rgba_byte_count = compute_rgba_byte_count(protocol, pixel_width, pixel_height)?;
    if rgba_bytes.len() != expected_rgba_byte_count {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol,
            expected_byte_count: expected_rgba_byte_count,
            actual_byte_count: rgba_bytes.len(),
        });
    }
    Ok(DecodedImage {
        pixel_width: u32::try_from(pixel_width)
            .map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        pixel_height: u32::try_from(pixel_height)
            .map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba_bytes: rgba_bytes.to_vec(),
    })
}

/// Validate nonzero image dimensions against the side and pixel limits.
///
/// Returns `ImageTooLarge` for zero or oversized dimensions and `InvalidDimensions` when pixel
/// multiplication overflows.
pub fn validate_image_dimensions(
    protocol: GraphicsProtocol,
    pixel_width: usize,
    pixel_height: usize,
) -> Result<(), GraphicsError> {
    if pixel_width == 0
        || pixel_height == 0
        || pixel_width > MAX_IMAGE_SIDE_PIXEL_COUNT
        || pixel_height > MAX_IMAGE_SIDE_PIXEL_COUNT
    {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    let pixel_count = pixel_width
        .checked_mul(pixel_height)
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if pixel_count > MAX_IMAGE_PIXEL_COUNT {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    Ok(())
}

/// Return `width * height * 4` after validating the image dimensions and byte limit.
///
/// Returns the same dimension errors as [`validate_image_dimensions`] and `InvalidDimensions` when the
/// RGBA byte count overflows or exceeds `MAX_IMAGE_BYTE_COUNT`.
pub fn compute_rgba_byte_count(
    protocol: GraphicsProtocol,
    pixel_width: usize,
    pixel_height: usize,
) -> Result<usize, GraphicsError> {
    validate_image_dimensions(protocol, pixel_width, pixel_height)?;
    pixel_width
        .checked_mul(pixel_height)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .filter(|byte_count| *byte_count <= MAX_IMAGE_BYTE_COUNT)
        .ok_or(GraphicsError::InvalidDimensions { protocol })
}

#[cfg(test)]
mod tests;
