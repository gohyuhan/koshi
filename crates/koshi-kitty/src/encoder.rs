//! Bounded Kitty image encoding and output.

use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::{Compress, Compression, FlushCompress, Status};
use koshi_image::{
    compute_rgba_byte_count, validate_image_dimensions, DecodedImage, GraphicsProtocol,
};
use thiserror::Error;

const KITTY_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Kitty;

/// The largest compressed byte slice in one Kitty graphics chunk.
pub const KITTY_IMAGE_CHUNK_BYTE_COUNT: usize = 3_072;

/// The largest number of Kitty graphics chunks emitted by one advance.
pub const KITTY_IMAGE_CHUNK_COUNT_PER_STEP: usize = 16;

/// The largest number of raw RGBA bytes compressed by one advance.
pub const KITTY_COMPRESSION_INPUT_BYTE_COUNT_PER_STEP: usize = 262_144;

/// The scratch output capacity used by one compression pass.
pub const KITTY_COMPRESSION_OUTPUT_BYTE_COUNT: usize = 65_536;

/// Kitty placement fields written by the placement command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyPlacement {
    /// The Kitty image number to place.
    pub image_number: u32,
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
    /// The image number is zero.
    #[error("Kitty image number must be nonzero")]
    InvalidImageNumber,
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
    /// A compressed chunk did not fit the fixed Base64 buffer.
    #[error("Kitty compressed chunk exceeds its Base64 buffer")]
    EncodedChunkTooLarge,
    /// The destination writer rejected output.
    #[error("Kitty output write failed: {0}")]
    Io(#[from] io::Error),
}

/// A zlib-compressed Kitty upload advanced in bounded output steps.
pub struct KittyUpload {
    decoded_image: Arc<DecodedImage>,
    image_number: u32,
    compressor: Compress,
    input_byte_offset: usize,
    compressed_bytes: Vec<u8>,
    compressed_byte_offset: usize,
    is_compression_complete: bool,
    has_started_transmission: bool,
}

impl fmt::Debug for KittyUpload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KittyUpload")
            .field("image_width_pixels", &self.decoded_image.pixel_width)
            .field("image_height_pixels", &self.decoded_image.pixel_height)
            .field("image_number", &self.image_number)
            .field("input_byte_offset", &self.input_byte_offset)
            .field("compressed_byte_count", &self.compressed_bytes.len())
            .field("compressed_byte_offset", &self.compressed_byte_offset)
            .field("is_compression_complete", &self.is_compression_complete)
            .field("has_started_transmission", &self.has_started_transmission)
            .finish()
    }
}

impl KittyUpload {
    /// Create a bounded zlib upload for one decoded image.
    pub fn from_decoded_image(
        decoded_image: Arc<DecodedImage>,
        image_number: u32,
    ) -> Result<Self, KittyOutputError> {
        if image_number == 0 {
            return Err(KittyOutputError::InvalidImageNumber);
        }
        validate_decoded_image(&decoded_image)?;
        let mut compressed_bytes = Vec::new();
        compressed_bytes
            .try_reserve_exact(KITTY_COMPRESSION_OUTPUT_BYTE_COUNT)
            .map_err(|_| KittyOutputError::AllocationFailed)?;
        Ok(Self {
            decoded_image,
            image_number,
            compressor: Compress::new(Compression::fast(), true),
            input_byte_offset: 0,
            compressed_bytes,
            compressed_byte_offset: 0,
            is_compression_complete: false,
            has_started_transmission: false,
        })
    }

    /// Return whether one complete Kitty chunk batch was written.
    #[must_use]
    pub fn has_started_transmission(&self) -> bool {
        self.has_started_transmission
    }

    /// Return the shared decoded image retained by the upload.
    #[must_use]
    pub fn get_decoded_image(&self) -> &Arc<DecodedImage> {
        &self.decoded_image
    }

    /// Return the nonzero Kitty image number used by the upload.
    #[must_use]
    pub fn get_image_number(&self) -> u32 {
        self.image_number
    }

    /// Return whether compression and transmission reached their ends.
    #[must_use]
    pub fn is_upload_complete(&self) -> bool {
        self.is_compression_complete
            && self.compressed_byte_offset == self.compressed_bytes.len()
            && self.has_started_transmission
    }

    /// Compress and write one bounded Kitty output step.
    pub fn advance_upload<W: Write>(&mut self, writer: &mut W) -> Result<(), KittyOutputError> {
        if !self.is_compression_complete
            && self
                .compressed_bytes
                .len()
                .saturating_sub(self.compressed_byte_offset)
                <= KITTY_IMAGE_CHUNK_BYTE_COUNT
        {
            self.compress_next_input_step()?;
        }
        self.write_output_chunks(writer)
    }

    fn compress_next_input_step(&mut self) -> Result<(), KittyOutputError> {
        if self.compressed_byte_offset != 0 {
            self.compressed_bytes
                .copy_within(self.compressed_byte_offset.., 0);
            self.compressed_bytes
                .truncate(self.compressed_bytes.len() - self.compressed_byte_offset);
            self.compressed_byte_offset = 0;
        }
        let input_end_byte_offset = self
            .input_byte_offset
            .saturating_add(KITTY_COMPRESSION_INPUT_BYTE_COUNT_PER_STEP)
            .min(self.decoded_image.rgba_bytes.len());
        let flush_mode = if input_end_byte_offset == self.decoded_image.rgba_bytes.len() {
            FlushCompress::Finish
        } else {
            FlushCompress::None
        };
        let mut compression_output_bytes = [0u8; KITTY_COMPRESSION_OUTPUT_BYTE_COUNT];
        loop {
            let input_byte_count_before = self.compressor.total_in();
            let output_byte_count_before = self.compressor.total_out();
            let compression_status = self
                .compressor
                .compress(
                    &self.decoded_image.rgba_bytes[self.input_byte_offset..input_end_byte_offset],
                    &mut compression_output_bytes,
                    flush_mode,
                )
                .map_err(|compression_error| KittyOutputError::Compression {
                    compression_message: compression_error.to_string(),
                })?;
            let consumed_byte_count = usize::try_from(
                self.compressor
                    .total_in()
                    .saturating_sub(input_byte_count_before),
            )
            .map_err(|_| KittyOutputError::CompressionNoProgress)?;
            let produced_byte_count = usize::try_from(
                self.compressor
                    .total_out()
                    .saturating_sub(output_byte_count_before),
            )
            .map_err(|_| KittyOutputError::CompressionNoProgress)?;
            self.input_byte_offset = self
                .input_byte_offset
                .checked_add(consumed_byte_count)
                .ok_or(KittyOutputError::CompressionNoProgress)?;
            self.compressed_bytes
                .try_reserve(produced_byte_count)
                .map_err(|_| KittyOutputError::AllocationFailed)?;
            self.compressed_bytes
                .extend_from_slice(&compression_output_bytes[..produced_byte_count]);
            if compression_status == Status::StreamEnd {
                self.is_compression_complete = true;
                return Ok(());
            }
            if self.input_byte_offset == input_end_byte_offset
                && input_end_byte_offset < self.decoded_image.rgba_bytes.len()
            {
                return Ok(());
            }
            if consumed_byte_count == 0 && produced_byte_count == 0 {
                return Err(KittyOutputError::CompressionNoProgress);
            }
        }
    }

    fn write_output_chunks<W: Write>(&mut self, writer: &mut W) -> Result<(), KittyOutputError> {
        let mut kitty_output_bytes = Vec::new();
        kitty_output_bytes
            .try_reserve_exact(KITTY_IMAGE_CHUNK_COUNT_PER_STEP * 4_160)
            .map_err(|_| KittyOutputError::AllocationFailed)?;
        let mut next_compressed_byte_offset = self.compressed_byte_offset;
        let mut is_first_chunk = !self.has_started_transmission;
        let mut written_chunk_count = 0usize;
        while written_chunk_count < KITTY_IMAGE_CHUNK_COUNT_PER_STEP {
            let available_byte_count = self
                .compressed_bytes
                .len()
                .saturating_sub(next_compressed_byte_offset);
            if available_byte_count == 0
                || (!self.is_compression_complete
                    && available_byte_count <= KITTY_IMAGE_CHUNK_BYTE_COUNT)
            {
                break;
            }
            let chunk_byte_count = available_byte_count.min(KITTY_IMAGE_CHUNK_BYTE_COUNT);
            let chunk_end_byte_offset = next_compressed_byte_offset + chunk_byte_count;
            let has_more_chunks = !self.is_compression_complete
                || chunk_end_byte_offset < self.compressed_bytes.len();
            write_upload_chunk(
                &mut kitty_output_bytes,
                &self.decoded_image,
                self.image_number,
                is_first_chunk,
                has_more_chunks,
                &self.compressed_bytes[next_compressed_byte_offset..chunk_end_byte_offset],
            )?;
            is_first_chunk = false;
            next_compressed_byte_offset = chunk_end_byte_offset;
            written_chunk_count += 1;
        }
        if kitty_output_bytes.is_empty() {
            return Ok(());
        }
        writer
            .write_all(&kitty_output_bytes)
            .map_err(KittyOutputError::Io)?;
        writer.flush().map_err(KittyOutputError::Io)?;
        self.compressed_byte_offset = next_compressed_byte_offset;
        self.has_started_transmission = true;
        Ok(())
    }
}

fn write_upload_chunk(
    kitty_output_bytes: &mut Vec<u8>,
    decoded_image: &DecodedImage,
    image_number: u32,
    is_first_chunk: bool,
    has_more_chunks: bool,
    compressed_bytes: &[u8],
) -> Result<(), KittyOutputError> {
    if is_first_chunk {
        write!(
            kitty_output_bytes,
            "\x1b_Ga=t,f=32,s={},v={},I={},q=2,o=z,m={};",
            decoded_image.pixel_width,
            decoded_image.pixel_height,
            image_number,
            u8::from(has_more_chunks),
        )?;
    } else {
        write!(
            kitty_output_bytes,
            "\x1b_Gq=2,m={};",
            u8::from(has_more_chunks)
        )?;
    }
    let mut encoded_base64_bytes = [0u8; 4_096];
    let encoded_byte_count = STANDARD
        .encode_slice(compressed_bytes, &mut encoded_base64_bytes)
        .map_err(|_| KittyOutputError::EncodedChunkTooLarge)?;
    kitty_output_bytes.extend_from_slice(&encoded_base64_bytes[..encoded_byte_count]);
    kitty_output_bytes.extend_from_slice(b"\x1b\\");
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
        "\x1b_Ga=p,I={},p={},x={},y={},w={},h={},",
        placement.image_number,
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

/// Write a Kitty command that deletes one image's data.
pub fn write_kitty_image_delete<W: Write>(
    writer: &mut W,
    image_number: u32,
) -> Result<(), KittyOutputError> {
    if image_number == 0 {
        return Err(KittyOutputError::InvalidImageNumber);
    }
    write!(writer, "\x1b_Ga=d,d=N,I={image_number},q=2;\x1b\\")?;
    Ok(())
}

/// Write a Kitty command that deletes one placement and retains image data.
pub fn write_kitty_placement_delete<W: Write>(
    writer: &mut W,
    image_number: u32,
    placement_id: u32,
) -> Result<(), KittyOutputError> {
    if image_number == 0 {
        return Err(KittyOutputError::InvalidImageNumber);
    }
    if placement_id == 0 {
        return Err(KittyOutputError::InvalidPlacementId);
    }
    write!(
        writer,
        "\x1b_Ga=d,d=n,I={image_number},p={placement_id},q=2;\x1b\\"
    )?;
    Ok(())
}

/// Write a Kitty command that deletes all visible placements and image data.
pub fn write_kitty_delete_all<W: Write>(writer: &mut W) -> Result<(), KittyOutputError> {
    writer.write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\")?;
    Ok(())
}

/// Write a Kitty command that deletes all visible placements and retains every
/// image's data. An image number transmitted before this command still places.
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
    validate_image_dimensions(KITTY_PROTOCOL, image_width_pixels, image_height_pixels).map_err(
        |_| KittyOutputError::InvalidImageDimensions {
            image_width_pixels: decoded_image.pixel_width,
            image_height_pixels: decoded_image.pixel_height,
        },
    )?;
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
    if placement.image_number == 0 {
        return Err(KittyOutputError::InvalidImageNumber);
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
