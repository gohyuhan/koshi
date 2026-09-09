//! Bounded base64, compression, raster, and raw-pixel codecs.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use flate2::{Decompress, FlushDecompress, Status};

use crate::{DecodedImage, GraphicsError, GraphicsProtocol};
use crate::{MAX_GRAPHICS_TRANSFER_BYTES, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE};

/// Decode standard or unpadded base64 data within `MAX_GRAPHICS_TRANSFER_BYTES`.
///
/// Returns `TransferTooLarge` when the input or decoded bytes exceed the limit and `InvalidBase64`
/// when the data has invalid alphabet or padding.
pub fn decode_base64(protocol: GraphicsProtocol, data: &[u8]) -> Result<Vec<u8>, GraphicsError> {
    if data.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    let decoded = STANDARD
        .decode(data)
        .or_else(|_| STANDARD_NO_PAD.decode(data))
        .map_err(|_| GraphicsError::InvalidBase64 { protocol })?;
    if decoded.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    Ok(decoded)
}

/// Decode one complete zlib stream without exceeding the transfer or image-byte limits.
///
/// Returns `DecodeFailure` for malformed, incomplete, or trailing input, `TransferTooLarge` for
/// oversized input, and `ImageTooLarge` for output above `MAX_IMAGE_BYTES`.
pub fn decompress_bounded(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<Vec<u8>, GraphicsError> {
    let (output, consumed) = decompress_bounded_prefix(protocol, data)?;
    if consumed != data.len() {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    Ok(output)
}

/// Decode one zlib stream and return its exact consumed input length.
///
/// Returns `DecodeFailure` for malformed or incomplete input, `TransferTooLarge` for oversized
/// input, and `ImageTooLarge` when output exceeds `MAX_IMAGE_BYTES`.
pub fn decompress_bounded_prefix(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<(Vec<u8>, usize), GraphicsError> {
    if data.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }

    let mut decoder = Decompress::new(true);
    let mut output = Vec::new();
    let mut input_offset = 0usize;
    let mut chunk = [0u8; 8192];

    loop {
        if input_offset > data.len() {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        let input = &data[input_offset..];
        let flush = if input.is_empty() {
            FlushDecompress::Finish
        } else {
            FlushDecompress::None
        };
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(input, &mut chunk, flush)
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        let consumed = usize::try_from(
            decoder
                .total_in()
                .checked_sub(before_in)
                .ok_or(GraphicsError::DecodeFailure { protocol })?,
        )
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        let produced = usize::try_from(
            decoder
                .total_out()
                .checked_sub(before_out)
                .ok_or(GraphicsError::DecodeFailure { protocol })?,
        )
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        if consumed > input.len() || produced > chunk.len() {
            return Err(GraphicsError::DecodeFailure { protocol });
        }
        input_offset = input_offset
            .checked_add(consumed)
            .ok_or(GraphicsError::DecodeFailure { protocol })?;
        if produced > 0 {
            let output_len = output
                .len()
                .checked_add(produced)
                .ok_or(GraphicsError::ImageTooLarge { protocol })?;
            if output_len > MAX_IMAGE_BYTES {
                return Err(GraphicsError::ImageTooLarge { protocol });
            }
            if output_len > output.capacity() {
                let target_capacity = output
                    .capacity()
                    .saturating_mul(2)
                    .max(output_len)
                    .min(MAX_IMAGE_BYTES);
                let additional = target_capacity
                    .checked_sub(output.len())
                    .ok_or(GraphicsError::DecodeFailure { protocol })?;
                output
                    .try_reserve_exact(additional)
                    .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
            }
            output.extend_from_slice(&chunk[..produced]);
        }
        if status == Status::StreamEnd {
            return Ok((output, input_offset));
        }
        if consumed == 0 && produced == 0 {
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
    data: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let format = guess_image_format(protocol, data)?;
    if let Some(format) = animated_raster_format(protocol, format, data)? {
        return Err(GraphicsError::UnsupportedMedia {
            protocol,
            format: format.to_string(),
        });
    }
    decode_static_raster(protocol, data)
}

/// Decode PNG data into validated RGBA pixels.
///
/// Returns `UnsupportedMedia` for unknown data, `DecodeFailure` for non-PNG or invalid PNG data,
/// and `ImageTooLarge` when the encoded input or decoded dimensions exceed the image limits.
pub fn decode_png(protocol: GraphicsProtocol, data: &[u8]) -> Result<DecodedImage, GraphicsError> {
    if guess_image_format(protocol, data)? != image::ImageFormat::Png {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    decode_static_raster(protocol, data)
}

pub(crate) fn guess_image_format(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<image::ImageFormat, GraphicsError> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    let format = image::guess_format(data).map_err(|_| GraphicsError::UnsupportedMedia {
        protocol,
        format: "unknown".to_string(),
    })?;
    if !matches!(
        format,
        image::ImageFormat::Bmp
            | image::ImageFormat::Gif
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Png
            | image::ImageFormat::Tiff
            | image::ImageFormat::WebP
    ) {
        return Err(GraphicsError::UnsupportedMedia {
            protocol,
            format: format!("{format:?}"),
        });
    }
    Ok(format)
}

pub(crate) fn decode_static_raster(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        let mut reader = image::ImageReader::new(Cursor::new(data))
            .with_guessed_format()
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        reader.limits(raster_limits());
        reader
            .decode()
            .map(|image| image.into_rgba8())
            .map_err(|error| map_image_error(protocol, error))
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })??;
    let (width, height) = decoded.dimensions();
    let width = usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let height = usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_dimensions(protocol, width, height)?;
    let rgba = decoded.into_raw();
    let expected = checked_rgba_len(protocol, width, height)?;
    if rgba.len() != expected {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    Ok(DecodedImage {
        width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba,
    })
}

fn animated_raster_format(
    protocol: GraphicsProtocol,
    format: image::ImageFormat,
    data: &[u8],
) -> Result<Option<&'static str>, GraphicsError> {
    match format {
        image::ImageFormat::Gif => gif_has_multiple_frames(protocol, data)
            .map(|animated| animated.then_some("animated GIF")),
        image::ImageFormat::Png => {
            png_is_animated(protocol, data).map(|animated| animated.then_some("animated PNG"))
        }
        image::ImageFormat::WebP => {
            webp_is_animated(protocol, data).map(|animated| animated.then_some("animated WebP"))
        }
        _ => Ok(None),
    }
}

pub(crate) fn png_is_animated(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<bool, GraphicsError> {
    catch_unwind(AssertUnwindSafe(|| {
        let decoder =
            image::codecs::png::PngDecoder::with_limits(Cursor::new(data), raster_limits())
                .map_err(|error| map_image_error(protocol, error))?;
        decoder
            .is_apng()
            .map_err(|error| map_image_error(protocol, error))
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

pub(crate) fn webp_is_animated(
    protocol: GraphicsProtocol,
    data: &[u8],
) -> Result<bool, GraphicsError> {
    use image::ImageDecoder;

    catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(data))
            .map_err(|error| map_image_error(protocol, error))?;
        decoder
            .set_limits(raster_limits())
            .map_err(|error| map_image_error(protocol, error))?;
        Ok(decoder.has_animation())
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn gif_has_multiple_frames(protocol: GraphicsProtocol, data: &[u8]) -> Result<bool, GraphicsError> {
    use image::{AnimationDecoder, ImageDecoder};

    catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(data))
            .map_err(|error| map_image_error(protocol, error))?;
        decoder
            .set_limits(raster_limits())
            .map_err(|error| map_image_error(protocol, error))?;
        let mut frames = decoder.into_frames();
        frames
            .next()
            .transpose()
            .map_err(|error| map_image_error(protocol, error))?
            .ok_or(GraphicsError::DecodeFailure { protocol })?;
        match frames.next() {
            None => Ok(false),
            Some(Ok(_)) => Ok(true),
            Some(Err(error)) => Err(map_image_error(protocol, error)),
        }
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

pub(crate) fn map_image_error(
    protocol: GraphicsProtocol,
    error: image::ImageError,
) -> GraphicsError {
    match error {
        image::ImageError::Limits(limit)
            if matches!(
                limit.kind(),
                image::error::LimitErrorKind::DimensionError
                    | image::error::LimitErrorKind::InsufficientMemory
            ) =>
        {
            GraphicsError::ImageTooLarge { protocol }
        }
        _ => GraphicsError::DecodeFailure { protocol },
    }
}

pub(crate) fn raster_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE as u32);
    limits.max_image_height = Some(MAX_IMAGE_SIDE as u32);
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    limits
}

/// Convert exactly `width * height` packed RGB bytes into opaque RGBA pixels.
///
/// Returns `DeclaredSizeMismatch` when `data.len()` differs from the required byte count.
pub fn raw_rgb(
    protocol: GraphicsProtocol,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let width = usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let height = usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_dimensions(protocol, width, height)?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if data.len() != expected {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol,
            expected,
            actual: data.len(),
        });
    }
    let mut rgba = Vec::with_capacity(checked_rgba_len(protocol, width, height)?);
    for pixel in data.chunks_exact(3) {
        rgba.extend_from_slice(pixel);
        rgba.push(255);
    }
    Ok(DecodedImage {
        width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba,
    })
}

/// Validate exactly `width * height` packed RGBA bytes and copy them into an image.
///
/// Returns `DeclaredSizeMismatch` when `data.len()` differs from the required byte count.
pub fn raw_rgba(
    protocol: GraphicsProtocol,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let width = usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let height = usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_dimensions(protocol, width, height)?;
    let expected = checked_rgba_len(protocol, width, height)?;
    if data.len() != expected {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol,
            expected,
            actual: data.len(),
        });
    }
    Ok(DecodedImage {
        width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba: data.to_vec(),
    })
}

/// Validate nonzero image dimensions against the side and pixel limits.
///
/// Returns `ImageTooLarge` for zero or oversized dimensions and `InvalidDimensions` when pixel
/// multiplication overflows.
pub fn validate_dimensions(
    protocol: GraphicsProtocol,
    width: usize,
    height: usize,
) -> Result<(), GraphicsError> {
    if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    let pixels = width
        .checked_mul(height)
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    Ok(())
}

/// Return `width * height * 4` after validating the image dimensions and byte limit.
///
/// Returns the same dimension errors as [`validate_dimensions`] and `InvalidDimensions` when the
/// RGBA byte count overflows or exceeds `MAX_IMAGE_BYTES`.
pub fn checked_rgba_len(
    protocol: GraphicsProtocol,
    width: usize,
    height: usize,
) -> Result<usize, GraphicsError> {
    validate_dimensions(protocol, width, height)?;
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|length| *length <= MAX_IMAGE_BYTES)
        .ok_or(GraphicsError::InvalidDimensions { protocol })
}

#[cfg(test)]
mod tests;
