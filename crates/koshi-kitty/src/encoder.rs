//! Bounded Kitty image encoding and output.

use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::{Compress, Compression, FlushCompress, Status};
use koshi_image::{checked_rgba_len, validate_dimensions, DecodedImage, GraphicsProtocol};
use thiserror::Error;

const KITTY_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Kitty;

/// The largest compressed byte slice in one Kitty graphics chunk.
pub const KITTY_IMAGE_CHUNK_BYTES: usize = 3_072;

/// The largest number of Kitty graphics chunks emitted by one advance.
pub const KITTY_IMAGE_CHUNKS_PER_STEP: usize = 16;

/// The largest number of raw RGBA bytes compressed by one advance.
pub const KITTY_COMPRESSION_INPUT_BYTES_PER_STEP: usize = 262_144;

/// The scratch output capacity used by one compression pass.
pub const KITTY_COMPRESSION_OUTPUT_BYTES: usize = 65_536;

/// Kitty placement fields written by the placement command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyPlacement {
    /// The Kitty image number to place.
    pub image_number: u32,
    /// The Kitty placement identity.
    pub placement_id: u32,
    /// The source rectangle's left pixel.
    pub source_x: u32,
    /// The source rectangle's top pixel.
    pub source_y: u32,
    /// The source rectangle's width in pixels.
    pub source_width: u32,
    /// The source rectangle's height in pixels.
    pub source_height: u32,
    /// The destination rectangle's width in cells.
    pub columns: u32,
    /// The destination rectangle's height in cells.
    pub rows: u32,
    /// The first-cell horizontal pixel offset.
    pub cell_offset_x: Option<u32>,
    /// The first-cell vertical pixel offset.
    pub cell_offset_y: Option<u32>,
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
    #[error("Kitty image dimensions {width}x{height} are invalid")]
    InvalidImageDimensions { width: u32, height: u32 },
    /// The decoded image RGBA byte count does not match its dimensions.
    #[error(
        "Kitty image RGBA length {actual} does not match {expected} bytes for {width}x{height}"
    )]
    InvalidImageData {
        /// The image width in pixels.
        width: u32,
        /// The image height in pixels.
        height: u32,
        /// The expected RGBA byte count.
        expected: usize,
        /// The received RGBA byte count.
        actual: usize,
    },
    /// The destination cell rectangle has a zero side.
    #[error("Kitty placement dimensions {columns}x{rows} are invalid")]
    InvalidPlacementDimensions { columns: u32, rows: u32 },
    /// The source rectangle does not fit inside the decoded image.
    #[error(
        "Kitty source rectangle ({x},{y}) {width}x{height} exceeds image {image_width}x{image_height}"
    )]
    InvalidSourceRect {
        /// The source rectangle's left pixel.
        x: u32,
        /// The source rectangle's top pixel.
        y: u32,
        /// The source rectangle's width in pixels.
        width: u32,
        /// The source rectangle's height in pixels.
        height: u32,
        /// The decoded image width in pixels.
        image_width: u32,
        /// The decoded image height in pixels.
        image_height: u32,
    },
    /// A vector allocation failed while preparing output.
    #[error("Kitty output allocation failed")]
    AllocationFailed,
    /// The zlib compressor returned an error.
    #[error("Kitty image compression failed: {message}")]
    Compression {
        /// The compressor's error text.
        message: String,
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
    image: Arc<DecodedImage>,
    image_number: u32,
    compressor: Compress,
    input_offset: usize,
    compressed: Vec<u8>,
    compressed_offset: usize,
    compression_complete: bool,
    transmission_started: bool,
}

impl fmt::Debug for KittyUpload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KittyUpload")
            .field("image_width", &self.image.width)
            .field("image_height", &self.image.height)
            .field("image_number", &self.image_number)
            .field("input_offset", &self.input_offset)
            .field("compressed_len", &self.compressed.len())
            .field("compressed_offset", &self.compressed_offset)
            .field("compression_complete", &self.compression_complete)
            .field("transmission_started", &self.transmission_started)
            .finish()
    }
}

impl KittyUpload {
    /// Create a bounded zlib upload for one decoded image.
    pub fn new(image: Arc<DecodedImage>, image_number: u32) -> Result<Self, KittyOutputError> {
        if image_number == 0 {
            return Err(KittyOutputError::InvalidImageNumber);
        }
        validate_image(&image)?;
        let mut compressed = Vec::new();
        compressed
            .try_reserve_exact(KITTY_COMPRESSION_OUTPUT_BYTES)
            .map_err(|_| KittyOutputError::AllocationFailed)?;
        Ok(Self {
            image,
            image_number,
            compressor: Compress::new(Compression::fast(), true),
            input_offset: 0,
            compressed,
            compressed_offset: 0,
            compression_complete: false,
            transmission_started: false,
        })
    }

    /// Return whether one complete Kitty chunk batch was written.
    #[must_use]
    pub fn started(&self) -> bool {
        self.transmission_started
    }

    /// Return the shared decoded image retained by the upload.
    #[must_use]
    pub fn image(&self) -> &Arc<DecodedImage> {
        &self.image
    }

    /// Return the nonzero Kitty image number used by the upload.
    #[must_use]
    pub fn image_number(&self) -> u32 {
        self.image_number
    }

    /// Return whether compression and transmission reached their ends.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.compression_complete
            && self.compressed_offset == self.compressed.len()
            && self.transmission_started
    }

    /// Compress and write one bounded Kitty output step.
    pub fn advance<W: Write>(&mut self, writer: &mut W) -> Result<(), KittyOutputError> {
        if !self.compression_complete
            && self.compressed.len().saturating_sub(self.compressed_offset)
                <= KITTY_IMAGE_CHUNK_BYTES
        {
            self.compress_step()?;
        }
        self.write_chunks(writer)
    }

    fn compress_step(&mut self) -> Result<(), KittyOutputError> {
        if self.compressed_offset != 0 {
            self.compressed.copy_within(self.compressed_offset.., 0);
            self.compressed
                .truncate(self.compressed.len() - self.compressed_offset);
            self.compressed_offset = 0;
        }
        let input_end = self
            .input_offset
            .saturating_add(KITTY_COMPRESSION_INPUT_BYTES_PER_STEP)
            .min(self.image.rgba.len());
        let flush = if input_end == self.image.rgba.len() {
            FlushCompress::Finish
        } else {
            FlushCompress::None
        };
        let mut output = [0u8; KITTY_COMPRESSION_OUTPUT_BYTES];
        loop {
            let input_before = self.compressor.total_in();
            let output_before = self.compressor.total_out();
            let status = self
                .compressor
                .compress(
                    &self.image.rgba[self.input_offset..input_end],
                    &mut output,
                    flush,
                )
                .map_err(|error| KittyOutputError::Compression {
                    message: error.to_string(),
                })?;
            let consumed = usize::try_from(self.compressor.total_in().saturating_sub(input_before))
                .map_err(|_| KittyOutputError::CompressionNoProgress)?;
            let produced =
                usize::try_from(self.compressor.total_out().saturating_sub(output_before))
                    .map_err(|_| KittyOutputError::CompressionNoProgress)?;
            self.input_offset = self
                .input_offset
                .checked_add(consumed)
                .ok_or(KittyOutputError::CompressionNoProgress)?;
            self.compressed
                .try_reserve(produced)
                .map_err(|_| KittyOutputError::AllocationFailed)?;
            self.compressed.extend_from_slice(&output[..produced]);
            if status == Status::StreamEnd {
                self.compression_complete = true;
                return Ok(());
            }
            if self.input_offset == input_end && input_end < self.image.rgba.len() {
                return Ok(());
            }
            if consumed == 0 && produced == 0 {
                return Err(KittyOutputError::CompressionNoProgress);
            }
        }
    }

    fn write_chunks<W: Write>(&mut self, writer: &mut W) -> Result<(), KittyOutputError> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(KITTY_IMAGE_CHUNKS_PER_STEP * 4_160)
            .map_err(|_| KittyOutputError::AllocationFailed)?;
        let mut next_offset = self.compressed_offset;
        let mut first = !self.transmission_started;
        let mut chunks = 0usize;
        while chunks < KITTY_IMAGE_CHUNKS_PER_STEP {
            let available = self.compressed.len().saturating_sub(next_offset);
            if available == 0
                || (!self.compression_complete && available <= KITTY_IMAGE_CHUNK_BYTES)
            {
                break;
            }
            let chunk_len = available.min(KITTY_IMAGE_CHUNK_BYTES);
            let end = next_offset + chunk_len;
            let more = !self.compression_complete || end < self.compressed.len();
            write_upload_chunk(
                &mut output,
                &self.image,
                self.image_number,
                first,
                more,
                &self.compressed[next_offset..end],
            )?;
            first = false;
            next_offset = end;
            chunks += 1;
        }
        if output.is_empty() {
            return Ok(());
        }
        writer.write_all(&output).map_err(KittyOutputError::Io)?;
        writer.flush().map_err(KittyOutputError::Io)?;
        self.compressed_offset = next_offset;
        self.transmission_started = true;
        Ok(())
    }
}

fn write_upload_chunk(
    output: &mut Vec<u8>,
    image: &DecodedImage,
    image_number: u32,
    first: bool,
    more: bool,
    bytes: &[u8],
) -> Result<(), KittyOutputError> {
    if first {
        write!(
            output,
            "\x1b_Ga=t,f=32,s={},v={},I={},q=2,o=z,m={};",
            image.width,
            image.height,
            image_number,
            u8::from(more),
        )?;
    } else {
        write!(output, "\x1b_Gq=2,m={};", u8::from(more))?;
    }
    let mut encoded = [0u8; 4_096];
    let encoded_len = STANDARD
        .encode_slice(bytes, &mut encoded)
        .map_err(|_| KittyOutputError::EncodedChunkTooLarge)?;
    output.extend_from_slice(&encoded[..encoded_len]);
    output.extend_from_slice(b"\x1b\\");
    Ok(())
}

/// Write one Kitty placement command.
pub fn write_kitty_placement<W: Write>(
    writer: &mut W,
    image: &DecodedImage,
    placement: &KittyPlacement,
) -> Result<(), KittyOutputError> {
    validate_placement(image, placement)?;
    write!(
        writer,
        "\x1b_Ga=p,I={},p={},x={},y={},w={},h={},",
        placement.image_number,
        placement.placement_id,
        placement.source_x,
        placement.source_y,
        placement.source_width,
        placement.source_height,
    )?;
    if let Some(offset) = placement.cell_offset_x {
        write!(writer, "X={offset},")?;
    }
    if let Some(offset) = placement.cell_offset_y {
        write!(writer, "Y={offset},")?;
    }
    write!(
        writer,
        "c={},r={},C=1,z={},q=2;\x1b\\",
        placement.columns, placement.rows, placement.z_index,
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

/// Write the cancellation sequence for an open Kitty APC transfer.
pub fn write_kitty_abort<W: Write>(writer: &mut W) -> Result<(), KittyOutputError> {
    writer.write_all(b"\x18\x1b\\")?;
    Ok(())
}

fn validate_image(image: &DecodedImage) -> Result<(), KittyOutputError> {
    let width =
        usize::try_from(image.width).map_err(|_| KittyOutputError::InvalidImageDimensions {
            width: image.width,
            height: image.height,
        })?;
    let height =
        usize::try_from(image.height).map_err(|_| KittyOutputError::InvalidImageDimensions {
            width: image.width,
            height: image.height,
        })?;
    validate_dimensions(KITTY_PROTOCOL, width, height).map_err(|_| {
        KittyOutputError::InvalidImageDimensions {
            width: image.width,
            height: image.height,
        }
    })?;
    let expected = checked_rgba_len(KITTY_PROTOCOL, width, height).map_err(|_| {
        KittyOutputError::InvalidImageDimensions {
            width: image.width,
            height: image.height,
        }
    })?;
    if image.rgba.len() != expected {
        return Err(KittyOutputError::InvalidImageData {
            width: image.width,
            height: image.height,
            expected,
            actual: image.rgba.len(),
        });
    }
    Ok(())
}

fn validate_placement(
    image: &DecodedImage,
    placement: &KittyPlacement,
) -> Result<(), KittyOutputError> {
    if placement.image_number == 0 {
        return Err(KittyOutputError::InvalidImageNumber);
    }
    if placement.placement_id == 0 {
        return Err(KittyOutputError::InvalidPlacementId);
    }
    if placement.columns == 0 || placement.rows == 0 {
        return Err(KittyOutputError::InvalidPlacementDimensions {
            columns: placement.columns,
            rows: placement.rows,
        });
    }
    validate_image(image)?;
    let valid = placement.source_width > 0
        && placement.source_height > 0
        && placement
            .source_x
            .checked_add(placement.source_width)
            .is_some_and(|end| end <= image.width)
        && placement
            .source_y
            .checked_add(placement.source_height)
            .is_some_and(|end| end <= image.height);
    if !valid {
        return Err(KittyOutputError::InvalidSourceRect {
            x: placement.source_x,
            y: placement.source_y,
            width: placement.source_width,
            height: placement.source_height,
            image_width: image.width,
            image_height: image.height,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
