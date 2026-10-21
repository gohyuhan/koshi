//! Bounded Kitty image encoding and output.

use std::io::{self, Write};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::{Compress, Compression, FlushCompress, Status};
use koshi_image::{compute_rgba_byte_count, DecodedImage, GraphicsProtocol};
use thiserror::Error;

const KITTY_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Kitty;

/// The largest compressed byte slice in one Kitty graphics chunk.
const KITTY_COMPRESSED_CHUNK_BYTE_COUNT: usize = 3_072;

/// The largest number of raw RGBA bytes compressed by one step.
const KITTY_COMPRESSION_RGBA_BYTE_COUNT_PER_STEP: usize = 262_144;

/// The scratch output capacity used by one compression pass.
const KITTY_COMPRESSION_OUTPUT_BYTE_COUNT: usize = 65_536;

/// Kitty placement fields written by the placement command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyPlacement {
    /// The Kitty image id to place, carried by `i=`.
    pub image_id: u32,
    /// The Kitty placement identity.
    pub placement_id: u32,
    /// The source rectangle's left pixel.
    pub source_x_pixels: u32,
    /// The source rectangle's top pixel.
    pub source_y_pixels: u32,
    /// The source rectangle's width in pixels.
    pub source_width_pixels: u32,
    /// The source rectangle's height in pixels.
    pub source_height_pixels: u32,
    /// The destination rectangle's width in cells.
    pub column_count: u32,
    /// The destination rectangle's height in cells.
    pub row_count: u32,
    /// The first-cell horizontal pixel offset.
    pub cell_pixel_offset_x: Option<u32>,
    /// The first-cell vertical pixel offset.
    pub cell_pixel_offset_y: Option<u32>,
    /// The placement z-index.
    pub z_index: i32,
}

/// Errors returned while validating or writing Kitty output.
#[derive(Debug, Error)]
pub enum KittyOutputError {
    /// The image id is zero.
    #[error("Kitty image id must be nonzero")]
    InvalidImageId,
    /// The placement identity is zero.
    #[error("Kitty placement id must be nonzero")]
    InvalidPlacementId,
    /// The decoded image dimensions exceed the shared limits.
    #[error("Kitty image dimensions {image_width_pixels}x{image_height_pixels} are invalid")]
    InvalidImageDimensions {
        /// The image width in pixels.
        image_width_pixels: u32,
        /// The image height in pixels.
        image_height_pixels: u32,
    },
    /// The decoded image RGBA byte count does not match its dimensions.
    #[error(
        "Kitty image RGBA length {actual_rgba_byte_count} does not match {expected_rgba_byte_count} bytes for {image_width_pixels}x{image_height_pixels}"
    )]
    InvalidImageRgbaByteCount {
        /// The image width in pixels.
        image_width_pixels: u32,
        /// The image height in pixels.
        image_height_pixels: u32,
        /// The expected RGBA byte count.
        expected_rgba_byte_count: usize,
        /// The received RGBA byte count.
        actual_rgba_byte_count: usize,
    },
    /// The destination cell rectangle has a zero side.
    #[error("Kitty placement dimensions {column_count}x{row_count} are invalid")]
    InvalidPlacementDimensions { column_count: u32, row_count: u32 },
    /// The source rectangle does not fit inside the decoded image.
    #[error(
        "Kitty source rectangle ({source_x_pixels},{source_y_pixels}) {source_width_pixels}x{source_height_pixels} exceeds image {image_width_pixels}x{image_height_pixels}"
    )]
    InvalidSourceRect {
        /// The source rectangle's left pixel.
        source_x_pixels: u32,
        /// The source rectangle's top pixel.
        source_y_pixels: u32,
        /// The source rectangle's width in pixels.
        source_width_pixels: u32,
        /// The source rectangle's height in pixels.
        source_height_pixels: u32,
        /// The decoded image width in pixels.
        image_width_pixels: u32,
        /// The decoded image height in pixels.
        image_height_pixels: u32,
    },
    /// A vector allocation failed while preparing output.
    #[error("Kitty output allocation failed")]
    AllocationFailed,
    /// The zlib compressor returned an error.
    #[error("Kitty image compression failed: {compression_message}")]
    Compression {
        /// The compressor's error text.
        compression_message: String,
    },
    /// The compressor consumed and produced no bytes.
    #[error("Kitty image compression made no progress")]
    CompressionNoProgress,
    /// The destination writer rejected output.
    #[error("Kitty output write failed: {0}")]
    Io(#[from] io::Error),
}

/// Compress the RGBA bytes of `decoded_image` into one zlib stream: the payload
/// that [`write_kitty_transmit`] carries for a Kitty `a=t,o=z` transmit.
///
/// Each step compresses at most 256 KiB of RGBA input, and `is_cancelled` is
/// called before each step. Returns `Ok(None)` when `is_cancelled` returns
/// `true` before the stream ends. Example: a 1×1 image with RGBA bytes
/// `[1, 2, 3, 4]` gives the 12 bytes whose Base64 is `eAFjZGJmAQAAGAAL`.
///
/// # Errors
/// [`KittyOutputError::InvalidImageDimensions`] for a size that
/// [`compute_rgba_byte_count`] refuses for Kitty,
/// [`KittyOutputError::InvalidImageRgbaByteCount`] for RGBA bytes whose count
/// does not match the size, [`KittyOutputError::AllocationFailed`] when
/// the compressed bytes cannot be stored, [`KittyOutputError::Compression`]
/// when the compressor fails, and [`KittyOutputError::CompressionNoProgress`]
/// when a compressor call consumes and produces no bytes.
pub fn compress_kitty_image(
    decoded_image: &DecodedImage,
    is_cancelled: impl Fn() -> bool,
) -> Result<Option<Vec<u8>>, KittyOutputError> {
    validate_decoded_image(decoded_image)?;
    let mut compressor = Compress::new(Compression::fast(), true);
    let mut compressed_bytes = Vec::new();
    compressed_bytes
        .try_reserve_exact(KITTY_COMPRESSION_OUTPUT_BYTE_COUNT)
        .map_err(|_| KittyOutputError::AllocationFailed)?;
    let mut compression_output_bytes = vec![0u8; KITTY_COMPRESSION_OUTPUT_BYTE_COUNT];
    let mut rgba_input_byte_offset = 0usize;
    loop {
        if is_cancelled() {
            return Ok(None);
        }
        let rgba_input_end_byte_offset = rgba_input_byte_offset
            .saturating_add(KITTY_COMPRESSION_RGBA_BYTE_COUNT_PER_STEP)
            .min(decoded_image.rgba_bytes.len());
        let compression_flush_mode = if rgba_input_end_byte_offset == decoded_image.rgba_bytes.len()
        {
            FlushCompress::Finish
        } else {
            FlushCompress::None
        };
        loop {
            let rgba_input_byte_count_before = compressor.total_in();
            let compressed_output_byte_count_before = compressor.total_out();
            let compression_status = compressor
                .compress(
                    &decoded_image.rgba_bytes[rgba_input_byte_offset..rgba_input_end_byte_offset],
                    &mut compression_output_bytes,
                    compression_flush_mode,
                )
                .map_err(|compression_error| KittyOutputError::Compression {
                    compression_message: compression_error.to_string(),
                })?;
            let consumed_rgba_byte_count = usize::try_from(
                compressor
                    .total_in()
                    .saturating_sub(rgba_input_byte_count_before),
            )
            .map_err(|_| KittyOutputError::CompressionNoProgress)?;
            let produced_compressed_byte_count = usize::try_from(
                compressor
                    .total_out()
                    .saturating_sub(compressed_output_byte_count_before),
            )
            .map_err(|_| KittyOutputError::CompressionNoProgress)?;
            rgba_input_byte_offset = rgba_input_byte_offset
                .checked_add(consumed_rgba_byte_count)
                .ok_or(KittyOutputError::CompressionNoProgress)?;
            compressed_bytes
                .try_reserve(produced_compressed_byte_count)
                .map_err(|_| KittyOutputError::AllocationFailed)?;
            compressed_bytes
                .extend_from_slice(&compression_output_bytes[..produced_compressed_byte_count]);
            if compression_status == Status::StreamEnd {
                return Ok(Some(compressed_bytes));
            }
            if rgba_input_byte_offset == rgba_input_end_byte_offset
                && rgba_input_end_byte_offset < decoded_image.rgba_bytes.len()
            {
                break;
            }
            if consumed_rgba_byte_count == 0 && produced_compressed_byte_count == 0 {
                return Err(KittyOutputError::CompressionNoProgress);
            }
        }
    }
}

/// Write the Kitty transmit of an image of `pixel_width` × `pixel_height`
/// pixels under the Kitty image id `image_id`, whose zlib payload is
/// `compressed_bytes`.
///
/// The payload is cut into 3 072-byte chunks, the last one shorter, and each
/// chunk travels Base64-encoded in one command. The first command is
/// `ESC _ G a=t,f=32,s=<width>,v=<height>,i=<id>,q=2,o=z,m=<more>;`, every
/// command after it is `ESC _ G q=2,m=<more>;`, and each ends with `ESC \`.
/// `<more>` is `1` on every chunk but the last, and `0` on the last. An empty
/// payload writes nothing.
///
/// Example: the payload of a 1×1 image with RGBA bytes `[1, 2, 3, 4]` under
/// id `7` writes `ESC _ G a=t,f=32,s=1,v=1,i=7,q=2,o=z,m=0;eAFjZGJmAQAAGAAL
/// ESC \`.
///
/// # Errors
/// [`KittyOutputError::InvalidImageId`] for an `image_id` of `0`, with
/// nothing written, and [`KittyOutputError::Io`] for a failed write.
pub fn write_kitty_transmit<W: Write>(
    writer: &mut W,
    pixel_width: u32,
    pixel_height: u32,
    image_id: u32,
    compressed_bytes: &[u8],
) -> Result<(), KittyOutputError> {
    if image_id == 0 {
        return Err(KittyOutputError::InvalidImageId);
    }
    let chunk_count = compressed_bytes
        .len()
        .div_ceil(KITTY_COMPRESSED_CHUNK_BYTE_COUNT);
    let mut encoded_base64_text = String::new();
    for (chunk_index, compressed_chunk_bytes) in compressed_bytes
        .chunks(KITTY_COMPRESSED_CHUNK_BYTE_COUNT)
        .enumerate()
    {
        let has_more_chunks = chunk_index + 1 < chunk_count;
        if chunk_index == 0 {
            write!(
                writer,
                "\x1b_Ga=t,f=32,s={pixel_width},v={pixel_height},i={image_id},q=2,o=z,m={};",
                u8::from(has_more_chunks),
            )?;
        } else {
            write!(writer, "\x1b_Gq=2,m={};", u8::from(has_more_chunks))?;
        }
        encoded_base64_text.clear();
        STANDARD.encode_string(compressed_chunk_bytes, &mut encoded_base64_text);
        writer.write_all(encoded_base64_text.as_bytes())?;
        writer.write_all(b"\x1b\\")?;
    }
    Ok(())
}

/// Write one Kitty placement command.
pub fn write_kitty_placement<W: Write>(
    writer: &mut W,
    decoded_image: &DecodedImage,
    placement: &KittyPlacement,
) -> Result<(), KittyOutputError> {
    validate_kitty_placement(decoded_image, placement)?;
    write!(
        writer,
        "\x1b_Ga=p,i={},p={},x={},y={},w={},h={},",
        placement.image_id,
        placement.placement_id,
        placement.source_x_pixels,
        placement.source_y_pixels,
        placement.source_width_pixels,
        placement.source_height_pixels,
    )?;
    if let Some(cell_pixel_offset_x) = placement.cell_pixel_offset_x {
        write!(writer, "X={cell_pixel_offset_x},")?;
    }
    if let Some(cell_pixel_offset_y) = placement.cell_pixel_offset_y {
        write!(writer, "Y={cell_pixel_offset_y},")?;
    }
    write!(
        writer,
        "c={},r={},C=1,z={},q=2;\x1b\\",
        placement.column_count, placement.row_count, placement.z_index,
    )?;
    Ok(())
}

/// Write a Kitty command that deletes the image `image_id`, every placement of
/// it and its data: `ESC _ G a=d,d=I,i=<image_id>,q=2; ESC \`.
///
/// # Errors
/// [`KittyOutputError::InvalidImageId`] for an `image_id` of `0`, with
/// nothing written, and [`KittyOutputError::Io`] for a failed write.
pub fn write_kitty_image_delete<W: Write>(
    writer: &mut W,
    image_id: u32,
) -> Result<(), KittyOutputError> {
    if image_id == 0 {
        return Err(KittyOutputError::InvalidImageId);
    }
    write!(writer, "\x1b_Ga=d,d=I,i={image_id},q=2;\x1b\\")?;
    Ok(())
}

/// Write a Kitty command that deletes all visible placements and image data.
pub fn write_kitty_delete_all<W: Write>(writer: &mut W) -> Result<(), KittyOutputError> {
    writer.write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\")?;
    Ok(())
}

/// Write a Kitty command that deletes all visible placements and retains every
/// image's data. An image id transmitted before this command still places.
pub fn write_kitty_visible_placement_delete<W: Write>(
    writer: &mut W,
) -> Result<(), KittyOutputError> {
    writer.write_all(b"\x1b_Ga=d,d=a,q=2;\x1b\\")?;
    Ok(())
}

/// Write the cancellation sequence for an open Kitty APC transfer.
pub fn write_kitty_abort<W: Write>(writer: &mut W) -> Result<(), KittyOutputError> {
    writer.write_all(b"\x18\x1b\\")?;
    Ok(())
}

fn validate_decoded_image(decoded_image: &DecodedImage) -> Result<(), KittyOutputError> {
    let image_width_pixels = usize::try_from(decoded_image.pixel_width).map_err(|_| {
        KittyOutputError::InvalidImageDimensions {
            image_width_pixels: decoded_image.pixel_width,
            image_height_pixels: decoded_image.pixel_height,
        }
    })?;
    let image_height_pixels = usize::try_from(decoded_image.pixel_height).map_err(|_| {
        KittyOutputError::InvalidImageDimensions {
            image_width_pixels: decoded_image.pixel_width,
            image_height_pixels: decoded_image.pixel_height,
        }
    })?;
    let expected_rgba_byte_count =
        compute_rgba_byte_count(KITTY_PROTOCOL, image_width_pixels, image_height_pixels).map_err(
            |_| KittyOutputError::InvalidImageDimensions {
                image_width_pixels: decoded_image.pixel_width,
                image_height_pixels: decoded_image.pixel_height,
            },
        )?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count {
        return Err(KittyOutputError::InvalidImageRgbaByteCount {
            image_width_pixels: decoded_image.pixel_width,
            image_height_pixels: decoded_image.pixel_height,
            expected_rgba_byte_count,
            actual_rgba_byte_count: decoded_image.rgba_bytes.len(),
        });
    }
    Ok(())
}

fn validate_kitty_placement(
    decoded_image: &DecodedImage,
    placement: &KittyPlacement,
) -> Result<(), KittyOutputError> {
    if placement.image_id == 0 {
        return Err(KittyOutputError::InvalidImageId);
    }
    if placement.placement_id == 0 {
        return Err(KittyOutputError::InvalidPlacementId);
    }
    if placement.column_count == 0 || placement.row_count == 0 {
        return Err(KittyOutputError::InvalidPlacementDimensions {
            column_count: placement.column_count,
            row_count: placement.row_count,
        });
    }
    validate_decoded_image(decoded_image)?;
    let is_valid_source_rectangle = placement.source_width_pixels > 0
        && placement.source_height_pixels > 0
        && placement
            .source_x_pixels
            .checked_add(placement.source_width_pixels)
            .is_some_and(|source_end_x_pixels| source_end_x_pixels <= decoded_image.pixel_width)
        && placement
            .source_y_pixels
            .checked_add(placement.source_height_pixels)
            .is_some_and(|source_end_y_pixels| source_end_y_pixels <= decoded_image.pixel_height);
    if !is_valid_source_rectangle {
        return Err(KittyOutputError::InvalidSourceRect {
            source_x_pixels: placement.source_x_pixels,
            source_y_pixels: placement.source_y_pixels,
            source_width_pixels: placement.source_width_pixels,
            source_height_pixels: placement.source_height_pixels,
            image_width_pixels: decoded_image.pixel_width,
            image_height_pixels: decoded_image.pixel_height,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
